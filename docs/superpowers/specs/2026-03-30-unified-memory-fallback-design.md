# 统一内存降级链设计：SHM Pool 动态扩容 + 磁盘溢写兜底

> **Status: ✅ COMPLETE** — Full three-tier memory system implemented in `c2-mem` crate: buddy SHM (dynamic expansion), dedicated SHM, and file-spill fallback. See `disk-spill-memhandle-design.md` for final implementation details.

> Date: 2026-03-30
> Status: Approved
> Scope: `c_two._native/c2-buddy` (Rust) + `c_two.transport` (Python)
> Roadmap: §2.1 SHM Pool 动态扩容 + §3.0 磁盘溢写兜底

---

## 1. 问题与目标

### 1.1 当前痛点

**SHM Pool 退化**: 并发高频请求下，buddy pool 的单一 segment (256 MB) 被耗尽时，退化到 per-request SHM（每次 `shm_open` + `ftruncate` + `mmap` + `munmap` + `shm_unlink` = 5 次系统调用），导致吞吐量骤降。

**大 payload OOM**: Chunked Streaming 的 reassembly buffer 使用 `mmap.mmap(-1, total_size)` 匿名映射。当系统内存不足时，直接抛出 `MemoryPressureError`，无降级路径。

### 1.2 设计目标

- **对 transport 层透明**: `pool.alloc(size)` 返回统一的 `PoolAlloc`，transport 不关心数据在哪一层
- **4 层自动降级**: buddy pool → 扩容新 segment → 独占 SHM → 磁盘文件
- **无通告协议**: 段名确定性可推导，对端惰性打开，零额外帧开销
- **帧头统一**: 所有 SHM-class 数据（>4KB）使用同一种 buddy frame 格式
- **空闲自动回收**: 空闲 segment 按超时 GC，降低长期内存占用

### 1.3 降级链总览

```
请求 payload 到达 transport 层:

  ≤ 4KB   →  inline UDS frame（现有，不变）
  > 4KB   →  pool.alloc(size) 进入 4-tier fallback:

  ┌─────────────────────────────────────────────────────┐
  │ Tier 1: 现有 buddy segment                          │
  │   零系统调用，spinlock-protected 位图分配             │
  │   条件: 现有 segment 有足够连续空间                   │
  ├─────────────────────────────────────────────────────┤
  │ Tier 2: 新建 buddy segment                          │
  │   5 syscall 首次创建，之后零系统调用                  │
  │   条件: segments.len() < max_pool_segments           │
  ├─────────────────────────────────────────────────────┤
  │ Tier 3: 独占 SHM segment (dedicated)                │
  │   5 syscall per alloc, one-to-one mapping            │
  │   条件: dedicated_count < max_dedicated_segments     │
  ├─────────────────────────────────────────────────────┤
  │ Tier 4: 磁盘文件 segment                            │
  │   file open + ftruncate + mmap, ~5-7x slower         │
  │   条件: enable_disk_spill = true                     │
  └─────────────────────────────────────────────────────┘

  全部失败 → MemoryPressureError
```

### 1.4 非目标

- 不做向后兼容（当前 0.x.x，无线上用户）
- 不改 inline 路径（≤4KB 小消息不变）
- 不做跨机磁盘共享（跨机走 HTTP relay，无 SHM）
- 不改 Chunked Streaming 的分片逻辑（仅改 reassembly buffer 的内存策略）

---

## 2. Rust 侧改动 — BuddyPool 扩展

### 2.1 核心数据结构变更

**`pool.rs`: `Vec<ShmSegment>` → `HashMap<u32, ShmSegment>`**

现有设计中 `seg_idx` 等于 Vec 的下标。当 `gc_buddy()` 弹出末尾段并重新创建时，新段复用旧下标但 SHM name 不同，导致消费端无法从 `seg_idx` 推导段名。

改为以 monotonic counter 作为 `seg_idx`，永不复用：

```rust
pub struct BuddyPool {
    segments: HashMap<u32, ShmSegment>,       // key = seg_idx = counter
    dedicated_shm: HashMap<u32, DedicatedSegment>,
    dedicated_disk: HashMap<u32, DiskSegment>, // 新增 Tier 4
    next_counter: u32,                         // 全局单调递增，覆盖 buddy + dedicated
    config: PoolConfig,
    name_prefix: String,                       // e.g. "cc3b0000abcd"
    idle_since: HashMap<u32, Option<Instant>>,  // 空闲追踪
}
```

### 2.2 AllocTier 枚举

替代原有 `is_dedicated: bool`：

```rust
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum AllocTier {
    Buddy = 0,           // Tier 1-2
    DedicatedShm = 1,    // Tier 3
    DedicatedDisk = 2,   // Tier 4
}

pub struct PoolAlloc {
    pub seg_idx: u32,
    pub offset: usize,
    pub actual_size: usize,
    pub level: u8,          // buddy level (0 for dedicated)
    pub tier: AllocTier,    // 替代 is_dedicated
}
```

PyO3 暴露: `AllocTier` 作为 `int` 传到 Python 层（0/1/2）。

### 2.3 PoolConfig 扩展

```rust
pub struct PoolConfig {
    pub segment_size: usize,              // 256 MB
    pub min_block_size: usize,            // 4 KB
    pub max_segments: usize,              // 4 (buddy segments)
    pub max_dedicated_segments: usize,    // 4 (SHM dedicated)
    pub dedicated_gc_delay_secs: f64,     // 5.0s

    // 新增
    pub enable_disk_spill: bool,          // false by default
    pub disk_spill_dir: String,           // "/tmp/c2disk"
    pub max_disk_segments: usize,         // 8
}
```

### 2.4 确定性命名规则

```rust
impl BuddyPool {
    /// 根据 seg_idx + tier 推导段名/路径
    pub fn derive_name(&self, idx: u32, tier: AllocTier) -> String {
        let tag = match tier {
            AllocTier::Buddy => "b",
            AllocTier::DedicatedShm => "d",
            AllocTier::DedicatedDisk => "f",
        };
        if tier == AllocTier::DedicatedDisk {
            format!("{}/{}_f{:04x}", self.config.disk_spill_dir, self.name_prefix, idx)
        } else {
            format!("/{}{}{:04x}", self.name_prefix, tag, idx)
        }
    }

    /// 暴露给 Python，用于握手交换
    pub fn prefix(&self) -> &str { &self.name_prefix }
}
```

### 2.5 分配 fallback 链 (`alloc()` 方法)

```rust
pub fn alloc(&mut self, size: usize) -> Option<PoolAlloc> {
    // Tier 1: 尝试现有 buddy segments
    for (&idx, seg) in &self.segments {
        if seg.free_bytes() >= size {
            if let Some((offset, level)) = seg.alloc(size) {
                self.idle_since.insert(idx, None); // mark active
                return Some(PoolAlloc { seg_idx: idx, offset, actual_size: size, level, tier: AllocTier::Buddy });
            }
        }
    }

    // Tier 2: 创建新 buddy segment
    if self.segments.len() < self.config.max_segments {
        let idx = self.next_counter;
        self.next_counter += 1;
        let name = self.derive_name(idx, AllocTier::Buddy);
        let seg = ShmSegment::create(&name, self.config.segment_size, self.config.min_block_size)?;
        self.segments.insert(idx, seg);
        let (offset, level) = self.segments.get(&idx)?.alloc(size)?;
        return Some(PoolAlloc { seg_idx: idx, offset, actual_size: size, level, tier: AllocTier::Buddy });
    }

    // Tier 3: 独占 SHM segment
    if self.active_dedicated_shm() < self.config.max_dedicated_segments {
        let idx = self.next_counter;
        self.next_counter += 1;
        let name = self.derive_name(idx, AllocTier::DedicatedShm);
        match DedicatedSegment::create(&name, size) {
            Ok(seg) => {
                self.dedicated_shm.insert(idx, seg);
                return Some(PoolAlloc { seg_idx: idx, offset: 0, actual_size: size, level: 0, tier: AllocTier::DedicatedShm });
            }
            Err(_) => {} // SHM 创建失败，继续尝试 Tier 4
        }
    }

    // Tier 4: 磁盘文件 segment
    if self.config.enable_disk_spill && self.active_disk() < self.config.max_disk_segments {
        let idx = self.next_counter;
        self.next_counter += 1;
        let path = self.derive_name(idx, AllocTier::DedicatedDisk);
        match DiskSegment::create(&path, size) {
            Ok(seg) => {
                self.dedicated_disk.insert(idx, seg);
                return Some(PoolAlloc { seg_idx: idx, offset: 0, actual_size: size, level: 0, tier: AllocTier::DedicatedDisk });
            }
            Err(_) => {}
        }
    }

    None // 全部耗尽
}
```

### 2.6 DiskSegment 实现

```rust
// segment.rs — 新增

pub struct DiskSegment {
    path: PathBuf,
    base: *mut u8,
    size: usize,
    is_owner: bool,
}

impl DiskSegment {
    pub fn create(path: &str, size: usize) -> Result<Self> {
        let aligned = (size + 4095) & !4095; // page-align
        let fd = OpenOptions::new()
            .read(true).write(true).create_new(true)
            .open(path)?;
        fd.set_len(aligned as u64)?;
        let base = unsafe {
            libc::mmap(ptr::null_mut(), aligned, PROT_READ | PROT_WRITE,
                       MAP_SHARED, fd.as_raw_fd(), 0) as *mut u8
        };
        drop(fd); // fd no longer needed after mmap
        Ok(Self { path: path.into(), base, size: aligned, is_owner: true })
    }

    pub fn open(path: &str) -> Result<Self> {
        let fd = OpenOptions::new().read(true).open(path)?;
        let size = fd.metadata()?.len() as usize;
        let base = unsafe {
            libc::mmap(ptr::null_mut(), size, PROT_READ,
                       MAP_SHARED, fd.as_raw_fd(), 0) as *mut u8
        };
        drop(fd);
        Ok(Self { path: path.into(), base, size, is_owner: false })
    }
}

impl Drop for DiskSegment {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.base as *mut _, self.size); }
        if self.is_owner {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}
```

### 2.7 GC 适配

`gc_buddy()` 从"弹出末尾"改为"遍历 HashMap，按 idle 时间回收"：

```rust
pub fn gc_buddy(&mut self) -> usize {
    let now = Instant::now();
    let threshold = Duration::from_secs_f64(self.config.dedicated_gc_delay_secs);
    let mut removed = 0;

    // 保留至少 1 个 buddy segment
    if self.segments.len() <= 1 { return 0; }

    let to_remove: Vec<u32> = self.segments.iter()
        .filter(|(idx, seg)| {
            seg.alloc_count() == 0  // 全部释放
            && self.idle_since.get(idx).and_then(|t| *t)
                .map_or(false, |t| now - t > threshold)
        })
        .map(|(&idx, _)| idx)
        .collect();

    for idx in to_remove {
        if self.segments.len() <= 1 { break; }
        self.segments.remove(&idx);
        self.idle_since.remove(&idx);
        removed += 1;
    }

    // GC dedicated SHM + disk (same pattern)
    // ...

    removed
}
```

---

## 3. 协议改动 — 确定性命名 + 惰性打开

### 3.1 设计原则: 无通告协议

传统做法需要 `CTRL_SEGMENT_ANNOUNCE` 帧在数据帧之前通告新段。本设计完全消除该需求：

- `seg_idx` 全局单调递增，永不复用
- 段名由 `pool_prefix + seg_idx + tier` 确定性推导
- 消费端首次遇到未知 `seg_idx` 时，惰性推导名称并打开
- UDS FIFO 保序确保帧到达时段已完全就绪（生产端先创建段，再发帧）

### 3.2 握手扩展

在现有字段之后追加 `pool_prefix` 和 `disk_dir`：

```
[现有 handshake fields: version, segments, capability_flags]
[1B prefix_len][prefix UTF-8]         ← 新增: e.g. "cc3b0000abcd"
[1B disk_dir_len][disk_dir UTF-8]     ← 新增: e.g. "/tmp/c2disk" (无磁盘能力时 len=0)
```

消费端缓存 `peer_prefix` 和 `peer_disk_dir` 为连接状态。

### 3.3 Buddy Payload 帧头变更

```
现有 (11 bytes):
  [2B seg_idx LE][4B offset LE][4B data_size LE][1B flags]
  flags: bit 0 = is_dedicated, bit 1 = reuse

改为 (13 bytes):
  [4B seg_idx LE][4B offset LE][4B data_size LE][1B flags]
  flags: bit 0-1 = tier (00=buddy, 01=dedicated_shm, 10=file)
         bit 2 = reuse
```

变化: `seg_idx` 从 `u16` 扩展到 `u32`（支持单调递增 counter 不溢出），总大小 +2B。

### 3.4 能力协商

```python
CAP_DISK_SPILL = 1 << 3  # 新增

# 仅当双方都声明 CAP_DISK_SPILL 时允许 Tier 4
# 否则 Tier 3 失败后直接 MemoryPressureError
```

### 3.5 段名推导公式

```python
def derive_segment_name(prefix: str, seg_idx: int, tier: int, disk_dir: str = '') -> str:
    if tier == TIER_BUDDY:
        return f'/{prefix}_b{seg_idx:04x}'
    elif tier == TIER_DEDICATED_SHM:
        return f'/{prefix}_d{seg_idx:04x}'
    elif tier == TIER_DISK:
        return f'{disk_dir}/{prefix}_f{seg_idx:04x}'
```

---

## 4. Python Transport 层改动

### 4.1 Client 侧 (`client/core.py`)

**删除 dedicated 拒绝逻辑** (现有 lines 549-557)。>4KB 全走 buddy frame:

```python
def _try_buddy_alloc(self, size, label=''):
    if size <= self._config.shm_threshold or self._buddy_pool is None:
        return None, None          # ≤4KB → inline

    with self._alloc_lock:
        alloc = self._buddy_pool.alloc(size)  # Rust 4-tier fallback
        if alloc is None:
            if size <= self._config.max_frame_size:
                return None, None  # inline fallback for small payloads
            raise error.MemoryPressureError(...)

        # 统一处理所有 tier — 不再拒绝 dedicated
        seg_mv = self._get_or_open_local_seg(alloc.seg_idx, alloc.tier)
        shm_buf = seg_mv[alloc.offset : alloc.offset + alloc.actual_size]
        return alloc, shm_buf
```

**本端段的惰性获取**:

```python
def _get_or_open_local_seg(self, seg_idx: int, tier: int) -> memoryview:
    """本端 pool 创建的段 — 从 pool 获取 data pointer。"""
    if seg_idx in self._seg_views:
        return self._seg_views[seg_idx]
    base, size = self._buddy_pool.segment_info(seg_idx)
    mv = memoryview((ctypes.c_char * size).from_address(base)).cast('B')
    self._seg_views[seg_idx] = mv
    return mv
```

**对端段的惰性打开** (recv_loop 中用于读取 server reply):

```python
def _open_peer_seg(self, seg_idx: int, tier: int) -> memoryview:
    """对端 pool 创建的段 — 根据 peer_prefix + seg_idx + tier 推导并打开。"""
    if seg_idx in self._peer_seg_views:
        return self._peer_seg_views[seg_idx]

    name = derive_segment_name(self._peer_prefix, seg_idx, tier, self._peer_disk_dir)
    if tier == TIER_DISK:
        fd = os.open(name, os.O_RDONLY)
    else:
        fd = posix_ipc.SharedMemory(name, flags=0).fd  # O_RDONLY
    size = os.fstat(fd).st_size
    buf = mmap.mmap(fd, size, access=mmap.ACCESS_READ)
    os.close(fd)
    self._peer_seg_views[seg_idx] = memoryview(buf)
    return self._peer_seg_views[seg_idx]
```

### 4.2 Server 侧 (`server/core.py` + `server/handshake.py`)

**`open_segments()` 改动**:
- 不再 `max_segments=len(segments)` 固定
- 从 `IPCConfig.max_pool_segments` 读取容量
- 缓存 `conn.peer_prefix` 和 `conn.peer_disk_dir`

**帧处理改动**:
- 收到含未知 `seg_idx` 的 buddy frame → 调用 `_open_peer_seg()` 惰性打开
- Server 分配 reply buffer 走 `pool.alloc()` — 新 segment 由 client 惰性打开

### 4.3 帧编码统一

```python
# buddy.py — 新编码
BUDDY_PAYLOAD_STRUCT = struct.Struct('<IIIB')  # seg_idx(u32), offset(u32), size(u32), flags(u8)

def encode_buddy_payload(seg_idx: int, offset: int, data_size: int,
                          tier: int, reuse: bool = False) -> bytes:
    flags = (tier & 0x03) | (0x04 if reuse else 0x00)
    buf = bytearray(13)
    struct.pack_into('<III', buf, 0, seg_idx, offset, data_size)
    buf[12] = flags
    return bytes(buf)

def decode_buddy_payload(payload: bytes | memoryview) -> tuple:
    seg_idx, offset, data_size = struct.unpack_from('<III', payload, 0)
    flags = payload[12]
    tier = flags & 0x03
    reuse = bool(flags & 0x04)
    return seg_idx, offset, data_size, tier, reuse
```

### 4.4 Reassembly Buffer 磁盘降级

`ChunkAssembler` 和 `_ReplyChunkAssembler` 的 reassembly buffer 是本地缓冲区（非跨进程），独立于 buddy pool 处理：

```python
def _create_reassembly_buffer(alloc_size: int, config: IPCConfig) -> mmap.mmap:
    """Anonymous mmap with disk spill fallback."""
    # 阈值主动降级
    if config.disk_spill_threshold and alloc_size > config.disk_spill_threshold:
        return _create_disk_buffer(alloc_size, config.disk_spill_dir)
    # 正常路径
    try:
        return mmap.mmap(-1, alloc_size)
    except OSError:
        logger.warning('Anonymous mmap(%d) failed, falling back to disk', alloc_size)
        return _create_disk_buffer(alloc_size, config.disk_spill_dir)

def _create_disk_buffer(size: int, spill_dir: str) -> mmap.mmap:
    os.makedirs(spill_dir, exist_ok=True)
    fd, path = tempfile.mkstemp(dir=spill_dir, prefix='c2buf_')
    try:
        os.ftruncate(fd, size)
        buf = mmap.mmap(fd, size)
    finally:
        os.close(fd)
    os.unlink(path)  # fd keeps file alive; process exit auto-cleans
    return buf
```

---

## 5. 配置项汇总

### 5.1 IPCConfig 新增字段

```python
@dataclass
class IPCConfig:
    # --- 现有（不变）---
    pool_enabled: bool = True
    pool_segment_size: int = 256 MB
    max_pool_segments: int = 4
    max_pool_memory: int = 1 GB
    pool_decay_seconds: float = 60.0
    # ...

    # --- 新增 ---
    max_dedicated_shm_segments: int = 4     # Tier 3 独占 SHM 上限
    enable_disk_spill: bool = False          # Tier 4 磁盘溢写开关
    disk_spill_dir: str = '/tmp/c2disk'      # 溢写文件目录
    max_disk_segments: int = 8               # Tier 4 磁盘段上限
    disk_spill_threshold: int = 0            # reassembly 主动降级阈值 (0=仅 OOM)
```

### 5.2 PoolConfig (Rust) 对应

```rust
pub struct PoolConfig {
    pub segment_size: usize,
    pub min_block_size: usize,
    pub max_segments: usize,               // buddy segments cap
    pub max_dedicated_segments: usize,     // Tier 3
    pub dedicated_gc_delay_secs: f64,
    pub enable_disk_spill: bool,           // 新增
    pub disk_spill_dir: String,            // 新增
    pub max_disk_segments: usize,          // 新增
}
```

---

## 6. 分批实施路线

### Phase 1 — 动态扩容核心

**目标**: buddy pool 自动扩容到 max_pool_segments，消除 per-request SHM 退化。

| 任务 | 范围 |
|------|------|
| Rust: `BuddyPool.segments` 从 `Vec` 改 `HashMap<u32, _>` | `pool.rs` |
| Rust: `seg_idx = next_counter` (monotonic) | `pool.rs` |
| Rust: `AllocTier` 枚举替代 `is_dedicated` | `pool.rs`, `ffi.rs` |
| Rust: `derive_name()` + `prefix()` API | `pool.rs` |
| Rust: `gc_buddy()` 适配 HashMap | `pool.rs` |
| Python: buddy payload `u16 → u32` | `ipc/buddy.py`, `wire.py` |
| Python: 删除 dedicated 拒绝逻辑 | `client/core.py` |
| Python: `_get_or_open_local_seg()` + `_open_peer_seg()` | `client/core.py`, `server/core.py` |
| Python: 握手增加 `prefix` + `disk_dir` | `protocol.py`, `server/handshake.py` |
| Python: `PoolConfig(max_segments=N)` 从 IPCConfig 读取 | `client/core.py` init |
| 测试: 多 segment 分配/释放/GC | 单元 + 集成 |
| Benchmark: per-request 退化频率对比 | measure_v3.py |

### Phase 2 — 磁盘溢写兜底

**目标**: SHM 全部耗尽时降级到磁盘文件，不再 MemoryPressureError。

| 任务 | 范围 |
|------|------|
| Rust: `DiskSegment` 实现 (create/open/Drop) | `segment.rs` |
| Rust: `alloc()` 第 4 层 fallback | `pool.rs` |
| Python: reassembly buffer 磁盘降级 | `server/chunk.py`, `client/core.py` |
| Python: `IPCConfig` 新增 disk_spill 配置 | `ipc/frame.py` |
| Python: `CAP_DISK_SPILL` 能力协商 | `protocol.py` |
| 测试: mmap 失败模拟 + 磁盘降级 E2E | 单元 + 集成 |

### Phase 3 — 空闲回收与监控

**目标**: 扩容后的 segment 在空闲时自动回收，降低内存占用。

| 任务 | 范围 |
|------|------|
| Rust: 基于 idle 时间的 buddy/dedicated/disk GC | `pool.rs` |
| Python: 消费端 seg_views GC (对端段 unlink 后清理) | `client/core.py`, `server/core.py` |
| 监控: `PoolStats` 增加 per-tier 分配/GC 计数 | `pool.rs`, `ffi.rs` |
| 文档: 更新 roadmap + architecture 文档 | docs/ |

---

## 7. 测试策略

### 7.1 单元测试

| 测试点 | 覆盖 |
|--------|------|
| Rust `alloc()` 4-tier fallback | 逐层触发：填满 buddy → 扩容 → dedicated → disk |
| Rust `gc_buddy()` HashMap 回收 | 空闲段被正确移除，seg_idx 不复用 |
| Rust `derive_name()` 确定性 | 同一 (prefix, idx, tier) 总产出相同名称 |
| Python `_open_peer_seg()` 惰性打开 | mock SHM segment，首次 open + 缓存命中 |
| Python reassembly buffer disk fallback | `mmap.mmap(-1, ...)` mock OSError → 走磁盘 |
| 帧编解码 roundtrip | `encode_buddy_payload(u32) ↔ decode_buddy_payload` |
| 握手 roundtrip | prefix + disk_dir 正确编解码 |

### 7.2 集成测试

| 测试点 | 覆盖 |
|--------|------|
| 多 segment 并发分配 | N 并发 client 打满 segment 0 → 自动扩容到 segment 1-3 |
| 扩容后 GC | 负载结束后空闲段在 decay_seconds 后被回收 |
| 磁盘溢写 E2E | 限制 max_pool_memory 极小值 → 触发 disk tier → 数据正确传输 |
| 惰性打开竞态 | 快速连续发送含新 seg_idx 的帧 → server 正确打开 |
| 进程崩溃清理 | producer 崩溃后，stale SHM/disk 段被 `cleanup_stale_shm()` 回收 |

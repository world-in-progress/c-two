# IPC v3 第二轮修缮 — 重点推进 Rust 侧改造

> 提供给 AI 编码助手（Claude Code / Copilot CLI / Cursor）的任务提示词。
> 本轮重点：**Rust 侧的 buddy allocator 架构补强**。前两轮修复有意回避了 Rust 代码修改，导致 4 个 IMPORTANT 级架构问题悬而未决。本轮必须直面 Rust 改造。

---

## 环境信息

- **分支**: `rust-buddy`
- **Rust crate**: `rust/c2_buddy/`（~2500 行 Rust，PyO3 FFI）
- **构建**: `cd rust/c2_buddy && cargo build --release`
- **Python 测试**: `uv run pytest -q`（686+ tests 必须全绿）
- **Rust 测试**: `cd rust/c2_buddy && cargo test`
- **可修改范围**: `rust/c2_buddy/src/*.rs` 全部可修改，`src/c_two/rpc/**/*.py` 全部可修改

---

## 约束

1. 所有 Python 测试 (`uv run pytest -q`) 和 Rust 测试 (`cargo test`) 必须通过
2. FFI 接口签名变更需同步修改 Python 调用方
3. 每个修复一个独立 commit，commit message 引用问题 ID
4. 性能不得回退——修复后运行 `uv run python measure_v3.py` 确认

---

## 第一组：Buddy Allocator 弹性伸缩（Rust 核心改造）

### Fix 1: BUDDY-NO-SHRINK — Buddy Segment 空闲回收

**文件**: `rust/c2_buddy/src/pool.rs`
**问题**: Buddy segment 一旦创建永不释放。默认 8 段 × 256MB = 2GB，突发负载后即使需求归零也不回收。
**复杂度**: 中高

**实现方案**:

1. 在 `BuddyPool` 中添加 `gc_buddy()` 方法，检测完全空闲的 buddy segment 并延迟回收：

```rust
// pool.rs — 新方法
pub fn gc_buddy(&mut self) -> usize {
    let now = std::time::Instant::now();
    let delay = std::time::Duration::from_secs_f64(
        self.config.dedicated_gc_delay_secs.max(0.0)
    );
    let mut removed = 0;

    // 从后往前遍历，保留至少 1 个 segment
    let mut i = self.segments.len();
    while i > 1 {
        i -= 1;
        let seg = &self.segments[i];
        let alloc = seg.allocator();

        // 检查是否完全空闲
        if alloc.alloc_count() > 0 {
            continue;
        }

        // 检查是否有 idle_since 标记，没有则打标
        // 需要在 segment 或 pool 中记录空闲开始时间
        // 如果空闲时间超过 delay，则移除

        // 安全移除：将最后一个 segment swap 到当前位置
        // 注意：这会改变 segment index！
        // 方案 A：只移除末尾的连续空闲 segment（不改变其他 segment 的 index）
        // 方案 B：使用 slot map / Option<ShmSegment> 代替 Vec

        // 推荐方案 A（最简单）：只 pop 末尾空闲段
        if i == self.segments.len() - 1 {
            self.segments.pop();  // 安全：不影响其他 segment 的 index
            removed += 1;
        }
    }
    removed
}
```

**关键设计决策**: Buddy segment index 被编码在 wire frame 中（`seg_idx`），因此**不能随意重排**。推荐只回收末尾连续的空闲段（从 `segments.len()-1` 向前 pop），避免 index 重映射问题。

2. 添加 idle 追踪：在 `BuddyPool` 中维护 `idle_since: Vec<Option<Instant>>`，当 segment `alloc_count` 降为 0 时记录时间，非 0 时重置为 None。

3. 在 FFI 层暴露 `gc_buddy()`（或合并到现有 `gc()` 方法中）。

4. Python 侧定期调用（可在 `reply()` 完成后或独立 timer 中调用）。

**测试**:
- Rust 测试: 创建 pool → alloc 触发 2 个 segment → 全部 free → gc_buddy → 验证 segment 数减少
- Python 测试: 确保 gc 后 alloc 仍能创建新 segment

### Fix 2: SPINLOCK-CRASH — 进程崩溃后 Spinlock 恢复

**文件**: `rust/c2_buddy/src/spinlock.rs`
**问题**: 进程在持锁时被 SIGKILL，锁永久停在 LOCKED 状态，其他进程 spin 10M 次后 panic。
**复杂度**: 中

**实现方案**:

将 spinlock 从简单的 0/1 改为 **PID-based ownership**:

```rust
// spinlock.rs — 修改 ShmSpinlock 结构
// 锁值: 0 = UNLOCKED, 非 0 = holder 的 PID

const UNLOCKED: u32 = 0;

impl ShmSpinlock {
    pub fn lock(&self) {
        let my_pid = std::process::id();
        loop {
            // 尝试 CAS: 0 → my_pid
            match self.state.compare_exchange_weak(
                UNLOCKED, my_pid,
                Ordering::Acquire, Ordering::Relaxed
            ) {
                Ok(_) => return, // 成功获锁
                Err(holder_pid) => {
                    // 检查 holder 是否仍存活
                    if holder_pid != 0 && !is_process_alive(holder_pid) {
                        // holder 已死，尝试强制解锁
                        if self.state.compare_exchange(
                            holder_pid, my_pid,
                            Ordering::Acquire, Ordering::Relaxed
                        ).is_ok() {
                            // 成功接管锁
                            // 注意：此时临界区内的数据可能不一致
                            // 可选：设置一个"需要恢复"标志
                            return;
                        }
                    }
                    // 正常自旋
                    std::hint::spin_loop();
                }
            }
        }
    }

    pub fn unlock(&self) {
        self.state.store(UNLOCKED, Ordering::Release);
    }
}

/// 检查 PID 对应的进程是否仍存活
fn is_process_alive(pid: u32) -> bool {
    // POSIX: kill(pid, 0) 检查进程是否存在
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}
```

**注意事项**:
- `kill(pid, 0)` 在 macOS 和 Linux 上均可用
- PID 复用理论上可能导致误判（概率极低），可以结合检查间隔（如只在 spin 超过 N 次后检查）
- 强制解锁后 allocator 元数据可能不一致——可在 `attach()` 时添加一致性检查（验证 bitmap 和 stats 的一致性）
- 需要将 `state: AtomicU32` 从 `0/1` 改为 `0/PID` 语义

**测试**:
- 模拟 holder 进程死亡（fork 子进程获锁后 exit），父进程应能获锁而非 panic

### Fix 3: BUDDY-NO-STALE-CLEANUP — 启动时清理 Stale SHM

**文件**: `rust/c2_buddy/src/pool.rs`, `rust/c2_buddy/src/segment.rs`
**问题**: 进程崩溃后 SHM 文件（`/cc3b*`）泄漏至重启。
**复杂度**: 中

**实现方案**:

1. 在 `BuddyPool::new()` 或单独的 `cleanup_stale()` 函数中添加启动清理：

```rust
// pool.rs — 新公共函数
pub fn cleanup_stale_segments(prefix: &str) {
    // 方案 1 (Linux): 扫描 /dev/shm/ 目录
    #[cfg(target_os = "linux")]
    {
        if let Ok(entries) = std::fs::read_dir("/dev/shm") {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if name_str.starts_with(prefix) {
                    // 从文件名提取 PID
                    if let Some(pid) = extract_pid_from_name(&name_str) {
                        if !is_process_alive(pid) {
                            // 进程已死，安全 unlink
                            let shm_name = format!("/{}", name_str);
                            unsafe { libc::shm_unlink(
                                std::ffi::CString::new(shm_name).unwrap().as_ptr()
                            ); }
                        }
                    }
                }
            }
        }
    }

    // 方案 2 (macOS): POSIX SHM 没有 /dev/shm，需要用 shm_open 探测
    // 遍历已知的命名模式尝试打开
    #[cfg(target_os = "macos")]
    {
        // macOS 上 /dev/shm 不存在
        // 策略：使用 prefix + 常见 PID 范围探测
        // 或者：在创建时将段名写入一个注册文件（如 /tmp/c2_buddy_segments.txt）
        // 推荐后者，更可靠
    }
}

fn extract_pid_from_name(name: &str) -> Option<u32> {
    // 解析 "cc3b{pid:08x}_seg{idx}" 格式
    if name.len() >= 12 && name.starts_with("cc3b") {
        u32::from_str_radix(&name[4..12], 16).ok()
    } else {
        None
    }
}
```

2. 在 FFI 层暴露：

```rust
// ffi.rs
#[pyfunction]
fn cleanup_stale_shm(prefix: &str) {
    BuddyPool::cleanup_stale_segments(prefix);
}
```

3. Python 侧在 server/client 启动时调用一次。

**macOS 替代方案**: 由于 macOS 没有 `/dev/shm` 目录，建议维护一个注册文件（如 `/tmp/c2_buddy_registry_{prefix}.json`），创建 segment 时注册、销毁时注销。启动清理时读注册文件、检查 PID、清理 stale 条目。

---

## 第二组：Rust 侧配置与防御加固

### Fix 4: CONFIG-NO-VALIDATE — Rust 侧 PoolConfig 验证

**文件**: `rust/c2_buddy/src/pool.rs`
**问题**: `BuddyPool::new()` 不验证 config，pathological 值（min_block_size=0, segment_size=0, max_segments=0）会导致 panic 或无限循环。FFI 层有验证但 Rust 原生调用者可绕过。

**实现方案**:

```rust
// pool.rs — 在 BuddyPool::new() 开头添加
impl BuddyPool {
    pub fn new(config: PoolConfig) -> Self {
        Self::validate_config(&config).expect("invalid PoolConfig");
        // ... 原有逻辑 ...
    }

    fn validate_config(config: &PoolConfig) -> Result<(), String> {
        if config.min_block_size == 0 || !config.min_block_size.is_power_of_two() {
            return Err(format!(
                "min_block_size must be a positive power of 2, got {}",
                config.min_block_size
            ));
        }
        if config.segment_size < 2 * config.min_block_size {
            return Err(format!(
                "segment_size ({}) must be >= 2 * min_block_size ({})",
                config.segment_size, config.min_block_size
            ));
        }
        if config.dedicated_gc_delay_secs < 0.0 {
            return Err("dedicated_gc_delay_secs must be >= 0".into());
        }
        if config.dedicated_gc_delay_secs.is_nan() {
            return Err("dedicated_gc_delay_secs must not be NaN".into());
        }
        Ok(())
    }
}
```

同时提供 `new_validated()` 返回 `Result` 的版本供需要 graceful error 的调用者使用。

### Fix 5: SHM-WRITE-NOLOCK — Reuse 路径 SHM 写入竞争

**文件**: `src/c_two/rpc/ipc/ipc_v3_server.py`
**问题**: 零拷贝 reuse 和 scatter-reuse 路径在 `_conn_lock` 外写入 SHM（header 覆写），并发 `destroy()` 可能 unmap 底层 SHM 导致 segfault。

**实现方案**:

在 `reply()` 中，将 reuse 路径的 SHM 写入移入 `_conn_lock` 保护范围内。但整个 reply 不应在锁内（阻塞其他连接），因此采用更细粒度的保护：

```python
# ipc_v3_server.py — reply() 中

# 方案：在 conn 上添加 per-connection 引用计数或 active_reply 标志
# destroy() 等待所有 active reply 完成后再 cleanup

# 简单方案：在 reuse 写入前再次检查 conn 是否仍然有效
with self._conn_lock:
    if conn_id not in self._connections:
        # 连接已被清理，放弃 reuse，回退到 inline
        # 释放 deferred free
        ...
        return
    # 快速写入 5 字节 header（在锁内，极短临界区）
    seg_mv[reply_start] = MsgType.CRM_REPLY
    struct.pack_into('<I', seg_mv, reply_start + 1, 0)
```

**或者更安全的方案**：在 `destroy()` 中设置一个 `_shutting_down` 标志，`reply()` 检查此标志后放弃 reuse 走 inline 回退。

---

## 第三组：Python 侧增补

### Fix 6: 测试覆盖 — Deferred Free / Request Reuse / Scatter-Reuse

**文件**: `tests/unit/test_ipc_v3.py` 或新建 `tests/unit/test_ipc_v3_lifecycle.py`
**问题**: 关键的 deferred free 生命周期、request block reuse、scatter-reuse 路径无直接测试。

**需要添加的测试**:

```python
class TestDeferredFreeLifecycle:
    """验证 client 的 buddy block 释放时序"""

    def test_buddy_block_freed_after_call(self):
        """第一次 call 分配 block，第二次 call 前释放"""
        # 检查 pool stats: alloc_count 在 call 后正确递减

    def test_close_connection_frees_deferred(self):
        """断开连接时释放所有 deferred block"""

    def test_large_response_returns_bytes_not_memoryview(self):
        """确认 call() 对 >= 1MB 响应返回 bytes（非 SHM memoryview）"""


class TestScatterReuse:
    """验证 server 的 scatter-reuse 路径"""

    def test_echo_reuses_request_block(self):
        """echo 场景：服务端 reuse 请求 block，不分配新 block"""
        # 通过 pool stats 验证: alloc_count 不增加

    def test_non_echo_allocates_new_block(self):
        """非 echo 场景：服务端分配新 block"""
```

### Fix 7: Wire 层 Method Name 缓存

**文件**: `src/c_two/rpc/util/wire.py`
**问题**: `decode()` 中每次请求都 `bytes(buf[...]).decode('utf-8')` 解码方法名。

**修复**:

```python
# wire.py — 模块级缓存
_method_name_cache: dict[bytes, str] = {}

# decode() CRM_CALL 分支中
method_raw = bytes(buf[3:header_end])
method_name = _method_name_cache.get(method_raw)
if method_name is None:
    method_name = method_raw.decode('utf-8')
    _method_name_cache[method_raw] = method_name
```

---

## 第四组：Rust 侧性能增强（可选，建议用 autoresearch 实验验证）

### Opt 1: `madvise(MADV_DONTNEED)` 释放已 free 的大块页面

**文件**: `rust/c2_buddy/src/allocator.rs`
**问题**: 大 buddy block 被 free 后，其页面仍在物理内存中驻留。OS 不知道这些页面可以回收。

**实现方案**: 在 `free_and_merge` 中，当合并后的 block 大小超过阈值（如 1MB）时，调用 `madvise(MADV_DONTNEED)` 提示内核回收页面：

```rust
// allocator.rs — free_and_merge 末尾
fn free_and_merge(&self, offset: u32, level: u16) -> Result<(), String> {
    // ... 现有 free + merge 逻辑 ...

    // 合并完成后，如果最终 block 足够大，归还页面
    let final_block_size = self.min_block << (self.levels - 1 - final_level as usize);
    if final_block_size >= 1024 * 1024 {  // >= 1MB
        let data_base = self.base as usize + self.data_offset as usize;
        let block_addr = data_base + final_offset as usize;
        unsafe {
            libc::madvise(
                block_addr as *mut libc::c_void,
                final_block_size,
                libc::MADV_DONTNEED,  // 或 MADV_FREE（macOS 推荐）
            );
        }
    }

    Ok(())
}
```

**macOS 注意**: 使用 `MADV_FREE`（延迟回收，仅标记为可回收）比 `MADV_DONTNEED`（立即丢弃）更高效。可以用 `cfg!(target_os)` 区分。

### Opt 2: Segment 级 Free-Space 快速检查（跳过满段扫描）

**文件**: `rust/c2_buddy/src/pool.rs`
**问题**: `alloc_buddy()` 线性扫描所有 segment，即使大部分已满。

**实现方案**: 在 `BuddyPool` 中维护 `largest_free_block: Vec<usize>`，每次 alloc/free 后更新。`alloc_buddy()` 跳过 `largest_free_block[i] < requested_size` 的 segment：

```rust
// pool.rs
struct BuddyPool {
    segments: Vec<ShmSegment>,
    largest_free: Vec<usize>,  // 每段最大可用 block 大小（近似值）
    // ...
}

fn alloc_buddy(&mut self, size: usize) -> Result<PoolAllocation, String> {
    for (i, seg) in self.segments.iter().enumerate() {
        if self.largest_free[i] < size {
            continue;  // 跳过明显不够的段
        }
        match seg.allocator().alloc(size) {
            Ok(alloc) => {
                // 更新 largest_free[i]（可能需要重新计算）
                return Ok(/* ... */);
            }
            Err(_) => {
                self.largest_free[i] = 0;  // 标记为满
                continue;
            }
        }
    }
    // ... 创建新段或 dedicated 降级 ...
}
```

**注意**: `largest_free` 不需要精确——它是一个上界近似值。free 时可以乐观更新为 `max(current, freed_block_size)`，alloc 失败时设为 0。

---

## 验证清单

每完成一组修复后：

```bash
# 1. Rust 编译 + 测试
cd rust/c2_buddy && cargo test && cargo build --release && cd ../..

# 2. Python 全量测试
uv run pytest -q

# 3. 性能回归检测
uv run python measure_v3.py

# 4. 检查 SHM 泄漏（Linux）
ls /dev/shm/cc3b* 2>/dev/null && echo "WARNING: stale SHM segments found"

# 5. Git diff 确认改动最小化
git diff --stat
```

---

## 相关文件速查

| 文件 | 行数 | 说明 |
|------|------|------|
| `rust/c2_buddy/src/spinlock.rs` | 180 | SHM 跨进程自旋锁 — Fix 2 主要修改 |
| `rust/c2_buddy/src/pool.rs` | 573 | 多 segment pool — Fix 1, 4, Opt 2 |
| `rust/c2_buddy/src/allocator.rs` | 585 | buddy 分配/释放 — Opt 1 |
| `rust/c2_buddy/src/segment.rs` | 366 | SHM segment 生命周期 — Fix 3 |
| `rust/c2_buddy/src/ffi.rs` | 465 | PyO3 FFI 导出 — 新接口暴露 |
| `src/c_two/rpc/ipc/ipc_v3_server.py` | ~600 | IPC v3 服务端 — Fix 5 |
| `src/c_two/rpc/ipc/ipc_v3_client.py` | ~450 | IPC v3 客户端 — Fix 6 |
| `src/c_two/rpc/util/wire.py` | ~300 | Wire codec — Fix 7 |
| `doc/op3-analysis.md` | 完整审计报告 | 所有问题的详细分析 |

---

## 不要做的事

- **不要** 回避 Rust 修改——这轮的核心就是 Rust 侧改造
- **不要** 因为 "Rust 改动影响大" 而只做 Python workaround——问题的根因在 Rust 层
- **不要** 修改现有测试的断言逻辑（可以新增测试）
- **不要** 在 commit message 中使用 `--no-verify`
- **不要** 引入新的 Rust 依赖（`libc` 已在 Cargo.toml 中，够用）
- **不要** 修改 IPC v2 代码或 MCP/CLI


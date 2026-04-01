# Disk Spill & MemHandle Unified Memory Abstraction

> **Status: ✅ COMPLETE** — All components implemented. MemHandle enum, file spill, ChunkAssembler, and PyO3 bindings all in production. 55 Rust tests + 500+ Python tests passing.

**Date:** 2026-03-31
**Status:** Approved
**Scope:** c2-mem (MemHandle, file spill), c2-wire (ChunkAssembler), c2-ffi (PyO3 bindings), Python integration

## Overview

当前 chunk reassembly 使用 Python 匿名 mmap，存在两个问题：

1. **8 GB 上限** — `max_reassembly_bytes` 硬限制，无法处理超大 payload
2. **峰值 RSS 翻倍** — `assemble()` 将 mmap 内容 `read()` 为 `bytes`，导致 payload × 2 的物理内存占用

本设计引入：
- **MemHandle** — 统一内存句柄抽象，封装 buddy SHM / dedicated SHM / 文件备份 mmap 三种后端
- **自动溢写** — Rust 层根据可用物理内存自动决策是否溢写到磁盘
- **ChunkAssembler 下沉 Rust** — 整个 chunk 重组逻辑迁移到 Rust，Python 仅通过 FFI 调用
- **零拷贝** — `assemble()` 返回 `memoryview`（buffer protocol），deserialize 直接消费，不再产生 bytes 拷贝

## Goals

- 突破 8 GB 上限，支持任意大小 payload
- 降低峰值 RSS — 溢写到磁盘时物理内存占用趋近 0
- Python 侧 API 统一：alloc / memoryview / release
- 平台支持：macOS + Linux（其他平台保守回退到文件溢写）

## Architecture

```
方案 B: MemHandle 在 c2-mem, ChunkAssembler 在 c2-wire

c2-mem/
  src/
    alloc/          (buddy 算法, bitmap, spinlock — 已有)
    segment/        (ShmRegion — 已有)
    pool.rs         (MemPool — 已有, 扩展 alloc_handle)
    handle.rs       (NEW: MemHandle enum)
    spill.rs        (NEW: 文件备份 mmap + 内存探测)
    config.rs       (扩展: spill_threshold, spill_dir)

c2-wire/
  src/
    assembler.rs    (NEW: ChunkAssembler)
    Cargo.toml      (新增依赖 c2-mem)

c2-ffi/
  src/
    mem_ffi.rs      (扩展: PyMemHandle, PyChunkAssembler)
```

## §1 MemHandle Abstraction (c2-mem)

### 类型定义

```rust
// c2-mem/src/handle.rs

use std::path::PathBuf;
use memmap2::MmapMut;

/// 统一内存句柄 — 封装三种后端存储
pub enum MemHandle {
    /// T1/T2: buddy 分配器中的块
    Buddy {
        seg_idx: u16,
        offset: u32,
        len: usize,
    },
    /// T3: 独占 SHM segment
    Dedicated {
        seg_idx: u16,
        len: usize,
    },
    /// T4: 文件备份 mmap（磁盘溢写）
    FileSpill {
        mmap: MmapMut,
        path: PathBuf,  // 仅用于日志/调试，文件已 unlink
        len: usize,
    },
}
```

### 操作接口

MemHandle **不持有** pool 引用 — Buddy/Dedicated 变体仅存储索引，实际数据访问统一通过 `MemPool` 方法进行。FileSpill 变体自包含 mmap，pool 方法内部直接委托。

```rust
impl MemHandle {
    /// 有效数据长度
    pub fn len(&self) -> usize;

    /// 显式释放底层资源
    /// - Buddy: free_at(seg_idx, offset)
    /// - Dedicated: unlink SHM segment
    /// - FileSpill: munmap（文件已 unlink，无需额外清理）
    pub fn release(self);
}

impl MemPool {
    /// 统一分配入口 — 自动选择后端
    pub fn alloc_handle(&mut self, size: usize) -> Result<MemHandle, PoolError>;

    /// 只读切片 — 用于 PyO3 buffer protocol → memoryview
    pub fn handle_slice<'a>(&'a self, handle: &'a MemHandle) -> &'a [u8];

    /// 可变切片 — 用于 chunk 写入
    pub fn handle_slice_mut<'a>(&'a mut self, handle: &'a mut MemHandle) -> &'a mut [u8];
}
```

### alloc_handle 决策流程

```
                  ┌──────────────────────┐
                  │  alloc_handle(size)   │
                  └────────┬─────────────┘
                           │
                  ┌────────▼─────────────┐
                  │ size ≤ buddy_max &&  │
                  │ existing segments    │  ← 纯 bitmap 操作，µs 级
                  │ have free space?     │
                  └──┬────────────┬──────┘
                 yes │            │ no
            ┌────────▼──┐  ┌─────▼───────────────┐
            │  Buddy    │  │ need new mapping:   │
            │  alloc()  │  │ should_spill(size)? │ ← 仅此处查 RAM
            │  ✅ done  │  └──┬──────────┬───────┘
            └───────────┘  yes│          │no
                     ┌────────▼──┐  ┌───▼──────────────┐
                     │ FileSpill │  │ size ≤ buddy_max? │
                     └───────────┘  └──┬───────────┬───┘
                                   yes │           │ no
                              ┌────────▼────┐ ┌───▼──────────┐
                              │ expand pool │ │ Dedicated    │
                              │ (new T2 seg)│ │ SHM segment  │
                              └──────┬──────┘ └──────────────┘
                                     │ fail?
                                ┌────▼────────┐
                                │ FileSpill   │ ← 扩容/创建失败兜底
                                └─────────────┘
```

**快路径优化：** 已有 buddy segment 有空间时，直接分配（纯 bitmap 操作），**不查询 RAM**。`should_spill()` 仅在需要创建新内存映射（buddy 扩容 / dedicated segment）时触发。

**TOCTOU 策略：** 不加额外锁。进程内 `RwLock<MemPool>` 已保证 `should_spill() → shm_open()` 串行执行。跨进程场景下 `should_spill` 为启发式判断，分配失败（如 `/dev/shm` 满）自动回退到 FileSpill。

## §2 File Spill Mechanism (c2-mem/src/spill.rs)

### 内存探测

```rust
// c2-mem/src/spill.rs

/// 查询系统可用物理内存
/// - macOS: host_statistics64(HOST_VM_INFO64) → (free_count + inactive_count) × page_size
/// - Linux: /proc/meminfo → MemAvailable 行
/// - 其他平台: 返回 0（保守策略 — 始终溢写到文件）
pub fn available_physical_memory() -> u64;

/// 判断是否应该溢写到磁盘
pub fn should_spill(requested: usize, threshold: f64) -> bool {
    let available = available_physical_memory();
    requested as u64 > (available as f64 * threshold) as u64
}
```

### 平台实现细节

**macOS:**
```rust
#[cfg(target_os = "macos")]
fn available_physical_memory() -> u64 {
    // mach_host_self() → host_statistics64(HOST_VM_INFO64)
    // available = (vm_stat.free_count + vm_stat.inactive_count) * page_size
    // page_size 通过 sysconf(_SC_PAGESIZE) 获取
}
```

**Linux:**
```rust
#[cfg(target_os = "linux")]
fn available_physical_memory() -> u64 {
    // 读 /proc/meminfo，解析 "MemAvailable:" 行
    // 此值由内核计算，包含 free + reclaimable cache
    // 比手动计算 free+buffers+cached 更准确
}
```

### 文件备份 mmap 创建

```rust
/// 创建文件备份的 mmap 缓冲区
pub fn create_file_spill(size: usize, spill_dir: &Path) -> Result<(MmapMut, PathBuf)> {
    // 1. fs::create_dir_all(spill_dir)
    // 2. 创建临时文件: {spill_dir}/c2_{pid}_{atomic_counter}.spill
    // 3. file.set_len(size as u64)  // ftruncate
    // 4. let mmap = unsafe { MmapMut::map_mut(&file)? };  // MAP_SHARED
    // 5. fs::remove_file(&path);  // unlink-on-create: 文件从目录消失
    //    mmap 仍有效 (POSIX 保证)，进程退出/crash 时 OS 自动回收
    // 6. Ok((mmap, path))  // path 仅用于日志
}
```

### Crash Safety 策略

采用 **unlink-on-create** 模式：
- 文件创建 + mmap 成功后立即 `unlink`（从文件系统删除）
- mmap 映射仍然有效（POSIX 保证：只要有 fd 或 mmap 引用，文件数据不会被回收）
- 正常释放：`MemHandle::release()` → `drop(MmapMut)` → `munmap()` → 数据回收
- 进程 crash：OS 自动关闭 fd + munmap → 数据回收，**无残留文件**

### 配置扩展

```rust
// c2-mem/src/config.rs (扩展已有 PoolConfig)

pub struct PoolConfig {
    // ...existing fields (initial_segments, segment_size, max_segments, etc.)...

    /// 溢写阈值比例：当 requested_size > available_ram * threshold 时启用文件备份
    /// 默认 0.8 (80%)。设为 0.0 强制始终溢写（测试用），1.0 禁用溢写
    pub spill_threshold: f64,  // default: 0.8

    /// 溢写文件目录，默认 "/tmp/c_two_spill/"
    pub spill_dir: std::path::PathBuf,
}
```

### Rust 依赖

c2-mem 新增依赖：
- `memmap2` — 跨平台 mmap 封装（已广泛使用，纯 Rust）
- `libc` — macOS `host_statistics64` FFI（Linux 路径不需要额外依赖）

## §3 ChunkAssembler (c2-wire/src/assembler.rs)

将 Python 的 `ChunkAssembler`（server/chunk.py）和 `_ReplyChunkAssembler`（client/core.py）统一下沉到 Rust。

### Rust 实现

```rust
// c2-wire/src/assembler.rs
use c2_mem::{MemHandle, MemPool};

pub struct ChunkAssembler {
    handle: Option<MemHandle>,    // 底层缓冲区（首次 feed 时 lazy 分配）
    total_chunks: u32,            // 总 chunk 数（从首个 chunk 帧头获取）
    received: u32,                // 已接收 chunk 数
    write_cursor: usize,          // 当前写入偏移
    actual_total_size: usize,     // payload 实际总大小
}

impl ChunkAssembler {
    /// 创建 assembler — 从 pool 分配缓冲区
    pub fn new(pool: &mut MemPool, total_size: usize, total_chunks: u32)
        -> Result<Self, AssemblerError>;

    /// 写入一个 chunk 到缓冲区
    /// data 是 chunk 的有效载荷（不含帧头）
    pub fn feed_chunk(
        &mut self,
        pool: &mut MemPool,  // 需要 pool 引用来访问 handle slice
        chunk_idx: u32,
        data: &[u8],
    ) -> Result<(), AssemblerError>;

    /// 是否所有 chunk 都已到齐
    pub fn is_complete(&self) -> bool {
        self.received == self.total_chunks
    }

    /// 消费 assembler，返回填充完成的 MemHandle
    /// 调用方通过 pool.handle_slice(&handle) 获取只读视图
    pub fn assemble(mut self) -> Result<MemHandle, AssemblerError>;

    /// 未完成时主动放弃，释放缓冲区
    pub fn abort(self, pool: &mut MemPool);
}

pub enum AssemblerError {
    PoolAllocFailed(String),
    DuplicateChunk(u32),
    ChunkOutOfRange { idx: u32, total: u32 },
    SizeOverflow { expected: usize, actual: usize },
    AlreadyAssembled,
}
```

### 与 Python 现有实现的对应关系

| Python (删除) | Rust (新增) |
|---|---|
| `ChunkAssembler.__init__()` 中 `mmap.mmap(-1, alloc_size)` | `pool.alloc_handle(total_size)` → MemHandle |
| `feed(chunk_idx, data)` 中 `buf.seek(); buf.write()` | `handle_slice_mut[offset..].copy_from_slice(data)` |
| `assemble()` → `buf.read(actual_total)` → bytes | `assemble()` → MemHandle → memoryview（零拷贝）|
| `buf.close()` | `handle.release()` (deserialize 之后) |
| `_ReplyChunkAssembler` (client 端) | 同一个 `ChunkAssembler`，server/client 共用 |

### c2-wire 依赖变化

```toml
# c2-wire/Cargo.toml
[dependencies]
c2-mem = { path = "../c2-mem" }
```

## §4 PyO3 Bindings (c2-ffi/src/mem_ffi.rs)

扩展已有的 `mem_ffi.rs`，新增 `PyMemHandle` 和 `PyChunkAssembler`。

### PyMemHandle — buffer protocol 实现

```rust
// c2-ffi/src/mem_ffi.rs (扩展)

/// Python 可见的统一内存句柄
/// 实现 buffer protocol，Python 端通过 memoryview(handle) 零拷贝访问
#[pyclass(name = "MemHandle")]
pub struct PyMemHandle {
    inner: Option<MemHandle>,       // Option 支持 release() 后标记已释放
    pool: Arc<RwLock<MemPool>>,     // 引用 pool（Buddy/Dedicated 需要 pool 来解引用）
}

#[pymethods]
impl PyMemHandle {
    /// Python buffer protocol: 返回只读 memoryview
    unsafe fn __getbuffer__(
        &self, view: *mut ffi::Py_buffer, flags: c_int
    ) -> PyResult<()> {
        // FileSpill: 直接从 self.inner.mmap 返回指针
        // Buddy/Dedicated: 通过 pool.handle_slice() 获取指针
        // release() 后调用: 抛 BufferError
    }

    unsafe fn __releasebuffer__(&self, _view: *mut ffi::Py_buffer) {
        // no-op — 资源由 release() 显式管理
    }

    /// 显式释放底层资源
    fn release(&mut self) -> PyResult<()> {
        match self.inner.take() {
            Some(handle) => {
                handle.release(); // 或通过 pool 释放 buddy/dedicated
                Ok(())
            }
            None => Err(PyValueError::new_err("MemHandle already released"))
        }
    }

    /// 在指定偏移写入数据（ChunkAssembler 内部使用）
    fn write_at(&mut self, offset: usize, data: &[u8]) -> PyResult<()>;

    #[getter]
    fn len(&self) -> PyResult<usize>;
}

/// GC 兜底: 未显式 release 时在析构时清理
impl Drop for PyMemHandle {
    fn drop(&mut self) {
        if let Some(handle) = self.inner.take() {
            handle.release();
        }
    }
}
```

### PyChunkAssembler

```rust
#[pyclass(name = "ChunkAssembler")]
pub struct PyChunkAssembler {
    inner: Option<ChunkAssembler>,
    pool: Arc<RwLock<MemPool>>,
}

#[pymethods]
impl PyChunkAssembler {
    #[new]
    fn new(pool: &PyMemPool, total_size: usize, total_chunks: u32) -> PyResult<Self> {
        // 从 pool 分配 MemHandle，创建 ChunkAssembler
    }

    /// 写入一个 chunk
    fn feed_chunk(&mut self, chunk_idx: u32, data: &[u8]) -> PyResult<()>;

    /// 是否完成
    fn is_complete(&self) -> bool;

    /// 返回 PyMemHandle（实现了 buffer protocol）
    fn assemble(&mut self) -> PyResult<PyMemHandle>;

    /// 放弃重组，释放资源
    fn abort(&mut self) -> PyResult<()>;
}
```

### Python 使用示例

```python
from c_two._native import MemPool, ChunkAssembler, MemHandle

pool = MemPool(config)
asm = ChunkAssembler(pool, total_size=1_000_000, total_chunks=10)

for idx, chunk_data in receive_chunks():
    asm.feed_chunk(idx, chunk_data)

if asm.is_complete():
    handle = asm.assemble()           # → MemHandle (PyO3 对象)
    mv = memoryview(handle)           # 零拷贝，buffer protocol
    result = deserialize(mv)          # 直接消费 memoryview
    handle.release()                  # 显式释放 mmap/SHM/文件
```

## §5 Python Integration

### Server 端改造

**transport/server/core.py — chunk 接收路径:**

```python
# 旧代码 (删除):
from ..server.chunk import ChunkAssembler
asm = ChunkAssembler(rid, total_size, total_chunks)
asm.feed(chunk_idx, chunk_data)
payload_bytes = asm.assemble()          # → bytes (RSS 翻倍)
result = deserialize(payload_bytes)

# 新代码:
from c_two._native import ChunkAssembler
asm = ChunkAssembler(self._pool, total_size, total_chunks)
asm.feed_chunk(chunk_idx, chunk_data)
handle = asm.assemble()                 # → MemHandle
mv = memoryview(handle)
result = deserialize(mv)               # 直接消费 memoryview
handle.release()                        # 显式释放
```

### Client 端改造

**transport/client/core.py — chunked reply 接收:**

```python
# 旧代码 (删除): _ReplyChunkAssembler 内部类 (~70 行)

# 新代码: 复用同一个 Rust ChunkAssembler
from c_two._native import ChunkAssembler
asm = ChunkAssembler(self._pool, total_size, total_chunks)
# ...循环接收 reply chunks...
handle = asm.assemble()
mv = memoryview(handle)
result = output_transferable.deserialize(mv)
handle.release()
```

### `__memoryview_aware__` 机制变更

当前 `transferable.py` 中有兼容逻辑：

```python
# 当前 (删除):
if (not getattr(input, '__memoryview_aware__', False)
        and isinstance(request, memoryview)):
    request = bytes(request)  # 不感知 memoryview 的类，降级为 bytes
```

改为：**所有 deserialize 统一接收 memoryview**，移除 `__memoryview_aware__` 标记和降级逻辑。

理由：
- `pickle.loads()` 接受 `memoryview`（Python 3.8+）
- PyArrow 接受 buffer protocol
- `struct.unpack_from()` 接受 buffer protocol
- 版本 0.3.0，不保留向后兼容包袱

### 需删除的 Python 代码

| 文件 | 内容 | 原因 |
|---|---|---|
| `transport/server/chunk.py` | 整个文件 | 被 Rust ChunkAssembler 替代 |
| `transport/client/core.py` | `_ReplyChunkAssembler` 类 (~70 行) | 被 Rust ChunkAssembler 替代 |
| `transport/ipc/frame.py` | `FLAG_DISK_SPILL` 常量 | 溢写决策在 Rust 内部，Python 无需感知 |
| `crm/transferable.py` | `__memoryview_aware__` 相关逻辑 | 统一 memoryview，无需标记 |

### 需修改的 Python 代码

| 文件 | 修改 |
|---|---|
| `transport/server/core.py` | chunk 接收路径改用 Rust ChunkAssembler |
| `transport/client/core.py` | reply chunk 接收路径改用 Rust ChunkAssembler |
| `crm/transferable.py` | 移除 memoryview → bytes 降级逻辑 |
| `c_two/mem/__init__.py` | 新增 re-export: MemHandle, ChunkAssembler |
| `c_two/_native` 模块 | 确保 PyMemHandle, PyChunkAssembler 已注册 |

## §6 Future Roadmap

本设计仅覆盖 MemHandle + ChunkAssembler 下沉。以下是后续通信层全面 Rust 化的规划：

### 近期（本次之后）

- **Wire 编解码下沉**: `transport/wire.py` 中的 `encode_call`/`decode` 迁移到 c2-wire FFI
- **Handshake 编解码下沉**: `transport/protocol.py` 中的 handshake v5 编解码迁移到 c2-wire FFI

### 中期

- **ServerV2 IPC 事件循环下沉**: asyncio event loop → tokio（c2-ipc 扩展或新 crate）
- **SharedClient 完全 Rust 化**: 当前 c2-ipc 已有 Rust IPC client，Python SharedClient 完全由 Rust 替代

### 长期目标

Python 层仅保留：
- PyO3 FFI binding
- CRM 方法执行（用户业务逻辑）
- `@transferable` serialize/deserialize（用户自定义序列化）
- ICRM Proxy（调用转发）
- 注册/连接 API（`cc.register()`, `cc.connect()` 等）

所有 transport / protocol / memory 管理逻辑均在 Rust 层。

**Python 对外 API 保持不变** — `cc.register()`, `cc.connect()`, `cc.close()`, `cc.shutdown()` 等顶层 API 不受底层实现切换影响。

## §7 Testing Strategy

### Rust 层测试

**c2-mem:**
- `test_alloc_handle_buddy` — 小块分配走 buddy 路径
- `test_alloc_handle_dedicated` — 大块分配走 dedicated 路径
- `test_alloc_handle_file_spill` — 模拟内存不足，强制走文件溢写（`spill_threshold = 0.0`）
- `test_file_spill_create_and_read` — 文件备份 mmap 写入/读取正确性
- `test_file_spill_unlink_on_create` — 验证文件创建后立即 unlink，目录无残留
- `test_available_physical_memory` — 平台内存探测返回合理值
- `test_handle_release` — 三种后端的 release 正确回收资源

**c2-wire:**
- `test_chunk_assembler_basic` — N 个 chunk 写入 + assemble，验证数据完整性
- `test_chunk_assembler_single` — 单 chunk（total_chunks=1）退化情况
- `test_chunk_assembler_abort` — 未完成时 abort，资源正确释放
- `test_chunk_assembler_with_file_spill` — 溢写模式下的完整流程

### Python 层测试

- 扩展已有 `test_mem_pool.py`：新增 MemHandle/ChunkAssembler FFI 调用测试
- 修改已有 chunk 相关集成测试：验证 Rust ChunkAssembler 与现有协议兼容
- 端到端测试：大 payload（超过 buddy 阈值）通过 IPC 传输，验证走 dedicated/file spill 路径

### 性能验证

- 对比基线：Rust ChunkAssembler vs Python ChunkAssembler 的 reassembly 吞吐量
- RSS 监控：溢写模式下峰值 RSS 应接近 payload 大小（而非 2×）

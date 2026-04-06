# IPC 缺陷修复设计

> **Status:** Draft
> **Date:** 2026-04-02
> **Scope:** c2-mem (gc 自动触发), c2-server (响应降级/chunked), c2-ipc (ResponseBuffer/零拷贝), c2-ffi (FFI 减拷贝)
> **Depends on:** `fix/ipc-perf-regression` 分支 (SHM 响应路径已实现)

## Overview

基于 [IPC 实现审计报告](2026-04-02-ipc-implementation-audit.md) 发现的 5 个缺陷，
按风险/复杂度分三阶段修复：

| Phase | 缺陷 | 内容 | 风险 |
|:-----:|------|------|:----:|
| 1 | A + E | gc_buddy 自动触发 + 响应栈 buffer | 低 |
| 2 | B + D | SHM 失败降级 + ResponseBuffer 零拷贝 | 中 |
| 3 | C | Chunked 响应路径 | 高 |

## §1 Phase 1: 内存生命周期 + 小优化

### §1.1 缺陷 A — gc_buddy() 自动触发

**现状：** `free_at()` 已正确标记 `idle_since`，`gc_buddy()` 逻辑完整，
但无任何调用方触发回收。buddy 段一旦扩展永不释放。

**设计：两条互补触发路径**

#### 路径 1 — 分配前回收（内存压力驱动）

`alloc_buddy()` 在现有段都满、准备 `create_segment()` 之前，先调 `gc_buddy()`
回收空闲段，然后重试现有段分配。

```rust
// pool.rs — alloc_buddy() 修改
fn alloc_buddy(&mut self, size: usize) -> Result<PoolAllocation, String> {
    // Layer 1: try existing segments (unchanged)
    for (idx, seg) in self.segments.iter().enumerate() { ... }

    // Layer 1.5 (NEW): GC before expansion
    let reclaimed = self.gc_buddy();
    if reclaimed > 0 {
        // Retry existing segments after GC freed some
        for (idx, seg) in self.segments.iter().enumerate() { ... }
    }

    // Layer 2: create new segment (unchanged)
    if self.segments.len() < self.config.max_segments { ... }

    Err("buddy allocation failed".into())
}
```

同样在 `alloc_handle()` 的 buddy 扩展路径前也加入 gc_buddy() + 重试。

#### 路径 2 — 释放后延迟回收（空闲驱动）

`free_at()` 返回值扩展，当段完全空闲时发出信号：

```rust
/// free_at 返回值
pub enum FreeResult {
    /// 正常释放，段仍有其他活跃分配
    Normal,
    /// 段完全空闲（最大空闲块 = 数据区大小），可延迟回收
    SegmentIdle { seg_idx: u16 },
}

// pool.rs
pub fn free_at(&mut self, ...) -> Result<FreeResult, String> {
    // ... existing free logic ...
    // After free, check if segment became fully idle
    let alloc = &self.segments[seg_idx as usize].allocator();
    if alloc.free_bytes() == alloc.data_size() as u64 {
        self.idle_since[seg_idx as usize] = Some(Instant::now());
        return Ok(FreeResult::SegmentIdle { seg_idx: seg_idx as u16 });
    }
    Ok(FreeResult::Normal)
}
```

**Server 侧处理 SegmentIdle 信号：**

```rust
// server.rs — dispatch_buddy_call() 中 free_peer_block() 后
if let FreeResult::SegmentIdle { seg_idx } = free_result {
    let pool = Arc::clone(&peer_pool);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(10)).await;
        let mut p = pool.lock().unwrap();
        p.gc_buddy();  // 检查 idle_since，超过 delay 才回收
    });
}
```

Client 侧 `read_and_free()` 中同理。

**延迟时序说明：**
- delayed task sleep(10s) 是"观察期"— 等待足够久看段是否真的空闲
- `gc_buddy()` 内部检查 `idle_since > dedicated_gc_delay_secs(5s)`
- 总效果：段空闲 10s 后 delayed task 触发，gc_buddy 发现 idle 已超 5s → 回收
- 考虑将 buddy GC delay 独立为 `buddy_gc_delay_secs` 配置项（当前复用 dedicated 的）

#### gc_buddy() 返回值

改为返回回收的段数，便于路径 1 判断是否需要重试：

```rust
pub fn gc_buddy(&mut self) -> usize {
    // ... existing logic: check idle_since > delay, unmap segment ...
    reclaimed_count
}
```

#### 线程安全注意

- `MemPool` 在 server/client 中由 `Mutex<MemPool>` 保护
- `gc_buddy()` 在持锁状态下调用，与 `alloc`/`free` 互斥
- delayed task 的 `pool.lock()` 与正常操作串行，无竞争

### §1.2 缺陷 E — 响应 inline 栈 buffer

**现状：** `write_reply_with_data()` 对所有大小都使用 `Vec::with_capacity` + 
两次 `extend_from_slice` + `encode_frame` 再分配。请求侧 `call_inline()` 
已有 1024B 栈 buffer 优化。

**设计：** 响应 inline 路径加入相同的栈 buffer 模式。

```rust
// server.rs — write_reply_with_data() 修改
async fn write_reply_with_data(
    writer: &Arc<Mutex<OwnedWriteHalf>>,
    request_id: u64,
    data: &[u8],
) {
    let ctrl = encode_reply_control(&ReplyControl::Ok, None);
    let payload_len = ctrl.len() + data.len();
    let hdr = encode_frame_header(request_id, FLAG_RESPONSE | FLAG_REPLY_V2, payload_len);

    // 小响应: 栈 buffer + 单次 write_all
    if hdr.len() + payload_len <= 1024 {
        let mut buf = [0u8; 1024];
        let mut off = 0;
        buf[off..off + hdr.len()].copy_from_slice(&hdr);
        off += hdr.len();
        buf[off..off + ctrl.len()].copy_from_slice(&ctrl);
        off += ctrl.len();
        buf[off..off + data.len()].copy_from_slice(data);
        off += data.len();
        let mut w = writer.lock().await;
        let _ = w.write_all(&buf[..off]).await;
        return;
    }

    // 大响应: 保持 Vec 逻辑 (不变)
    let mut payload = Vec::with_capacity(hdr.len() + payload_len);
    payload.extend_from_slice(&hdr);
    payload.extend_from_slice(&ctrl);
    payload.extend_from_slice(data);
    let mut w = writer.lock().await;
    let _ = w.write_all(&payload).await;
}
```

**收益：** 64B 响应 inline: 消除 1 次堆分配 + 2 次 extend_from_slice，
改为栈上 copy_from_slice + 单次 write_all。

---

## §2 Phase 2: SHM 降级 + ResponseBuffer 零拷贝

### §2.1 缺陷 B — SHM alloc 失败优雅降级

**现状：** `write_buddy_reply_with_data()` 中 `pool.alloc()` 失败时仅
log warning，响应静默丢失。客户端请求侧已有 buddy→chunked 降级。

**设计：** `smart_reply_with_data()` 改为 Result 返回，失败时降级。

```rust
// server.rs
async fn smart_reply_with_data(
    response_pool: &Mutex<MemPool>,
    writer: &Arc<Mutex<OwnedWriteHalf>>,
    request_id: u64,
    data: &[u8],
    shm_threshold: u64,
    chunk_size: usize,  // NEW: 传入 chunk_size 配置
) {
    if data.len() as u64 > shm_threshold {
        if write_buddy_reply_with_data(response_pool, writer, request_id, data)
            .await
            .is_ok()
        {
            return;
        }
        // SHM 分配失败 → 降级
    }

    // 降级路径: chunked (Phase 3) 或 inline
    if data.len() > chunk_size {
        write_chunked_reply(writer, request_id, data, chunk_size).await;
    } else {
        write_reply_with_data(writer, request_id, data).await;
    }
}
```

`write_buddy_reply_with_data()` 签名改为 `-> Result<(), String>`，
alloc 失败返回 Err 而非 log + continue。

> **注意：** Phase 3 之前 `write_chunked_reply` 尚未实现。
> Phase 2 先实现框架（返回 Result + 降级到 inline），Phase 3 补充 chunked。

### §2.2 缺陷 D — ResponseBuffer 零拷贝

**现状：** SHM 响应经历 2 次拷贝：
1. `read_and_free()`: SHM → Vec\<u8\>
2. `PyBytes::new()`: Vec → PyBytes

**设计：** 引入 `ResponseBuffer` 类型，SHM 响应实现 memoryview 零拷贝。

#### Rust 类型定义

```rust
// c2-ipc/src/response.rs (NEW)

/// 统一响应缓冲区 — 封装 inline 和 SHM 两种后端
pub enum ResponseData {
    /// UDS inline 数据（已在 Rust 堆中）
    Inline(Vec<u8>),
    /// SHM buddy/dedicated 数据（在共享内存中）
    Shm {
        seg_idx: u16,
        offset: u32,
        data_size: u32,
        is_dedicated: bool,
    },
}
```

#### Client 侧改动

`decode_response()` 不再 `read_and_free()`，而是返回 `ResponseData`：

```rust
// client.rs — decode_response() 修改
fn decode_response(payload: &[u8], flags: u16) -> Result<ResponseData, IpcError> {
    if is_buddy {
        let (bp, _) = decode_buddy_payload(payload)?;
        // 不读取数据，仅返回 SHM 坐标
        Ok(ResponseData::Shm {
            seg_idx: bp.seg_idx,
            offset: bp.offset,
            data_size: bp.data_size,
            is_dedicated: bp.is_dedicated,
        })
    } else {
        Ok(ResponseData::Inline(payload[consumed..].to_vec()))
    }
}
```

`call_full()` 返回 `ResponseData` 而非 `Vec<u8>`。

#### FFI 层 — PyResponseBuffer

```rust
// c2-ffi/src/client_ffi.rs

#[pyclass(name = "ResponseBuffer", frozen)]
pub struct PyResponseBuffer {
    inner: Mutex<Option<ResponseBufferInner>>,
}

enum ResponseBufferInner {
    Inline(Vec<u8>),
    Shm {
        pool: Arc<Mutex<MemPool>>,  // ServerPoolState 的 pool
        seg_idx: u16,
        offset: u32,
        data_size: u32,
        is_dedicated: bool,
    },
}

#[pymethods]
impl PyResponseBuffer {
    /// buffer protocol — memoryview(response) 可用
    unsafe fn __getbuffer__(&self, view: *mut Py_buffer, flags: c_int) -> PyResult<()> {
        let guard = self.inner.lock().unwrap();
        let inner = guard.as_ref().ok_or(PyBufferError::new_err("released"))?;
        match inner {
            ResponseBufferInner::Inline(vec) => {
                // 返回 vec 内部指针
                fill_buffer_view(view, vec.as_ptr(), vec.len(), flags)
            }
            ResponseBufferInner::Shm { pool, seg_idx, offset, data_size, .. } => {
                // 返回 SHM 内存指针 — 零拷贝！
                let p = pool.lock().unwrap();
                let ptr = p.data_ptr_at(*seg_idx as u32, *offset, *is_dedicated)?;
                fill_buffer_view(view, ptr, *data_size as usize, flags)
            }
        }
    }

    /// 显式释放底层资源
    fn release(&self) -> PyResult<()> {
        let mut guard = self.inner.lock().unwrap();
        match guard.take() {
            Some(ResponseBufferInner::Shm { pool, seg_idx, offset, data_size, is_dedicated }) => {
                let mut p = pool.lock().unwrap();
                let _ = p.free_at(seg_idx as u32, offset, data_size, is_dedicated);
                Ok(())
            }
            Some(ResponseBufferInner::Inline(_)) => Ok(()), // Vec dropped
            None => Err(PyValueError::new_err("already released")),
        }
    }

    fn __len__(&self) -> PyResult<usize> {
        let guard = self.inner.lock().unwrap();
        match guard.as_ref() {
            Some(ResponseBufferInner::Inline(v)) => Ok(v.len()),
            Some(ResponseBufferInner::Shm { data_size, .. }) => Ok(*data_size as usize),
            None => Err(PyValueError::new_err("released")),
        }
    }

    /// 转换为 bytes（兼容旧代码，会拷贝）
    fn __bytes__(&self, py: Python<'_>) -> PyResult<Py<PyBytes>> { ... }
}

/// GC 兜底
impl Drop for PyResponseBuffer {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.lock().unwrap().take() {
            if let ResponseBufferInner::Shm { pool, seg_idx, offset, data_size, is_dedicated } = inner {
                if let Ok(mut p) = pool.lock() {
                    let _ = p.free_at(seg_idx as u32, offset, data_size, is_dedicated);
                }
            }
        }
    }
}
```

#### buffer protocol 生命周期安全

`__getbuffer__` 返回的原始指针在 Python 持有 `memoryview` 期间必须有效。
关键保证：

1. **SHM 映射不会被 unmap** — 只有 `gc_buddy()`/`gc_dedicated()` 会 unmap 段，
   而只有 `free_at()` 之后段才可能被标记为可回收。`release()` 调用 `free_at()` 后
   inner 被 take 为 None，`__getbuffer__` 会拒绝新的 buffer 请求。

2. **release 与 memoryview 互斥** — 添加 `exports: AtomicU32` 引用计数：

```rust
#[pyclass(name = "ResponseBuffer", frozen)]
pub struct PyResponseBuffer {
    inner: Mutex<Option<ResponseBufferInner>>,
    exports: AtomicU32,  // 活跃 memoryview 数量
}

// __getbuffer__: exports.fetch_add(1)
// __releasebuffer__: exports.fetch_sub(1)
// release(): if exports.load() > 0 → 返回错误 "buffer is exported"
```

这确保：先 release 再创建 memoryview → BufferError；先创建 memoryview 再 release → 报错要求先释放 memoryview。
```

#### Python 侧改动

`transferable.py` 中 `@auto_transfer` 装饰器的响应处理：

```python
# transferable.py — response handling
response = client.call(method_name, data)  # ResponseBuffer
mv = memoryview(response)
try:
    output = output_transferable(mv)
finally:
    response.release()  # 显式释放 SHM
return output
```

`proxy.py` 的 `call()` 返回类型从 `bytes` 变为 `ResponseBuffer`。
`ResponseBuffer` 同时支持 `bytes(response)` 转换（兼容性）。

#### 线程安全

- `PyResponseBuffer` 标记 `frozen`（free-threading 安全）
- 内部 `Mutex<Option<...>>` 保证 `release()` 和 `__getbuffer__` 互斥
- SHM 指针在 `__getbuffer__` 返回后，Python 持有 memoryview 期间有效
- `release()` 后 memoryview 访问会触发 `BufferError`（`__getbuffer__` 检查 None）

#### 性能收益

| 场景 | 当前拷贝 | 新方案拷贝 | 改善 |
|------|:--------:|:----------:|:----:|
| 100MB SHM 响应 | 200MB (2×) | 0 | 100% |
| 4KB SHM 响应 | 8KB (2×) | 0 | 100% |
| 64B inline 响应 | 128B (2×) | 64B (1×) | 50% |

---

## §3 Phase 3: Chunked 响应路径

### §3.1 缺陷 C — 响应无 chunked 路径

**现状：** 响应只有 inline 和 buddy 两种路径。当 SHM alloc 失败且
响应 > chunk_size 时，无法发送。最大响应受限于 response_pool 总容量 (~1GB)。

**设计：** 镜像现有客户端 chunked 请求协议，方向反转。

#### Wire 协议扩展

新增 flag 常量：

```rust
// c2-wire/src/frame.rs
pub const FLAG_REPLY_CHUNKED: u16 = 0x20;
```

Chunked 响应帧格式（复用请求侧 chunk 帧头结构）：

```
┌─────────────────────┬──────────────────────────┬──────────────┐
│ Frame Header (12B)  │ Chunk Meta              │ Chunk Data   │
│ req_id + flags      │ total_size(8) +         │ actual data  │
│ FLAG_RESPONSE |     │ total_chunks(4) +       │              │
│ FLAG_REPLY_CHUNKED  │ chunk_idx(4)            │              │
└─────────────────────┴──────────────────────────┴──────────────┘
```

每个 chunk 帧都携带 total_size 和 total_chunks（便于接收方在首帧即创建 assembler）。

#### Server 侧 — 发送 chunked 响应

```rust
// server.rs — new function
async fn write_chunked_reply(
    writer: &Arc<Mutex<OwnedWriteHalf>>,
    request_id: u64,
    data: &[u8],
    chunk_size: usize,
) {
    let total_chunks = ((data.len() + chunk_size - 1) / chunk_size) as u32;
    let total_size = data.len() as u64;

    for (idx, chunk) in data.chunks(chunk_size).enumerate() {
        let meta = encode_chunk_meta(total_size, total_chunks, idx as u32);
        let flags = FLAG_RESPONSE | FLAG_REPLY_V2 | FLAG_REPLY_CHUNKED;
        let payload_len = meta.len() + chunk.len();
        let hdr = encode_frame_header(request_id, flags, payload_len);

        let mut w = writer.lock().await;
        let _ = w.write_all(&hdr).await;
        let _ = w.write_all(&meta).await;
        let _ = w.write_all(chunk).await;
    }
}
```

#### Client 侧 — 接收 chunked 响应

在 client 的 response dispatch 中识别 `FLAG_REPLY_CHUNKED`：

```rust
// client.rs — response handler 扩展
if flags & FLAG_REPLY_CHUNKED != 0 {
    let (total_size, total_chunks, chunk_idx, chunk_data) = 
        decode_chunk_meta(payload);

    // 首帧: 创建 assembler
    if chunk_idx == 0 {
        let mut pool = reassembly_pool.lock().unwrap();
        let asm = ChunkAssembler::new(&mut pool, total_size, total_chunks)?;
        assemblers.insert(request_id, asm);
    }

    // 写入 chunk
    let asm = assemblers.get_mut(&request_id)?;
    {
        let mut pool = reassembly_pool.lock().unwrap();
        asm.feed_chunk(&mut pool, chunk_idx, chunk_data)?;
    }

    // 完成: 返回 MemHandle → ResponseBuffer::Shm
    if asm.is_complete() {
        let asm = assemblers.remove(&request_id).unwrap();
        let handle = asm.assemble()?;
        // 返回 ResponseData 用于 ResponseBuffer 包装
    }
}
```

使用 `ChunkAssembler`（已在 c2-wire 实现，支持 `alloc_handle()` 全四级级联）。

#### 与 Phase 2 的集成

Chunked 响应重组使用 `alloc_handle()` → 完整 T1-T4 支持。
重组后返回 `ResponseData::Handle(MemHandle)` → `ResponseBuffer` 包装
→ Python 侧 `memoryview()` 零拷贝。

需要扩展 `ResponseData` 添加 Handle 变体：

```rust
pub enum ResponseData {
    Inline(Vec<u8>),
    Shm { seg_idx, offset, data_size, is_dedicated },
    Handle(MemHandle),  // chunked 重组后的 MemHandle
}
```

`ResponseBufferInner` 对应增加 `Handle` 变体。

---

## §4 改动文件清单

### Phase 1

| 文件 | 改动 |
|------|------|
| `c2-mem/src/pool.rs` | `free_at()` 返回 `FreeResult`；`alloc_buddy()` 扩展前 gc；`gc_buddy()` 返回 usize |
| `c2-server/src/server.rs` | `write_reply_with_data()` 栈 buffer；free_peer_block 处理 SegmentIdle |
| `c2-server/src/connection.rs` | `free_peer_block()` 返回 FreeResult |
| `c2-ipc/src/client.rs` | `read_and_free()` 返回 FreeResult，delayed gc |
| `c2-ffi/src/mem_ffi.rs` | `free_at()` FFI 返回值适配 |

### Phase 2

| 文件 | 改动 |
|------|------|
| `c2-ipc/src/response.rs` | **新文件** — ResponseData enum |
| `c2-ipc/src/client.rs` | `decode_response()` 返回 ResponseData；`call_full()` 返回 ResponseData |
| `c2-ipc/src/sync_client.rs` | `call()` 返回 ResponseData |
| `c2-server/src/server.rs` | `smart_reply_with_data()` → Result；`write_buddy_reply_with_data()` → Result |
| `c2-ffi/src/client_ffi.rs` | **PyResponseBuffer** pyclass；`call()` 返回 ResponseBuffer |
| `src/c_two/crm/transferable.py` | `@auto_transfer` 响应处理使用 memoryview + release |
| `src/c_two/transport/client/proxy.py` | `call()` 返回类型更新 |

### Phase 3

| 文件 | 改动 |
|------|------|
| `c2-wire/src/frame.rs` | `FLAG_REPLY_CHUNKED`；`encode_chunk_meta()`/`decode_chunk_meta()` |
| `c2-server/src/server.rs` | `write_chunked_reply()` 新函数 |
| `c2-ipc/src/client.rs` | chunked 响应接收 + ChunkAssembler 集成 |
| `c2-ipc/src/response.rs` | `ResponseData::Handle` 变体 |
| `c2-ffi/src/client_ffi.rs` | `ResponseBufferInner::Handle` 变体 |

---

## §5 测试策略

### Phase 1 测试

- **单元测试** (`c2-mem`):
  - `test_gc_buddy_auto_trigger_on_alloc` — alloc 扩展前自动 gc
  - `test_free_at_returns_segment_idle` — 段完全空闲时返回 SegmentIdle
  - `test_gc_buddy_returns_count` — 返回回收段数

- **集成测试** (Python):
  - 扩展已有 `test_ipc_buddy_reply.py`：验证大量请求后 buddy 段被回收

### Phase 2 测试

- **单元测试** (`c2-ipc`):
  - `test_response_buffer_inline` — inline 响应 memoryview 正确
  - `test_response_buffer_shm` — SHM 响应零拷贝 memoryview
  - `test_response_buffer_release` — release 后 SHM 已释放
  - `test_response_buffer_double_release` — 二次 release 抛错

- **集成测试** (Python):
  - `test_memoryview_response_round_trip` — memoryview 反序列化正确性
  - `test_shm_alloc_fallback_inline` — SHM 满时降级 inline

### Phase 3 测试

- **单元测试** (`c2-wire`, `c2-server`):
  - `test_chunked_reply_encode_decode` — chunk 帧编解码往返
  - `test_chunked_reply_reassembly` — 多 chunk 重组完整性

- **集成测试** (Python):
  - `test_chunked_response_large_payload` — 超大响应 chunked 传输
  - `test_chunked_response_with_file_spill` — T4 溢写 + chunked 响应

---

## §6 兼容性

### Python API 变更

`proxy.call()` 返回类型从 `bytes` 变为 `ResponseBuffer`。影响范围：

1. **`@auto_transfer` 装饰器** — 内部改动，对用户透明
2. **直接调用 `proxy.call()` 的测试** — 需适配
3. **`ResponseBuffer` 兼容性**:
   - `bytes(response)` → 转换为 bytes（兼容旧代码）
   - `memoryview(response)` → 零拷贝访问（推荐）
   - `len(response)` → 数据长度
   - `response.release()` → 显式释放

### Wire 协议兼容

Phase 3 新增 `FLAG_REPLY_CHUNKED`。旧客户端不识别此 flag 会报错。
由于 c-two 目前无跨版本兼容承诺，这是可接受的 breaking change。

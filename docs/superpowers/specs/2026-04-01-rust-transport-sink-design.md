# 通信·协议·传输层 Rust 下沉设计

**Date:** 2026-04-01
**Status:** Draft
**Scope:** c2-wire (扩展), c2-server (新建), c2-ipc (扩展), c2-http (新建), c2-ffi (扩展), Python transport 层重构
**Target:** v0.4.0

## Overview

当前 C-Two 的通信层存在 Python/Rust 双重实现：Wire 编解码、Handshake 协议、帧格式在两侧各有一套，
Server 和 Client 核心逻辑完全由 Python 实现。本设计将通信、协议和传输全面下沉至 Rust，
Python 层仅保留业务逻辑（CRM 方法执行、序列化、类型代理）。

### 当前 Python vs Rust 边界

| 层级 | Python (行数) | Rust | 状态 |
|------|--------------|------|------|
| 内存池 | 薄 FFI wrapper | c2-mem | ✅ Rust-only |
| Chunk 重组 | 薄 FFI wrapper | c2-wire/assembler | ✅ Rust-only |
| HTTP Relay | 薄 FFI wrapper | c2-relay | ✅ Rust-only |
| Wire 编解码 | wire.py (506) | c2-wire (1963) | ⚠️ 双重实现 |
| Handshake | protocol.py (300) | c2-wire/handshake (271) | ⚠️ 双重实现 |
| 帧层 | frame.py + shm_frame.py (567) | c2-wire + c2-ipc | ⚠️ 双重实现 |
| Flag/信号 | protocol.py + msg_type.py | c2-wire/flags.rs | ⚠️ 散落两处 |
| Server | server/ (1774) | ❌ | 🔴 纯 Python |
| Client | client/core.py (982) | c2-ipc (558, relay 专用) | ⚠️ 范式不同 |
| Client 辅助 | pool.py + proxy.py + http.py (735) | ❌ | 🔴 纯 Python |
| IPCConfig | 38 项参数 | ❌ | 🔴 纯 Python |

### 目标架构

```
下沉后 Python 仅保留:
├── registry.py          cc.register/connect/close/shutdown (调用 Rust FFI)
├── config.py            Python-only 业务配置
├── client/
│   └── proxy.py         ICRMProxy (类型安全代理, 不涉及 I/O)
└── crm/                 CRM 方法执行 + @transferable 序列化

所有 transport / protocol / memory 管理在 Rust:
├── c2-mem               内存池 (已完成)
├── c2-wire              Wire 编解码 + Handshake + ChunkAssembler (已大部分完成)
├── c2-ipc               IPC client: async + sync wrapper (扩展)
├── c2-http              HTTP client: reqwest (新建, WASM 复用)
├── c2-server            IPC server: tokio (新建)
├── c2-relay             HTTP relay (已完成)
└── c2-ffi               PyO3 统一入口 (扩展)
```

### 设计约束

- **向后兼容**: 允许较大 API 重构，迈向 v0.4.0
- **渐进实施**: 4 Phase，中间状态允许测试失败，最终全部通过
- **Server 策略**: Python asyncio 完全移除，用 tokio 替代
- **Client 策略**: 保留同步 API + 新增 async API（双模式）

---

## Phase 1: 基础设施统一 — Wire FFI + MsgType + Config

**目标**: 消除 Python/Rust 双重编解码，建立 Rust 为 codec/config/signal 的唯一真相源。

**风险**: 低。纯重构，不改变运行时行为。

### §1.1 c2-wire 扩展

#### MsgType 枚举

```rust
// c2-wire/src/msg_type.rs (新建)

/// IPC 消息/信号类型 — 唯一真相源
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsgType {
    Ping           = 0x01,
    Pong           = 0x02,
    CrmCall        = 0x03,
    CrmReply       = 0x04,
    ShutdownClient = 0x05,
    // 0x06 reserved (was SHUTDOWN_SERVER, removed)
    ShutdownAck    = 0x07,
    Disconnect     = 0x08,
    DisconnectAck  = 0x09,
}

impl MsgType {
    /// 预编码 signal bytes (1B type 包装在 16B frame 中)
    pub fn signal_frame(&self) -> &'static [u8];

    /// 从 frame payload 的第一字节解析
    pub fn from_byte(b: u8) -> Option<Self>;
}
```

#### IpcConfig 结构体

```rust
// c2-wire/src/config.rs (新建)

/// IPC 通信配置 — Rust 侧唯一真相源
/// Python IPCConfig 中与通信相关的参数移入此处
pub struct IpcConfig {
    // ── 帧层 ──
    pub max_frame_size: usize,        // default: 2_147_483_648 (2GB)
    pub shm_threshold: usize,         // default: 4096 (>4KB 走 SHM)

    // ── 池 ──
    pub pool_segment_size: usize,     // default: 268_435_456 (256MB)
    pub pool_min_block: usize,        // default: 4096
    pub pool_max_segments: u16,       // default: 4

    // ── 分块 ──
    pub chunk_size: usize,            // default: 1_048_576 (1MB)
    pub max_reassembly_bytes: usize,  // default: 8_589_934_592 (8GB)

    // ── 心跳 ──
    pub heartbeat_interval_secs: u64, // default: 30
    pub heartbeat_timeout_secs: u64,  // default: 90

    // ── 溢写 ──
    pub spill_threshold: f64,         // default: 0.8
    pub spill_dir: PathBuf,           // default: /tmp/c_two_spill/
}

impl Default for IpcConfig { ... }
```

### §1.2 c2-ffi 扩展: wire_ffi.rs

新建 `c2-ffi/src/wire_ffi.rs`，暴露以下 PyO3 接口：

```rust
// ── 帧编解码 ──
#[pyfunction]
fn encode_frame(request_id: u64, flags: u32, payload: &[u8]) -> PyResult<Vec<u8>>;

#[pyfunction]
fn decode_frame(data: &[u8]) -> PyResult<(u64, u32, &[u8])>;  // (rid, flags, payload)

// ── 控制面编解码 ──
#[pyfunction]
fn encode_call_control(name: &str, method_idx: u16) -> PyResult<Vec<u8>>;

#[pyfunction]
fn decode_call_control(data: &[u8]) -> PyResult<(String, u16)>;

#[pyfunction]
fn encode_reply_control_ok() -> Vec<u8>;

#[pyfunction]
fn encode_reply_control_error(error_bytes: &[u8]) -> Vec<u8>;

#[pyfunction]
fn decode_reply_control(data: &[u8]) -> PyResult<(bool, Option<Vec<u8>>)>;

// ── Handshake ──
#[pyfunction]
fn encode_client_handshake(routes: Vec<PyRouteInfo>, prefix: &str) -> PyResult<Vec<u8>>;

#[pyfunction]
fn encode_server_handshake(routes: Vec<PyRouteInfo>, prefix: &str, segments: Vec<String>) -> PyResult<Vec<u8>>;

#[pyfunction]
fn decode_handshake(data: &[u8]) -> PyResult<PyHandshake>;

// ── 信号 ──
#[pyclass(name = "MsgType")]
struct PyMsgType { ... }  // 枚举 + pre-encoded bytes

// ── 配置 ──
#[pyclass(name = "IpcConfig")]
struct PyIpcConfig { inner: IpcConfig }
```

### §1.3 Python 侧删除清单

| 文件 | 操作 | 行数 | 替代 |
|------|------|------|------|
| `transport/wire.py` | 删除编解码函数，保留为 re-export shim | ~400 | `c_two._native.encode_*` |
| `transport/protocol.py` | 删除 handshake 编解码 + flag 常量 | ~250 | `c_two._native.{encode,decode}_handshake` |
| `transport/ipc/frame.py` | 删除帧编解码 | ~150 | `c_two._native.{encode,decode}_frame` |
| `transport/ipc/msg_type.py` | 整个文件删除 | ~40 | `c_two._native.MsgType` |
| **合计** | | **~840** | |

### §1.4 过渡策略

Phase 1 可选择两种过渡方式：

**方式 A (推荐): 薄 shim 层**
```python
# transport/wire.py — 改为 re-export
from c_two._native import (
    encode_call_control,
    decode_call_control,
    encode_reply_control_ok,
    encode_reply_control_error,
    decode_reply_control,
)
```
所有 import 路径不变，server/client 代码无需修改。Phase 4 时删除 shim。

**方式 B: 直接修改 import**
全部 `from c_two.transport.wire import ...` 改为 `from c_two._native import ...`。一步到位但改动面大。

---

## Phase 2: Server 下沉 — asyncio → tokio

**目标**: 新建 c2-server crate，用 tokio 完全替代 Python asyncio server。CRM 方法执行通过 PyO3 回调。

**风险**: 中-高。涉及 Rust↔Python 回调、GIL 管理、并发调度。

### §2.1 新 Crate: c2-server

```
c2-server/
  Cargo.toml
  src/
    lib.rs              公开 API: Server, ServerConfig
    server.rs           tokio UDS accept loop
    connection.rs       per-client 状态 (SHM segments, flight counter, last_activity)
    dispatcher.rs       CRM 方法路由 (route_name × method_idx → CrmCallback)
    heartbeat.rs        per-connection PING/PONG (tokio::time::interval)
    scheduler.rs        read/write 并发控制
```

**Cargo.toml 依赖:**
```toml
[dependencies]
c2-wire = { path = "../c2-wire" }
c2-mem = { path = "../c2-mem" }
tokio = { version = "1", features = ["net", "rt-multi-thread", "sync", "macros", "io-util", "time"] }
tracing = "0.1"
```

### §2.2 核心: CRM 回调机制

Rust server 需要调用 Python 业务逻辑。设计 trait 抽象：

```rust
// c2-server/src/dispatcher.rs

/// CRM 方法回调 — Rust server 通过此 trait 调用 Python CRM 方法
/// 实现方持有 Python callable 引用（通过 PyO3）
pub trait CrmCallback: Send + Sync + 'static {
    /// 执行一次 CRM 方法调用
    ///
    /// # Arguments
    /// * `route_name` — CRM 路由名（对应 cc.register(name=...) 的 name）
    /// * `method_idx` — 方法索引（handshake 时协商的索引）
    /// * `payload`    — 序列化后的参数 bytes
    ///
    /// # Returns
    /// 序列化后的结果 bytes，或 CrmError
    fn invoke(
        &self,
        route_name: &str,
        method_idx: u16,
        payload: &[u8],
    ) -> Result<Vec<u8>, CrmError>;
}

/// CRM 调用错误
pub enum CrmError {
    /// CRM 方法抛出的 Python 异常（已序列化为 bytes）
    UserError(Vec<u8>),
    /// 内部错误（方法未找到、类型错误等）
    InternalError(String),
}
```

#### PyO3 实现 (c2-ffi 侧)

```rust
// c2-ffi/src/server_ffi.rs

/// 持有 Python CRM dispatch 函数的回调实现
struct PyCrmCallback {
    /// Python 侧的 dispatch 函数: fn(route: str, method_idx: int, payload: bytes) -> bytes
    dispatcher: Py<PyAny>,
}

impl CrmCallback for PyCrmCallback {
    fn invoke(&self, route_name: &str, method_idx: u16, payload: &[u8]) -> Result<Vec<u8>, CrmError> {
        // 1. 获取 GIL（free-threading 下 = 获取 Python 解释器访问权）
        Python::with_gil(|py| {
            // 2. 调用 Python dispatcher
            let result = self.dispatcher.call1(py, (route_name, method_idx, payload));
            // 3. 处理结果/异常
            match result {
                Ok(obj) => Ok(obj.extract::<Vec<u8>>(py)?),
                Err(e) => Err(CrmError::UserError(serialize_python_error(py, &e))),
            }
        })
    }
}
```

#### GIL 交互模型

```
tokio recv_loop (无 GIL)
    │
    ├── 读帧、解码控制面 ← 纯 Rust，无 GIL
    │
    ├── scheduler 获取 read/write guard ← Rust RwLock
    │
    ├── tokio::task::spawn_blocking(|| {
    │       Python::with_gil(|py| {       ← 获取 GIL
    │           callback.invoke(...)       ← 执行 CRM 方法
    │       })                             ← 释放 GIL
    │   })
    │
    └── 编码 reply，写回 UDS ← 纯 Rust，无 GIL
```

**关键点**: CRM 方法执行在 `spawn_blocking` 线程池中，不阻塞 tokio I/O 线程。
GIL 仅在实际执行 Python 代码时持有，I/O 和编解码完全无 GIL。

### §2.3 Scheduler（读写并发控制）

当前 Python `Scheduler` 实现 writer-priority RwLock。下沉方案：

```rust
// c2-server/src/scheduler.rs

/// 方法访问级别（从 ICRM @cc.read / @cc.write 获取）
#[derive(Clone, Copy)]
pub enum AccessLevel {
    Read,       // 允许并发执行
    Write,      // 独占执行
}

/// per-CRM 调度器
pub struct Scheduler {
    lock: tokio::sync::RwLock<()>,
    access_map: HashMap<u16, AccessLevel>,  // method_idx → access level
}

impl Scheduler {
    /// 在调度保护下执行 CRM 方法
    pub async fn execute<F, R>(&self, method_idx: u16, f: F) -> R
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        let access = self.access_map.get(&method_idx).copied().unwrap_or(AccessLevel::Write);
        match access {
            AccessLevel::Read => {
                let _guard = self.lock.read().await;
                tokio::task::spawn_blocking(f).await.unwrap()
            }
            AccessLevel::Write => {
                let _guard = self.lock.write().await;
                tokio::task::spawn_blocking(f).await.unwrap()
            }
        }
    }
}
```

**access_map 来源**: Python 侧 `cc.register()` 时解析 ICRM 的 `@cc.read`/`@cc.write` 注解，
将 `{method_idx: AccessLevel}` 传入 Rust server config。

### §2.4 Server 生命周期

```rust
// c2-server/src/server.rs

pub struct Server {
    config: IpcConfig,
    routes: HashMap<String, CrmRoute>,  // route_name → (Scheduler, CrmCallback)
    listener: Option<tokio::net::UnixListener>,
    shutdown: tokio::sync::watch::Sender<bool>,
}

pub struct CrmRoute {
    scheduler: Arc<Scheduler>,
    callback: Arc<dyn CrmCallback>,
    method_table: MethodTable,
}

impl Server {
    pub fn new(address: &str, config: IpcConfig) -> Self;

    /// 注册一个 CRM 路由
    pub fn register_route(
        &mut self,
        name: String,
        callback: Arc<dyn CrmCallback>,
        method_table: MethodTable,
        access_map: HashMap<u16, AccessLevel>,
    );

    /// 启动 server (创建 tokio runtime, 开始 accept)
    pub fn start(&self) -> JoinHandle<()>;

    /// 优雅关闭
    pub fn shutdown(&self);
}
```

#### Accept Loop

```rust
async fn accept_loop(server: Arc<Server>) {
    let listener = UnixListener::bind(&server.config.address)?;
    loop {
        tokio::select! {
            Ok((stream, _)) = listener.accept() => {
                let server = server.clone();
                tokio::spawn(async move {
                    handle_connection(server, stream).await;
                });
            }
            _ = server.shutdown_signal() => break,
        }
    }
}
```

#### Connection Handler

```rust
async fn handle_connection(server: Arc<Server>, stream: UnixStream) {
    // 1. Handshake: 交换路由表 + pool prefix + capabilities
    let handshake = do_handshake(&stream, &server.routes).await?;

    // 2. 创建 Connection 状态
    let conn = Connection::new(handshake, &server.config);

    // 3. 启动心跳任务
    let hb = tokio::spawn(heartbeat_task(conn.clone(), stream.clone()));

    // 4. Frame recv loop
    loop {
        let frame = read_frame(&stream).await?;
        conn.touch();  // 更新活跃时间

        if frame.is_signal() {
            handle_signal(&conn, &stream, &frame).await;
            continue;
        }

        // 5. 解码控制面，路由到 CRM
        let (route_name, method_idx) = decode_call_control(&frame.payload)?;
        let route = server.routes.get(&route_name)?;

        // 6. 提取 payload (SHM buddy / inline / chunked)
        let payload = extract_payload(&conn, &frame).await?;

        // 7. 调度执行
        let callback = route.callback.clone();
        let result = route.scheduler.execute(method_idx, move || {
            callback.invoke(&route_name, method_idx, &payload)
        }).await;

        // 8. 编码 reply，写回
        let reply_frame = encode_reply(&conn, result)?;
        write_frame(&stream, &reply_frame).await?;
    }

    hb.abort();
    conn.cleanup();
}
```

### §2.5 SHM 管理

Server 持有接收端 `MemPool`:
- Handshake 时交换 pool prefix → Rust 侧完成
- Client 扩容的 segment → Rust connection 懒加载 (`open_segment`)
- Buddy payload 的 decode/encode 在 Rust 完成 (c2-wire/buddy.rs)
- 专用 SHM segment 的创建/打开 在 Rust 完成 (c2-mem)

### §2.6 Python 侧变更

**删除** (~1774 行):
| 文件 | 行数 |
|------|------|
| `server/core.py` | 954 |
| `server/connection.py` | 145 |
| `server/handshake.py` | 172 |
| `server/reply.py` | 213 |
| `server/dispatcher.py` | 102 |
| `server/scheduler.py` | 264 |
| `server/heartbeat.py` | 68 |

**新增** — `c2-ffi/src/server_ffi.rs`:
```rust
#[pyclass(name = "RustServer")]
pub struct PyServer {
    inner: Arc<Server>,
    rt: tokio::runtime::Runtime,
}

#[pymethods]
impl PyServer {
    #[new]
    fn new(address: &str, config: &PyIpcConfig) -> PyResult<Self>;

    /// 注册 CRM 路由
    /// dispatcher: Python callable (route, method_idx, payload) -> bytes
    /// access_map: dict[int, str] (method_idx → "read"|"write")
    fn register_route(
        &self,
        name: &str,
        dispatcher: Py<PyAny>,
        method_names: Vec<String>,
        access_map: HashMap<u16, String>,
    ) -> PyResult<()>;

    fn start(&self, py: Python<'_>) -> PyResult<()>;
    fn shutdown(&self, py: Python<'_>) -> PyResult<()>;
}
```

**Python registry.py 改造**:
```python
# 旧: Python asyncio server
from c_two.transport.server.core import Server
server = Server(address, config)
await server.start()

# 新: Rust tokio server
from c_two._native import RustServer
server = RustServer(address, config)
server.register_route(name, crm_dispatcher, method_names, access_map)
server.start()  # 内部启动 tokio runtime 线程
```

---

## Phase 3: Client 统一 — SharedClient → Rust

**目标**: 扩展 c2-ipc 为通用 IPC client，支持 SHM buddy 收发，暴露 sync + async 双模式 Python API。

**风险**: 中。c2-ipc 已有 async client 基础，主要是补充 SHM 路径和 sync 包装。

### §3.1 c2-ipc 扩展

当前 `IpcClient` 是 relay 专用（只走 inline frame，无 SHM）。扩展为全功能 client：

```rust
// c2-ipc/src/client.rs (扩展)

impl IpcClient {
    /// 全功能 call — 自动选择传输路径
    pub async fn call_full(
        &self,
        route_name: &str,
        method_name: &str,
        data: &[u8],
        pool: Option<&MemPool>,
    ) -> Result<Vec<u8>, IpcError> {
        let method_idx = self.method_table.get_idx(route_name, method_name)?;

        if let Some(pool) = pool {
            if data.len() > self.config.shm_threshold {
                // SHM buddy path
                return self.call_buddy(route_name, method_idx, data, pool).await;
            }
        }

        if data.len() > self.config.chunk_size {
            // Chunked inline path
            return self.call_chunked(route_name, method_idx, data).await;
        }

        // Inline path (现有逻辑)
        self.call_inline(route_name, method_idx, data).await
    }

    /// SHM buddy 路径
    async fn call_buddy(&self, route: &str, method_idx: u16, data: &[u8], pool: &MemPool)
        -> Result<Vec<u8>, IpcError>
    {
        // 1. pool.alloc(data.len()) → (seg_idx, offset)
        // 2. 写入数据到 SHM
        // 3. 发送 buddy frame (seg_idx, offset, size, flags)
        // 4. 等待 reply (可能是 buddy reply 或 inline)
        // 5. 读取结果，释放分配
    }

    /// 分块传输路径
    async fn call_chunked(&self, route: &str, method_idx: u16, data: &[u8])
        -> Result<Vec<u8>, IpcError>
    {
        // 1. 计算 chunk 数量
        // 2. 逐 chunk 发送 (FLAG_CHUNKED, 最后一个 FLAG_CHUNK_LAST)
        // 3. 等待 reply (可能是 chunked reply)
    }
}
```

### §3.2 SyncClient（新建）

为 Python 同步调用提供阻塞包装：

```rust
// c2-ipc/src/sync_client.rs (新建)

/// 同步 IPC client — 内嵌 tokio runtime
/// Python 侧 cc.connect() 返回的底层 client
pub struct SyncClient {
    inner: Arc<IpcClient>,
    pool: Option<Arc<RwLock<MemPool>>>,
    rt: tokio::runtime::Handle,  // 共享 runtime (不独占)
}

impl SyncClient {
    pub fn connect(address: &str, config: &IpcConfig) -> Result<Self, IpcError> {
        // 使用全局 tokio runtime (与 server 共享)
        let rt = get_or_create_runtime();
        let inner = rt.block_on(IpcClient::connect(address, config))?;
        let pool = rt.block_on(create_pool(config))?;
        Ok(Self { inner: Arc::new(inner), pool, rt })
    }

    pub fn call(&self, route: &str, method: &str, data: &[u8]) -> Result<Vec<u8>, IpcError> {
        self.rt.block_on(self.inner.call_full(route, method, data, self.pool_ref()))
    }

    pub fn close(&self) {
        self.rt.block_on(self.inner.close());
    }
}
```

### §3.3 ClientPool（新建）

```rust
// c2-ipc/src/pool.rs (新建)

/// 引用计数的 client pool — 管理 SyncClient 生命周期
pub struct ClientPool {
    clients: Mutex<HashMap<String, PoolEntry>>,
    grace_period: Duration,  // 引用归零后的宽限期
}

struct PoolEntry {
    client: Arc<SyncClient>,
    ref_count: usize,
    last_release: Option<Instant>,
}

impl ClientPool {
    pub fn acquire(&self, address: &str, config: &IpcConfig) -> Arc<SyncClient>;
    pub fn release(&self, address: &str);
    fn sweep_expired(&self);  // 清理超过宽限期的 client
}
```

### §3.4 Python 双模式 API

**c2-ffi/src/client_ffi.rs (新建)**:

```rust
#[pyclass(name = "RustClient")]
pub struct PyRustClient {
    inner: Arc<SyncClient>,
}

#[pymethods]
impl PyRustClient {
    #[new]
    fn new(address: &str, config: &PyIpcConfig) -> PyResult<Self>;

    /// 同步调用
    fn call(&self, py: Python<'_>, route: &str, method: &str, data: &[u8])
        -> PyResult<PyObject>
    {
        py.allow_threads(|| {
            self.inner.call(route, method, data)
        }).map(|bytes| PyBytes::new(py, &bytes).into())
    }

    fn close(&self, py: Python<'_>) -> PyResult<()>;
}

/// 异步 client — 返回 Python awaitable
#[pyclass(name = "RustAsyncClient")]
pub struct PyRustAsyncClient {
    inner: Arc<IpcClient>,
    pool: Option<Arc<RwLock<MemPool>>>,
}

#[pymethods]
impl PyRustAsyncClient {
    #[new]
    fn new(address: &str, config: &PyIpcConfig) -> PyResult<Self>;

    /// 异步调用 — 返回 coroutine
    fn call<'py>(&self, py: Python<'py>, route: &str, method: &str, data: &[u8])
        -> PyResult<Bound<'py, PyAny>>
    {
        // 使用 pyo3-async 或手动 Future→coroutine 转换
        pyo3_async::tokio::future_into_py(py, async move {
            self.inner.call_full(route, method, data, self.pool_ref()).await
        })
    }

    fn close<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>>;
}
```

**Python proxy.py 改造**:

```python
# ICRMProxy 保留，但底层 client 切换

class ICRMProxy:
    @classmethod
    def thread_local(cls, crm, scheduler, access_map):
        """同进程直连 — 零序列化（保持不变）"""
        ...

    @classmethod
    def ipc(cls, client: 'RustClient', route_name: str, icrm_class):
        """IPC 连接 — 底层改为 Rust SyncClient"""
        ...

    @classmethod
    def ipc_async(cls, client: 'RustAsyncClient', route_name: str, icrm_class):
        """异步 IPC 连接 — 新增"""
        ...
```

### §3.5 Python 侧删除清单

| 文件 | 操作 | 行数 |
|------|------|------|
| `client/core.py` (SharedClient) | 删除 | 982 |
| `client/pool.py` (ClientPool) | 删除 | 185 |
| `client/http.py` (HttpClient + HttpClientPool) | 删除 | 327 |
| **合计** | | **1494** |

**保留**:
- `client/proxy.py` (ICRMProxy) — 类型安全代理，纯 Python 逻辑

### §3.6 c2-http: Rust HTTP Client (新建)

**目标**: 替代 Python `HttpClient` + `HttpClientPool`，完全用 Rust 实现。

**战略意义**: c2-http 的核心逻辑（wire codec + HTTP 调用）可编译为 WASM，
供未来 TypeScript 端的 ICRM 代理直接使用 — 实现 transport 层多语言复用。

```
c2-http/
├── Cargo.toml
└── src/
    ├── lib.rs           # 公开 API
    ├── client.rs        # HttpClient (reqwest, 单连接)
    └── pool.rs          # HttpClientPool (引用计数 + 连接复用)
```

**依赖**:
```toml
[dependencies]
c2-wire = { path = "../c2-wire" }         # Wire 编解码 (错误反序列化)
reqwest = { version = "0.12", features = ["rustls-tls"] }
tokio = { version = "1", features = ["rt", "sync", "time"] }

[target.'cfg(target_arch = "wasm32")'.dependencies]
reqwest = { version = "0.12", features = ["wasm"] }  # WASM: 使用 fetch API
```

**核心接口**:
```rust
pub struct HttpClient {
    client: reqwest::Client,   // 内部连接池
    base_url: String,
    timeout: Duration,
}

impl HttpClient {
    pub fn new(base_url: &str, timeout_secs: u64, max_connections: usize) -> Self;

    /// 同步调用 (block_on)
    pub fn call(&self, method_name: &str, payload: &[u8], route_name: &str) -> Result<Vec<u8>>;

    /// 异步调用
    pub async fn call_async(&self, method_name: &str, payload: &[u8], route_name: &str) -> Result<Vec<u8>>;

    /// 健康检查
    pub async fn health(&self) -> Result<bool>;
}

pub struct HttpClientPool {
    clients: RwLock<HashMap<String, (Arc<HttpClient>, AtomicUsize)>>,  // (client, ref_count)
    grace_period: Duration,
}
```

**HTTP/2 支持**: reqwest 的 `rustls-tls` feature 默认支持 ALPN 协商 HTTP/2。
当 relay 侧启用 HTTP/2 时，客户端自动升级，**零配置切换**。

**PyO3 FFI (在 c2-ffi/http_ffi.rs)**:
```rust
#[pyclass]
struct RustHttpClient(Arc<HttpClient>);

#[pymethods]
impl RustHttpClient {
    #[new]
    fn new(base_url: &str, timeout: f64, max_connections: usize) -> Self;
    fn call(&self, method: &str, payload: &[u8], route: &str) -> PyResult<Vec<u8>>;
    fn call_async<'py>(&self, py: Python<'py>, ...) -> PyResult<Bound<'py, PyAny>>;
}

#[pyclass]
struct RustHttpClientPool(HttpClientPool);

#[pymethods]
impl RustHttpClientPool {
    fn acquire(&self, address: &str) -> PyResult<RustHttpClient>;
    fn release(&self, address: &str) -> PyResult<()>;
}
```

**Python 侧集成** (registry.py 修改):
```python
# Before (v0.3.x):
from c_two.transport.client.http import HttpClient, HttpClientPool

# After (v0.4.0):
from c_two._native import RustHttpClient, RustHttpClientPool
```

**消除 `httpx` 依赖** — pyproject.toml 中移除 `httpx>=0.28.1`。

### §3.7 线程优惠保持不变

`ICRMProxy.thread_local()` 路径完全不受影响 — 同进程直连，零序列化，
不经过任何 transport。这是性能最优路径。

**读写锁保护**: thread_local proxy 在构造时由 `registry.py` 注入
共享的 `Scheduler` 实例和 `access_map`。`call_direct()` 通过
`scheduler.execution_guard(access)` 强制执行 read 并行 / write 独占：

```python
# proxy.py — call_direct() 关键路径
access = self._access_map.get(method_name, MethodAccess.WRITE)
with self._scheduler.execution_guard(access):
    return method(*args)  # 受读写锁保护
```

**Free-threading 下的行为**:
- 多个 `@cc.read` 方法**真正并行**执行 (3.14t 下无 GIL 限制)
- `@cc.write` 方法独占执行 (writer-priority RW lock)
- Scheduler 使用 `threading.Condition`，不依赖 GIL，free-threading 下行为不变

---

## Phase 4: 清理 + v0.4.0 API 定型

**目标**: 删除所有 Python transport 残留代码，定型 v0.4.0 公开 API。

**风险**: 低。前三个 Phase 已完成所有迁移，Phase 4 仅做清理和测试。

### §4.1 Python transport 目录最终结构

```
src/c_two/transport/
├── __init__.py          # 公开 API re-export
├── registry.py          # cc.register/connect/close/shutdown (调用 Rust FFI)
├── config.py            # Python-only 业务配置 (max_workers 等)
├── client/
│   ├── __init__.py
│   └── proxy.py         # ICRMProxy (类型安全代理)
└── relay/
    └── __init__.py      # NativeRelay re-export
```

**删除的目录/文件**:
```
transport/
├── wire.py              # Phase 1 已迁移到 Rust FFI
├── protocol.py          # Phase 1 已迁移到 Rust FFI
├── ipc/                 # Phase 1+2 已迁移
│   ├── frame.py
│   ├── shm_frame.py
│   ├── msg_type.py
│   └── envelope.py
├── server/              # Phase 2 已迁移
│   ├── core.py
│   ├── connection.py
│   ├── handshake.py
│   ├── reply.py
│   ├── dispatcher.py
│   ├── scheduler.py
│   └── heartbeat.py
└── client/
    ├── core.py          # Phase 3 已迁移
    ├── pool.py          # Phase 3 已迁移
    └── http.py          # Phase 3 已迁移 (→ c2-http)
```

### §4.2 v0.4.0 公开 API

```python
import c_two as cc

# ── Server 侧 ──
cc.register(IGrid, grid_instance, name='grid')     # 注册 CRM → Rust server
cc.set_address('ipc://my_server')                   # 设置 IPC 地址
cc.serve()                                          # 启动 Rust tokio server
cc.unregister('grid')                               # 注销 CRM
cc.shutdown()                                       # 关闭 server

# ── Client 侧 (同步) ──
grid = cc.connect(IGrid, name='grid')               # 同进程 → 线程优惠
grid = cc.connect(IGrid, name='grid', address='ipc://server')  # 跨进程 → Rust SyncClient
result = grid.some_method(arg)
cc.close(grid)

# ── Client 侧 (异步, v0.4.0 新增) ──
grid = await cc.connect_async(IGrid, name='grid', address='ipc://server')
result = await grid.some_method(arg)
await cc.close_async(grid)

# ── Relay ──
relay = cc.relay.start(bind='0.0.0.0:8080')        # Rust NativeRelay
relay.register_upstream('grid', 'ipc://server')
relay.stop()
```

### §4.3 测试更新

- 所有现有 679 测试更新 import 路径
- 新增 Rust server 集成测试
- 新增 async client 测试
- 新增 sync + async 混合场景测试
- 保留参数化测试: thread / ipc / http transport

---

## 总览: Rust Crate 架构 (Phase 4 完成后)

```
src/c_two/_native/
├── Cargo.toml          workspace root
├── c2-mem/             共享内存子系统 (已完成)
│   ├── alloc/          buddy 分配算法
│   ├── segment/        POSIX SHM 生命周期
│   ├── pool.rs         统一池 MemPool
│   ├── handle.rs       MemHandle (Buddy/Dedicated/FileSpill)
│   ├── spill.rs        文件溢写 + RAM 检测
│   └── config.rs       PoolConfig
│
├── c2-wire/            Wire 协议层 (Phase 1 扩展)
│   ├── control.rs      Call/Reply 控制面编解码
│   ├── frame.rs        帧头编解码
│   ├── handshake.rs    Handshake v6
│   ├── buddy.rs        SHM buddy payload 编解码
│   ├── flags.rs        所有 flag 常量
│   ├── assembler.rs    ChunkAssembler (已完成)
│   ├── msg_type.rs     MsgType 枚举 (Phase 1 新增)
│   └── config.rs       IpcConfig (Phase 1 新增)
│
├── c2-ipc/             IPC Client (Phase 3 扩展)
│   ├── client.rs       async IpcClient (全功能: inline + buddy + chunked)
│   ├── sync_client.rs  SyncClient 阻塞包装 (Phase 3 新增)
│   ├── pool.rs         ClientPool 引用计数管理 (Phase 3 新增)
│   └── shm.rs          SegmentCache
│
├── c2-server/          IPC Server (Phase 2 新建)
│   ├── server.rs       tokio UDS accept loop
│   ├── connection.rs   per-client 状态
│   ├── dispatcher.rs   CRM 回调 trait + 路由
│   ├── heartbeat.rs    PING/PONG
│   └── scheduler.rs    read/write 并发控制
│
├── c2-relay/           HTTP Relay (已完成)
│   ├── server.rs       axum HTTP server
│   ├── router.rs       HTTP→IPC 路由
│   └── state.rs        UpstreamPool
│
├── c2-http/            HTTP Client (Phase 3 新建)
│   ├── client.rs       reqwest HttpClient (sync + async)
│   └── pool.rs         HttpClientPool (引用计数)
│
└── c2-ffi/             PyO3 统一入口
    ├── lib.rs           模块注册
    ├── mem_ffi.rs       MemPool + MemHandle + ChunkAssembler (已完成)
    ├── relay_ffi.rs     NativeRelay (已完成)
    ├── wire_ffi.rs      Wire 编解码 FFI (Phase 1 新增)
    ├── server_ffi.rs    RustServer FFI (Phase 2 新增)
    ├── client_ffi.rs    RustClient + RustAsyncClient FFI (Phase 3 新增)
    └── http_ffi.rs      RustHttpClient + RustHttpClientPool FFI (Phase 3 新增)
```

### 依赖关系图

```
c2-mem ──────────┐
                 │
c2-wire ─────────┤──── c2-server ────┐
  (依赖 c2-mem)  │                   │
                 │                   │
c2-ipc ──────────┤──── c2-relay ─────┤──── c2-ffi (PyO3)
  (依赖 c2-wire) │  (依赖 c2-ipc)   │   (依赖全部)
                 │                   │
c2-http ─────────┘                   │
  (依赖 c2-wire)                     │
                 └───────────────────┘
```

---

## 实施代码量估算

| Phase | Rust 新增 | Python 删除 | Python 修改 | 净变化 |
|-------|----------|------------|------------|--------|
| 0. Free-threading 修复 | ~120 行 (frozen 重构) | 0 | ~60 行 (加锁) | +180 行 |
| 1. 基础设施统一 | ~800 行 (wire_ffi.rs + msg_type.rs + config.rs) | ~840 行 | ~200 行 (shim) | -40 行 |
| 2. Server 下沉 | ~2000 行 (c2-server + server_ffi.rs) | ~1774 行 | ~150 行 (registry.py) | +226 行 |
| 3. Client + HTTP 统一 | ~1600 行 (sync_client + pool + c2-http + http_ffi) | ~1494 行 | ~100 行 (proxy.py) | +106 行 |
| 4. 清理 | ~0 | ~200 行 (shim + envelope) | ~300 行 (测试 import) | -200 行 |
| **合计** | **~4520 行 Rust** | **~4308 行 Python** | **~810 行 Python** | **+272 行** |

**总结**: 净代码量几乎不变，但 ~4300 行 Python transport 代码被等量 Rust 替代，
得到性能提升 (无 GIL I/O) + 类型安全 + 单一编解码实现 + 多语言复用 (WASM)。

---

## 风险与缓解

| 风险 | 影响 | 缓解措施 |
|------|------|---------|
| PyO3 `&mut self` 在 3.14t 下的 data race | Phase 0 | 全部 `#[pyclass(frozen)]` + 内部可变性 (§6.1.1) |
| CRM 回调并发执行 Python 方法 | Phase 2 | Scheduler read/write 注解 + `tokio::sync::RwLock` (§6.3) |
| `Py<PyAny>` 跨 tokio 任务安全性 | Phase 2-3 | PyO3 0.23+ 中 `Py<T>` 已是 `Send + Sync`；回调仍需 `Python::with_gil()` (§6.2) |
| tokio runtime 与 Python asyncio 冲突 | Phase 2-3 | Rust server 使用独立 tokio runtime（非 Python 主线程的 asyncio loop） |
| Handshake 协议版本升级 | Phase 1 | Rust 和 Python 共享同一 `HANDSHAKE_VERSION`，通过 FFI 确保一致 |
| pyo3-async crate 稳定性 | Phase 3 | 备选方案：手动实现 Python coroutine wrapper (已有社区示例) |
| 性能回归（FFI 调用开销） | Phase 1 | Wire codec FFI 开销 < 1µs/call，远低于 IPC I/O 延迟 (100µs+) |
| Relay 大 payload 内存缓冲 | 现有 | 长期方案：relay 直接 SHM 转发（不缓冲 HTTP body）— 可在 Phase 2 后探索 |

---

## 与既有设计文档的关系

| 文档 | 关系 |
|------|------|
| `2026-03-31-disk-spill-memhandle-design.md` §6 | 本设计是其 Future Roadmap 的具体展开 |
| `c-two-rpc-v2-roadmap.md` §3.0-4.x | 本设计覆盖 P2 磁盘溢写(已完成) 之后的全部传输层演进 |
| `2026-03-29-server-core-decoupling-design.md` | Phase 2 Server 下沉基于其解耦后的模块结构 |
| `2026-03-30-unified-memory-fallback-design.md` | c2-mem 的 alloc_handle 三层架构在本设计中被 c2-server 直接使用 |

---

## §5 Future Extensions: 多语言复用与协议演进

### §5.1 Transport-Agnostic Binding 架构

下沉完成后，C-Two 的 transport 层形成一个语言无关的 Rust 核心：

```
                     ┌─────────────────────────────────┐
                     │       Rust Transport Core       │
                     │                                 │
                     │  c2-wire (no_std, WASM-safe)    │
                     │  c2-http (reqwest / fetch)      │
                     │  c2-ipc  (tokio, UDS)           │
                     │  c2-server (tokio)              │
                     │  c2-relay (axum)                │
                     └──────┬──────────────┬───────────┘
                            │              │
                     ┌──────▼──────┐ ┌─────▼──────────┐
                     │ PyO3 FFI    │ │ wasm-bindgen   │
                     │ (c2-ffi)    │ │ (c2-wasm)      │
                     └──────┬──────┘ └─────┬──────────┘
                            │              │
                     ┌──────▼──────┐ ┌─────▼──────────┐
                     │  Python     │ │  TypeScript     │
                     │  binding    │ │  binding        │
                     │  (registry, │ │  (icrm proxy,   │
                     │   proxy,    │ │   type gen)     │
                     │   crm)      │ │                 │
                     └─────────────┘ └────────────────┘
```

高级语言只负责：
1. **类型映射** — ICRM 接口定义、类型安全代理
2. **业务逻辑** — CRM 方法执行、数据序列化
3. **运行时集成** — Python asyncio / TS event loop 桥接

所有传输细节（协议版本、编解码、连接管理、重试、心跳）由 Rust 层统一实现。

### §5.2 TypeScript ICRM 代理 (c2-wasm)

**目标**: 让浏览器/Node.js 端直接调用 C-Two CRM 资源。

**可复用组件**:
- `c2-wire` — 编译到 `wasm32-unknown-unknown`，提供 Wire 编解码
- `c2-http` — WASM 构建使用 `reqwest` 的 `wasm` feature (底层为 `fetch` API)

**不可复用** (需 TS 原生实现):
- ICRM 类型代理 — TypeScript 泛型 + Proxy 对象
- @transferable 序列化 — 需 TS 侧的 serialize/deserialize 实现

**构建工具链**: `wasm-pack` → npm 包发布

```
@c-two/transport (npm)
├── c2_wire_bg.wasm     # c2-wire 编译产物
├── c2_http_bg.wasm     # c2-http 编译产物 (fetch backend)
├── index.ts            # TypeScript binding
└── proxy.ts            # ICRMProxy<T> 泛型代理
```

### §5.3 HTTP/2 升级路径

**当前状态**: Relay (axum/hyper) 已支持 HTTP/2，但默认 HTTP/1.1。

**升级步骤** (仅 Rust 配置变更，零 Python/TS 代码改动):

1. **Relay 侧**: axum `serve()` 启用 TLS → hyper 自动 ALPN 协商 HTTP/2
2. **Client 侧**: `reqwest` 已默认支持 HTTP/2 (有 TLS 时)
3. **收益**:
   - 多路复用 — 单 TCP 连接承载多个并发 RPC，消除 head-of-line blocking
   - 头部压缩 (HPACK) — 减少重复 header 开销
   - 服务器推送 — 可用于 CRM 事件通知 (未来)

**长期**: HTTP/3 (QUIC) — `hyper` 和 `reqwest` 社区已有实验性支持。
当 QUIC 稳定后，同样只需 Rust 层配置切换。

### §5.4 迁移影响总结

| 维度 | 当前 (v0.3.x) | 下沉后 (v0.4.0) | 长期 |
|------|--------------|-----------------|------|
| 传输实现 | Python + Rust 双重 | Rust 单一 | Rust 单一 |
| 支持语言 | Python only | Python | Python + TypeScript |
| HTTP 协议 | HTTP/1.1 (httpx) | HTTP/1.1 + 2 (reqwest) | + HTTP/3 (QUIC) |
| Python 依赖 | httpx, asyncio | 无额外依赖 | 无额外依赖 |
| Wire codec 修改 | 改两处 (Py + Rs) | 改一处 (Rust) | 改一处 (Rust) |
| 新传输协议 | 需改 Python | 仅改 Rust | 仅改 Rust |

---

## §6 Free-Threading Safety (Python 3.14t)

C-Two 明确以 Python 3.14t (free-threading / no-GIL) 为目标平台。
本节定义 (1) 现有代码的修复清单 (Phase 0)，(2) 新代码必须遵循的并发安全模式。

### §6.1 Phase 0: 现有代码修复 (Phase 1 前置)

Phase 0 不涉及功能变更，仅修复已有代码的自由线程安全性问题。

#### 6.1.1 Rust FFI: `#[pyclass(frozen)]` + 内部可变性

**问题**: 当前 `#[pyclass]` 类型使用 `&mut self` 方法，在 free-threading 下
多线程并发调用会导致未定义行为 (data race on Rust side)。

**修复模式**: 所有 `#[pyclass]` 统一采用 `frozen` + 内部可变性：

```rust
// ❌ 当前 (不安全)
#[pyclass(name = "NativeRelay")]
pub struct PyNativeRelay {
    server: Option<RelayServer>,
}
#[pymethods]
impl PyNativeRelay {
    fn start(&mut self, py: Python<'_>) -> PyResult<()> { ... }
}

// ✅ 修复后
#[pyclass(name = "NativeRelay", frozen)]
pub struct PyNativeRelay {
    state: Mutex<RelayState>,  // 内部可变性
}
#[pymethods]
impl PyNativeRelay {
    fn start(&self, py: Python<'_>) -> PyResult<()> {
        let mut state = self.state.lock().unwrap();
        // ...
    }
}
```

**需修复的类型**:

| PyClass | 文件 | `&mut self` 方法 | 修复方式 |
|---------|------|-----------------|---------|
| `PyNativeRelay` | relay_ffi.rs | `start`, `stop` | `Mutex<RelayState>` |
| `PyMemHandle` | mem_ffi.rs | `write_at`, `release`, `drop` | `Mutex<Option<MemHandle>>` |
| `PyChunkAssembler` | mem_ffi.rs | `feed_chunk`, `finish`, `abort` | `Mutex<AssemblerState>` |

**已安全的类型** (无需修改):
- `PyMemPool` — 已使用 `Arc<RwLock<MemPool>>`，方法为 `&self` ✅

#### 6.1.2 Python: 竞态条件修复

**ICRMProxy._closed 标志** (`proxy.py`):
```python
# ❌ 当前: 无锁读写
def call_direct(self, method_name, args):
    if self._closed:  # race
        raise RuntimeError(...)

# ✅ 修复: 加锁 (或使用 threading 原子操作)
def __init__(self):
    self._close_lock = threading.Lock()
    self._closed = False

def call_direct(self, method_name, args):
    with self._close_lock:
        if self._closed:
            raise RuntimeError(...)
```

**ServerV2._client_tasks** (`server/core.py`):
```python
# ❌ 当前: append/remove 无锁
self._client_tasks.append(task)

# ✅ 修复: 统一用 _slots_lock 保护
with self._slots_lock:
    self._client_tasks.append(task)
```

#### 6.1.3 Phase 0 完整修复清单

| 优先级 | 组件 | 修复 | 影响范围 |
|--------|------|------|---------|
| P0 | PyNativeRelay | `frozen` + `Mutex` | relay_ffi.rs |
| P0 | PyMemHandle | `frozen` + `Mutex<Option<>>` | mem_ffi.rs |
| P0 | PyChunkAssembler | `frozen` + `Mutex` | mem_ffi.rs |
| P1 | ICRMProxy._closed | 加 `_close_lock` | proxy.py |
| P1 | ServerV2._client_tasks | 加锁保护 | server/core.py |
| P2 | IpcClient | 显式 `unsafe impl Send + Sync` | client.rs |

### §6.2 PyO3 安全模式规范 (所有新代码)

Phase 1-4 中所有新建的 `#[pyclass]` 必须遵循以下规范：

#### 规则 1: 全部使用 `#[pyclass(frozen)]`

```rust
// 强制 frozen — Python 侧不可赋值属性
#[pyclass(name = "RustServer", frozen)]
pub struct PyRustServer { ... }
```

**理由**: `frozen` 使 PyO3 在编译期阻止 `&mut self` 方法，
强制开发者使用内部可变性，从源头消除 data race。

#### 规则 2: 内部可变性选择标准

| 场景 | 选用 | 理由 |
|------|------|------|
| 短暂互斥 (< 1ms) | `std::sync::Mutex` | 简单，低争用下性能好 |
| 读多写少 | `std::sync::RwLock` | 允许并发读 |
| 简单标志/计数器 | `AtomicBool` / `AtomicU64` | 无锁，零开销 |
| 跨 await 持有 | `tokio::sync::Mutex` | 异步友好，不阻塞 runtime |
| 一次初始化 | `OnceLock<T>` | 零运行时开销 |

#### 规则 3: Python 对象引用

```rust
// ❌ 禁止在 #[pyclass] 中存储裸 PyObject
struct Bad {
    callback: PyObject,  // 不安全: PyObject 非 Send
}

// ✅ 使用 Py<T> + Send 包装
struct Good {
    callback: Py<PyAny>,  // Py<T> 在 free-threading 下是 Send
}
```

`Py<PyAny>` 在 PyO3 0.23+ 中是 `Send + Sync`，可安全跨线程持有。
但调用 Python 方法仍需 `Python::with_gil()`。

#### 规则 4: CRM 回调的 GIL 交互 (Phase 2 关键)

```rust
// c2-server 中的 CRM 回调执行
async fn dispatch_crm_call(
    callback: &Py<PyAny>,
    method_idx: u16,
    payload: &[u8],
) -> Result<Vec<u8>> {
    // 在 spawn_blocking 中执行，避免阻塞 tokio runtime
    tokio::task::spawn_blocking(move || {
        // with_gil: free-threading 下不真正获取 GIL，
        // 但确保 Python 解释器可用
        Python::with_gil(|py| {
            let result = callback.call1(py, (method_idx, payload))?;
            result.extract::<Vec<u8>>(py)
        })
    }).await?
}
```

**Free-threading 下的行为差异**:
- `Python::with_gil()` 不再阻塞等待 GIL，近乎零开销
- 多个 `spawn_blocking` 任务可**真正并行**执行 Python CRM 方法
- **CRM 方法本身**需要线程安全 — 这是用户的责任，框架通过 Scheduler 的
  read/write 注解提供保护

### §6.3 各 Phase 并发设计要点

#### Phase 1: Wire FFI

- `wire_ffi.rs` 中的编解码函数均为**无状态函数** (`#[pyfunction]`)
- 无 `#[pyclass]`，无共享状态 → **天然线程安全**
- `MsgType` 和 `IpcConfig` 若暴露为 `#[pyclass]`，用 `frozen` + 只读属性

#### Phase 2: Server

```rust
#[pyclass(name = "RustServer", frozen)]
pub struct PyRustServer {
    // 所有状态通过 Arc 共享给 tokio 任务
    inner: Arc<ServerInner>,
    runtime: Arc<tokio::runtime::Runtime>,
}

struct ServerInner {
    slots: RwLock<HashMap<String, CrmSlot>>,  // CRM 注册表
    shutdown: tokio::sync::Notify,             // 关闭信号
}

struct CrmSlot {
    callback: Py<PyAny>,        // CRM 实例 (Send + Sync via Py<T>)
    access_map: Vec<AccessMode>,  // 方法级 read/write 标注
}
```

- `Py<PyAny>` 持有 CRM Python 对象，跨 tokio 任务安全共享
- Scheduler 使用 `tokio::sync::RwLock` 实现 read 并行 / write 互斥
- 注册/注销操作通过 `slots: RwLock<...>` 保护

#### Phase 3: Client + HTTP

```rust
#[pyclass(name = "RustClient", frozen)]
pub struct PyRustClient {
    inner: Arc<SyncClient>,  // SyncClient 内部全 Arc + Mutex
}

#[pyclass(name = "RustHttpClient", frozen)]
pub struct PyRustHttpClient {
    client: Arc<HttpClient>,  // reqwest::Client 自身是 Clone + Send + Sync
}

#[pyclass(name = "RustHttpClientPool", frozen)]
pub struct PyRustHttpClientPool {
    pool: Arc<RwLock<HashMap<String, PoolEntry>>>,
}
```

- `reqwest::Client` 原生 `Send + Sync`，内置连接池
- `SyncClient` 通过 `Arc<IpcClient>` + `rt.block_on()` 实现阻塞调用
- `PyRustHttpClientPool` 用 `RwLock` 保护连接映射

### §6.4 测试策略

```bash
# Phase 0 验证: 在 free-threading 模式下跑全量测试
PYTHON_GIL=0 uv run pytest tests/ -q --timeout=30

# 并发压力测试: 多线程同时调用同一 CRM
uv run pytest tests/integration/test_free_threading.py -q
```

**新增测试用例**:
- `test_concurrent_proxy_close`: 多线程同时 close 同一 proxy
- `test_concurrent_crm_read`: 多线程并行读操作 (应不阻塞)
- `test_concurrent_crm_write`: 多线程写操作 (应串行)
- `test_relay_concurrent_start_stop`: 并发 start/stop relay
- `test_mem_handle_concurrent_write_at`: 并发写入同一 MemHandle (应正确阻塞或报错)


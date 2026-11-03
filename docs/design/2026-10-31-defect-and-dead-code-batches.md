# 真缺陷与死代码两批的执行计划

日期：2026-10-31。输入：分支 `socu/interface-slimming`（接口精简第一批已验收）。依据：[过度防御复审](2026-10-31-over-defense-review.md)第一、二节；用户 2026-10-31 同意先做这两批。状态：已实施并验收，fork 全量 CI [41192397960](https://github.com/Dsssyc/c-two/actions/runs/41192397960)（`e1630cf1`）在四个平台上通过；余项见[待办清单](backlog.md)。

## 范围

复审第一节的真缺陷与第二节的死代码，按 crate 分成四个微任务，写入范围互不重叠。

不在这两批里的：

- D-tmp：先在 Linux 上复现，单独做。
- E-host：归入结构调整。
- A9 与 B1：都涉及协议，归入协议批。
- F7 中的 `/_admin/unregister`，以及注销的 202/204/404 区分：等用户决定。
- G2、G8：等用户决定。
- 各区"零碎"里属于重复校验的部分：归入下一批的重复校验。

## 共同要求

写进每个微任务的任务书。

- **0.x 一次切干净。** 修根因，不在症状处收口；不留别名、旧路径、兼容参数或"已删除"的提示；不加防御性检查。校验只在进程边界上做一次，出错就直接失败。
- **测试。** 被取代的代码、测试与文档一并删除。只钉源码文本、钉已删名字的测试，随对应的生产代码一起删掉。缺陷修复先写能复现的测试。
- **环境。** 测试只用测试自己创建的端点；临时探针用新建的 0700 目录作 IPC 根。不启动、停止或重启日常服务。
- **范围外。** 写入范围之外什么也不删；不用 git stash，不动分支与标签。要改范围之外的文件，停下来问。AGENTS.md 由 Host 改。
- **门禁与报告。** 依赖相邻 `../fastdb` 的门禁由 Host 在并排的检出里补跑，微任务的检出不必复现。日志放在 `target/<任务名>-validation/`；最终报告不超过 1500 字。

## M1：内存、wire 与服务端

写入范围：

- `core/foundation/c2-mem`、`core/foundation/c2-mem-ffi`、`core/protocol/c2-wire`、`core/transport/c2-server`
- `sdk/python/native/src/mem_ffi.rs`、`sdk/python/native/src/lease_ffi.rs`
- Python 侧引用 `BufferLeaseTracker(track_transient=)` 与租约统计字段的代码和测试

缺陷：

- **B8：** `c2-wire/src/assembler.rs:85` 把全局在途分片数（默认 512）当成单个请求的分片上限。改为单个请求只受 `max_payload_size` 与 `max_reassembly_bytes` 约束，删掉按片数计的单请求上限；尺寸超限报尺寸错误，不报 ProtocolViolation。全局在途分片的准入保持不变。先写测试：强制分片时 100 MiB 的请求能成功。
- **B10：** 格式错误的调用控制帧只记一条 warn 就丢弃（`c2-server/src/server.rs:2428-2438`、`:2485-2490`、`:3516`、`:3665-3670`），对端一直等到超时。进程边界上的畸形输入改为直接以协议错误断开连接。
- **A8（池一侧）：** 删掉 `c2-mem/src/pool.rs:1196` "返回 0 即不建 buddy"的静默降级，几何非法即报错。c2-config 那一处的拒绝由 M3 做。

死代码：

- **A2：** `FreeResult` 与 c2-server 的延迟 peer GC。`free_at` 自己已经丢弃 peer 视图（`pool.rs:856-858`），所以 `gc_peer_buddy` 任务（`server.rs:3548-3556` 及 4 处调用、`connection.rs:256-263`）总是扑空。`free_at` 与 `release_handle` 改为返回 `()`。
- **A3：** 租约跟踪的 Transient 维度从未构造，没有调用方传 `track_transient=true`；`active_leases` 与 `active_holds` 两套计数恒等，`StorageLeaseStats` 与 `DirectionLeaseStats` 相同，守卫的 Option 恒为 Some。见 `lease.rs:25-38`、`:71-139`、`:153-158`、`:227-262`、`:277-315`、`:353-362`，以及 `lease_ffi.rs:184-187`。删除后只留一套计数，Python 侧的 `memory_stats()["holds"]` 随之收为一套字段。
- **A12 中的死 API 与不可达分支：**
  - 只用一次的预算守卫断言与恒为 Some 的守卫字段（`buddy_segment.rs:74-80`、`dedicated.rs:93-99`、`spill.rs:52-58`）；
  - 没有调用方的公开 API：`validate_owner_incarnation`、`ensure_ready`、`BuddySegment::data_slice*`/`region`/`is_owner`、`ShmRegion::as_slice*`、`LevelBitmap::init_all_free`/`word_count`/`block_count`、`BuddyAllocator::is_poisoned`；
  - 空洞的 `cfg!(target_has_atomic)` 断言（`buddy.rs:164-171`）；
  - FFI `request_pool_write` 里不可达的清理分支（`c2-mem-ffi/src/lib.rs:878-918`）；
  - `MemPool` 构造器在名字非法时 panic，迫使 FFI 与 Python 端用 `OWNER_INCARNATION_SUFFIX_LEN=41` 预先复刻私有格式。改为构造器返回 `Result`，删掉预先校验（`c2-mem-ffi/src/lib.rs:180`、`:688`，`mem_ffi.rs:560`）。
- **B10 零碎中的死代码：**
  - 不可达且没有使用者的 `HeartbeatResult::Disabled`；
  - 重复的 `Server::ipc_address` 与 `PeerShmState.prefix`；
  - 无用的 `FLAG_SHM`、`FLAG_POOL`、`FLAG_DISK_SPILL`；
  - `chunk/mod.rs` 里"向后兼容"的重导出；
  - `lib.rs` 里过时的文档（11 字节 buddy 载荷、段公告）；
  - 只服务测试的 `dispatch_call`（`server.rs:3504-3542`）：它复制了接收循环，让 14 个测试绕开真实循环。删掉它，改为让这些测试走真实循环。

检查：

- `cargo test --manifest-path core/Cargo.toml -p c2-mem -p c2-mem-ffi -p c2-wire -p c2-server`
- `uv sync --reinstall-package c-two`
- `uv run python tools/dev/test_native.py`
- Python 测试中与内存、租约、分片相关的部分，以及 `tests/repo`

## M2：IPC 客户端与本地端点

写入范围：

- `core/transport/c2-ipc`、`core/transport/c2-local`
- 为 C3 牵连到的几行，另外授权：`core/runtime/c2-core/src/error.rs` 里映射被删 IPC 错误的分支、`core/transport/c2-http/src/relay/state.rs:144-155`、`:486`

缺陷：

- **C8：** `c2-ipc/src/control.rs:338-340` 把格式错误的 shutdown ACK 当成"未确认"，改为解析失败即报错。
- **D4：** `c2-local/src/unix_managed.rs:720` 的 `INIT_SCAN_LIMIT=64` 删掉，扫描到底；同时改掉正在断言这一行为的现有测试。
- **D-drop：** Drop 遇到 gate 争用时按设计放弃清理，8 路并行 Drop 256 对留下 50 组 socket 与 lease。改为在争用时把收尾交给一个短期后台线程，用与 close 相同的有界等待；Drop 本身不阻塞调用线程。先写测试复现留下的残余。
- **D9：** `c2-local/src/lib.rs:345` 的 `MAX_ADDRESSES = 4096` 没有依据，删掉。

死代码：

- **C1：** SyncClient 的预分配 API、注入池与无池构造器，没有生产调用方。涉及：
  - `sync_client.rs`：`:73-164`、`:293-331`、`:463-623`；
  - `client.rs`：`IpcClient::new`、`with_pool*`；`pool_injected`、`pool_transport_owned` 等标志；`copy_and_release_armed`；方向门、池身份检查与第三个 u32 上限。

  连同只为它们存在的测试一起删掉。AGENTS.md 里"Rust IPC preallocation remain unchanged"的说法由 Host 改。
- **C3：** IPC 端 route-watch 恢复是死代码。涉及 `CatalogCompacted`/`WatchUnavailable` 分支、NACK 映射、重建分支、dirty flag、`ensure_route_contract`（`client.rs:5159-5196`、`sync_client.rs:677-682`）、从未构造的 `IpcError::Handshake`。连带删掉 Core 与 relay 里对应的映射；那几行已在上面单独授权。
- **C8 零碎中的死代码：**
  - 恒为 Some 的 `server_pool` Option，以及它在 `response.rs:130-132`、`:210-212` 的检查；
  - 死绑定 `_server_pool`（`client.rs:3681`）；
  - 接收循环里不可达的 `Err(_) => continue`（`client.rs:6090-6097`）。帧头固定 16 字节，解码只会在输入过短时失败，所以改为不会失败的帧头解码；
  - 只有 c2-http 测试使用的生产方法 `force_connected`：移到 test-support 后面；
  - 钉源码文本的测试（`sync_client.rs:1150-1162`、`tests.rs:334-356`、`client.rs:8046-8058`）；
  - `client.rs:4587-4591` 写着"Unreachable"的注释说错了（关闭写端超时时可以走到）：只改注释，分支保留。

检查：

- `cargo test --manifest-path core/Cargo.toml -p c2-ipc -p c2-local -p c2-core -p c2-http`
- `uv sync --reinstall-package c-two`
- Python 的 IPC、端点与生命周期测试，以及 `tests/repo`

## M3：Core、配置与 Python

写入范围：

- `core/runtime/c2-core`：除 M2 授权的那几行外
- `core/foundation/c2-config`、`core/foundation/c2-contract`
- `sdk/python/src`：除 M1 的租约部分外
- `sdk/python/native`：除 M1 的 `mem_ffi.rs` 与 `lease_ffi.rs` 外
- `cli/src/contract.rs`、`sdk/rust/src/contract.rs`
- `docs/configuration.md`、`docs/configuration.en.md`、`sdk/python/README.md`

缺陷：

- **H3：** 删掉 `C2_RELAY_ROUTE_MAX_ATTEMPTS`，以及它在 c2-config、Core（`session.rs:1306` 的 `clamp(1, 2)`）、Python 与文档里的全部痕迹。Core 固定为"一次观察，加至多一次刷新"。
- **H12：** `max_execution_workers` 只对默认值保留上限，显式值不再设 64 的硬上限。
- **A8（配置一侧）：** c2-config 在一处拒绝向上取 2 的幂后超出 u32 的 `pool_segment_size`。
- **G4：** relay 连接路径把任何非语义异常都改标成 `RegistryUnavailable: Relay unavailable`（`registry.py:712`）。改为只映射语义错误，其余原样抛出。
- **E13：** Core 控制面把格式错误的地址变成 `Ok(false)` 或空的 ACK（`control.rs` 里的 `validate_address` 分支），改为报错，与 `direct_ipc_endpoint` 一致；`host.rs:1185` 的 `let _ = shutdown_and_wait` 吞掉了错误，删掉这次重复的调用，交给 6 行之后的 Core 事务。

死代码：

- **E3：** 一次性的 delivered 交接（`session.rs:192`、`:251`、`:2017-2022`、`:349-352`）。
- **E6：** 只服务测试的 scope 重建回退（`session.rs:1875-1889`）。
- **E7：**
  - 从未构造的错误变体 `RelayDuplicateRoute` 与 `RelayHttp`，连同原生分支与 `error.py` 里的对应项；
  - 从不置位的 `http_clients_drained`；
  - 可由 `ipc_client_close_details` 推出的 `ipc_clients_abnormally_closed`。

  Rust 结构、PyO3 投影、Python 的 `ShutdownOutcome` 与文档同步改。
- **H2：** c2-config 里完整的 resolver 只有测试在用（`resolver.rs:168-194`、`:220-249`、`:416-437`，以及 `ResolvedRelayConfig` 的死字段）。删掉这套聚合 resolver；`session.rs:1287-1300` 读了 4 次 `.env`，改为经一个窄的 relay 客户端 resolver 只读一次。
- **H5：** contract 准入的可配置限额没有使用者：
  - 只有一个变体的 `ContractLimitsProfile`；
  - `with_*` 构造器：`with_max_methods` 还会静默截断；
  - 调试用的线程局部准入计数器，以及 `cli/src/contract.rs:216-229`、`codegen_ffi.rs:23-30` 里永远成立的断言；
  - 五个 `*_with_limits`。

  全部改为常量。`sdk/rust` 里的 `ContractLimits`、`ContractLimitsProfile` 导出随之删掉。
- **H13 中 c2-config 的部分：**
  - 没有生产使用者的 `BASE_IPC_OVERRIDE_KEYS`，以及互相断言键列表的测试；
  - 只有一项的 `FORBIDDEN_IPC_OVERRIDE_KEYS`；
  - `ClientIpcConfig` 里与默认值相同的"64 MB 覆盖"；
  - 重复的 `validate_ipc_region_id`（与 `validate_server_id` 相同）和 `validate_named_route_name`（与 `validate_call_route_key` 相同）。
- **G1：** 服务端的 write-plan sink 没有生产者：`sdk/python/native/src/writable_sink.rs`、`core_ffi.rs:20`、`:683-699`、`lib.rs:36-37`，以及相关测试。AGENTS.md 里"server-side writable sinks remain unchanged"的说法由 Host 改。
- **G6：** R23 留在 PyO3 层的参数与零调用方法（`runtime_session_ffi.rs`）：
  - `register_route` 的 `relay_anchor_address`；
  - `unregister_route` 的 `_relay_anchor_address`；
  - `shutdown` 的 `route_names` 与 `relay_anchor_address`，以及 `check_shutdown_route_names`；
  - `effective_relay_anchor_address`、`clear_relay_projection_cache`、`shutdown_timeout_seconds`。

  Python 侧约 12 处调用随之改。
- **G-slot：** Python 注销时"未排空就保留槽位"的分支走不到：Core 的 unregister 阻塞到排空，close 恒为成功（`session.rs:1857`）。
- **G10 中的死代码与静默分支：**
  - `cc.close` 静默忽略非代理对象（`registry.py:1196-1198`），而同类函数都抛 `TypeError`：改为抛 `TypeError`；
  - 已关闭代理的 `CRMProxy.__getattr__` 抛 `RuntimeError`，使 `hasattr` 失效（`proxy.py:177-183`）；
  - `_rpc_parameters` 吞掉签名错误，结果静默变成无载荷（`transferable.py:653-656`）：改为报错；
  - `serve` 里不可达的 `except KeyboardInterrupt`（`registry.py:933`）；
  - 只服务测试的 `_resolve_server_ipc_config`（`config/ipc.py:88-112`）；
  - 没有调用方的 `get_slot_info` 与 `_extract_namespace`。

检查：

- `cargo test --manifest-path core/Cargo.toml -p c2-core -p c2-config -p c2-contract`
- `cargo test --manifest-path cli/Cargo.toml`
- `cargo test --manifest-path sdk/rust/Cargo.toml --all-features`
- `uv sync --reinstall-package c-two`
- `uv run python tools/dev/test_native.py`
- Python 测试：除依赖 `../fastdb` 的部分外全部运行
- Python 3.10 语法检查与 `tests/repo`

## M4：relay、代码生成与 CLI

写入范围：

- `core/transport/c2-http`：除 M2 授权的 `state.rs` 那几行外
- `core/foundation/c2-codegen`
- `cli`：除 `cli/src/contract.rs` 外
- `cli/README.md`

缺陷：

- **F5c：** mesh join 只拒绝更新的协议版本，接受更旧的（`relay/peer_handlers.rs:38`）。各处统一用相等比较。
- **F6：** HTTP `/_register` 对任何连接错误在 500 ms 内每 10 ms 重试一次，包括 AGENTS.md 规定为终止、不得重试的版本不匹配；CLI `--upstream` 路径另有一套注册与重试。把两条注册路径合一，只对传输错误重试。
- **F9：** `relay/client_lifecycle.rs:14` 的 `MAX_CLIENT_OWNERS = 1024` 没有依据，删掉。
- **H10：** `c3 registry` 把缺字段或格式错误的响应显示成"No routes registered."，peers 显示成"?"（`cli/src/registry.rs:74`）。改为按类型反序列化，格式错误就报错。
- **H11：** TS 资产照抄一份错误名表来核对 NACK 里冗余的 name 字段（`c2-codegen/assets/typescript_transport.ts`），c2-error 新增错误码后会被拒为"非法 NACK 信封"。改为只校验信封的形状。

死代码：

- **F1：** relay 的路由事件历史、watch 与 compaction 没有读取方：
  - `route_table.rs`：`:30-41`、`:43-77`、`:106-137`、`:215-218`、`:399-412`、`:506-513`、`:536-545`、`:563-608`、`:973-983`；
  - `types.rs:103`、`state.rs:982-984`、`background.rs:110-131`。

  不改 mesh 线上格式；如果发现牵涉到 mesh 消息，停下来问。
- **F7 中不等用户决定的部分：** RelayAwareHttpClient 里没有调用方的 `call_phased`、`call_controlled`、`resolve_target`、`resolve_http_target`、`resolve_target_after_local_ipc_failures`（及其 `_with_deadline_async`），以及 `HttpClientPool` 只服务测试的按 URL 手动释放（`pool.rs:141-157`、`:259-296`）。`/_admin/unregister` 与注销状态码不动。
- **F10 中的死代码：**
  - 种子节点加入做了两次（`server.rs:413-451` 与 `background.rs:320-372`），合为一处；
  - 运行时初始化里不可能发生的 `cell.set` 失败分支（`http_client.rs:71-85`）；
  - `authority.rs:478-484` 把错误又包进它自己；
  - `ControlError` 范围过宽导致的 `unreachable!` 分支（`router.rs:966`、`:982`、`:1022`，`server.rs:679`）：收窄类型，删掉这些分支。
- **H13 中 CLI 与代码生成的部分：**
  - relay-id 后缀的清洗：`uuid[..8]` 总是十六进制（`cli/src/relay.rs:84-104`），清洗是多余的；
  - 代码生成里从 c2-contract 复制来的 `canonical_json` 与 `lower_hex`（`compile.rs:357-382`、`lib.rs:111-118`）：改为调用 c2-contract 已有的公开函数，不改 c2-contract。

检查：

- `cargo test --manifest-path core/Cargo.toml -p c2-http -p c2-codegen`。c2-codegen 中依赖 `../fastdb` 的部分由 Host 补跑。
- `cargo test --manifest-path cli/Cargo.toml`
- `uv sync --reinstall-package c-two`
- Python 的 relay 与 CLI 相关测试，以及 `tests/repo`

## 集成与验收

- **集成。** Host 按 M1、M2、M3、M4 的顺序以三方合并集成，并逐个读完差异。
- **跨范围调用点。** 一个微任务改了签名的 API（例如 M1 把 `MemPool` 的构造器改为返回 `Result`、删掉 chunk 的旧路径重导出），它在别的写入范围里的调用点，并行期间谁都不改；四个微任务集成之后，另起一个微任务 M5，在集成后的树上统一迁移这些调用点，并跑全套检查。M2 的 C1、C3 与 C8 零碎中的死代码也移到 M5：删掉 `IpcClient::new` 要改 c2-http 里约 100 处测试调用，与正在改 relay 的 M4 冲突。M4 的 H13 同样移到 M5：relay-id 的清洗实际在 c2-config，代码生成要改用的 `canonical_json`、`lower_hex` 在 c2-contract 里还没有公开，两处都在 M3 的范围内。
- **合并后的检查。** 在与 CI 相同的并排布局里跑全套：Python、Rust、代码生成、矩阵与 `tests/repo`。
- **Host 的文档。** Host 同步更新 AGENTS.md（预分配与服务端 sink 的说法、环境变量表）、发行说明与 CHANGELOG。
- **CI 与验收。** 合并后的候选在 fork 上跑一次全量 CI，通过后再在看板上验收四个微任务；CI 失败时，交给对应的微任务继续修。

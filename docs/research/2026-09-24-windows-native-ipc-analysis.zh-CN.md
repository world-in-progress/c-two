# C-Two 原生 Windows 适配分析与实施建议

日期：2026-09-24。状态：Proposed，源码审查与方案；Windows 后端尚未实现，本轮未触发 Windows Actions，也没有 Windows 编译、运行或安装包通过记录。

审查基线：C-Two `dev-feature`，commit `58ebac546eb05ecdedac036091733a5f12051b1d`；相邻 FastDB checkout 为 `4daf3e9ba31658d9dfc6b286281d2892e589da02`。FastDB 当前 checkout 与旧候选证据使用的 `7eb74734926bd8fe911229eee9744a6dd8172487` 不同，实施时必须明确选择并固定依赖源码。本文中的新类型、模块、测试与 workflow 名称均为设计建议。

## 1. 建议的目标结构

Windows 旁路应当是 Rust 底层的平台后端：本地控制通道使用 Named Pipe，共享内存使用 Windows file mapping；CRM、路由、调度、错误、租约和 shutdown 仍由现有 Core 统一管理。Python/Rust 使用相同公共 API；同进程 Python 调用继续直接传递 Python 对象；跨机器继续使用 HTTP relay。

建议第一个支持目标为 `x86_64-pc-windows-msvc`，原生 Windows client、resource host 与独立 `c3 relay` 均能运行，默认本机 IPC 范围为同一用户、同一登录会话。ARM64、Windows Service 跨会话共享内存、Windows 桌面系统版本的认证范围分别记录，不能由 Windows Server CI 通过自动推导。Windows 11 x64 是合适的桌面验收目标，具体最低版本要由实际执行证据确定。

| 层与 owner | 公共职责 | Unix 后端 | Windows 后端 |
| --- | --- | --- | --- |
| `c2-core` | Resource/route identity、准入、重试分类、生命周期、lease ordering | 共用 | 共用 |
| `c2-wire` | 帧、完整契约、payload 引用、shutdown 控制协议 | 共用 | 共用 |
| 建议新增 `core/transport/c2-local` | 本地 endpoint、connect/listen、读写拆分、关闭和取消 | Tokio UDS | Tokio byte-mode Named Pipe |
| `c2-mem` | 分配器、共享区域、内存预算、租约统计和释放 | POSIX SHM/mmap | Named file mapping / mapped file |
| `c2-ipc` / `c2-server` | 同一客户端/服务端协议与调度实现 | 消费 `c2-local` 和 `c2-mem` | 消费相同边界 |
| Rust/Python/生成的 Node 客户端 | 语言接口、payload adapter | 共用契约与各语言投影 | 平台差异尽量在 native owner 内解决 |

这个拆分让 `c2-ipc` 和 `c2-server` 共用 endpoint 与 stream 实现，避免 client、server、ping/shutdown、relay 各自拼一套 Windows 路径。`c2-local` 只负责本机 I/O，不依赖 CRM 或上层 runtime；内存映射继续留在 `c2-mem`，避免创建一个拥有所有机制的通用 platform 模块。

逻辑地址继续使用 `ipc://<server_id>`。OS 名称由 Core 侧根据逻辑 ID 和本地访问范围生成，Windows 可映射为 `\\.\pipe\c_two-<scope>-<id-hash>`。使用确定性摘要处理名称长度、Unicode 和 Windows 名称大小写规则，握手仍验证原始 `server_id`、`server_instance_id` 和完整 `ExpectedRouteContract`。pipe 名称部分不再包含目录分隔符；pipe 路径与 kernel object 名称不应进入 CRM descriptor 或 payload schema。[CreateNamedPipe 名称要求](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-createnamedpipea)

Tokio 1.53.1 已在本仓库 lockfile 中，提供 Windows Named Pipe API；`UnixStream` 本身仍只在 Unix 编译。Windows 有某些 AF_UNIX 能力，也不能据此直接编译当前 Tokio UDS 路径。[Tokio Named Pipe](https://docs.rs/tokio/1.53.1/tokio/net/windows/named_pipe/)、[Tokio UnixStream](https://docs.rs/tokio/1.53.1/tokio/net/struct.UnixStream.html)

## 2. 当前阻塞的源码证据

以下行号以审查基线为准；这是静态证据，不能代替 Windows 执行结果。

| 范围 | 证据位置 | 对适配的影响 |
| --- | --- | --- |
| IPC client/server | `core/transport/c2-ipc/src/client.rs:15`；`core/transport/c2-server/src/server.rs:14`、`:1026`；`core/transport/c2-server/src/heartbeat.rs:9` | 无条件导入 Unix stream/listener/owned half，Windows 编译被阻塞。 |
| 控制面 helper | `core/transport/c2-ipc/src/control.rs:14`、`:37`、`:112` | 地址固定 `/tmp/c_two_ipc`，用 socket 文件是否存在推断 endpoint/shutdown 状态。 |
| 主 SHM owner | `core/foundation/c2-mem/src/segment/shm.rs:28`、`:78`、`:159` | 直接调用 `shm_open/ftruncate/fstat/mmap/munmap/shm_unlink`。 |
| 遗留 SHM reader | `core/transport/c2-ipc/src/shm.rs:9`、`:43`；`core/transport/c2-ipc/src/lib.rs:34` | 另有独立 POSIX 映射实现；当前 Rust 仓库内只有自身测试与 re-export，没有生产调用者。按 0.x 规则删除这套遗留 surface，映射统一由 `c2-mem` 管理。 |
| 死进程与内存检测 | `core/foundation/c2-mem/src/alloc/spinlock.rs:39`；`core/foundation/c2-mem/src/spill.rs:63` | 非 Unix 始终判定进程存活；未知 OS 可用内存返回 0，需要新 backing 的 `alloc_handle()` 慢路径会被导向 spill。 |
| 文件 spill | `core/foundation/c2-mem/src/spill.rs:96`、`:114` | 依赖 mmap 后立即 unlink，删除失败被忽略；Windows 需要明确 mapped file 的关闭与删除生命周期。 |
| 内存池对象身份 | `core/foundation/c2-mem/src/pool.rs:264`、`:1093`；`core/transport/c2-server/src/connection.rs:54` | idle 尾段被移除后可重新使用同一 idx/name，对端缓存按 idx 复用；Windows named mapping 没有 POSIX unlink 同等语义。 |
| pool 配置与 native 投影 | `core/foundation/c2-config/src/pool.rs:39`；`core/transport/c2-server/src/server.rs:312`、`:333`；`sdk/python/native/src/mem_ffi.rs:52` | `/tmp` 默认值散落在多处，需统一由 Rust config/platform 导出目录。 |
| 依赖闭包 | `core/runtime/c2-core/Cargo.toml:13`；`core/foundation/c2-codegen/Cargo.toml:11`；`sdk/rust/Cargo.toml:17`；`pyproject.toml:46` | Core 无条件依赖 IPC/mem；codegen、Rust SDK、Python 依赖相邻 FastDB。选择 HTTP 调用不会绕过整个 native 构建。 |
| Node native 包 | `core/foundation/c2-mem-ffi/bindings/typescript/package.json:6`；`scripts/build-node-addon.mjs:17`；`native/node_c2_mem_ffi_loader.c:3`；`src/index.ts:316`（后三项相对此包） | 平台只列 darwin/linux，Windows build 被跳过，loader 依赖 `dlfcn`，没有 DLL 路径。 |
| 生成的 Node transport | `core/foundation/c2-codegen/assets/typescript_transport.ts:947`、`:1949`、`:2097` | 硬编码 UDS/POSIX SHM 路径，需要 Named Pipe endpoint 和 native mapping provider。 |
| 现有 CI 与候选工具 | `.github/workflows/ci.yml:3`、`:57`、`:130`；`tools/local_rc/build_candidate.py:434`、`:927` | CI 仅 Ubuntu；候选 manifest 明列 Windows unsupported，native inventory 只识别 `.dylib/.so`。 |

仅将 `pool_enabled` 设为 false 也不能构成可靠旁路：`Server::new_with_identity()` 当前仍对 response pool 调用 `ensure_ready()`，见 `core/transport/c2-server/src/server.rs:341`。必须完成依赖与 allocation path 的审查，不能以运行参数替代平台实现。

## 3. 控制通道设计

`c2-local` 提供内部 `LocalEndpoint`、`LocalListener`、`LocalStream` 与 owned read/write halves。Unix 保留现有直接 I/O 行为；Windows 封装 NamedPipeClient/NamedPipeServer 的差异。优先使用编译期后端和有限 wrapper，让既有 frame codec、writer ordering、heartbeat 与 backpressure 接入相同接口，不在每个调用点加入 `cfg(windows)`。

现有 `c2-core::direct_ipc_socket_path`（`core/runtime/c2-core/src/control.rs:24`）与 relay 的 socket-path 地址校验（`core/transport/c2-http/src/relay/authority.rs:402`）也要收敛到 endpoint 概念；删除过时的 socket-only 投影，避免让 Windows 上层伪造一个文件路径。

Windows pipe 使用 duplex、byte mode、overlapped I/O。C-Two 继续拥有 framing，不引入第二层 message-mode 协议。首个实例使用 `first_pipe_instance`，每次 accept 后先补齐下一监听实例，再把已连接实例交给连接处理，处理 `ERROR_PIPE_BUSY` 时服从统一连接截止时间。Tokio 文档明确说明了这个监听顺序与重复 listener 检测方式。[ServerOptions](https://docs.rs/tokio/1.53.1/tokio/net/windows/named_pipe/struct.ServerOptions.html)、[NamedPipeServer](https://docs.rs/tokio/1.53.1/tokio/net/windows/named_pipe/struct.NamedPipeServer.html)

访问控制继承当前本机 IPC 的范围：显式拒绝远程 pipe client，使用当前 logon SID 的 DACL；mapping 采用匹配的会话范围。默认安全描述符不等价于当前 Unix socket 的 `0600`。Windows Service 跨会话模式若要支持，需要独立设计对象 namespace、访问主体和部署权限；不能默认改用 `Global\` 后要求用户以管理员运行。[Named Pipe security](https://learn.microsoft.com/en-us/windows/win32/ipc/named-pipe-security-and-access-rights)、[Named shared memory](https://learn.microsoft.com/en-us/windows/win32/memory/creating-named-shared-memory)

关闭语义需要单独实现并验收。当前 UDS owned-half drop、发送后睡眠、abort 接收任务的行为不能直接用作 Windows 的完成证明。使用已有协议 ACK、runtime drain barrier 和有界取消管理关闭；区分“停止接收新调用”“已接收调用排空”“写入完成”“对端已消费”“本地 handle 已释放”。本地 ping/shutdown 也通过同一 endpoint/transport，不能对 pipe 使用 `Path.exists()`。

Tokio Named Pipe 的 `flush()/shutdown()` 不能当作对端消费屏障；同步 `FlushFileBuffers` 又可能一直等待读端消费。后者不可用来阻塞 Tokio 工作线程修补关闭顺序。需要验证不读响应的客户端、半帧断开、写入取消、重复 shutdown 和在途 callback 场景。[Tokio 1.53.1 源码](https://docs.rs/tokio/1.53.1/src/tokio/net/windows/named_pipe.rs.html)、[FlushFileBuffers](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-flushfilebuffers)

## 4. 共享内存设计与主要难点

### 4.1 区域与访问权限

`c2-mem` 内部拆分 raw region 的 Unix/Windows 后端，上层 buddy、dedicated、lease tracker 继续共用。Windows 通过 `CreateFileMappingW(INVALID_HANDLE_VALUE, ...)`、`OpenFileMappingW`、`MapViewOfFile`、`UnmapViewOfFile`、`CloseHandle` 管理对象。创建必须区分新对象与 `ERROR_ALREADY_EXISTS`，后者不可被当作新区域并重新初始化 allocator。[CreateFileMapping](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-createfilemappinga)

映射接口应区分 read-only data reader 与需要修改 allocator/read_done metadata 的 participant，不能把所有 peer mapping 一律改成 read-only。应用层 FastDB checked read-only view 与底层 allocator metadata 的写权限是两个边界。

映射长度也必须显式处理：buddy backing 包含 header/bitmap/data，当前 handshake 中的 size 来自 `allocator.data_size()`，Unix open 通过 `fstat` 获得完整 backing 长度。Windows 不可简单将 handshake 的 data size 当作整个 view size；必须获得并验证完整可映射范围，再对格式、offset/length 做检查。实现可使用完整 view 和映射查询，或在传输元数据中明确 backing 长度，但必须让所有语言遵循同一规则。对应源码为 `core/foundation/c2-mem/src/buddy_segment.rs:23`、`core/foundation/c2-mem/src/segment/shm.rs:90`、`core/transport/c2-server/src/server.rs:1668`。[MapViewOfFile](https://learn.microsoft.com/en-us/windows/win32/api/memoryapi/nf-memoryapi-mapviewoffile)

### 4.2 段的身份与回收

这是适配前必须明确的共享生命周期约束。当前 `gc_buddy()` 会删除尾段，后续再用相同 prefix/index 创建；接收端缓存不带 generation。Windows 旧 mapping 仍被其他进程引用时，同名 create 会返回旧对象。仅增加每个 pool 的随机前缀，可以避免进程重启或 PID 重用的冲突，不能解决同一个 pool 内的 index 重用。

建议最终设计为：每个 pool 有 incarnation，每次段 backing 重建有独立 generation；wire 中的段引用和 cache key 对应具体 backing；段退休先停止新分配，在既有调用与租约释放后通知相关连接卸载 cache，再结束该 backing 的生命周期。新 generation 使用可独占创建的新 OS 对象名。generation 必须跟随引用，单独在新 backing header 中写 generation 无法帮助仍映射旧对象的 receiver。精确字段宽度、retire/ACK 时序和 OS 名称编码应在这一阶段的协议规格中固定，包含现有 u16/u32 边界和 macOS 名称长度约束。尤其 dedicated 内部索引递增为 u32，部分 handle/wire 投影转换为 u16，耗尽必须明确报错或切换 incarnation，禁止截断后命中旧对象。

这属于所有平台共享的内存身份问题。若测试证明保留现有 idle 物理回收需要变更 wire，应进行 0.x clean cut，同步更新 Rust/Python/Node codec、FFI 和测试，拒绝不匹配的旧 peer；公共 CRM release/payload schema 无须随平台改变。现在存在的 `CTRL_SEGMENT_ANNOUNCE` 只有 codec/FFI 表面，没有运行中的 announce/retire 状态机，不能宣称已有协议可直接使用，见 `core/protocol/c2-wire/src/ctrl.rs:39` 和 `core/transport/c2-server/src/server.rs:1768`。

retire 必须覆盖 peer 失联和 crash：connection owner 有界结束协议等待，本地仍存活的 held/mapping owner 保持原 lease。无法证明既有引用结束时隔离该 generation 并报告状态，不能仅因 ACK 超时或连接断开就复用、重置或提前 unmap backing。新 generation 可以使用独立对象名，不必等待旧名字重新可用。

保留同一 backing 到 pool/connection drain、避免重建同名段，是不改 wire 的可行设计条件，但会改变 idle 物理内存回收能力。它可以作为有明确上限的验证原型；不建议把 Windows 永久关闭 GC 作为最终结果。以上是机械替换 API 的已确认设计阻塞，本轮未复现数据损坏，也未将其定性为已验证运行缺陷。

### 4.3 内存预算、spill 与进程终止

Windows 内存策略需要同时考虑可用物理内存与 commit 余量，不能把 Windows pagefile-backed mapping 当作 Linux overcommit。先使用清晰的 allocation budget 和可靠 fallback，保持 `max_payload_size`、checked chunking 与 pool 上限。现有 256 MiB 段与 2 GiB 示例配置都需要在 Windows 测量，不能直接宣称同样合适。[Windows mapping commitment](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-createfilemappinga)

spill 应有专门的 file owner，使用 Windows 可验证的临时文件/删除策略，确保 unmap、close 与删除顺序，并在正常退出和强杀两类测试中检查文件残留。`std::env::temp_dir()` 与私有子目录的选择由 Rust config/platform 统一管理。强杀后仍可能残留的情形要有按 owner identity 判断的清理机制，不能直接删除某个公用目录内的所有文件。

死进程检测补充 Windows process handle 查询，权限不足时不得误判死亡；不能继续返回固定 true。PID 重用、owner identity 和半途失败要有测试。死持锁进程退出不自动证明 allocator 更新是完整的，应验证恢复后的结构，无法确认时让受影响的 pool/connection 明确失败，而不是无条件夺锁后继续使用。

### 4.4 payload 与生命周期承诺

继续保持 request/response 两个方向的内存 owner 边界，SHM request 到 native buffer 再到 Python memoryview 的路径不应改成 Python bytes 中转。客户端 held response 和服务端 borrowed input 都保持 FastDB checked owner 先失效、transport lease 后释放。

当前 portable receive adapter 是 copy-backed，Rust `sdk/rust/src/payload.rs:11`、`:16`、`:55` 均调用 `Payload::open_copy`。Windows SHM 适配完成也不能据此宣称 FastDB 在 C-Two 最终响应区直接构建，或消除了 payload decode 的所有复制。hold/borrowed 是生命周期契约，可以在 inline、SHM、handle、spill 路径保持一致；unsafe raw pointer/NumPy alias 不属于可机械失效的 checked-view 承诺。

## 5. SDK、FastDB 与交付范围

Python SDK 不需要第二套 registry、scheduler 或 Windows service runtime。注册、连接、关闭沿用现有 native authority。生产代码未发现必须依赖 `fork()` 的进程启动主路径，现有独立 subprocess 集成测试有较好的迁移基础；`serve()` 的 console event 与自动测试退出机制仍需单独处理。

FastDB 已有 Windows build 分支：`../fastdb/setup.py:76` 处理 x64 与 `.pyd/.dll`；`../fastdb/fastcarto/fastdb/CMakeLists.txt:222` 处理 MSVC/Python library；`../fastdb/bindings/rust/fastdb-sys/build.rs:89` 识别 Windows import library。它们只是源码基础，没有在本轮形成当前 commit 的 Windows 通过证据。FastDB 编译/运行阻塞在 W0 发现后就应回 FastDB owner 仓库修复，以便后续 Core/SDK 阶段执行；W4 负责制品与隔离安装闭合。C-Two 保持 opaque payload 与官方 binding 边界。

生成的 Node 客户端需要一起列入完整交付。Node 本身支持 Windows Named Pipe，因此 socket 部分可以沿用 Node IPC 接口；OS 名称规则由 native 层统一导出或使用可验证的规范实现。`@c-two/c2-mem-ffi` 需要 Windows N-API loader、DLL 构建/装载与正确的 native file inventory。现有生成模板中的 POSIX SHM file helper 要变为显式平台能力或迁移至 Core-backed provider，不能复制一套 TypeScript Windows allocator。[Node IPC](https://nodejs.org/api/net.html#ipc-support)

即使测试 HTTP，`sdk/python/tests/fixtures/typescript_real_call.mjs:27` 也会先初始化 native runtime；需要按选定 transport 初始化 provider。FastDB 的 WASM/npm artifact 可在独立构建 job 产出，再以明确 hash 交给 Windows job 安装和调用；Windows 本地源码构建脚本中的 `rm/cp/bash` 则另行修正。

完整产物包括可安装 Windows Python wheel、FastDB DLL/相关 binding、Rust consumer、`c3.exe` 和现有 Node native package。每种包都要在隔离目录安装后执行真实调用，记录实际 import/load 来源。编译树内 import 成功不等于 wheel 或 npm package 可分发。

## 6. 没有本地 Windows 时如何验证

### 6.1 开发反馈闭环

GitHub Actions 可以承担主要 Windows 开发验证。建议实施第一步建立 `.github/workflows/windows-native.yml`，在隔离的 `socu/windows-native-ipc` 分支上由 push 触发，先保留真实构建失败，后续每个实现提交都获取同一套 Windows 诊断。使用 `windows-2022` 的 MSVC x64 作为首个稳定 runner，随后增加 `windows-2025`；不要让 `windows-latest` 漂移决定支持边界。GitHub 当前提供这两个固定标签。[GitHub runner reference](https://docs.github.com/en/actions/reference/runners/github-hosted-runners)

workflow 必须并列 checkout `c-two/` 与 `fastdb/`，固定两个完整 SHA，输出 workflow SHA、被测源码 SHA 和所有依赖 artifact hash。Windows 使用 PowerShell 或 Python 调度脚本，环境变量通过 YAML `env` 设置；不要直接复制 `C2_RELAY_ANCHOR_ADDRESS= uv run ...` 这种 POSIX shell 语法。用 `C2_ENV_FILE: ""` 和空 relay anchor 隔离 runner 环境。

首次 baseline 按构建层记录结果：FastDB toolchain/dependency、Core、CLI、Rust SDK、PyO3；没有生成 native extension 的 job 应显示 build failure，不能把未执行的运行测试算成通过。采用 `if: always()` 留存日志/JUnit/JSON receipt，但最终退出码必须保留失败；不设置 blanket `continue-on-error` 或大面积 Windows skip。

当前 `ci.yml` 不响应功能分支 push，也没有 dispatch。GitHub 要求 workflow 先存在于默认分支才能通过 `workflow_dispatch` 手动触发；所以首次验证采用功能分支 push，之后再纳入常规 PR/merge queue 与手动运行。远程只读查询显示 fork Actions 已启用；当前 `origin` 的旧 URL 会重定向到 `Dsssyc/c-two`，后续 `gh` 操作需要显式指定目标 repository，避免命中默认 upstream。[GitHub 手动运行要求](https://docs.github.com/en/actions/how-tos/manage-workflow-runs/manually-run-a-workflow)

### 6.2 验收矩阵

| 验证层 | 执行环境 | 必须覆盖的结果 |
| --- | --- | --- |
| 原生构建与基础测试 | Windows Server 2022/2025，MSVC x64 | Core workspace、CLI、Rust SDK、PyO3 可构建并通过相关测试；Python 3.10 最低版本、3.12 常规版本与 3.14t 分别记录。 |
| 独立进程本机调用 | 同一 Windows VM 上分别启动 host/client | Python 同进程直调；Python/Rust 各进程 RPC；relay 未配置或失效时显式 direct IPC 成功。 |
| 传输和租约 | 同一 Windows VM | inline/buddy/dedicated/chunked/spill；边界大小、重复 release、checked view 失效、materialize 后存活、异常路径 lease 归零。 |
| Windows 原生故障行为 | 同一 Windows VM 的真实子进程 | 重复 listener、pipe busy、半帧/不读响应、host/client 强杀、启动失败重试、drain 与 hook 次数、句柄/文件残留、段 GC 后再增长。 |
| 已有跨语言矩阵 | 同一 Windows VM | 现有 18 行 Rust/Python direct/relay 全部实际执行，另保留 Python→Python 与同进程基础测试；Node 现有 12 行真实调用单独验证。 |
| 跨系统 relay | 同一受控网络中的 Windows 与 Linux 两台 VM | Windows client→Linux resource 与反向调用；契约不匹配、过期 route、超时与断连；两端实际 HTTP 路径和结果 digest。 |
| 安装包隔离 | 新 venv/新工作目录，或新 job | wheel/native DLL/c3.exe/npm 包安装后执行真实调用，不能引用 sibling checkout/build tree。 |
| 普通用户与桌面系统 | 明确普通用户 token；Windows 11 x64 VM | 同用户访问、其他用户拒绝、会话范围、无需提升权限、console shutdown；记录实际 OS 与 token。 |

普通 PR 首先固定 Windows 2022 + Python 3.12 的完整功能 gate，并覆盖 3.10 最低版本和既有 3.14t 目标；较重的第二 OS、Node package、长时间故障循环可以分 job 执行。表中宣称支持的能力必须在交付前有执行记录，不能仅因某行耗时较长就永久缺失。大型 payload 的日常测试可通过小 pool 和小 chunk 阈值强制走相同分支，另外在容量足够的 runner 验证真实大尺寸边界。

每次运行新增独立的 run-evidence envelope，记录 runner image/OS、架构、Python/Rust/MSVC/uv 版本、源码 SHA、包 hash、测试行 ID、实际调用路径、payload 类型/大小、通过/失败/skip、退出和清理结果。现有 Rust/Python v1 receipt 要求完整 18 行且 status 为 passed，Node v1 也要求完整通过，不能塞入 baseline 失败或随意添加字段。完整通过时由 envelope 按 hash 引用符合原 schema 的矩阵 receipt。已有 2026-07-24 receipt 保留为原平台历史证据，Windows 新建证据记录；只有实际通过的能力进入已验证平台集合。

### 6.3 Hosted runner 的实际边界

GitHub Windows runner 默认管理员运行且 UAC 关闭；普通测试全绿不能证明普通用户权限正确。应明确以非管理员进程执行对应验收。`windows-2022/2025` 证明的是相应 Windows Server 环境；Windows 11 x64 的桌面行为需要额外云 VM 或 self-hosted runner。[GitHub runner privileges](https://docs.github.com/en/actions/reference/runners/github-hosted-runners#administrative-privileges)

两个 Actions job 是独立 VM，不能通过各自 `127.0.0.1` 联通；artifact 传递也不是实时网络。跨 OS 完整验收建议用临时 Windows/Linux 双 VM 私网或明确建立的 overlay network，完成后回收。单 Windows runner 上运行本地 relay 只证明 Windows relay 通路。GitHub `services:` 容器要求 Linux runner，不能直接拿来在 Windows job 中启动 Linux relay。[GitHub service containers](https://docs.github.com/en/actions/tutorials/use-containerized-services/use-docker-service-containers)

macOS 交叉编译或 Windows target `cargo check` 可辅助查找编译条件错误，不能证明 Named Pipe、mapping、DLL loader 和关闭行为。WSL 运行 Linux 测试只证明 Linux 路径；Wine 也不作为 Windows 原生验收依据。

### 6.4 测试工具本身需要移植

`sdk/python/tests/fixtures/portable_matrix.py:410`、`sdk/python/tests/integration/test_portable_payload_cross_language.py:198` 和 `tools/local_rc/package_consumers.py:443` 使用 selector 读取 subprocess stdout。Windows 的 Python selectors 不支持 pipe，应改成 reader thread + queue 或明确支持 Windows pipe 的异步实现，并保留超时、stdout/stderr 和 EOF 诊断。[Python selectors](https://docs.python.org/3/library/selectors.html)

从 Cargo JSON `compiler-artifact.executable` 获取可执行文件路径，避免漏加 `.exe`。进程退出测试使用明确的控制协议或平台 console event；强杀只用于故障注入和最终兜底清理。通过 Windows Job Object 等机制拥有整个测试进程树，检查没有子进程残留，不能由 runner 销毁来掩盖清理失败。当前 `tools/local_rc/process_guard.py:23` 的 Windows 分支只处理顶层 process。

裸 AF_UNIX heartbeat 测试、固定 `.sock` 路径断言、POSIX unlink 断言应迁移成共同语义断言和明确的 backend 测试。平台不同可以有不同 OS 断言，但 route validation、lease、drain、重连等通用语义不能整体 skip。

## 7. 实施顺序与退出条件

以下阶段是实施顺序，最终结构以第 1 节为目标；每个阶段只在对应证据齐全后标记完成。

| 阶段 | 改动范围 | 退出条件 |
| --- | --- | --- |
| W0：建立基线 | 隔离分支、固定双仓库 SHA、Windows native workflow、日志与 run-evidence；发现 FastDB 阻塞即回 owner 修复 | 获取一次真实 Windows 运行，明确当前失败层；这一步红灯是诊断记录，不是适配通过。 |
| W1：确定共享底层契约 | `c2-local` 接口、endpoint、region access/size、segment identity/retire 规格；移植测试 harness | Unix 现有测试通过；Windows 真实双进程 pipe/mapping 探针锁定目标行为；段回收测试可以区分旧/新 backing。 |
| W2：实现 Windows 内存后端 | `c2-mem`、删除遗留 IPC 映射、process/memory/spill、必要的共享 wire clean cut | mapping create/open/size/权限/回收/重启通过；held/borrowed release 顺序无回归；不依赖全局关闭 GC。 |
| W3：接入完整 runtime | `c2-ipc`、`c2-server`、control、heartbeat、readiness、close/drain 与 relay upstream | 原生 Windows Python/Rust host/client 和 c3 relay 运行；direct IPC 独立性、18 行矩阵及故障测试通过。 |
| W4：闭合语言与制品 | FastDB 制品与 DLL、Node pipe/native DLL、构建脚本、wheel/CLI/npm 包 | Node 12 行、隔离安装消费者与 DLL 来源检查通过；平台能力清单准确。 |
| W5：产品验收 | Windows 2025、3.10/3.14t、普通用户、Windows 11、跨机器 relay、Unix 回归 | 对声明支持的每个平台/能力具备对应 receipt；文档、release workflow、artifact inventory 与实际支持一致。 |

涉及 frame/SHM 变更的阶段要同时验证 Unix，防止 Windows 实现造成 Linux/macOS 回归。Windows native 性能在正确性和生命周期通过后测量：小消息延迟、大 payload 吞吐、并发、commit 峰值、句柄数和 idle 回收，不预先给出与 UDS 等价的吞吐承诺。

## 8. 其他路线的取舍

| 路线 | 可满足的范围 | 代价与适用判断 |
| --- | --- | --- |
| Named Pipe + Windows mapping | 原生本机 client/host、大 payload SHM、相同 runtime 语义 | 推荐目标；主要工作是对象生命周期、关闭顺序和完整测试。 |
| Loopback TCP + owned/chunked payload | 原生 Windows 本机 RPC，可以保留 hold/borrowed 语义 | 需端口发现和本地访问控制；大 payload 有额外复制；仍要清理 native 依赖闭包。适合用户明确接受该性能/能力档位时独立设计。 |
| Windows 仅 HTTP client | 消费远端 Linux/macOS 资源 | 范围较小，但当前 Core/Python native 仍无条件拉入 Unix IPC/mem，必须先做真实可编译的依赖边界。 |
| WSL 中运行 C-Two | 使用现有 Linux 程序 | 可供 Linux 环境使用，不能满足 Windows 原生 Python/Rust/Node 进程直接参与本机 IPC 的目标。 |

建议的第一个实施动作是 W0 的 Windows 原生基线与 W1 的两个最小双进程探针。没有本地 Windows 机器并不阻碍这个闭环；真正需要先确定的是共享段生命周期和各 SDK/安装包的支持边界。

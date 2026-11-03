# IPC 14 版本诊断修缮验收

状态：实现、针对性与完整跨平台回归、同环境延迟对照和独立审查均完成。基线 `79d544cc1efabaf34b3eac2af7075dc8a556cd43`，实现与测试候选 `968f7d29153ff23fc967f767d97506823554de1d`。对应[批准方案](2026-10-25-ipc-version-mismatch.md)；本批不 bump 包版本、不发版、不合并上游，验证只推送 fork 的专用分支。当前发布包仍为 C-Two 0.9.0 / c3 0.5.0。

## 实现边界与实际增量

普通 RPC 握手改用永久固定的 25 B 前导，先交换协议版本，再交换同版元数据。删除旧元数据中的单字节版本，不解析旧格式、尝试旧端点名、协商降级或自动重试。Rust/Core、公开 Python `CoreError` 和生成 TypeScript 同步保留本端与对端版本；畸形、EOF、权限及身份错误不会伪造对端版本。Core 与 TS 的 relay-local 版本不符终止选路，HTTP relay 自己的上游错误仍通过原 `ProtocolViolation` 携带那一对版本。

服务端复用现有握手调用栈及状态，在最终元数据 ACK 写完后才开放业务与 heartbeat；未握手 CALL/路由 CTRL、重复握手及中途 owner 角色切换均拒绝。版本拒绝共用一个 5 秒绝对预算，覆盖 writer 锁、现有异步 write/flush 和 peer-close 等待；shutdown 可取消。没有新增会话框架、后台观察任务或配置项。独立 SIGNAL ping/shutdown 与 owner-control 协议保持。

Windows 继续使用 0.9.0 的 TokenUser SID 与逻辑地址派生名字，协议号不进入名称。0.8.2 与 0.9.0 旧名字不同的原因是登录 SID 改为用户 SID；本批没有再迁移 namespace。操作系统身份核对仍先于任何前导发送。ABI 6、SHM 3、owner control 1、业务帧与 CRM hash 均保持，RPC 握手明确切换为 IPC 14。

| 范围 | 新增 | 删除 | 净变化 |
| --- | ---: | ---: | ---: |
| 生产代码，含注释 | 510 | 103 | +407 |
| 测试与测试夹具 | 1375 | 119 | +1256 |
| 生产与测试合计 | 1885 | 222 | +1663 |
| 文档与验收收据 | 869 | 22 | +847 |

统计按基线至收尾的物理行，Rust 内联测试按 `cfg(test)` 分开计数；不把测试记为生产代码。实现超出方案的生产 +150–250、测试 +200–350 静态估算，应明确纠正估算。生产净增主要为 wire 75、IPC client 83、server 129、TS 72、错误投影和终态分类 48 行。测试净增主要为 TS 合同 347、server 330、IPC 138、跨平台原生假对端及 Python/Node 投影 246、其余边界与夹具迁移 195 行。覆盖包含固定字节、两阶段准入、未知输入、Windows 有界交付、终态与实际 fallback 入口，不能把本项称为瘦身或性能提升。

## 基线、平台与失败处理

生产保持基线时先运行反例：旧 metadata golden 不符合新布局，旧客户端没有结构化版本错误，旧服务不能返回固定回绝，并会接纳未握手 inline CALL。候选逐项拒绝；公开 Python 使用真实原生异常类，Node 包括官方连接器，不用 mock 代替真实 Unix socket / Named Pipe。固定 golden 手写，与生产 encoder 独立。

定向回归覆盖 u32 高低版本、零与矛盾版本、错误长度/flags/RID/magic、截断、最小合法 frame 配置、metadata 完整校验、ACK 后准入、角色切换、两个阶段共享原 connect 期限、发送后不重试、relay-local 终态及 HTTP 上游错误。合法 `max_frame_size=17` 仍能收到固定 21 B body 的版本回绝；普通超限 SIGNAL 只读 header 即拒绝，元数据仍受原上限约束。

Windows 11 x64 开发机（系统 build 26200、Rust 1.99.1、Python 3.12.13、Node 24.22.1）执行真实双向版本拒绝、延迟 150 ms 读取、同一 5 秒关闭预算及 shutdown 竞争、pool 58、connect deadline 7、公开 Python 6、错误单元 139、Core/relay 投影及官方 Node 连接。最后的 frame 边界改动另跑受影响两项并核对源哈希；完整最终来源由下述 Actions 负责，没有将较早 wheel 冒充最后候选构建。

macOS 与 Windows 均用已发布 0.9.0 wheel 和新构建交换服务端/客户端角色。两方向均保持普通连接失败、没有结构化版本信息、业务调用数 0、端点/lease 残留 0；macOS 约 38/47 ms，Windows 约 46/31 ms。这是旧版没有固定前导的已验证限制，不包装成类型化 mismatch；Linux 旧 wheel 混用本轮未单独实测。未来采用本前导的不同版本则由手写异版本对端在三系统真实验证。

本轮失败及修正保留完整记录，没有靠重跑同一失败候选或放宽期限/断言筛选通过：

- Windows 假对端在客户端关闭后执行冗余 flush，结构化错误已经正确返回，但夹具报 232。定位到夹具写入/收尾，改为明确的原始异步写入并持有流直到客户端 EOF；生产复用 `LocalWriteHalf.write_all` 已包含的异步 flush，删除重复调用。
- 第一轮完整 CI 暴露 Python 严格错误字段/类别 golden 仍是旧表，六个平台均失败。更新独立手写表，保留精确相等断言。原生穷举匹配和旧单阶段 raw peer 也同步迁移。
- 受控最小 frame 配置暴露固定前导被一般帧上限挡住；先证明合法配置下旧候选 EOF，再只给通过固定长度/flags 校验的前导放行。
- macOS 的退出回收夹具使用共享默认 namespace，单次检查可能与其他测试持 gate 冲突。修缮前基线受控持 gate 时确实返回 WouldBlock，释放 gate 后为 Present 并可精确 reap；只改两行使用既有私有 TestNamespace，保留原断言。原 CI 没有保存那个返回分支，不能声称已逐字重建其现场。
- Linux idle pool 夹具在 READY 后立即采集全部 Unix FD。修缮前基线受控保留真实 Ping accepted FD，得到 baseline 5、期待 6、实际 5：探针退出被新业务连接抵消。修正只在原 8 秒预算内等待该端点只剩 listener，再采全进程基准；后续全量计数、60 秒 grace、两轮 62 秒及最终归还全部保留。同一现有测试用真实 socket 证明 peer EOF 后 accepted FD 仍计入，只有本地 close 才减少，末尾增加本批 socket/lease 为零检查；不新增 Windows skip。基线与候选检查通过，最终组织形式定向 1 passed，125.97 秒。原 CI 未保留逐 FD 详情，此实验不冒充现场归属证据。

各次取消/失败的候选、原因和最终门禁收据见[机器记录](evidence/2026-10-26-ipc14-acceptance.json)。本机 C++ 标准头路径及 Windows 临时 CMake 安装问题使用任务环境修正，未改变系统默认工具链。

## 同环境延迟

macOS arm64、Python 3.12.10、Rust 1.91.0、相同编译参数，基线/候选按交替顺序测量，全部样本保留。最新 Python 候选 wheel SHA-256 为 `cb9ac3f348f97ce06e4d440a5250e896e26eebf6a68a2406b56d3f70ed7cc0c4`；之后只调整测试隔离与观测，没有再改生产代码。

| 场景 | 每版样本 | 基线 P50 → 候选 P50，ms | 基线 P95 → 候选 P95，ms |
| --- | ---: | ---: | ---: |
| Python 新物理连接 | 21 | 1.191 → 1.249 | 1.329 → 1.467 |
| Python 复用连接调用 | 4200 | 0.0794 → 0.0761 | 0.1536 → 0.1780 |
| Node 新物理连接 | 155 | 0.444 → 0.549 | 1.297 → 1.092 |
| Node 复用连接调用 | 2500 | 0.1132 → 0.1155 | 0.2270 → 0.2107 |
| Node relay-aware 本地路径 | 500 | 0.5766 → 0.6206 | 0.8970 → 1.0090 |

成功首建多一次往返和净 48 B；复用后的业务调用不增加往返。Node relay-aware 本地路径仍每次新建/关闭连接，因此每次调用承担该开销，测试确认其实际路径和建连次数。本项没有改变复用策略，也没有把样本波动解释为性能收益。31 次新版本结构化拒绝从 `cc.connect` 入口量至异常返回（含清理），P50 0.744 ms、P95 0.914 ms；不等待服务端 5 秒 peer-close 预算。

## 完整门禁、独立审查与清理

最终 [Source Validation 40226200305](https://github.com/Dsssyc/c-two/actions/runs/40226200305) 在 attempt 1 完成 10/10 jobs；六份来源收据均绑定 `968f7d29153ff23fc967f767d97506823554de1d` 与 FastDB `abba785ef2a7d3fbee1bbc608393a3e6c7b042c4`，147 个适用 source 步骤全部 passed/exit 0。五个 full 通道实际执行启用 `c2-http/relay` 的完整 Core、Rust SDK、原生 Python、wheel、IPC memory matrix 与 Node；3.14t 执行其原有 Python 适用集。

| 环境 | 完整 relay 单元 | Python ordinary，通过 / 平台跳过 | Rust/Python / TypeScript |
| --- | ---: | ---: | ---: |
| macOS arm64，3.12 | 417 | 1257 / 10 | 25 / 15 |
| Linux x64，3.14t | 由 full 通道覆盖 | 1257 / 10 | 25 / 15 |
| Linux x64，3.12 | 417 | 1257 / 10 | 25 / 15 |
| Windows 11 ARM64，3.12 | 412 | 1220 / 47 | 25 / 15 |
| Windows 2022 x64，3.12 | 412 | 1220 / 47 | 25 / 15 |
| Windows 2025 x64，3.12 | 412 | 1220 / 47 | 25 / 15 |

六个平台均通过严格 18/18 Rust/Python direct/relay 与 12/12 TypeScript 矩阵收据校验，跨语言组无 skip；Windows 包括原生架构与普通用户消费者。Node 进程、host/relay 进程和 reader 收尾字段全部为 true，新增真实拒绝夹具检查私有 socket/lease 清空。最终源码字节与本机冻结候选一致，后续收尾文档不是新的构建来源。

hey-my-buddy 独立审查覆盖初始实现、旧 fixture 迁移、Windows flush、frame 边界、退出夹具及 idle baseline。审查发现逐项处理，最后 idle 差量复核 `de61efd2-7191-4dc9-a971-9e895e265ee3` 无阻塞。静态审查与实际运行证据分开，所有交付产物由 Host 按固定提交核验后接受；源记录绑定的具体 run/artifact 见机器收据。

本机已清理 6,868,225,006 B、Windows 已清理 2,892,516,412 B 的任务产物，合计约 9.76 GB（目录逻辑字节统计，不声称等量物理空间）。Cargo target、验证环境、wheel、Node build/dist/node_modules、传输归档和本批 FastDB symlink 已移除；源码、原始日志、样本、哈希与收据保留于 `~/.codex/diagnostics/c2-ipc14-implementation-mxyfpx5g/`。Windows Rust 工具链保留。

没有清理历史对象。一次本机 `connect_timeout` 文件误用了默认上下文，7 项通过并由原生机制清理本批唯一端点，记录为执行范围偏差；其余新夹具明确使用私有 namespace，完整 relay 在干净 CI runner 验证，未再次用全局 `C2_IPC_ROOT` 假定整套 Rust 测试已隔离。

瘦身清单保留 17 完成 / 13 部分完成 / 8 待复核 / 1 明确保留的 R 条目统计。本修缮单列，不计作净减收益。C04 的只读复核未发现有明确整体收益的替换，记录暂缓；下一优先项转回 R07 的独立正确性分析，先方案、后确认，R35 与两份控制会话重构继续暂缓。

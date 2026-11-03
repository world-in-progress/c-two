# 所有者接管：实现验收与精简复审

C 已完成实现、独立审查及精确候选的跨平台验收。实现源为 `ea511c7a00be024134bade2cc3e5a137f06a96fe`，FastDB 为 `abba785ef2a7d3fbee1bbc608393a3e6c7b042c4`。[Source Validation 39618680376](https://github.com/Dsssyc/c-two/actions/runs/39618680376) 的 9 个 jobs、118 个适用门禁全部成功。仅推送 fork 的 `socu/owner-takeover-ci` 验证分支；本地 HEAD 仍为 `0acf784f6742487cd619a7d419379f693434a337`，本地分支和标签未移动，没有发版。

本批统计基线为 `1a8b4bf620a3a7d6e0090a7df2bf2523846a6739`，已含此前 A/B 与 FastDB 更新。不能用本地 HEAD 计算 C 的增量，否则会重复计入这些工作。[机器记录](evidence/2026-10-21-owner-takeover.json)保存源、门禁、审查、性能和清理证据。

## 已落实的边界

`owner_control_pair()` 返回 `OwnerControl` 和 Rust `OwnerBootstrap` / Python `NativeOwnerBootstrap`；旧单向类型和别名已删除。当前 owner 可以发行、幂等撤销恢复票据；准备对象显式保留原票据和可预存的 successor，commit 只能提交一次。完整拒绝、确定未发送和发送后失去确认保持不同结果；失去确认关闭专用会话，不自动重发或尝试另一把钥匙。

Core 在同一提交边界验证原 deadline、当前 grant/epoch、server instance 与原生目录身份，消费旧 grant、安装 successor 并恢复准入。旧命令、旧票据、旧 EOF 和旧 timer 失权。普通 IPC admin shutdown 对 OwnerBound 返回原负 ACK；有效 owner 的 shutdown ACK 只证明 initiate，可信进程内关闭仍沿用既有 drain。

OwnerMissing 保留路由 Ready、UID/revision 与 relay 登记，新调用返回既有 ResourceUnavailable。恢复只打开主机准入，不重开注销/关闭的路由，不复活旧 pending 许可或迟到登记 token。身份、合同与方法错误顺序、active/held/borrowed 释放规则继续有效。

owner 控制采用独立 version 1、最多 4 KiB 的消息，复用原帧封装；IPC 12、SHM 3、C/FFI ABI 5 和业务编码不变。Unix 使用私有 socketpair，Windows 使用当前登录 SID 的本地双向 Named Pipe；bootstrap 在子进程中才注册 reactor/IOCP，没有 owner 轮询或第二监听服务。接口与接入示例见[生命周期指南](../reference/local-endpoint-lifecycle.md)。

## 反例、修正与审查

实施前在上述基线补测，三个新边界如预期失败：OwnerBound 接受握手前普通 shutdown、接受握手后普通 shutdown、owner 丢失后路由变 Closed 并返回 ResourceClosed。每例在安全断言前完成原生关闭，socket/lease 均为 0；测试补丁快照为 `12cd3f5ca5a09b01f77e9b6e83c530ac378d0b74`。

独立审查发现并推动修正了：取得准入锁后再次判断 grace；shutdown 回复事务避免被自身 drain 取消；普通角色后续 owner 帧也在分配前拒绝；Python guard 最终准入错误正确投影；raw owner 关闭与串行槽释放在同一临界区交接。Persistent listener 不额外持有 owner 专用目录 pins。真实 relay 用例按连接 ID 和明确 DISCONNECT 识别 data idle 释放，随后验证 relay 重启、FULL_SYNC、同身份恢复与最终撤回。

跨平台失败记录保留，没有跳过新增 owner 测试或放宽期限：

- Windows 原生探针确认零配额 pipe 的写完成会等待对端读取。私有 owner pair 改为每方向请求 4 KiB 缓冲；没有新增配置，普通业务 pipe 不变。
- Windows 对端发送完整 ACK 后关闭时，额外零字节 flush 可以报 232，而完整 ACK 仍可读取。owner exchange 删除多余 flush，以完整、相关联的应答确认；写失败、错应答和失去确认仍使会话失效。
- 两个原始流测试改用既有 Tokio 多线程 runtime 的一个 worker，确保两次 block_on 之间 IOCP 继续执行。终态夹具保留真实写完成与对端消费断言；关闭结果仅接受 EOF 或 Windows 233。
- “激活前已关闭”的 Windows 用例改用未注册的同步 peer，实际关闭原生句柄后才激活被测端，保留两方向和已缓冲字节。没有用 sleep、轮询或扩大超时制造前置条件。
- heartbeat 测试分别验证未分类连接的原超时与发送普通首帧后的 Ping/Pong。Python 端点测试精确验证 status/reason，再断言关闭后 ping=false，不冻结无关诊断字段的字典形状。

先前失败的完整运行 `39597387037`、`39612054566`，以及被修正候选取代的运行均保留。定向 Windows 运行 `39616827006` 两组各 10 项通过，只作补充；最终接受依据是 `39618680376`。

新版 hey-my-buddy 的实现审查与复审均已验收。最终原生夹具审查 `f8bd7c63-356f-4b57-bf29-8d83184e0f34` 无阻塞；最终源的关闭交接和 relay 识别复审 `ea486d0f-4cf2-46a6-b3a7-b30cdabc6522` 确认此前两项 P2 已修复。审查未代替运行证据。

## 验证结果

| 平台 | 门禁 | 普通 Python 用例 | Rust/Python 与 TypeScript |
| --- | ---: | --- | --- |
| Linux Python 3.12 | 25/25 | 1241 通过，5 个适用性跳过 | 25 与 14 全部通过 |
| macOS Python 3.12 | 25/25 | 1241 通过，5 个适用性跳过 | 25 与 14 全部通过 |
| Windows 2022 | 26/26 | 1204 通过，42 个平台跳过 | 25 与 14 全部通过 |
| Windows 2025 | 26/26 | 1204 通过，42 个平台跳过 | 25 与 14 全部通过 |
| Linux Python 3.14t | 16/16 | 1241 通过，5 个适用性跳过 | 25 与 14 全部通过 |

每组跨语言测试均含完整 18 行 Rust/Python direct/relay 矩阵和 12 行生成 TypeScript 真调用矩阵，无跳过。四组 full 还通过完整 Core/relay、CLI、9 行内存矩阵与安装包消费。两组 Windows 原生 local 各 45 项、owner 生命周期各 35 项、HTTP/relay 各 411 项通过；普通用户安装包收据均为 administrator=false。全部 Python 执行步骤确认进程与临时目录收尾，安装包主机正常关闭。

本机隔离验证包括基线反例、38 个真实 owner 生命周期用例、控制器期限/取消、完整 relay、实际 Rust/Python 消费和同环境性能对照。Windows 完整产品验证来自 Actions；开发机 SSH 的 Win32 探针仅证明缓冲与 ACK 的原生行为，不冒充本机产品完整测试。

本次自有 socket、协调记录和临时目录最后审计均为 socket 0、lease 0，正常 gate/marker 保留。Windows 通过原生关闭/再绑定、关闭后 ping=false 及子进程退出验证收尾，不能单凭 kernel-managed 字样认定清理成功。失败初始化留下的本次已知端点曾在确认进程退出后由原生 credential/reap 精确回收，没有手工删端点。

审计同时记录了一次执行偏差：已取消 worker 曾在未改名源码导出中运行原生测试；这些输出不计作隔离验收。其未验收草稿未直接合入，弱化断言和轮询方案被拒绝。Host 未对历史对象执行清理或迁移，后续验收采用任务隔离命名空间和 CI。

## 增量与开销

下表记录 C 完整候选及首次验收记录快照 `fe406326` 相对 C 基线的增量；后续局部精简单列于末节。Rust 内联 cfg(test) 和整个测试文件均计测试，示例计生产，锁文件单列。

| 类别 | 新增 | 删除 | 净增 |
| --- | ---: | ---: | ---: |
| 生产与示例 | 3799 | 1032 | 2767 |
| 测试 | 4997 | 832 | 4165 |
| 构建配置与锁文件 | 21 | 3 | 18 |
| 文档与证据 | 557 | 20 | 537 |

性能对照使用同一 macOS arm64 环境，基线 `1a8b4bf6` 与实施阶段候选 `9fcde496` 交替运行 3 轮；Rust release、Python dev native。之后修改集中在 owner 控制关闭/应答、Windows 私有缓冲及测试，业务调用路径未变。这不是最终源在全部系统上的性能测量。

| 每次调用中位数 | 基线 | 候选 | 差值 |
| --- | ---: | ---: | ---: |
| Python 本地 Persistent | 7.049 µs | 8.162 µs | +1.113 µs / +15.8% |
| Python 本地 OwnerBound | 7.138 µs | 7.908 µs | +0.769 µs / +10.8% |
| Rust IPC OwnerBound，64 B | 30.893 µs | 37.452 µs | +6.559 µs / +21.2% |
| Rust IPC OwnerBound，64 KiB | 97.840 µs | 89.110 µs | −8.730 µs |
| Rust IPC OwnerBound，4 MiB | 1136.618 µs | 1122.292 µs | −14.326 µs |

IPC 样本波动明显，不能据此宣称吞吐优化或零开销。实现阶段 Python 控制调用的中位数为取票 0.481 ms、撤销 0.472 ms、prepare 0.788 ms、commit 3.248 ms；取票/撤销各 100 次，接管 20 次。Windows 私有 pair 每对请求 8 KiB 内核缓冲，另有原生对象开销；这不是整个进程 RSS。

## 收尾精简复审

按用户追加要求完成两路独立只读瘦身审查后，用户批准并实施以下两项。精简基线为 `fe406326bf36ade401b7dd8e764b36ccd8631589`，实现快照为 `b346e2b3303f0a0fa90f90bf68481897fb55e0be`。

| 项目 | 删除依据与替换 | 实际净减 | 回归范围 |
| --- | --- | ---: | --- |
| Unix owner 临时 active 状态 | activate 内无 await；所有返回点 active 均空。改用局部 watcher，删除持久字段和不可达的 take_stdio 分支；保留注册失败/探测失败 shutdown、cleanup alias 与错误顺序 | 生产 14 行 | 原 Unix owner、继承别名、fork 与关闭前置条件测试 |
| 11 处 owner 测试初始化 | 用一个具体初始化函数复用 pair→attach→host；调用点仍显式持有 root/runtime，保留各自关闭、drop 顺序和全部断言 | 测试 79 行 | 原 owner 生命周期测试；核对零残留与失败路径 |

两项合计净减 93 行：生产 `+21/-35`，测试 `+20/-99`。11 处初始化之外的测试内容逐字不变，包括全部断言、期限、catch_unwind、测试名和关闭顺序；Windows 生产文件 SHA256 不变。API、ABI 和 wire 无变化。约 10 行重复 timeout 转换继续保留，不混入本次修改。

不采纳为节约二十多行把 OwnerStatus 改成 wire 类型别名：会改变公开 Rust 名义类型及 Debug 表示。也不为微量去重新增跨 crate 通用帧读取接口，或用约 70 行的新测试管理器接管关闭顺序。SessionUse 关闭交接、跨 await 的最终准入、Windows 已注册 watcher、目录原生身份复核都有实际反例支撑，不能直接删。

定向回归在 macOS arm64 隔离命名空间和 Linux aarch64 独立容器各通过 10 项原生 owner 测试、38 项 owner 生命周期测试，无失败或跳过；两端 socket/lease 均为 0。新版 buddy 独立审查 `b91cb68f-ca33-475f-b7f2-daa5c0abbca9` 对该固定快照无阻塞发现，已验收。此前完整 relay、跨语言和 Windows CI 仍归属于 `ea511c7a`；本次未重复不受影响的全量门禁。

2026-10-22 按追加要求补测 Windows 开发机。开发机没有 Rust 工具链，因此由[定向 CI 39630296353](https://github.com/Dsssyc/c-two/actions/runs/39630296353)构建实现 `b346e2b3` 对应的测试程序；CI 源 `8d5266aa` 仅额外包含验证工作流和文档。9 个受影响用例在 CI 通过后，同一 SHA256 的程序经 SSH 在 `Dsssyc-x86-PC`（Windows 11 Pro x64，build 26200）实际执行，9/9 通过、退出码全部为 0。全部测试进程已退出，任务临时目录及 40,242,410 文件字节已清理。首次 PowerShell 驱动未取得退出码，单项测试日志已通过；改用 System.Diagnostics.Process 完整采集后确认上述结果，未修改生产代码、测试断言或系统应用控制策略。

本次本机 target 已清理 426,199,152 文件字节，Linux 容器及其编译目录已自动移除；保留源码、日志和收据。正常 gate/marker 保留，历史目录未作为测试或清理对象。本地分支、HEAD、标签未变，未发版。

## 非 Hello 拒绝期限修复

2026-10-22 按单独批准的最小范围，将 `OwnerSupervisor::candidate` 的非 Hello → Malformed 回复移入现有 setup future，仍在 expire/status 前执行，复用同一绝对 deadline 和 biased shutdown select；取消后由原 OwnerSession Drop 释放。没有增加定时器、状态或抽象，API、ABI、协议与身份、准入、错误及释放语义不变。两份会话重构继续暂缓。

本批基线为 `00270c6a0d6138ccc02fd2ef94a63279bcbabf1c`，实现候选为 `3312a3d6a91a29caf783909dc67da4ef0e26fa14`。生产 `+9/-9`，净增 **0 行**；测试 `+81/-4`，净增 **77 行**。文档与证据 `+65/-0`，净增 65 行。新增原生流背压测试，在未修复生产基线上确实以 `rejection escaped the setup deadline` 失败，再在候选通过；已有协议集成测试增加首操作 GetRecoveryTicket 被 Malformed 拒绝后，原票据仍可接管的样本。背压是显式注入的前置条件，不把它描述为普通未读首回复已导致生产阻塞。

| 环境 | owner 单测 | 生命周期 | relay 接入 | 完整 c2-http relay |
| --- | ---: | ---: | ---: | ---: |
| macOS arm64 | 22 | 38 | 7 | 416 |
| Linux arm64 容器 | 22 | 38 | 7 | 416 |
| Windows 11 x64 开发机 | 22 | 35 | 3 | 411 |
| Windows 2022 CI | 22 | 35 | 3 | 411 |
| Windows 2025 CI | 22 | 35 | 3 | 411 |

所有门禁退出码为 0。relay 接入的 Unix 2 个、Windows 1 个 ignored 条目是由父测试显式执行的子进程夹具；其余计数差异来自既有 Unix 平台用例，本批没有新增跳过或放宽断言。macOS 在隔离副本替换两处默认命名空间字面量，两个改动源码文件与候选逐字相同；Linux 与 Windows 使用未替换的候选。[Windows CI](https://github.com/Dsssyc/c-two/actions/runs/39712853886) 的工作流快照为 `c8e485eb`，明确检出上述实现 SHA。首次 CI 工作流因 job env 不支持 runner 表达式而在编译前拒绝，修正步骤位置后运行；该临时工作流不进入本地交付改动。

Windows 开发机首次构建被 Smart App Control 拦截，错误 4551，事件 3033/3077 指向 libc 构建程序；未执行到 C-Two 测试。用户关闭该设置后，核实状态为 Off，以相同源码归档重新构建并通过上述四组测试。没有改测试、编译依赖或生产实现来绕过拦截。

macOS/Linux 本次 socket/lease 均为 0；Windows 既有原生关闭/重绑定断言通过，全部测试进程退出，任务临时文件为 0。已清理 macOS 构建及本次命名空间 530,482,948 字节、本地源码归档 6,422,233 字节、Windows 两次构建目录 941,381,892 字节；Linux 容器及 CI target 已清理，Windows 工具链保留。历史目录未作为检查或清理对象。

新版 hey-my-buddy 独立审查 `55d59240-fdd4-4f4b-97fc-db5c5e089334` 对固定候选无阻塞发现，Host 核对空补丁后验收并回收只读工作树。原始日志哈希、逐平台结果与清理数据见[机器记录](evidence/2026-10-22-owner-setup-deadline.json)。本地 HEAD、分支和标签保持原值，未发版。

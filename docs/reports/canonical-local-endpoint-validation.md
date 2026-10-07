# 唯一原生本地端点最终验收

2026-10-07。按用户明确的“新版未发布、没有依赖方”条件，C-Two 已直接采用唯一原生端点，并删除旧 Unix 兼容实现和公开选择开关。Unix 自动使用私有 v2.2 托管目录、nonce gate/marker、原 gate inode pin 及可退休每端点 lease；Windows 使用真实当前登录 SID Named Pipe 与非继承监听内核租约。默认 Persistent 和显式 OwnerBound 分别承担常驻服务与控制器专用子进程用途。

实际 hosted 构建源码为 C-Two `e49652e85a384f1dd3489f393c13f4d9fff27f9f`、FastDB `4f99f86a662b0e950a0dd29800c25a1c9fca4def`。后续文档/注释提交不是这些产物来源。所有 ZIP、内部哈希、c3 和 wheel 字节及源码/测试账目保留于[结构化摘要](evidence/2026-10-07-canonical-endpoint-validation.json)。本次候选没有创建 PR、合并或正式发布。

## 代码与接口

生产者 `129cedad27bb9ba71d030aeea4c17a18c268c72d` 相对 `9e34991d24bc25bc46416e0d12edacc19de780a1` 修改 44 个源码/测试路径，新增 1,658 行、删除 4,771 行。移除了 LegacyV1、旧 Unix 地址/永久每地址锁/记录/绑定与维护分支，以及 endpoint_protocol 配置、环境读取、with_protocol 构造/控制探针和 SDK/CLI/FFI 选择参数。共享可信目录、身份校验与 descriptor-relative 工具保留于 unix_common，实际生命周期算法继续由 Rust 所有。

Rust 唯一入口为 LocalEndpoint::from_address。Python 为 inspect_endpoint(address)、reap_endpoint(address, credential) 和 sweep_endpoints(*, addresses=..., max_entries=..., max_ms=...)；relay、CLI 和 SDK 使用同一 OS 派生。凭据 managed-v2 / named-pipe 只为格式元数据，不能选择后端。Unix incarnation 和 identity 为必填内部状态；JSON 缺失值、未知字段、版本与跨平台拒绝仍保留。

C endpoint 参数改变后 ABI discriminator 同步升至 3。Rust/C header、Node addon、TypeScript 和独立安装消费者一致。真实 ABI-2 库包含会 abort 的端点/pool 符号，ABI-3 addon 在调用前拒绝它。两个独立审查逐函数确认全部 15 个 pool callback/helper 与原输入相同，初次误改导致的 Node SIGSEGV 已修复，最终 Node 37 项与真实包消费者通过。正式包版本未改。

## 实际门禁与证据边界

| 环境/源码 | 实际执行 | 结果 |
| --- | --- | --- |
| Linux / e49652e | [run37559074669](https://github.com/Dsssyc/c-two/actions/runs/37559074669) | 3/3 作业；Core1,337、聚焦502、CLI65、RustSDK12；Python1,023 |
| Windows Server2022 / e49652e | [run37559074657](https://github.com/Dsssyc/c-two/actions/runs/37559074657) | full23/23；local-platform776；Core1,292、CLI56；Python1,023及harness34 |
| Windows Server2025 / e49652e | 同上，独立作业/产物 | full23/23；local-platform776；Core1,292、CLI56；Python1,023及harness34 |
| macOS / 98d4e3d470857709d6c4a2e517eadb09d5115b1a | `/tmp/c2-endpoint-single-mechanism-1007/mac-component-verified.json` | 真正 native rebuild37.03s、native2、CLI65、RustSDK12、repo392；Python1,023；Core只有一项测试失败，随后修复 |

各完整 SDK 集合为 984 普通、25 Rust/Python portable、14 TypeScript，共 1,023；JUnit 没有失败、错误或跳过，普通部分保持 -n2 --dist loadfile / 30秒超时。collection、plan与实际分组对应，严格18行Rust/Python及12行TypeScript development收据通过，Python3.10最低版本语法检查实际执行。Rust为正常并行harness，数字来自成功log摘要，failed/ignored为0；不是另行逐ID采集的完整Rust清单。

Mac 原 fullCore 有一个明确失败：public scoped sweep 测试最多读64个目录名称，当前共享目录72个，尚未读EOF就断言完成。scope约束删除，不约束枚举的预算计数。e496仅修改该cfg(test)文件：每批仍<=1，非完成批次必须推进1，有限10秒期限，检查EOF、零I/O错误、无中断/namespace变化及全部死/活对象断言。修复后local65+owner12通过，MSVC纯测试编译通过。Mac runtime生产源码与e496相同，但不能把98d组件结果说成e496整套Mac执行通过。旧失败记录保留；旧98d Windows运行由最终push自动取消，未伪装为产品失败。

## 测试迁移、生命周期与压力

生产者逐项记录54个移除/改名测试和18个新增/改名条目。仅删除旧选择器、旧协议round-trip或不可用后端选择的断言；活跃满backlog、重复绑定唯一赢家、旧流重启、完整登记强杀孤儿、旧凭据、未知/损坏/符号链接/替换/权限、scoped范围与预算/EOF负例有具体替代。Host独立审查补回了未知socket reap的MissingOwnership、身份不变和不制造lease断言。当前不会沿用此前候选“原函数ID零删除”的说法，实际映射在摘要中。

Persistent普通断连仍服务，OwnerBound能力在就绪前准备；EOF停止准入、有限宽限后排空，通道不可重连/接管。shutdown deadline不取消原事务，未完成保留Session、bridge和hook；监听关闭、工作排空与payload lease分别报告。relay默认上游派生、完整server/instance/contract核验及精确route UID/revision注销测试保留；内存tier、held/borrowed与FastDB检查视图仍由原模块释放。

最终e496默认路径真实压力：10,000新地址，9,500正常、500完整登记后kill+OSwait、100旧凭据阻断、100旧流仍存活时重启；3个只含本次地址的受限轮次、25批、1,478名称访问、400孤儿回收，剩余本次socket/lease均0。循环9.74秒，最慢批次6,902微秒；10ms是调度预算，不是文件系统硬实时承诺。688个源码blob在运行前后核验，driver哈希和实际输出匹配。

Native6.1/high审查固定生产者diff后提出Windows错误字段、未知reap覆盖和不可达空值三个收尾，Host修复并测试。后续Buddy6.1/high核验固定e496收尾，exact finalArtifact2ab5c908-2c23-439c-9a7b-379702e26e3e / integration int-76e2cbee-fb70-4f81-9d95-0f94112c0485 已验收；报告UTF-8 SHA256 fbdf8ea89823e156b990fd200e11eb4b0573b41a91a5a51530241d81a0d6359a。它未验证hosted产物，以下由Host独立检查。

## 不可变产物与安装消费者

7个下载ZIP SHA256均与GitHub API digest一致，成员无重复/越界/符号链接。Windows每个manifest内部文件大小和哈希匹配；e496/FastDB4f/runner及实际job attempt全部绑定。两套full均满足23项applicable gates，并核对8个当前端点/所有者测试实际通过。

| 产物 | artifact | ZIP SHA256 |
| --- | --- | --- |
| Linux `local-endpoint-validation-e49652e-python-suite` | [11456250936](https://github.com/Dsssyc/c-two/actions/runs/37559074669/artifacts/11456250936) | `b1cb20c27920fc041dd70b62a3be802d1bffccb80564b0fe4799822a624a4220` |
| Linux `local-endpoint-validation-e49652e-core-test` | [11455269746](https://github.com/Dsssyc/c-two/actions/runs/37559074669/artifacts/11455269746) | `3e60b61de832ccbdba89bd2d16533742f8307d643d668abbb7d9e78b0a5dcb49` |
| Linux `local-endpoint-validation-e49652e-focused-local` | [11454864904](https://github.com/Dsssyc/c-two/actions/runs/37559074669/artifacts/11454864904) | `f6bd05044c214b27911a885f0abc80ec2c241ea982c1e2626212b3f65fb8a5b1` |
| Windows `windows-native-windows-2025-full-e49652e` | [11457155432](https://github.com/Dsssyc/c-two/actions/runs/37559074657/artifacts/11457155432) | `307434b6625f4666f4490883112932d11aa101b320415c9d2e717409ebf631fe` |
| Windows `windows-native-windows-2022-full-e49652e` | [11456293082](https://github.com/Dsssyc/c-two/actions/runs/37559074657/artifacts/11456293082) | `cc37dbddac684bec87c63eaf9a9312b7ddaf60b72da3e61f9c62c332c7f7e1a3` |
| Windows `windows-native-windows-2022-local-platform-e49652e` | [11455379792](https://github.com/Dsssyc/c-two/actions/runs/37559074657/artifacts/11455379792) | `fb0bd5f5b269cfe93241ec3f2ab223a758672ad75c5c4afeb0db320801627e66` |
| Windows `windows-native-windows-2025-local-platform-e49652e` | [11455314681](https://github.com/Dsssyc/c-two/actions/runs/37559074657/artifacts/11455314681) | `b2fd521a84e30092dd6ec60155e6ae65b7dff5a6677e7f599ca3697cf3a14d19` |

两套Windows普通管理员及另建非管理员用户wheel消费者均验证真实IPC/HTTP、安装wheel字节、held视图失效、4次borrowed输入失效、正常关闭、进程/临时目录清理。标准用户wrapper另外确认账户/workspace删除、无剩余PID；内存矩阵严格检查通过。下载c3.exe与矩阵及两种wheel收据所测字节一致：2022为75f149cc005104bd81171af17be8b0c42064752ebd4299a2ac5279fe0fe4ae56，2025为f1e259c36dbad9676398843b1bf4d165959e50f0599455e2d32bba7ab495bcc6。Linux runner c3 hash20416138a432ce28749d8f84470e2bd29caeca3717f2c8bae2a39082323f1a9c绑定收据且运行前后不变，ZIP本身不保留Linux可执行文件。wheel完整哈希在摘要。

## 交付范围

[当前接入说明](../local-endpoint-lifecycle.md)与AGENTS/方案只描述自动唯一后端；Buddy指出的四处旧注释/docstring在最终文档提交修正，可执行Python AST不变，Rust改动仅注释。正式构建产物仍绑定e496。

旧/tmp/c_two_ipc数据未删除，未知/损坏/部分初始化对象仍保守拒绝；不承诺任意登记崩溃位置自动恢复。已安装hey-my-buddy接入、正式发布和Windows11桌面验收分别需要证据。下载物、wheel、二进制与构建输出均在/tmp，Git只保留源码/文档和小型摘要；未改原始checkout、main/dev-feature、历史冻结收据。

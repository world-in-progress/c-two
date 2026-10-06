# 本地端点生命周期最终验收

2026-10-07。C-Two 的端点回收、Unix 托管租约退休和 OwnerBound 生命周期候选已完成实施及固定源码验收。普通业务连接断开后，Persistent 服务仍可接待新客户端；专用子进程可由私有所有者能力触发原生排空。回收只处理身份匹配的端点，旧凭据、未知对象与活跃监听者均不能被当作已确认的孤儿。

托管验收源码为 C-Two `bbfdbf48cb78d66eccccbc8ad7d33771b9dae344`、FastDB `4f99f86a662b0e950a0dd29800c25a1c9fca4def`。本报告及其他文档的后续提交不是这些产物的构建输入。实现位于 `socu/local-endpoint-lifecycle`，托管测试使用 `socu/local-endpoint-validation` 的同一源码；本次候选未创建 PR、合并或正式发布。结构化[验收摘要](evidence/2026-10-07-local-endpoint-validation.json)保留完整源码、ZIP、c3、wheel 哈希及计数。

## 实际通过的门禁

| 环境与源码 | 完整执行证据 | 结果 |
| --- | --- | --- |
| Linux，`bbfdbf4` | [run 37517266473](https://github.com/Dsssyc/c-two/actions/runs/37517266473) | 3/3 作业成功；Core 1,361、CLI 68、Rust SDK 12；聚焦门禁 521 |
| Windows Server 2022，`bbfdbf4` | [run 37517266371](https://github.com/Dsssyc/c-two/actions/runs/37517266371) | 完整门禁 23/23；平台门禁 779；Core 1,300、CLI 59、Rust SDK 12、Python native 2、FastDB Rust 22 |
| Windows Server 2025，`bbfdbf4` | 同上，独立作业及产物 | 完整门禁 23/23；平台门禁 779；Core 1,300、CLI 59、Rust SDK 12、Python native 2、FastDB Rust 22 |
| macOS，`980acec1bd32d960771e6a4a14a1ba4fc4d786cb` | `/tmp/c2-endpoint-implementation-1006/local-980acec1bd32/verified.json` | Core 1,360、CLI 68、Rust SDK 12、Python native 2；仓库测试 392；Python SDK 1,030 |

各环境 Python SDK 实际执行为 991 项普通测试、25 项 Rust/Python portable 测试及 14 项 TypeScript 测试，共 1,030 项，JUnit 无失败、错误或跳过。Linux 的 collection、分组计划和实际 call reports 已另外逐项核对，覆盖完整集合且分组不重复。Python 3.10 最低版本语法检查实际执行。Windows 每个 full 作业还运行了 34 项 harness 测试。Rust 数字是成功 `test result` 摘要的求和，失败及 ignored 均为 0；不将其描述为另一份逐 ID collection 证明。

macOS 全量证据严格绑定 `980acec1`。其后到最终 `bbfdbf4` 只修改了三份测试文件：Windows credential 拒绝字段的精确断言、relay 包装错误的精确断言和 Node 的原生错误/`v2.2` 目录断言；完整路径保存在验收摘要。生产实现无变化。后两份修复的生产者提交为 `73894c0bd1d1a6a7a85813a5da6e847aecb3f8ad`，macOS 聚焦验证为 Node 38 项及 relay 原测试 1 项。上述 macOS 全量结果不构成最终提交的执行证明。

## 测试保留与并行

源文件函数清单以 `01667b8b02f2fb447628b96b4d9236897300e81f` 为基线：Python 1,037 → 1,128，Rust 1,308 → 1,457，没有移除原有函数 ID。这里统计的是源码函数，包含 fixture/helper，不等同于参数化后的 pytest collection 数。改写的旧测试保留原方法 ID，并核对原有错误码、路由 UID、驱逐及生命周期约束；平台断言修复没有修改生产错误来迎合旧测试。

Rust 使用正常并行 harness，Python 普通测试使用 `-n 2 --dist loadfile`，每项 30 秒超时。portable 与 TypeScript 分组按原有外部产物、子进程及共享构建要求执行；Windows 2022/2025 与 Linux 的独立作业并行。冷 Rust 示例在 collection 阶段准备，未放宽普通业务测试超时。旧失败记录保留为诊断，最终成功来自修复后的新源码运行。

## 生命周期与回收证明

完整门禁包含 Persistent 断连后重连、重复绑定失败不影响原监听者、OwnerBound 控制器退出/强杀/启动前死亡、重复 EOF 与无关子进程不继承能力、有限 shutdown 观察、不提前释放 borrowed/held、原生 hook 只消费一次及端点重启。停止监听、工作排空和进程外 payload lease 分别观察，`completed=false` 不抹去原事务或 Python 绑定。共享原生 reaper 在观察者释放后仍负责 OS 子进程回收。

relay 使用解析后的端点协议进行上游身份核验、连接和路由调用；注销携带名称、server ID、instance ID、route UID 与 revision，旧实例注销不能删掉新路由。普通 relay idle 驱逐、业务断连与请求取消不获得 OwnerBound 关闭权限。

Unix `managed-v2` 使用规范 `v2.2` 目录和格式 2 gate/marker，核对设备号、inode 及随机 nonce，监听者固定原 gate inode 到租约结束。Linux 的真实 inode 复用失败推动了该 nonce 修复，原失败保留。忙、未知、损坏、替换对象和初始化未完成返回独立分类；没有年龄/PID/连接失败自动删除规则。Windows 沿用当前登录 SID 的 Named Pipe 和独立非继承内核监听租约，明确拒绝 Unix managed 协议。

macOS 真实压力验证绑定 `980acec1`：10,000 个本次生成的逻辑地址，9,500 次正常退出、500 次完整登记后的 kill 加 OS wait；包含 100 次旧凭据阻断与 100 次旧流仍存活时重启。只对本次地址做 3 轮受限维护，21 批、1,258 条访问、400 个孤儿回收，最慢批次 9,686 微秒，结束时本次 socket 与 lease 剩余 0。同步文件系统调用的 10 ms 预算是调度目标，该测量不构成实时上限。压力运行含构建总计 17.03 秒，真实循环 9.44 秒。

## 不可变产物与消费验证

7 个下载 ZIP 的实际 SHA-256 均匹配 GitHub API digest。核验拒绝重复成员、越界路径与符号链接，解包文件与 ZIP 一致；Windows 原生 manifest 中每个内部文件的大小和哈希也匹配。产物 ID、下载链接及完整 ZIP 哈希记录在下表及验收摘要。

| 产物 | GitHub artifact | ZIP SHA-256 |
| --- | --- | --- |
| Linux `local-endpoint-validation-bbfdbf4-focused-local` | [11437726344](https://github.com/Dsssyc/c-two/actions/runs/37517266473/artifacts/11437726344) | `c9357d8b13c28d2ca77829344027e294a303e598525050fa6f731668e0323df6` |
| Linux `local-endpoint-validation-bbfdbf4-python-suite` | [11437707535](https://github.com/Dsssyc/c-two/actions/runs/37517266473/artifacts/11437707535) | `f653747a97f9c9b1964e10beb28e5b15857318ab9271076a7a1a8050a25aefe6` |
| Linux `local-endpoint-validation-bbfdbf4-core-test` | [11437079748](https://github.com/Dsssyc/c-two/actions/runs/37517266473/artifacts/11437079748) | `fb0f9841cf9293b00dfa4ccdc4e9bcc03b7a760492eb3f558bbb69b9db6695f2` |
| Windows `windows-native-windows-2025-full-bbfdbf4` | [11440641670](https://github.com/Dsssyc/c-two/actions/runs/37517266371/artifacts/11440641670) | `1a931ababeb68de077d478606ace3207d75a6b4cf492aba6cccc321842dd0d0e` |
| Windows `windows-native-windows-2022-full-bbfdbf4` | [11440280909](https://github.com/Dsssyc/c-two/actions/runs/37517266371/artifacts/11440280909) | `79ea178dccb032b2e0b231150e2216b3b9bb11895a2da1ebee0ecd4141e7e256` |
| Windows `windows-native-windows-2022-local-platform-bbfdbf4` | [11437846508](https://github.com/Dsssyc/c-two/actions/runs/37517266371/artifacts/11437846508) | `d3e65ac65636626ae840d092f068c65fd78e27122a16ab097e2e00ef39431319` |
| Windows `windows-native-windows-2025-local-platform-bbfdbf4` | [11437129491](https://github.com/Dsssyc/c-two/actions/runs/37517266371/artifacts/11437129491) | `0bcd8ced63cfdf26673ea99fe54db7b9e98092cea8e452f89f6982d892747bdc` |

Linux、Windows 2022 和 Windows 2025 各有严格验证的 18 行 Rust/Python direct/relay 和 12 行 TypeScript 收据，阶段为 `development`。下载的 Windows `cli/c3.exe` 字节与对应矩阵及两个 wheel 消费收据测试的字节一致：

- Windows 2022：`e3f296fa078fa61c114cb9c71c89014b8c78b7f09c1fe1c74cc877321131c7e0`。
- Windows 2025：`b9a5246ba22d2aca746514bedd3adcb671044a5bf07c4493d1bf0c1006c93f03`。
- Linux runner：`19067e3c255463a1d95ebd811157c97e9287f338f483d7287cc3a9e4f63ac4b9`，运行前后确认 c3/native 字节不变并绑定收据；Linux ZIP 本身不包含 c3 可执行文件。

每套 Windows full 产物的普通管理员消费者及另建非管理员账户消费者均验证实际 IPC/HTTP 模式、安装 wheel 字节、held 视图失效、4 次 borrowed 输入失效、服务正常关闭及进程/临时目录清理。标准用户 wrapper 还确认账户、工作目录删除和无剩余进程。现有内存矩阵严格检查通过，端点维护未替代 buddy/dedicated/file spill 或 retained lease 的释放权威。

Host 检查真实 diff、聚焦结果及上述原始文件后，由独立 `gpt-6.1-sol/high` 对 7 个实际 ZIP、Linux collection/call reports、Windows job/attempt/receipt/wheel 清理和公开 Python API 做一次只读抽查，未发现确认阻塞项。Buddy 命令退出和看板完成状态只作为执行状态，未用来替代产物验收。

## 交付边界与接入

默认为 Persistent / legacy-v1；Unix 完整登记的 managed 端点可收敛，legacy 的永久 rendezvous 锁继续保留。部分初始化、损坏或未知历史对象保持 `unverified`，未承诺任意持久化窗口崩溃后的自动恢复。本次未删除 `/tmp/c_two_ipc` 的历史文件。

C-Two 的 Rust/Python 接口及真实子进程测试已交付，运行器接入顺序见[说明](../local-endpoint-lifecycle.md)。已安装 hey-my-buddy 采用新接口、正式候选包发布及 Windows 11 桌面验证需要各自的实际证据；本报告不宣称这些已完成。下载物、wheel、二进制及构建输出全部留在 `/tmp`，Git 只保存源码、文档及小型证据摘要。

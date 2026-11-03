# 独立 Python Runtime 与失败启动清理回执验收

已按 `e4ddcc85` 的[公开 Runtime 方案](2026-10-27-public-independent-runtime.md)及单独批准的[失败清理回执方案](2026-10-27-failed-start-cleanup-observation.md)完成。实现来源为 `e0b5af2d8aabd225dc286d75e5d1ef5c6b492855`；本记录及随后文档修订不是构建来源。本批没有 bump、发版、合入 main 或修改本地分支和标签。

## 实现与边界

原注册对象直接公开为 `cc.Runtime()`，模块入口委托默认实例；显式实例拥有自己的代码覆盖、Python 绑定和既有 Core session。空 Host 的 `start()` 复用原就绪/PONG，Bridge 预检读取所属 session，代理关闭不再初始化默认实例。没有复制一套运行时、信号协调器、自动目录清理或生命周期状态机。

失败启动使用本次具体 Server 的既有 `ShutdownTransaction` 回执观察 pending/终态，原启动错误和终态错误分别保留。NativeSession 在原 Host 锁下选择及观察回执，恢复 GIL 前释放锁；线程启动和等待 runtime 构造失败也交付同一事务的回执。旧 Host journal、delivered、清理预算和释放顺序保留，不遍历合并历史事务。

Python 新增 Runtime 门面；低层 Rust 新增 `host_with_failed_start_receipt` 和只读 `FailedStartShutdownReceipt`，原 `server_ipc_config` 改为公开供 native 预检复用。既有 Rust host 签名、IPC 14、SHM 3、C/FFI ABI 6、owner 控制和凭据格式不变；旧私有 `_ProcessRegistry` 不留别名。

## 验证

[最终 Source Validation](https://github.com/Dsssyc/c-two/actions/runs/40532138553) attempt 1 为 **10/10 jobs success，147/147 适用 gate 通过**。六份原始回执的来源均为上述实现提交，摘要和哈希见[证据](evidence/2026-10-28-public-runtime-acceptance.json)。

| 平台 | 适用 gate | Rust/Python 矩阵 | Node 矩阵 |
| --- | ---: | ---: | ---: |
| Linux / Python 3.12 | 25/25 | 18/18 | 12/12 |
| macOS / Python 3.12 | 25/25 | 18/18 | 12/12 |
| Windows 2022 / Python 3.12 | 27/27 | 18/18 | 12/12 |
| Windows 2025 / Python 3.12 | 27/27 | 18/18 | 12/12 |
| Windows ARM64 / Python 3.12 | 27/27 | 18/18 | 12/12 |
| Linux / Python 3.14t | 16/16 | 18/18 | 12/12 |

完整 Core/relay、Rust SDK、Python、安装包探针、真实跨语言和已有矩阵按各 lane 的适用范围执行。Windows 三条 lane 的普通用户安装探针也通过；Node 进程/读取器和安装探针的进程/临时目录清理回执均正常。除完整下载并校验的 3.14t 包外，归档通过范围读取提取 JSON 并校验成员 CRC；证据中的归档摘要来自 GitHub，不冒充本地完整归档散列计算。

基线四项配置/Bridge 来源反例、两项 Core pending 清理反例及两项真实 Native 首次失败/失败重启终态错误丢失反例均在预期断言失败。候选 Core 五项通过；pending 交错由原 startup gate 固定，终态错误保真另有真实 duplicate-bind Native/Bridge 证据。没有把 Core 确定性交错描述为真实 PyO3 pending 窗口已实测，也没有声称下游默认五秒预算已复现此故障。

本机扩展回归首轮为 271 通过、3 处新测试预期/旧替身问题、2 项平台跳过；修正后相应 3 项通过。连接/示例 71 项、SDK 边界 49 项及 macOS 真实 sandbox 绑定拒绝 1 项通过。另验证双实例真实 IPC、同名本地路由、独立有限调用预算、事件、长目录及 FastDB 持有响应跨关闭的所有权。异常投影重入探针未死锁。

Windows 11 x64 开发机实测 127 项通过，1 项 Unix 限定用例不适用；最终候选的五项 Core 回执测试全部通过。源码归档哈希与候选一致；独立实例预算互不串用，关闭后的 `WaitNamedPipeW` 返回 `ERROR_FILE_NOT_FOUND`。运行时代码未改的 127 项结果按来源比较复用，没有重复执行。

[首轮 CI](https://github.com/Dsssyc/c-two/actions/runs/40526344938) 的 Windows 失败有两项测试接入原因：既有 tempfile 测试依赖只向 Unix 声明，而新增测试签名无条件使用 TempDir；新 Unix sandbox 用例未列入严格平台适用性表。修正仅扩展既有 dev-dependency 的目标范围，并登记精确 nodeid/setup/reason，不放宽通用跳过规则。新增 runner 反例先失败；修复后 101 通过、1 项平台跳过。原始 Windows 报告的 pytest 退出码为 0，失败测试报告为 0。报告重放为 JUnit 存在性使用了明确标注的临时桩，不充当原始 JUnit 证明；最终候选重新完成上述正式门禁。

## 净量、审查与清理

基线到实现候选的实际 Git 文本差异如下，包含空白行；Rust `cfg(test)`、测试依赖和执行器声明归入测试，示例单列。

| 类别 | 增加 | 删除 | 净量 |
| --- | ---: | ---: | ---: |
| 生产 | 449 | 194 | +255 |
| 测试 | 986 | 307 | +679 |
| 实现阶段文档 | 113 | 19 | +94 |
| 示例 | 76 | 0 | +76 |

生产中 Python 运行时层净增 32 行，Core/Native 净增 223 行。本批是能力公开和正确性修复，不计作纯瘦身收益，也不提出吞吐提升结论。包含验收记录及证据后的文档实际净量为 **+329（353 增/24 删）** 行；示例仍单列。

独立审查修正了 Host 锁/GIL 交错、两条失败早退及示例清理表述；最终生产源码和 CI 接入修复分别完成只读复核。受控并发 register/shutdown 在基线和候选均有同一旧会话换代/hook 归属问题，已[单列待修](2026-10-27-runtime-concurrent-shutdown-followup.md)。本批没有隐含修复这条并发路径。

本次 13 个私有测试目录的 socket/lease 为零；这些目录、本机 Rust targets、临时 Python 环境/缓存、生成 native 库及 Windows 两轮任务目录均已清理。候选归档、临时 index 和专用 CI 分支也已清理；日志与回执保留。历史 namespace、hey-my-buddy 仓库和状态未触碰。应用示例只在确认终态且所有清理检查通过后删除自己新建的私有目录；启动错误仍抛出，关闭未确认或报错时保留目录。

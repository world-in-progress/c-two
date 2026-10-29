# Runtime 退役与迟到连接收尾

状态：实现、针对性回归、同环境对照、完整三端门禁和独立审查已完成。基线为 `9e8da3aacbfe64f704279a61a1c7f7198574d6c0`；生产实现为 `e66d250982d76fef2815a4d6fa3f06c77c1060ab`；最终测试候选为 `f747a7c9edd0171293c4f65fa4acefb3dac30b0e`，后者只调整两处集成测试和文档，生产文件逐字相同。最终提交仅补验收文档，构建与测试来源以上述候选为准；未 bump、未发版。

## 行为与所有权

复用 Core 的 ShutdownTransaction、ClientPool 原 epoch/retired record、FreshClientGuard 和关闭 observer。Runtime 退役范围永久封闭准入：首次建连、瞬时重试和最终入池均受同一原生事实约束，Preparing 也计入未完成工作。旧 Host 事务仍 pending 时升级同一个 driver；旧事务已经终态时只读原 journal、收取尚未结束的客户端工作。准备 executor、driver 或封池锁失败发生在交接之前，借用的 Host/failed-start owner 不被消费。

调用方 deadline 限制本次等待；已交接的原生工作继续按既有清理预算执行。`completed=false` 保留旧 SDK session/slots，后续必须再次 shutdown 消费终态并换代。已有 Host 关闭/重启语义保留。已有真实 owner 的 payload、预算、快照与释放顺序保留；R07 最终 Drop 继续单列。HTTP-only 不因退役创建 IPC 池，本范围也不证明所有 HTTP keepalive socket 已关闭。

Python 锁内启动真实事务，锁外观察并构造关闭错误；完成后先准备替换，再唯一消费 hook-safe slots，锁外调用 hooks。新增准入拒绝读取原生不可逆事实，不保存 Python closing 标志。最终 remote connect 在原 ConnectAttempt 下捕获 session 身份，锁外先检查原期限/退役，再报告身份变化；成功连接继续复用，发送后不重试。

新增低层 Rust RuntimeShutdownTarget / RuntimeShutdownReceipt、begin_session_retirement、PoolRetired、RuntimeRetired / ShutdownWaitTimeout，以及有界只读 journal 观察。Python shutdown 签名保留；数值字符串不再作为 timeout，pending 期间新 start/register/remote connect 被拒。IPC 14、SHM 3、C/FFI ABI 6 不变。公开错误取值与待完成后的再次观察规则已同步到 README、API、配置与生命周期文档。

## 验证记录

[原始四项基线反例](2026-10-28-runtime-late-connect-baseline.md)保留了错误 traceback 和旧 session，证明公开 shutdown 后迟到 acquire 可以建立连接，以及原期限耗尽后仍留下物理连接。正式回归分别固定在 Native 入口前、真实 acquire 入池后；关闭断言先于 peer 收尾，有效预算和过期预算都必须到达真实停点。

本机 macOS 的 Core 退役单元 6 项、公开集成 8 项、池关闭 20 项、failed-start 退役 1 项通过；Native 默认并行 17 项、受影响 Python 199 项通过。最后两处集成测试修正另做 owner 2 项及 memory 2 项验证；相同 memory 夹具在基线生产上也通过 2 项。真实 held owner 跨关闭与后续释放覆盖包含在这些检查中。原始日志摘录和哈希见[证据](evidence/2026-10-28-runtime-retirement-acceptance.json)。

Windows 11 x64 开发机的 Core 6 + 8、池 20 项通过；这部分生产和测试在最终实现中未变化，复用原记录。最终 Native 默认并行 17 项、Python 106 项通过，1 项既有 Unix 权限用例不适用。独立真实探针确认客户端物理关闭早于 peer shutdown，server shutdown 后另一 Runtime 连接原地址得到原生 `not_found` / Win32 2。构建时补齐本批 CMake，并将本批 target 放到短路径解决 MSBuild 的 260 字符限制；开发机源码与归档哈希保留，环境问题未修改生产行为。

[最终 Source Validation](https://github.com/Dsssyc/c-two/actions/runs/40639884911)首轮 **10/10 jobs success、六线 147/147 适用 gate 通过**。逐份回执确认 C-Two 来源为 `f747a7c9`、FastDB 来源为 `abba785e`，Python ordinary/portable/TypeScript 三组通过且其子进程清理确认。门禁覆盖完整 relay、Rust/Python/Node、Linux、macOS、Windows 2022/2025、Windows ARM64 和 Python 3.14t。此前 [e66d2509 的运行](https://github.com/Dsssyc/c-two/actions/runs/40635311574)保留失败事实：两个 owner 参数仍匹配旧错误文案；memory 夹具在 50 ms caller wait 返回后放行 callback，与原生最多 100 ms 的 disconnect courtesy 竞争。修正改为先等待真实调用失败，再放行 callback，原 join/watchdog、ClientCallResource、held、预算及释放断言均保留。后续旧域数量失败来自前项失败保留的 frame/owner；基线/候选连续定向回归及最终六线全套均通过。Windows 2025 同轮另有 node-gyp 开发文件准备阶段退出 6；保留该失败，未据此更改 C-Two 运行时、依赖或添加重试。修正测试后的最终候选通过该平台全部 27 项门禁；这不构成该外部工具故障已获根治的结论。

## 独立审查

Core/IPC 由 hey-my-buddy 的独立 reviewer 检查；首轮的失效 Weak 身份和封池锁期限问题已修正，复审无剩余阻塞。Native/SDK 复审补齐了错误投影重入、raw shutdown 名单锁预算字段、并行 Native 测试隔离，以及两个真实 shutdown observer 的一次换代/hook 交错；最终静态结论无阻塞。CI 两处测试修正另经独立只读审查，确认未放宽断言、未将 callback watchdog 异常混作客户端关闭。审查仅证明静态结论，运行证据由 Host/CI 单独取得；固定 run/artifact 标识在证据 JSON 中。

## 实际增量与开销

相对基线，测试候选生产增加 1,230 行、删除 310 行，净 **+920**；测试增加 1,868 行、删除 30 行，净 **+1,838**。Rust 的 cfg(test) 区域按测试计，不将内嵌测试混入生产。最终方案、文档与报告净增 347 行，原始基线、日志及机器回执净增 1969 行，分别列账。生产分项为 Core +555、IPC/HTTP +142、Native +195、Python +28。明显超过方案估计，本批属于正确性补强，不计作纯瘦身收益；未引入第二套 driver、历史 session 列表或清理框架。

同一 macOS / CPython 3.12 环境按 ABBAABBA 顺序比较基线与最终生产，8 轮均验收私有目录 socket/lease 零残留。每版缓存 connect 2,000 个样本、空关闭 200 个、带 Host 关闭 80 个。中位数如下；测的是本机观察值，不作跨平台性能保证。

| 路径 | 基线 | 当前 | 差值 |
| --- | ---: | ---: | ---: |
| 复用连接的 connect | 161.4165 μs | 164.6455 μs | +3.229 μs（约 2.0%） |
| 空 Runtime shutdown | 7.083 μs | 58.146 μs | +51.063 μs |
| 带 Host shutdown | 1.673229 ms | 1.6730005 ms | 约相同 |

空关闭增加了原生退役事务和执行资源准备；正常连接与业务调用没有新增清理线程。原始样本及可复跑脚本分别见[数据](evidence/2026-10-28-runtime-retirement-benchmark.json)和[脚本](evidence/2026-10-28-runtime-retirement-benchmark.py.txt)。

## 清理与后续范围

已完成的本机首轮 target/env、基线目录及 Windows 源码/env/CMake/target 已清理，日志和回执保留；CI 测试修正的本机补验 target/env 和临时 CoreSDK 也已清理。历史命名空间、本地分支和标签、hey-my-buddy 源码保持原状；两条专用 fork CI 分支已删除，7 份本批 buddy checkout 均已回收，日志和固定产物引用保留。R07 最终 Drop、旧配置/启动错误的回调重入、私有 reset/atexit 和 standalone Bridge 的任意并发重绑仍保持单列。

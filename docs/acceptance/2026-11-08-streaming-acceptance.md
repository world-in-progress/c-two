# 流式 RPC 第二、三阶段验收记录

日期：2026-11-08。基线：上游 main `d772ca3a`。依据：[流式 RPC 设计](../design/2026-11-03-streaming-rpc-design.md)第九、十节与 [ADR 0003](../decisions/0003-server-streams.md)。第二阶段是块 5 至 8，IPC 16 与 C/FFI ABI 8 同批切换，SHM 4、relay mesh 7 与契约 v2 不变；第三阶段是块 9 的交叉验收。第一阶段见[第一阶段验收记录](2026-11-04-streaming-phase-1-acceptance.md)。代码与文档在 fork 的分支 `socu/streaming` 上验收，2026-11-08 经 [world-in-progress/c-two#219](https://github.com/world-in-progress/c-two/pull/219) 合入上游 main，见“合入上游 main”一节。CI 各线的工具链与二进制 SHA-256、配对基准各平台的收据摘要与逐次样本见[证据摘要](evidence/2026-11-08-streaming-acceptance.json)。

## 确切候选与 CI

确切候选是 `4df9120c1ef672717071f2c59a6db22fa34e91a6`。[Source Validation 42389270549](https://github.com/Dsssyc/c-two/actions/runs/42389270549) 以 `force_full=true` 手动触发，第一次尝试 9 个作业全部通过。macOS arm64、Windows x64、Windows ARM64 与 Linux x64 的 Python 3.12 跑完整范围：Rust Core、c3、Rust SDK、wheel 的构建与安装后消费、Python 全套及 portable 与 TypeScript 门禁、IPC 内存矩阵、Node 绑定的测试与打包；两条 Windows 线另跑标准用户的消费测试与管道身份测试，Linux x64 的 Python 3.12 线还另建 Python 3.10 环境跑普通测试组。Linux x64 的 Python 3.14t 跑 Python 范围。各线用 rustc 1.99.1（2026-10-13）与 Node v22.23.4；Python 在 macOS 与 Windows 上是 3.12.10，在 Linux 上是 3.12.16，自由线程线是 3.14.10，3.10 环境是 3.10.22。wheel 沿用版本号 0.10.1，只凭候选提交与安装后原生扩展的 SHA-256 认定，各线的值记在证据摘要里。确切候选之后只有文档提交。

第二、三阶段在整合分支上跑过的全平台 CI 如下，各块的行为与本机检查写在[发行说明](../releases/unreleased.md)对应的节里。表里写“只改测试”的修正，产品代码都没有改。块 9 各步的编号沿用设计第十节与待办里的记法。

| 内容 | 候选 | 运行 | 结果 |
| --- | --- | --- | --- |
| 块 5a，IPC 16 调用记录 | `b5b4853c` | [41911808218](https://github.com/Dsssyc/c-two/actions/runs/41911808218) | 第一次尝试全部通过 |
| 块 5b，IPC 传输层的服务端流 | `d6c808f9` | [41936786846](https://github.com/Dsssyc/c-two/actions/runs/41936786846) | 重跑失败的作业后全部通过；第一次在 Windows x64 上有一个 c2-server 关闭测试读回复超时，记在待办的“要核实的事”里 |
| 块 6a，Rust 的服务端流 | `ae4b0647` | [41962137139](https://github.com/Dsssyc/c-two/actions/runs/41962137139) | 第一次尝试全部通过 |
| 块 6b，Python 的服务端流 | `ca405b34` | [41978967958](https://github.com/Dsssyc/c-two/actions/runs/41978967958) | 两处测试竞态，只改测试 |
| | `293bbe37` | [41987564596](https://github.com/Dsssyc/c-two/actions/runs/41987564596) | 重跑一个失败的作业后全部通过 |
| Windows 上的连接关闭原因 | `76f164e1` | [42000339894](https://github.com/Dsssyc/c-two/actions/runs/42000339894) | macOS 上一个 c2-ipc 测试的断言过严，只改测试 |
| | `c396652b` | [42003739984](https://github.com/Dsssyc/c-two/actions/runs/42003739984) | 第一次尝试全部通过 |
| 块 7，Node 与 C/FFI ABI 8 | `d543846b` | [42014116482](https://github.com/Dsssyc/c-two/actions/runs/42014116482) | 两条 Windows 线失败在两处测试问题，`d742c0fe` 修好 |
| | `f1979bb5` | [42022054809](https://github.com/Dsssyc/c-two/actions/runs/42022054809) | Windows 上 TypeScript 夹具直接启动 `npm`，随块 8 修；块 7 在 Windows 上的验证随块 8 的 CI 完成 |
| 块 8，relay 的服务端流 | `ffe9ee4f` | [42084831707](https://github.com/Dsssyc/c-two/actions/runs/42084831707) | Linux 3.12 上三个 Rust 测试失败：转发接收方等终态时可能丢掉唤醒，是块 8 的产品缺陷；一个 c2-http 测试有竞态；第三个是待办里那个 c2-server 关闭测试 |
| | `07526724` | [42102516034](https://github.com/Dsssyc/c-two/actions/runs/42102516034) | Linux 3.12 上一个测试：转发入口先交出结果、后删待决记录，改为先删 |
| | `6b6b446e` | [42110086137](https://github.com/Dsssyc/c-two/actions/runs/42110086137) | 重跑 Windows x64 后全部通过；第一次失败是 relay 没读请求体就回 404，Windows 在客户端读到回复之前重置了连接，这个缺陷块 8 之前就有 |
| | `4e29a3f3` | [42120578509](https://github.com/Dsssyc/c-two/actions/runs/42120578509) | 修好上一行的缺陷，第一次尝试全部通过 |
| OPEN 带上调用模式 | `a55219ee` | [42160416841](https://github.com/Dsssyc/c-two/actions/runs/42160416841) | 第一次尝试全部通过 |
| 块 9b，补测 | `0608c583` | [42223663715](https://github.com/Dsssyc/c-two/actions/runs/42223663715) | macOS 与两条 Windows 线失败在新的门禁测试本身，9b16、9b17 只改测试 |
| | `bf91abe4` | [42243559739](https://github.com/Dsssyc/c-two/actions/runs/42243559739) | Windows ARM64 上两处测试时序，9b18 只改测试 |
| | `3f67ef0a` | [42254418051](https://github.com/Dsssyc/c-two/actions/runs/42254418051) | 第二次尝试全部通过；第一次 macOS 的托管运行器失联，Linux 3.12 上一个与流式无关的 c2-local 端点测试偶发失败，记在待办里 |
| 块 9c3，小调用退化的修复 | `484fb5b2` | [42373792581](https://github.com/Dsssyc/c-two/actions/runs/42373792581) | 9c3 的新测试在 Windows 上指定了 Unix 的端点根目录，9c3b 只改测试；relay 获取上游连接时把自己刚撤下的路由报成 702，是分支起点就有的产品缺陷，9c3c 修好 |
| | `0f85a6be` | [42378277784](https://github.com/Dsssyc/c-two/actions/runs/42378277784) | 三处流测试的时序，9c3d 只改测试 |
| | `145494ee` | [42384536676](https://github.com/Dsssyc/c-two/actions/runs/42384536676) | Linux 3.12 上一个 Node 测试自己先提交了取消，9c3e 只改测试 |
| | `4df9120c` | [42389270549](https://github.com/Dsssyc/c-two/actions/runs/42389270549) | 确切候选，第一次尝试全部通过 |

## 配对基准

9c 的第一次 unary 配对基准（[42266088975](https://github.com/Dsssyc/c-two/actions/runs/42266088975)，基线 `d772ca3a`，候选 `3f67ef0a`）在四个平台上的正确性全部通过，4 MiB 载荷持平或更快，1 字节小调用变慢。五组配对比值的中位数，默认配置与关闭 buddy 两行在 Linux 上是 1.042 / 1.033，在 Windows x64 上是 1.051 / 1.059，在 Windows ARM64 上是 1.068 / 1.058；Windows 越过设计第十二节第 7 条的 5% 门槛。

按第 7 条先定位。按块二分的三次运行（5a 之后 [42273303771](https://github.com/Dsssyc/c-two/actions/runs/42273303771)、6b 之后 [42277670106](https://github.com/Dsssyc/c-two/actions/runs/42277670106)、块 8 之后 [42273336371](https://github.com/Dsssyc/c-two/actions/runs/42273336371)）与两次相邻配对（块 8 对最终候选 [42349023784](https://github.com/Dsssyc/c-two/actions/runs/42349023784)，块 5a 对 6b [42349045296](https://github.com/Dsssyc/c-two/actions/runs/42349045296)）显示，退化主要落在两段：分支起点到 5a，只有跨运行的数字；5b 与块 6 在同一次运行里约 +2%（默认配置下 Linux 1.017，Windows ARM64 1.020）。块 8 到最终候选，两行在 Linux 上是 0.995 / 1.008，在 Windows x64 上是 1.008 / 1.020，在 Windows ARM64 上是 1.013 / 1.015。本机 macOS 上逐提交统计两个进程每次 1 字节调用的指令数、系统调用数与线程切换数（每轮 3 万次调用，16 轮交替测量取中位数）：系统调用与线程切换在所有提交上都不变，多出的指令来自块 2、5a、6a 与 6b，块 7 至 9 持平。隔离实验证实块 2 的增量来自 Python 客户端绑定新增的 GIL 交接；5a 的 OPEN 记录在 unary 路径上另建两个小 `Vec` 再拼进帧里，这一点是读代码所得，没有单独测量；6b 让服务端每次调用多一次 `getattr`；6a 与 6b 的原生部分没有定位到函数。

维护者 2026-11-07 决定修。9c3（`d557c36b`）让 Python 客户端一次普通 unary 调用的 GIL 交接从 8 对减到 2 对（分支起点是 4 对），每条调用记录一次编码进预先算好长度的整帧，服务端在注册时就记下方法是不是流。同一次修复去掉了 `invalidate_then_release` 持着响应的锁调用 Python invalidator 的死锁，这个死锁分支起点就有。在本机同一轮测量里，每次 1 字节往返的指令数，客户端在分支起点、修复前与修复后分别约为 245.5k、252.0k 与 247.5k，服务端约为 209.2k、214.9k 与 212.3k；两边合计比分支起点多出的部分从 2.7% 降到 1.1%。剩下的部分在 5a、6a 与 6b 的原生代码里，没有定位到函数。

确切候选上的配对基准是 [42391112745](https://github.com/Dsssyc/c-two/actions/runs/42391112745)，基线 `d772ca3a`，候选 `4df9120c`，按维护者的要求在全平台 CI 全绿之后派发。四个平台的正确性全部通过：每个平台 22 种异常客户端场景与对照在五组里都通过（候选 110 次），没有阻断性失败。下表是候选与基线之比，每格依次是默认配置与关闭 buddy 两行。p50 与吞吐用收据自己的统计，即五组配对比值的中位数；四客户端两行取全部组与客户端的中位数。收据只给 p99 的比值，表里的 p95 由各组的原始摘要算出。

| 平台 | 1 字节 p50 | 1 字节 p95 | 四客户端 64 KiB p50 | 4 MiB p50 | 4 MiB p95 |
| --- | --- | --- | --- | --- | --- |
| Linux x64 | 0.995 / 0.990 | 1.000 / 0.994 | 0.982 / 0.965 | 1.012 / 1.020 | 1.134 / 1.113 |
| Windows x64 | 1.003 / 1.011 | 1.017 / 1.000 | 1.050 / 0.983 | 1.007 / 1.014 | 1.003 / 1.028 |
| Windows ARM64 | 1.010 / 1.015 | 0.998 / 1.008 | 1.021 / 1.044 | 0.896 / 0.891 | 0.888 / 0.915 |
| macOS arm64 | 1.339 / 0.895 | 1.015 / 0.912 | 0.789 / 0.964 | 0.913 / 0.928 | 0.821 / 0.986 |

吞吐在 Linux 与两种 Windows 上都不低于基线的 0.96 倍。四客户端两行的比值在各组之间从 0.81 到 1.19 不等，最高的 Windows x64 默认配置正好是 1.050，它的 p95 是 0.994。macOS 的托管运行器上同一行各组的比值从 0.52 到 1.87，分辨不出 5% 的差别，前几轮也是这样。

越过门槛的只有 Linux 上 4 MiB 科学负载的两行，p95 是基线的 1.134 与 1.113 倍。这两行每组只测 8 次，p95 与 p99 都落在最慢的那一次上。逐次样本显示候选在 Linux 上晚一次调用才进入稳态：在 10 对样本里，8 次测量的第一次候选都要 15.5–17.8 ms，基线是 13.7–14.8 ms；之后的稳态基线约 13.9 ms，候选约 14.2 ms。三次预热加第一次测量，候选合计比基线少 3.5–4.1 ms；SHM 与文件后备的峰值不变；Windows 上没有这种偏移。维护者 2026-11-08 把它接受为已知的预热现象；晚一次收敛的原因没有查。

同一次运行在候选上跑了 9c2 加的流对照：26 行在四个平台上都跑通，没有失败。它用同一份内容比较整份 unary、多次 unary 与一条服务端流。流的吞吐与多次 unary 相比，4 MiB 条目是 1.10–1.67 倍，四客户端的 64 KiB 条目是 0.92–1.56 倍，1 字节条目是 0.89–1.15 倍，4 MiB 的 FastDB 连续数组是 1.00–1.13 倍。大流与短 unary 混合时，短调用的 p99 是单独运行时的 1.8–11.0 倍，同一份内容换成多次 unary 时是 2.6–11.8 倍；短调用的 p50 在走 SHM 时是单独运行时的 0.88–1.13 倍，不走 SHM 时是 1.05–2.17 倍，换成多次 unary 时是 1.40–2.34 倍。第九节要求的复制次数没有测：没有现成的观察点，也没有为此加产品探针，收据如实写成未测量。

## 第九节的验收范围

验收范围按设计第十节 2026-11-06 的补记收窄：relay 不把调用转给另一个 relay，双跳组合取消；生成的 TypeScript 经 relay 的流随 TypeScript 运行时那一批再做（第十二节第 10 条），Node→Python 的 relay 组合随之顺延。relay 单跳做 Python 客户端→Rust 资源与 Rust 客户端→Python 资源两种组合。9a 把第九节的直接 IPC 矩阵、relay 组合与异常表，以及[闲置驱逐审查](2026-10-10-idle-eviction-regression-audit.md)的六道门禁，逐项对到现有测试；维护者决定把缺口全部补齐，9b 分十项补测。准备门禁时发现 Runtime 客户端连接池的空闲回收时限写死为 60 秒，按第十二节第 12 条做成配置项（9b11，`3a9e21a0`）；实现时一并改为后台按时驱逐，此前过期的连接要等下一次查找才被丢掉。六道门禁在 Python（27 例）、Rust SDK（25 例）与生成的 Node 客户端（21 例）上补齐，覆盖持久直连、relay-local 与 HTTP 三条路径，每个文件在 macOS 上连续 20 轮通过；真实的对端结束在三种客户端的流测试里另有覆盖。

块 9 测出或查出的产品缺陷都在合入前修好，各自的说明在发行说明里：

- 生成的 TypeScript 客户端在 relay-local 路径上每次调用都新建并关闭本地连接；ABI 8 原把期望的 route token 放在端点上，改为在 prepare 时按路由传入（9b12，`79a1f6d1`）。
- Runtime 客户端连接池对同一端点的并发建连不合并（9b13，`94c4b80f`）。
- Host 在 Runtime 关闭期间停止接纳调用之后，路由查询仍报 Ready（9b14，`c49ad680`）。
- relay-aware 客户端在路由重新注册之后按整个 relay 或整个端点排除候选，连不上同名的新路由（`93dbd2de`）。
- `invalidate_then_release` 持锁调用 Python invalidator，与另一个线程对同一响应的缓冲导出互相等待（9c3，`d557c36b`，分支起点就有）。
- relay 撤下关闭的路由时停掉了共享的上游客户端，正在获取连接的请求被报成上游暂时不可达（702）。路由关闭后并发 800 次调用的复现里，修复前有 110 次报 702、23 次报 704，修复后都没有；现在获取连接的每个失败出口都按当前路由归类（9c3c，`f7b857a9`，分支起点就有）。

块 9b 的门禁还测出 D5：relay-aware 客户端经 HTTP 的旧代理，在路由换成新实例之后可能一次 `RouteStale` 也收不到就进了新实例，它报出的 route UID 仍是旧的。维护者 2026-11-06 决定不单独修，改为重新定路由的身份（[ADR 0004](../decisions/0004-route-name-identity.md)），流式批次把 D5 记为已知问题。

## 合入上游 main

确切候选之后，`fe94478f` 与 `6a6c5344` 只改文档。开 PR 之前，上游 main 合入了 #218，只把 `core/Cargo.lock` 里的 rustix 从 1.1.5 升到 1.1.6。main 的保护规则要求分支与 main 同步，PR 的 CI 测的又是 head 提交而不是合并结果，所以先把上游 main `0e3d2ffd` 用普通合并并进分支，得到 `098e3b53`，没有冲突。[world-in-progress/c-two#219](https://github.com/world-in-progress/c-two/pull/219) 在 `098e3b53` 上的 [Source Validation 42435098894](https://github.com/world-in-progress/c-two/actions/runs/42435098894) 第一次尝试全部通过，源码各线的范围与确切候选那次运行相同。2026-11-08 以普通合并提交 `a5ab5eaa` 合入，它的树与 `098e3b53` 相同；main 上这次推送的 [Source Validation 42449036052](https://github.com/world-in-progress/c-two/actions/runs/42449036052) 凭同一棵树已通过的 PR 运行跳过了源码各线。rustix 的升级没有再跑配对基准。

## 留给后续的

- 路由身份（ADR 0004，[验收记录](2026-11-09-route-identity-acceptance.md)）解决 D5，要在任何包含 IPC 16 的版本发布之前完成，并加回 9b8b 为等那处修复而去掉的 relay-local 首次 acquire 用例。
- 生成的 TypeScript 经 relay 的流，随 TypeScript 运行时那一批再做（待办“TypeScript 与 Node”）。
- `sdk/python/native/src/runtime_session_ffi.rs` 里还有两类持锁碰 Python 的写法，与 9c3 修好的死锁属同一类，只做了静态核对（待办“按区域的结构调整与重复校验”）。
- 配对基准的收据只给 p99 的比值，第十二节第 7 条的门槛用 p95；`run_matrix` 曾在 Windows 上把一次正常结束的行报成超过等待时限（待办“发版工具，方案 B”的顺带项）。
- 经 relay 的流的提前量，以及 Host 关闭返回时空闲连接可能还没关完，仍待维护者决定（待办“待维护者决定”）。

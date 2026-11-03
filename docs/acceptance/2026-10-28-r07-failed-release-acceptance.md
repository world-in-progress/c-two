# R07 失败释放与 Prepared 发布界线验收

2026-10-28，R07 按用户批准的[收窄方案](2026-10-28-r07-complete-release-followup.md)（规划提交 `b1ee0e35f830c6a2dfa39ef49318c8f84e5d3c43`）完成并验收关闭。实现经 hey-my-buddy 微任务 `0b9f2da1-03b8-44bf-9b86-dc332bb00c1c` 委派，Worker 依次交付 `f73b5dde`、`af0ae01b`、`c770b18be0263a11362daeb3a69a3326c8955d77`。整合提交 `000985b6417714df80e88fe56c50b1e5445853b8` 由 Host 压平生成：代码与 `c770b18b` 逐字节相同，证据目录按下文精简。本记录及之后的文档提交不作为构建来源；未 bump 版本，未发版。

## 改了什么

显式释放失败时，原持有者保留原 lease、预算与具体连接，可再次释放；失效回调在进入前标记，异常或 panic 后不重放。消费式出口与最终载体 Drop 失败时保守放弃：不 free、不退预算，由最低层载体计一次 `abandoned_releases`。该计数加在 `BudgetSnapshot`、`MemoryScopeStats` 和 Python 每个 memory scope 上。

Prepared SHM 回复在整帧写完、flush 之前移交 buddy 释放权；之后 flush 出错、到期或被取消都不本地 free，计一次结果不明。trailing-inline、chunk 与拒绝丢弃的对端请求共用同一个 `PeerShmRequest`，删除了把释放错误换成成功的旧旁路。`PendingResponse` 把暂忙与真实错误分流，真实错误不再进入每 5 ms 的重试循环。

验收中纳入两处方案修订（已写入方案第 10 节）：

- chunk 注册表的 abort、连接清理与 GC 抽取原先把永久释放错误当暂忙无限重试，会卡住接收任务、让 close 无法确认。现在真实错误放弃该重组 backing（计一次，扣住原计费），忙时照旧重试。这是基线就有、方案漏列的同类问题；副作用是若错误其实会自行消失，该块要到客户端重组池拆除时才回收。
- pending 拿到真实错误且是 `RequestReleaseState` 的唯一持有者时，直接最终放弃，不再在 pending 表锁里阻塞重试。被省掉的那次重试不可能成功（已派发 dedicated 的错误只来自确定性的代次与身份校验），结果与原写法等价。所有引用与 permit 都在 pending 表锁之外丢弃。

用户要求在本批一并完成的发版前事项：Windows 真实 overlapped flush 的端到端回归（自然触发，不加测试开关）；在文档中写明计数可能被同用户异常对端抬高；Python cleanup 错误在 3.11 起通过 `add_note` 可见，同时保留 `__context__` 链给 3.10；新的对端请求回归同时在 Windows 上运行。

C/FFI ABI 6、IPC 14、SHM 3、业务帧、CRM hash、owner-control、端点与凭据格式均未改。

## 规模

| 分类 | 新增 | 删除 | 净量 |
| --- | ---: | ---: | ---: |
| 生产代码 | 1088 | 589 | +499 |
| 测试（含内嵌 cfg(test)） | 1469 | 60 | +1409 |
| 测试依赖清单 | 9 | 0 | +9 |
| 文档 | 24 | 7 | +17 |
| 验收证据 | 1090 | 0 | +1090 |

按 Git 行数统计，含注释与空行，Host 独立核算。生产净增在方案估计的 400–650 行内。测试超出估计的 650–1000 行，超出部分对应验收中补要求的覆盖：注册表三个抽取出口、pending 唯一持有者与锁序、`ServerShmResponse` 与 `RequestReleaseState` 的最终计数、Prepared rollback 双失败、dedicated 发送成功后 settle 失败，以及 Windows 自然触发回归。属于正确性修复，不计瘦身收益。

## 验证

- 审查：Host 读完全部差异；另请一个独立 Opus 子代理只读审查第一版，约 45 万 token、11 条线索，经 Host 复现后采用 5 条（注册表无限重试、pending 锁内重试、拷贝持写锁、测试名实不符与覆盖缺口、过时注释与死代码），其余记为后续或文档。
- 定向变异 7 处全部被对应回归抓住：整帧写完后 flush 出错改为本地释放、帧事实改到 flush 之后记录、失效回调改为执行后才标记、未领取回复的真实错误改为继续重试、取消后已发布帧改为本地释放、注册表真实错误改为保留重试、pending 唯一持有者改为交给会重试的 Drop。
- 本机 macOS 在 `c770b18b` 上，与固定 FastDB `abba785` 并列的检出：Core workspace（含 c2-http/relay）1928 通过、0 失败、5 忽略；Rust SDK 22、Python native 18 通过；Python 普通组 1295 通过，Rust/Python 跨语言与 18 行矩阵 25 通过，矩阵收据 26 通过，Python 3.10 语法 1 通过。普通组另有 3 项只因本机环境未通过：两项 TypeScript 持久 IPC 用例需要预先构建的 Node 依赖；一项 bind 拒绝用例在 `sandbox-exec` 下被剥掉 `DYLD_LIBRARY_PATH`，而本机扩展按 system link 构建。CI 以 source link 构建并准备 Node 依赖，覆盖这三项。
- 默认命名空间：最终检查前后 `/tmp/c2-v3-<uidhex>` 与 coordination-v3 的条目名一致。首轮 Core 全量曾留下一对 socket 与 lease（c2-ipc 夹具 `capped_ack_peer` 的地址，Drop 时协调锁被同进程其他测试占用，按设计留给之后回收）；它不是本批引入的，按约定未手工清理。
- CI：首轮 [Source Validation 40690805433](https://github.com/Dsssyc/c-two/actions/runs/40690805433) 跑在第一版代码上（提交 `728967a9`，即 `f73b5dde` 去掉证据目录），六条源码线的 Core、Rust SDK、native、portable 矩阵与 TypeScript 均通过，只有 Python 普通组的 SDK 边界测试失败（junit 1306 项中唯一失败），已在第二轮修复。确切候选见下表。
- 确切候选：[Source Validation 40711889990](https://github.com/Dsssyc/c-two/actions/runs/40711889990) 跑在整合提交 `000985b6` 上。第 1 次尝试的 Windows 2022 失败在 c2-mem Node 测试为端点上下文用例现编的 Rust 夹具：编译第三方 crate `windows-sys` 时 rustc 以退出码 2 中止，尚未编到 c2 代码；同一用例在首轮 CI 的 Windows 2022 以及本轮其他 Windows 线上都通过。只重跑该任务后通过，第 2 次尝试 10/10 jobs 成功。六条源码线的适用门禁共 147 个全部通过（Windows 2022、2025 与 ARM64 各 27，Linux x64 与 macOS 各 25，Linux 3.14t 16），每条线的 Python 普通组、portable 矩阵与 TypeScript 组都通过。作业日志不逐条打印测试名：Windows 2022 重跑的日志里能直接看到 dedicated 变体的自然触发回归通过；其余新增用例由各线 core-test 步骤整体通过证明，未下载逐线 run-evidence 逐条核对。
- Windows：Worker 在 `Dsssyc-x86-PC`（x64）上开发了自然触发回归：先用不读取的对端填满管道写缓冲，再发 prepared 帧并关闭对端。buddy 与 dedicated 两条连续 10 轮共 20 次通过，工作目录已删除。这些只是开发证据；验收以 CI 的 Windows 2022、2025 与 ARM64 为准。

## 已知边界

- Unix 上"整帧写完、flush 之前"的窗口实际不存在（UDS flush 为空操作，中间没有挂起点），因此 Unix 只做分层回归；完整发送路径由 Windows 自然触发回归覆盖。
- 为让 SDK 边界测试通过，PyO3 crate 的 `c2-ipc` 开发依赖被移除，原生层"释放失败时保留跟踪、重试或放弃后归还"的真实分配测试随之删除。对应语义仍由 c2-core 的真实分配测试覆盖；是否收窄边界测试并恢复该用例，列为发版前待办。
- benchmark 是第一版在同一环境、dev profile 下的窄微基准（每组 7 轮×2000 次的中位数，ns/次）：small 103→89，SHM 2240→2347，chunk 4482→4552，Held 4552→4603。第二轮只把对端请求拷贝改回读锁，成功路径没有新增开销，未重测；它不代表端到端 RPC 吞吐。
- 计数会被已协商 SHM 的同用户异常对端（无效、重放或过期坐标）抬高，已写入文档；计数非零不等于仍泄漏同样多的块，为零也不证明没有内存问题。

验收中发现、但不属于本批的事项已列入发版前待办：c2-codegen 生成的客户端在 spec 哈希不匹配时丢弃释放错误的原因；c2-ipc 夹具在默认命名空间留下残留；c2-local `final_lease_stat_io_releases_close_and_drop_leases` 的低频偶发失败需要定位根因。

## 证据与清理

仓库内证据目录由 Host 从 Worker 交付的 63 个文件精简为 24 个：红绿回归与修正前后输出、开发中间失败的原始输出、benchmark 补丁与数值、命名空间前后对比、清理记录、检查汇总与第一次 Windows 运行输出。可由 git 推出的 numstat 与逐项 ok 列表不进仓库。Worker 的完整证据保存在 hey-my-buddy 封存产物 `6c5e42c7-6f72-4eb4-807f-2bb3ae4333f4` 中，其累计补丁（基线 `b1ee0e35`，输出 `c770b18b`）SHA-256 为 `c5eed37b8829541fff8e018289760c4bf485f14b2df78c79a29128f7389d0433`。

清理：本机验收检出、Cargo target（约 9.2 GiB 与 2.4 GiB）、CLI target（约 1.2 GiB）、虚拟环境、FastDB 克隆与 Core SDK 均在本记录提交后删除；hey-my-buddy 的 Worker 检出已由 accept 回收，Windows 测试机上的任务目录由 Worker 删除。fork 上本批的两个 CI 分支 `socu/r07-ci-1`（`728967a9`）与 `socu/r07-ci-2`（`000985b6`）按确切 SHA 删除。没有对默认命名空间做 sweep、reap 或手工清理，历史目录未动；本地分支与标签保持。

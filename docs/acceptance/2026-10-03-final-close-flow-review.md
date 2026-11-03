# 15578b5 最后一轮非阻塞关闭交接复核

## 结论与证据边界

**本轮有限静态审查未发现固定提交 `15578b5c8be1c010012ee44aee6bb776c435ec35` 的新增关闭交接存在阻塞回归。阻塞 finding：0。** 未找到可由具体源码路径推出的新增丢 owner、双释放、早退款、清理 RID 覆盖、post-dispatch 重放、取消／正常 completion／close 重试之间的可持续死锁，或新增的多帧无界响应滞留。

审查以 `1c78cd6e5fc49cd97126ec6f5707757cdaf99d6a` 区分基线行为，并与关闭候选 `97bba5c8a19878d222234decaf2c8dec8b86bf22` 对照。重点为 `core/transport/c2-ipc/src/client.rs` 的 pending、completion、两个 Drop、receiver footer、maintenance 和 close 重试，以及 c2-wire 的 try-release／try-cleanup 接口；只追踪必要的 owner、物理释放和错误阶段依赖，没有重审全部 97 文件。

本轮只读源码、差异和已有测试断言，未运行 Cargo、SDK、Python、动态探针或红转绿实验。任务提供的 wire 149、IPC 151、Core 错误边界 2 项通过，以及上一轮 `600c0a6` 的红转绿，是 Host 提供的既有证据；仓库中的 `2026-10-02-close-owner-fix.md` 也记录了这些结果。本轮没有读取其外部机器收据，不将这些数字作为本轮亲自执行的结果，也不推断正在另跑的完整门禁已经通过。已修 ABBA 和 receiver 同步 Drop 延迟没有重复列为 finding。

## 所有权与并发复核

以下是支撑结论的源码检查，均不是缺陷条目。行号对应上述固定提交。

| 路径与行号 | 触发条件与交接结果 |
| --- | --- |
| `client.rs:1548–1602`、`4545–4567` | waiter 已取消或 RID 未知时，失败的 `tx.send` 返回完整结果，`Ok(response)` 进入 `PendingResponse.response`。请求结算与 response 释放分别尝试；只有 response 释放成功才取走 response，只有两者都完成才删除条目。请求仍忙而 response 已成功释放时，条目只保留请求，不会下一次重复释放同一 response。 |
| `client.rs:4573–4592`、`4779–4897` | 每次 completion 都由单一 recv loop 顺序 `await`。未交付 response 仍忙时，循环不返回到下一帧读取；每轮的 pending guard 在 5ms 异步等待前释放。因此 `entry.response = Some(response)` 不会被同一 receiver 的下一帧覆盖，也没有新增 response 清理队列。若 response 已释放但请求仍忙，允许继续接收，保留的只是请求释放状态。 |
| `client.rs:1608–1622`、`2743`、`3004`、`3250`、`3380`、`3464` | 所有这几类 unary 注册共用 pending 锁下的 vacant-entry 搜索。活跃调用和 cleanup-only 条目都占用 RID，counter 遇到已占用值（包括回绕）时跳过，不覆盖其 request/carrier。这个保护针对仍在 map 中的 owner，不是对整个 RID 历史或无限请求数的承诺。 |
| `client.rs:783–824`、`854–944`、`3294–3310` | dispatch 与 release 共用 permit 锁；在发出任何字节前，dispatch 状态和 retire permit 已交接，pending 已挂上共享 request authority。try-release 遇到 permit／pool／executor 锁忙时不改 phase、不取 permit。dedicated 成功物理结算后，在仍持有 executor guard 时提交携带原 pool Arc 的 job，或归还不再需要的 permit，最后才发布 Released；不存在“先标记完成、再等待交接”的窗口。 |
| `client.rs:870–879`、`948–956`、`1070–1079`、`1685–1709`、`3331–3365` | 已 dispatch buddy 不在本地 free。已 dispatch RequestBlock 的自动 Drop 仅尝试释放；sent guard 先关闭 waiter，请求忙则留下原条目，有 response 时也不删除。partial-write 取消仍 abort 流并保留发布 authority。发送失败的 prealloc 分支同样保留忙的已发布 owner；正常 response 返回不再同步等待请求池。最终 `RequestReleaseState::Drop` 仍是同步后备，安全性依赖上述 pending owner 保留，不能泛称所有 RAII Drop 都非阻塞。 |
| `client.rs:4520–4542`、`2600–2661`、`4900–4906` | maintenance 只清理 `tx=None` 或 receiver 已关闭的条目，不终止活跃 waiter。close/footer 可先唤醒 waiter，再尝试结算；busy/Err 留在 map 中。maintenance 的 registry/request-pool guard 在访问 pending 前已结束，后续锁只尝试获取，没有持这些 guard 跨 await 的路径。 |
| `chunk/backing.rs:55–61`、`113–143`、`299–363`；`assembler.rs:206–212`；`chunk/registry.rs:479–559` | backing 的 owner incarnation 在 admission 的原 guard 内捕获，try-release 与阻塞 release 共用同一临界区验证原 owner 并 free。busy/Err 保留 handle、reservation 和 pool；成功后才移除状态、先 drop handle 再退 reassembly charge。registry 仅在 `Ok(true)` 后移除 assembler、减计数；随后 Drop 已无 live backing，不会“探锁成功后再阻塞 Drop”。 |
| `client.rs:1089–1116`、`4803–4887`；`sync_client.rs:73–94` | response admission／feed／finish 错误仍通过 `IpcError::Chunk` 交付，属于 DispatchUncertain。Capacity 类型没有退化为表示请求尚未发出的 `Pool` 错误；`is_retry_safe` 只接受 PreDispatch，未见本轮新清理路径要求重放已 dispatch 调用。这里只核对这一错误出口，不扩展为对全部重试体系的验收。 |

额外核对了 retire worker 的锁关系：`client.rs:615–651` 在取出 job 后释放 executor guard，才进入 pool 工作；`719–728` 在离开 pool 临界区后才归还 permit。与新增 request settle 的 permit→pool→executor 尝试链之间，未找到持 executor 阻塞等待 pool 的闭环。旧有上限仍是两个线程、256 个已授予 permit（`162–176`）；忙的 dedicated request 本身仍占有 permit，没有因 response 已送达而提前扩大 admission。

## receiver 暂停时的 control 与 lifecycle

触发序列是：取消调用的晚到 response（或未知 RID response）进入 completion → 原 response pool 的释放锁忙 → 完整 response 留在 pending → receiver 在 `client.rs:4591` 等待。期间后续帧不会被读入，因此 PING、control reply 和 DISCONNECT_ACK 都可能延后。这是同一连接上的读取背压，不能表述为“control 帧仍正常处理”。

生命周期关闭有独立出口：`close_shared_bounded` 先停止／join maintenance（`4256–4270`），对 writer/disconnect 使用同一绝对 deadline（`4275–4290`），随后尝试 graceful join；不完成时，直接 abort stream 和 recv task，再 join 原槽内句柄（`4292–4309`）。这个取消不依赖 receiver 读到 ACK，completion 在 await 前已经把完整 carrier 放入 pending，因而取消其 future 不会同步丢弃那份 response。

recv 的 `ConnectionAssemblyCleanup::Drop` 只作 try-cleanup（`4598–4606`），未完成 assembly 留在同一 registry。close 在任务终态后用同一 pending 和 conn_id 重试（`4328–4339`）；释放还忙则返回 false，保留可达 owner，不谎称所有清理已结束。句柄由 `join_close_task` 在槽内轮询，取消 closer 不会取走未终态任务（`1943–1970`）。`close_incomplete` 直到确认后才清除，`connect` 拒绝未完成的重连（`2275–2285`、`4350`）。解锁后的下一次 close 可继续同一事务；只有确认后才 detach transport-owned pool slot（`4342–4347`、`4394–4420`）。

因此，在本次新增交接中，**释放忙不会令生命周期关闭只能等待 receiver 读取 control 帧才能推进**；完整清理可诚实地保持未确认。若业务 callback 持有释放所需的读锁并等待同一连接的后续业务结果，读取背压本身不提供业务进展保证；基线的同步 unclaimed-response release 已有同一依赖。本轮没有把这种既有依赖当作新死锁，也不要求持锁期间 close 返回 true。

“最多当前一帧”指新增的、由 receiver 接收后无法交付的完整 response carrier 数量。已有 partial assemblies、已经成功交付给用户的 held carrier，以及 `recv_buf` 的历史容量仍可能同时存在；前两者有各自预算／所有权，不能把这句话解读为整个进程只占一帧内存。当前新增路径在一个 response 留存时暂停下一次读取，未见多帧清理积压；close abort 后保留该条目，重连门禁又阻止在旧事务未清理时启动新 receiver。

上层 try 接口也不等于物理 free 的硬实时 API：`MemPool::free_at` 仍调用既有 buddy free（`pool.rs:792–839`、`alloc/buddy.rs:273–305`），OS mapping 销毁仍有实际成本。这不构成本轮新增可持续锁死的证据，未假设 OS 暂停或以此另列 finding。

## 基线限制与不作出的结论

低层同一个 `IpcClient` reconnect 后仍复用 `server_pool` 这个 Arc slot，新握手在 `client.rs:2437–2440` 替换其内容；坐标型 `ResponseData::Shm` 及 `ResponseLease` 依赖该 slot（`response.rs:15–21`、`74–84`、`102–106`、`132–136`）。旧 SHM lease 跨这种低层重连的限制已在 `1c78cd6` 中存在，不是本轮 `PendingResponse.response` 新增的 owner 交接回归。新门禁防止的是 cleanup pending 未完成时的重连，不是承诺修复已交付的所有旧 SHM lease。reassembly Handle 则携带原 pool 与 reservation，不能与坐标型 SHM 混为一谈。

审查不证明全部 SDK、Python、Windows 或 runtime 生命周期正确，不代替 Host 的完整门禁，不构成发布批准。没有提出架构重写、兼容 shim、删除测试或扩大冻结修改范围。

## 最窄现有验证入口（仅阅读，未执行）

| 验证对象 | 仓库已有入口与应观察的断言 |
| --- | --- |
| 未交付 response、RID 保留、close 取消等待 | `client::tests::close_unclaimed_file_completion_keeps_carrier_for_retry`（`client.rs:5325`）：已取消／未知 RID 两分支；busy 时完整 carrier 与 charge 留存、相同 counter 跳到下一 RID、receiver 已终态、close false；解锁 retry true，独立 held 仍有效，release 后退款。该测试以 completion task 接入，不冒充实际多帧 recv 流测试。 |
| 真实 receiver abort 的 partial assembly | `client::tests::owner_aborted_real_receiver_file_cleanup_is_retryable`（`5519`）：真实 LocalStream 和 recv_loop；持真实 carrier callback 时 receiver 终态、未完成 assembly 与预算保留；解锁重试再清理，不破坏 finished carrier。 |
| 正常 completion／footer／caller token Drop | `client::tests::owner_receiver_request_completion_and_footer_are_retryable`（`5667`）：pending 是最后 request owner；持锁时 Drop 与两个终态入口可返回，phase/charge 保留，close false；解锁 retry，真实 peer read_done 后预算归零。 |
| dedicated 交接所需三把锁 | `client::tests::owner_dispatched_dedicated_locks_keep_settlement_retryable`（`5847`）：分别阻塞 permit、pool、executor，验证失败尝试不改 phase、不丢 permit／owner、不早退 charge，retry 后仍等真实 read_done。 |
| maintenance 不终止活跃 waiter | `client::tests::close_maintenance_file_callback_is_retryable`（`5419`）、`client::tests::close_maintenance_request_pool_contention_does_not_stall_runtime`（`5468`）：busy tick 可结束，cancelled waiter 被清理而 live waiter 保留，close 可继续。 |
| wire owner 和 charge | `chunk::backing::tests::try_release_file_preserves_owner_under_live_carrier_callback`、`chunk::registry::tests::try_cleanup_file_retains_connection_charge_and_other_owners`、`assembler::tests::try_release_file_keeps_partial_assembly_for_retry`；以及 backing 的 colliding buddy/dedicated replacement 和 peer-authority 测试。只读其源码，没有声称本轮通过。 |
| post-dispatch admission 错误 | `sync_client::tests::response_admission_is_dispatch_uncertain_and_not_retry_safe`（`sync_client.rs:544`）：已收到 reply 后 admission 失败必须不允许自动重试。 |

对于“忙 response 后仍有第二 response／control 帧排队”这一组合，本轮结论来自单 receiver 的逐次 await 和独立 abort/join 路径，未亲自运行该动态组合。若 Host 要单独验证它，最窄探针是固定第一份未交付 carrier 的释放锁，在真实 LocalStream 排入第二帧和 ACK，观察 pending 中未交付 carrier 不增长、close 可取消 receiver 且忙时 false；解锁后 retry 完成，预算按 owner 顺序退款。这是证据边界说明，不是发现阻塞问题或要求重跑全套。

## 本轮实际只读检查命令

以下命令实际在 allocated checkout 执行，列出与结论相关的固定点、差异、源码和测试断言读取。没有把上一轮 Cargo 命令列为本轮执行记录。初始 `git status --short` 无输出；HEAD 与输入一致。较大的初次 diff/read 输出出现截断，关键范围随后拆分重读。

```bash
git status --short
git rev-parse HEAD
git log -6 --oneline
git show 15578b5 --format=fuller --no-patch
git rev-parse 1c78cd6 97bba5c8 14cc7d57 600c0a6
git show --stat 15578b5
git diff 15578b5^ 15578b5 -- core/transport/c2-ipc/src/client.rs
git diff --numstat 97bba5c8 15578b5 -- core/transport/c2-ipc/src/client.rs core/protocol/c2-wire/src/chunk/backing.rs core/protocol/c2-wire/src/chunk/registry.rs core/protocol/c2-wire/src/assembler.rs
git diff 97bba5c8 15578b5 -- core/transport/c2-ipc/src/client.rs
git diff 15578b5^ 15578b5 -- core/transport/c2-ipc/src/sync_client.rs
git diff --unified=3 1c78cd6 15578b5 -- core/transport/c2-ipc/src/client.rs | sed -n '1,270p'
cat docs/reports/2026-10-02-close-owner-fix.md
rg -n 'PendingResponse|complete_unary_pending|RequestBlock|SendGuard|try_drain_pending|try_release|try_cleanup|maintenance|recv_loop|receiver' core/transport/c2-ipc/src/client.rs
rg -n 'try_release|try_cleanup' core/protocol/c2-wire
rg -n 'pending.*(insert|remove|drain|clear|retain)|entry\.response|entry\.request|tx\.send' core/transport/c2-ipc/src/client.rs
nl -ba core/transport/c2-ipc/src/client.rs | sed -n '440,740p'
nl -ba core/transport/c2-ipc/src/client.rs | sed -n '843,1068p'
nl -ba core/transport/c2-ipc/src/client.rs | sed -n '1540,1780p'
nl -ba core/transport/c2-ipc/src/client.rs | sed -n '1938,1978p'
nl -ba core/transport/c2-ipc/src/client.rs | sed -n '2230,2320p'
nl -ba core/transport/c2-ipc/src/client.rs | sed -n '2380,2456p'
nl -ba core/transport/c2-ipc/src/client.rs | sed -n '2480,2669p'
nl -ba core/transport/c2-ipc/src/client.rs | sed -n '2735,2773p'
nl -ba core/transport/c2-ipc/src/client.rs | sed -n '2780,2940p'
nl -ba core/transport/c2-ipc/src/client.rs | sed -n '2990,3070p'
nl -ba core/transport/c2-ipc/src/client.rs | sed -n '3188,3372p'
nl -ba core/transport/c2-ipc/src/client.rs | sed -n '3370,3575p'
nl -ba core/transport/c2-ipc/src/client.rs | sed -n '4200,4420p'
nl -ba core/transport/c2-ipc/src/client.rs | sed -n '4450,4668p'
nl -ba core/transport/c2-ipc/src/client.rs | sed -n '4670,4912p'
nl -ba core/transport/c2-ipc/src/client.rs | sed -n '4907,4921p'
nl -ba core/transport/c2-ipc/src/client.rs | sed -n '5298,5418p'
nl -ba core/transport/c2-ipc/src/client.rs | sed -n '5418,5517p'
nl -ba core/transport/c2-ipc/src/client.rs | sed -n '5518,5647p'
nl -ba core/transport/c2-ipc/src/client.rs | sed -n '5660,5780p'
nl -ba core/transport/c2-ipc/src/client.rs | sed -n '5780,5950p'
nl -ba core/transport/c2-ipc/src/response.rs | sed -n '1,220p'
nl -ba core/transport/c2-ipc/src/sync_client.rs | sed -n '15,105p'
git show 1c78cd6:core/transport/c2-ipc/src/client.rs | rg -n 'complete_unary_pending|release_unclaimed_response|server_pool|try_drain_pending|PendingResponse|recv_loop'
git show 1c78cd6:core/transport/c2-ipc/src/client.rs | sed -n '4255,4310p'
git show 1c78cd6:core/transport/c2-ipc/src/client.rs | sed -n '2228,2260p'
nl -ba core/protocol/c2-wire/src/chunk/backing.rs | sed -n '35,148p'
nl -ba core/protocol/c2-wire/src/chunk/backing.rs | sed -n '268,386p'
nl -ba core/protocol/c2-wire/src/chunk/registry.rs | sed -n '464,605p'
nl -ba core/protocol/c2-wire/src/assembler.rs | sed -n '202,219p'
nl -ba core/foundation/c2-mem/src/pool.rs | sed -n '610,630p;786,845p'
nl -ba core/foundation/c2-mem/src/alloc/buddy.rs | sed -n '270,310p'
```

一个内部 Codex subagent 仅协助只读核查 wire owner／cleanup 路径，未派 Buddy peer、未写文件、未执行动态测试。主 agent 重读了其关键 release、registry 和 owner 临界区。最终仅新增本中文报告；未实现、改 Host／其他 worker／原始树，未提交、push、PR 或发布。

最终文档核验实际执行 `git diff --check`、`git status --short`、`git rev-parse HEAD`，均 exit 0；status 唯一记录是 `?? docs/reports/2026-10-03-final-close-flow-review.md`。另用只读 `python3` 脚本断言固定 HEAD、完整 porcelain status 只含授权文件、`git diff --quiet HEAD --` 返回 0、报告为非空 UTF-8 且无行尾空白，全部成立。另读 `sync_client.rs:535–562` 并用 `rg` 核对表中 wire 测试函数确实存在。这些是文档／范围检查，不是 IPC 动态测试。

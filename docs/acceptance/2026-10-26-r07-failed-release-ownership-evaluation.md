# R07 失败释放的所有权与预算评估

状态：R07a 已按用户批准边界完成并提交；实现/测试来源 `b059c02d`，完整验收见[记录](2026-10-26-r07a-explicit-release-acceptance.md)。生产净增 134、测试净增 427 行；确切来源 Actions 10/10、真实三端、完整 relay/跨语言、端点零残留及独立审查完成，构建产物已清理，未发版。基线 `7a1dd40fd12bb2d5d35dd4fffb55bdf653ab7be0`，IPC 14、SHM 3、C/FFI ABI 6。C04、R35 与两份控制会话重构继续暂缓。R07a 只覆盖仍有明确接收者的显式释放；完整 R07 保持开放。

## 已复现的原因

`MemPool::release_handle` 按值消费 move-only `MemHandle`，buddy 分支又把 `free_at` 的错误转换成 `Ok(FreeResult::Normal)`。owner 校验失败虽返回 String 错误，句柄也已被消费。`MemHandle` 自身没有替 SHM 执行释放的 Drop；丢失句柄不会让活分配自动消失。

正常收发路径还有同类边界：`ResponseLease::release` 和 `RequestLease::release` 先从自己的 Option 取出 carrier，再执行可能失败的释放。内部 `ReassemblyBacking` 能保留失败的 state/charge，但外层立即 Drop 它；最终失败时 backing Drop 保守遗留 reservation，不能恢复再次释放能力。Core Held 随后取走 lease 并标记逻辑 released，Rust/Python held 包装也会撤销一次性 owner/callback；因此修正底层两个 lease 不等于修复所有 SDK 出口。

原有设计有明确用途：`424dce3e6542` 把三种存储统一为消费式释放，SHM free 错误当时被忽略；`ba1b2914cc9b` 返回 FreeResult 供延迟 GC 使用，并延续吞错。`93321ed6bb10` 使 allocator 对竞争耗尽、poison/dead-holder 拒绝修改；上层消费契约没有同步收敛。`f5c9aa332bad`、`15578b5c8be1` 已建立重组与 pending 的失败保留事务，可直接复用。R02 的 `047cd58b580b` 明确保留旧失败策略，把本问题留给 R07；此前成功后仍留下可访问句柄的 `try_release_handle(&mut MemHandle)` 已被否决，不再采用。

旧清单“只有测试调用”需要限定：当前 SyncClient 两处 data_ptr 错误回滚调用了 release_handle，但这两个 helper 仓内仍只有测试消费者；而且它们刚在同一把 pool 锁内成功分配，data_ptr 只检查代际与映射，尚未找到合法 safe 操作使此分支失败的条件。共享锁竞争/poison 不会直接触发 data_ptr 失败。fill 回调报错或 panic 则实际可达，走 RequestBlock；不能将其混称为 data_ptr 回滚的生产复现。正式请求准备与请求/响应 lease 还存在不经过 release_handle 的释放路径。

## 隔离证据与预算区别

Host 以原样生产源码构建单独 Rust 探针；仅缩小 workspace 成员并增加评估可执行文件，核对了 180 个非 manifest 源文件、各 crate manifest 和原锁定依赖。没有修改锁预算、身份或释放实现。使用真实 SHM 映射和原 1000 万次 spin budget 的 ShmSpinlock；poison 通过原 with_lock 的受控 panic 产生，没有重置或偷锁。

macOS arm64 / Rust 1.91.0 与 Windows 11 x64 build 26200 / Rust 1.99.1 的七个场景一致：

| 场景 | 基线结果 |
| --- | --- |
| 原 MemHandle，真实竞争或 poison | 返回 Ok，活分配仍为 1；映射 charge 保持 12288 B。 |
| 原 MemHandle，错误 pool | 返回 owner-incarnation 错误，活分配仍为 1；返回值不携带原句柄。后者同时由公开返回类型确认。 |
| 直接 ReassemblyBacking，竞争后解锁 | 首次 Err，保留原 state 和 4096 B 重组 charge；第二次实际释放，活分配与重组 charge 都归零，后续 admission 成功。 |
| ResponseLease / RequestLease，竞争后解锁 | 首次 Err；第二次却返回 Ok，活分配仍为 1、重组 charge 仍为 4096 B，下一次同容量 admission 被预算拒绝。 |
| Core Held，同样的竞争 | 同一滞留；逻辑 released 为 true，失效回调只执行一次，复制快照仍完整。再次调用不清理该分配。 |

映射预算与重组预算不能混算：buddy block 释放成功后，映射可按原缓存/GC 策略继续占用 SHM charge；重组 reservation 应在存储释放成功后归还。对照场景正是 alloc_count=0、reassembly=0、SHM=12288，而非三个值同时为零。BufferLeaseTracker 另管观测元数据，不是上述物理释放权威。

本轮验证实际内存/carrier/Core 方法，没有启动完整 RPC、Unix listener 或 Named Pipe，也没有执行 Python SDK、Linux 或 Windows ARM64 运行测试。dead-holder 的拒绝语义依据源码，未另造死亡持有者实验。实施验收仍须补齐真实 SHM RPC 与跨语言路径，不能用这些结果替代完整门禁。

每个场景在记录故障后，仅通过本批 pool.destroy/Drop 结束其唯一映射；原生重开同名 SHM 均失败，临时 scope 目录为空并移除。失败场景的遗留重组 charge 只在独立探针进程中，进程随后退出；没有把它写成重试成功。本机清理 416382143 B、Windows 清理 509463053 B 的 target；未使用历史 IPC 目录、inspect/reap 或历史清理。Windows 另读回十份关键源码/probe/lock 哈希，与输入一致。[机器记录](evidence/2026-10-26-r07-evaluation.json)保留摘要与原始文件哈希；源码与日志在 `~/.codex/diagnostics/c2-r07-evaluation-dzwd7ix1/`。

## 推荐首批：R07a

1. `MemPool::release_handle(MemHandle) -> Result<FreeResult, ReleaseHandleError>`。错误包含原 MemHandle 与原原因；先借用它完成 owner 校验及实际释放，成功才消费，失败原值返回。原句柄不复制、不凭坐标重建；成功后不留下可访问的 owner。MemHandle 本身仍不替调用者延长 MemPool 寿命。
2. `SyncClient::pool_alloc_and_write/fill -> Result<RequestBlock, RequestPreparationError>`。分配失败无待清理块；准备失败先按原路径尝试清理，未完成时错误私有持有原 RequestBlock、主原因及释放原因。data_ptr 回滚用返还的原句柄与原 pool Arc，在退出 pool 锁作用域后构成该块；fill 失败保留现有块。错误仅提供原因查询和一次清理操作，不能取出半填充块重新发布，也不重放 fill 回调。`pool_free` 直接返回已有 `block.release()` 的 Result。
3. `ResponseLease` / `RequestLease` 使用原 Option 借用 carrier 执行释放，成功后才 take，错误原地保留。RequestData 的私有释放方法相应借用。沿用 ReassemblyBacking 的 storage-before-charge 事务，不增加共享 owner、请求 phase、退役队列或 worker。

owner 校验、原 free_at 的代际检查及 GC 时机保持。失败保留的是目标分配及其 reservation；free_at 本来可能先回收其他 dedicated backing，不能把“整个 pool 的所有统计不变”误写成错误路径契约。

竞争时，原 owner 可在锁正常释放后再尝试；poison/dead-holder 继续拒绝，不能因为保留了 owner 就承诺恢复。错误 pool 应交回正确原池；过期代际/id 不能通过重试变成有效，不能影响新分配。不解析错误文字驱动自动重试，不新增后台重试。成功后的原容量释放、dedicated freed_at/read_done/crash-timeout 与 file unmap-before-refund 保持。dedicated 原有无效槽位成功无操作不顺改；buddy 原先被吞掉的 stale/缺 backing 错误则会显露，相关旧测试需明确迁移。

| 生产替换范围 | 估算与维护成本 |
| --- | --- |
| c2-mem 的 pool/lib；c2-ipc 的 sync_client/lib/response；c2-server 的 dispatcher，约六个文件 | 约 140–240 行增改，预计净增约 90–160 行。两个具体错误只承载本次失败所有权，不改通用 IpcError，不建通用释放框架。 |
| 对应 pool、SyncClient、request/response lease 与现有 ownership 编译探针 | 约 220–400 行增改，预计净增约 180–300 行。替换假成功/失败已 released 的断言，复用现有夹具，不叠加仿制 owner。 |

这些是静态方案预算，尚无实现补丁或实际 numstat。本项为正确性增量，没有代码净减或性能收益声明。成功路径预计无新锁、worker 或 payload 复制；具体错误值变大、失败错误对象保留原 pool/预算，以及后续 Drop 等待都是需要验证的成本。

## 公开影响、剩余范围及回归

首批有四处低层 Rust 返回签名变更及两个可命名的具体错误类型，不留旧吞错入口。两个 transport lease 的签名不变，失败后仍未释放、第二次可实际释放，属于明确的错误语义修正。Python 与正式 Rust SDK 签名、C/FFI ABI 6、IPC 14、SHM 3、业务帧和 CRM hash 不变；不能声称 Python held 的失败保留已修复。

Core Held 可另用现有 released 表示逻辑失效、lease Option 表示物理待清理，不必新增字段；但 connection pin 必须按物理完成归还，invalidator 失败而 transport 成功也应归还。Rust Held 的 owners.take、Python 一次性 callback、consuming 物化/构造失败、PreparedShmResponse 的消费式回滚和最终 Drop，还需明确将失败 owner 交给哪个存活对象。直接重放任意 release_cb、把已交付响应塞进现有 pending，或忘掉 reservation 后宣布完成，都不能作为方案。首批不改这些边界，完整 R07 不核销；后续先单独确认接管责任，不能暗中增加后台机制。

重点风险为：错误转换时再次丢 owner；持 pool 锁析构 RequestBlock；暴露半初始化的可发布块；重复归还 permit/charge；将逻辑失效误当物理完成；以及保留 Option 后，既有 Drop 可能增加一次有界 allocator 等待。保留状态提供重试条件，不构成最终析构的可靠接管保证。

确认后按以下范围验收：先把真实竞争/poison、错误 pool、返还同一 allocation_id/generation 与两类 lease 的再释放反例纳入现有测试；补 helper 的主/释放双错误、fill 仅一次、错误块不能发布；保留 trim 到 0/1 的原容量、dedicated 退休、file、快照、copy/release 双错误与 dispatch/取消覆盖。dead-holder 必须继续拒绝且不改锁字。随后运行定向、完整 relay、Rust/Python 18 行和 Node 12 行矩阵、真实三系统及确切候选 Actions（含 Windows ARM64/3.14t），本批 socket/lease 零残留，独立审查后提交；Windows 优先开发机，完成即清理构建产物。

独立只读核查使用 hey-my-buddy run `5cc2f3dc-23b6-4250-b055-8c7d724dc5b7`、空补丁 artifact `20b2b746-b6f8-43e4-9c9b-72559d88f1a2`。Host 复核源码/历史并执行上述实验；worker 没有运行实验，不把 Host 结果当作其独立运行证据。报告已接受，工作树已回收；只读环境拒绝过一条需 heredoc 临时文件的计数辅助命令，没有绕过，不影响 Host 的复现及本方案交付。

## R07a 实施后边界

首批四处低层返回签名及两类显式 lease 已验收；上述静态估算由[实际行数与开销](2026-10-26-r07a-explicit-release-acceptance.md)取代。Core/Held、consuming、构造回滚与最终 Drop 的失败接管责任继续开放；R07 不整体核销。失败保留提供调用者再次清理的条件，poison/dead-holder、过期身份及最终析构不因此获得恢复保证。

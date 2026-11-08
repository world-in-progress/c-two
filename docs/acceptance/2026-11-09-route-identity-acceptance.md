# 路由身份验收记录

这份记录验收[路由身份的执行计划](../design/2026-11-08-route-identity-plan.md)，它实施 [ADR 0004](../decisions/0004-route-name-identity.md)。整合分支 `socu/route-identity` 从上游 main 的 `fd33e140` 切出。每一块交付之后读完差异，在整合分支加上这份差异的候选上跑受影响的测试与完整检查，通过之后才合入；各块定下的细节写在执行计划各块的执行记录里。

## 确切候选与 CI

全平台 CI 在 fork 上运行 Source Validation，覆盖 Linux x64 的 Python 3.12（同时用 Python 3.10 跑普通测试）与 3.14t、macOS arm64、Windows x64 与 ARM64。

| 内容 | 候选 | 运行 | 结果 |
| --- | --- | --- | --- |
| 块 1、2、5 | `25a93160` | [42473351631](https://github.com/Dsssyc/c-two/actions/runs/42473351631) | 失败在两处测试问题：relay 控制测试用的注册证明工具在会话夹具里编译，冷的 runner 上超过 30 秒的单个测试时限；relay 流测试的帧转发器把对端的连接重置当作错误。`0ca57533` 只改测试与运行脚本。Linux 3.14t 一条是下载 FastDB SDK 超时。Windows x64 上 c2-core 的 `server_streams` 测试进程还以 `STATUS_HEAP_CORRUPTION` 退出过一次，没有复现，记在待办 |
| 块 1、2、3、5 | `633a24ed` | [42481482052](https://github.com/Dsssyc/c-two/actions/runs/42481482052) | 第一次尝试全部通过 |
| 加上块 4 | `6c87c7ac` | [42490629331](https://github.com/Dsssyc/c-two/actions/runs/42490629331) | Linux 3.12 上 Node 绑定的测试报未处理的 rejection：原生准备用完调用预算才交回 promise 时，生成的传输先查截止时间，没有给 promise 挂上处理函数。这是块 4 的产品缺陷，`2951e5aa` 修好。Windows ARM64 上两处流测试是流式批次留下的，`6f7352a6` 修好 |
| 加上块 6 与执行器拆除的修正 | `d774f992` | [42506043902](https://github.com/Dsssyc/c-two/actions/runs/42506043902) | 失败在四处，都是测试问题：HTTP 流测试先探测空闲端口、放开之后再在它上面启动 relay，并行的测试可能先占走这个端口（Windows 上的 10048），`33ee933b` 让 relay 先绑定监听地址并报出实际地址，测试改为绑定端口 0；Windows 上执行器拆除的三个流测试没有驱动或释放命名管道，`402ac175` 只改测试；macOS 上注册回执测试的 HTTP 服务器绑定时反查 127.0.0.1 的主机名而卡住；Windows x64 上生成的 Node 客户端的冲突测试只等了一个 relay 的视图，后两处由 `88847903` 只改测试修好 |
| 确切候选 | `72ffc97b` | [42520322994](https://github.com/Dsssyc/c-two/actions/runs/42520322994) | 第一次尝试全部通过 |

## 门禁

[闲置驱逐审查](2026-10-10-idle-eviction-regression-audit.md)的六道门禁里，门禁 1、3、4、6 照旧；门禁 2 与门禁 5 以执行计划第五节的新表述为准，另加两道新门禁。照录如下：

门禁 2：路由经过就绪、关闭、撤下与同名重新注册之后，新的获取与已有代理都到达当前的注册；没有在线注册时，调用在派发前失败，不进入任何资源；已经派发给旧注册的调用报它真实的终态或派发不确定，不重放；调用方看不到 route UID。

门禁 5：同名的 Host 重启、control watch 中断后恢复、旧候选与新持有者并发时，关于旧注册的证据不能撤下或关闭新的持有者；已有代理在下一次调用时跟到新的注册；发往旧进程、还没有结束的调用报完成或派发不确定。

新门禁 7（路由冲突）：两个 relay 在分区愈合后各有同一个名字的在线注册时，新的获取报 `RouteConflict` 并列出两次注册；已有代理继续用自己那一份；注销其中一份之后冲突解除，那一份的代理跟到留下的一份。

新门禁 8（失联 relay）：失联 relay 的名字不参与解析（报 702），也不能在别处注册（报 703），直到它回来后自己撤下，或者部署方对它强制离开。

### 证据的标准

门禁要的是真实的 IPC 服务器，涉及 relay 的用真实的 c3 relay，不要求每个 Host 都在独立的子进程里。在测试进程里用自己的 Runtime、经真实 IPC 端点提供服务的 Host 就是门禁说的真实 IPC 服务器。只有门禁的表述本身关乎进程的地方才要独立的 Host 进程：门禁 3 第二个用例里真实的流断开与对端结束，以及门禁 5 的 Host 重启与发往旧进程的调用；这些地方用的是被杀掉再重启的 Host 进程。Rust SDK 的测试改写 namespace，让 relay-aware 客户端经 HTTP 到达同一台机器上的 Host，这个测试接缝可以接受。

门禁 5 的“control watch 中断后恢复”没有单独的故障模型。Host 重启会中断 relay 到它的控制连接，重启测试覆盖这一点；Host 在线时只断控制连接，现有的接缝做不到，没有单独测，也不为它在产品里加测试后门。

### 门禁 4 的读法

门禁 4 原文里 B 的“token 过期”，指的是 B 在同一个端点上以同名重新注册。客户端的 route token 已按 ADR 0004 退役，这次重新注册对调用方透明：A 在途的调用与它的载体不受影响，B 的下一次调用到达新的注册。各接口的门禁 4 测试用 `replaced` 或 `reregistered` 参数覆盖这个情形，并断言 B 的下一次调用到达新的注册。

### 盘点

八道门禁、三条路径（持久直连 D、relay-local L、relay HTTP H）、三个接口（Python、Rust SDK、实际生成的 Node 客户端）共 72 格。门禁 7 与门禁 8 的直接 IPC 共 6 格不适用：直接 IPC 的名字范围是一个端点，不经过 relay 网格。其余 66 格都有测试覆盖，下面按门禁列出。文件的缩写：P 是 `sdk/python/tests/integration/test_persistent_client_lifecycle.py`，M 是同目录的 `test_route_identity_mesh.py`，N 是同目录的 `test_typescript_persistent_ipc.py`，NM 是同目录的 `test_typescript_route_identity_mesh.py`，R 是 `sdk/rust/tests/persistent_client_lifecycle.rs`，RM 是 `sdk/rust/tests/route_identity_mesh.rs`。Python 与 Node 的测试用参数区分路径，Rust 用测试名。

| 门禁 | Python | Rust SDK | 生成的 Node 客户端 |
| --- | --- | --- | --- |
| 1 | P `test_late_builder_uses_persistent_manager_endpoint` | R `persistent_direct_sees_later_builder_without_reconnecting`、`persistent_relay_local_sees_later_builder_without_http_fallback`、`persistent_relay_http_sees_later_builder` | N `test_generated_node_persistent_dynamic_acquire_and_identity_fences`（D）、`test_generated_node_relay_local_late_route_reuses_physical_connection`（L）、`test_generated_node_http_late_route_reuses_upstream_connection`（H） |
| 2 | P `test_route_close_remove_and_reregister_use_current_authority`、`test_real_process_same_endpoint_reregistration_reaches_current_resource`、`test_http_old_proxy_uses_current_registration_after_reacquisition`（H） | R `direct_closed_removed_and_reregistered_routes_use_current_authority`、`direct_real_process_same_endpoint_reregistration_uses_current_resource`（D）；`relay_local_closed_acquire_denies_same_endpoint_fallback_and_retains_ipc_cause`、`relay_local_removed_and_reregistered_routes_preserve_old_binding`、`relay_local_first_acquire_uses_current_registration`、`relay_local_real_process_same_endpoint_reregistration_uses_current_resource`（L）；`relay_http_closed_probe_rejects_acquire_and_preserves_entered_call`、`relay_http_removed_route_refuses_acquire_and_old_call`、`relay_http_reregistered_route_accepts_old_proxy`、`relay_http_other_acquire_does_not_change_old_proxy_target`（H） | N `test_generated_node_closed_route_keeps_dispatched_sibling_and_first_cause`、`test_generated_node_call_time_route_change_does_not_end_sibling` |
| 3 | P `test_background_idle_eviction_then_concurrent_reconnect`、`test_peer_eof_is_uncertain_and_existing_proxy_follows_new_registration` | R 三条路径的 `*_idle_eviction_retains_route_and_coalesces_concurrent_reconnects` 与 `*_peer_end_preserves_delivered_item_and_existing_proxy_follows` | N `test_generated_node_background_idle_eviction_and_concurrent_reconnect`（D、L）、`test_generated_node_http_relay_idle_eviction_preserves_route`（H）、`test_generated_node_real_peer_end_is_uncertain_without_replay` |
| 4 | P `test_other_route_refusal_preserves_inflight_held_response`（closed、removed、replaced）、`test_peer_eof_is_uncertain_and_existing_proxy_follows_new_registration` | R 三条路径的 `*_route_errors_preserve_other_route_inflight_call_and_carrier` 与 `*_peer_end_preserves_delivered_item_and_existing_proxy_follows` | N `test_generated_node_closed_route_keeps_dispatched_sibling_and_first_cause`、`test_generated_node_call_time_route_change_does_not_end_sibling`（removed、reregistered）、`test_generated_node_real_peer_end_is_uncertain_without_replay` |
| 5 | P `test_peer_eof_is_uncertain_and_existing_proxy_follows_new_registration`、`test_late_old_response_cannot_close_restarted_direct_target`（D）、`test_late_candidate_cannot_withdraw_or_close_current_owner`（L、H）、`test_explicit_http_proxy_follows_across_real_c3_relays_after_one_refusal`（H） | R `direct_peer_end_preserves_delivered_item_and_existing_proxy_follows`、`direct_existing_proxy_follows_restart_with_unavailable_relay_anchor`、`direct_late_old_result_and_terminal_preserve_restarted_target`（D）；`relay_local_peer_end_preserves_delivered_item_and_existing_proxy_follows`、`relay_aware_proxy_switches_local_http_local_through_real_c3_mesh`（L）；`relay_http_peer_end_preserves_delivered_item_and_existing_proxy_follows`、`explicit_http_proxy_follows_between_real_c3_relays_after_one_refusal`（H） | N `test_generated_node_real_peer_end_is_uncertain_without_replay`、`test_generated_node_direct_observes_instance_reacquired_at_dispatch`、`test_generated_node_direct_late_old_response_observes_its_actual_target`（D）、`test_generated_node_old_candidate_cannot_discard_restarted_relay_owner`（L、H）、`test_generated_node_new_stream_follows_restarted_host_without_replaying_old_stream`（D、L）、`test_generated_node_reacquires_route_on_another_relay`（H） |
| 6 | P `test_stateful_resource_failure_is_not_replayed`、`test_peer_eof_is_uncertain_and_existing_proxy_follows_new_registration`、`test_route_close_remove_and_reregister_use_current_authority`（L 的回退被拒） | R 三条路径的 `*_resource_error_is_terminal_without_replay_or_reconnect`、`*_known_terminals_and_items_survive_later_peer_end` 与 `*_peer_end_preserves_delivered_item_and_existing_proxy_follows`，`relay_local_closed_acquire_denies_same_endpoint_fallback_and_retains_ipc_cause`（L） | N `test_generated_node_real_peer_end_is_uncertain_without_replay`、`test_generated_node_direct_late_old_response_observes_its_actual_target`（D）、`test_generated_node_background_idle_eviction_and_concurrent_reconnect`（L）、`test_generated_node_reacquires_route_on_another_relay`（H） |
| 7（L、H） | M `test_independent_registrations_join_conflict_and_existing_proxies_follow`（relay-local 与显式 relay） | RM `real_c3_join_conflict_preserves_old_proxies_and_clears_after_withdrawal`（两种连接方式） | NM `test_generated_node_mesh_conflict_retains_old_targets_and_follows`（relay-local 与 HTTP） |
| 8（L、H） | M `test_unreachable_owner_keeps_name_until_host_withdraws`（Host 撤下与强制离开）、`test_force_leave_allows_registration_then_restored_owner_conflicts` | RM `real_c3_{local,http}_suspect_and_dead_names_recover_after_{owner_withdrawal,force_leave}` | NM `test_generated_node_mesh_unreachable_owner_retains_name_until_release`（relay-local 与 HTTP，Host 撤下与强制离开） |

块 7 补的测试：门禁 7 与门禁 8 在三个接口上的 L 与 H；门禁 2 新表述里还没有断言的几句（调用方看不到 route UID，没有在线注册时的拒绝在派发前，不进入任何资源），补在现有测试上；门禁 5 的直接 IPC 在 Python 与 Rust 上各一个新测试，证明重启之后旧连接上迟到的结果或结束不会关闭新的目标（Node 原来就有）。原来驱动生成的 TypeScript 传输的专用 Node 夹具与它的冲突测试，由经实际生成的 `ContractClient` 的门禁 7 测试完整取代，一并删除。

## 独立核对

块 1 到块 6 照执行计划第六节验收：读完差异，在整合分支加上这份差异的候选上跑受影响的测试；改到协议、relay 或 Core 客户端的块再跑一次完整检查，包括 Rust Core 与 Rust SDK、原生测试、Python 全量、可移植矩阵、Node 绑定与 TypeScript 代码生成的测试，以及 `tests/repo`；块 4 跑 Node 与 TypeScript 相关的全部测试。块 6 的差异另请一位审查者独立核对过一次，它的发现复现之后才采用，见执行计划块 6 的执行记录。

块 7 的补测只改测试，没有改产品代码。三处 CI 修正的原因都核对过。Windows x64 上那次 719 的来历是：Host A 注销之后，跟随的那次获取在 relay A 上解析到 B 的注册，再在 relay B 上探测，而 `/_probe` 按 B 自己的视图检查冲突，B 当时还没有收到 A 的撤下；所以要等参与的每个 relay 都只剩一份注册。这是网格视图的先后，不是产品缺陷：每个 relay 按自己看到的注册如实报告。补测的差异基于 `d774f992`，`33ee933b` 去掉了 Rust 网格测试里不再用的 `TcpListener` 导入，新的测试又用到它，合入提交 `e8ba4283` 补上了这一行。

在 macOS 上，带 FastDB 兄弟目录，`88847903` 上的完整检查：Rust Core 工作区 2011 个测试通过（7 个忽略），c3 91 个，Python 原生测试、Node 绑定 78 个（1 个只在 Windows 上跑）、Python 普通组 1689 个（另有 11 个只在 Windows 上跑）、可移植组 25 个、TypeScript 组 14 个与 `tests/repo` 1034 个通过，默认命名空间前后一致。`e8ba4283` 与它只差那一行导入，Rust SDK 的 77 个测试在 `e8ba4283` 上通过。

有针对性的核对：Python 与 Node 的门禁 8 强制离开用例在旧持有者撤下并重新加入之后才获取。用一个不提交的临时测试核对旧持有者仍失联时的情形：relay B 对 A 强制离开、Host B 在 B 上注册之后，A 仍持有自己的注册、仍联系不上，经 B 的 relay-aware 获取走 relay-local IPC 到达 Host B，业务调用成功，Host A 没有收到调用；连跑三次都通过。显式 relay 的获取要到持有者的通告地址上探测，这套夹具的通告地址就是被阻断的 gate，所以在这里做不到；Rust 的用例只隔断对端的联系地址，两条路径都在旧持有者失联时获取成功。

## 配对基准

执行计划要求以 `fd33e140` 为基线，在确切候选上跑 `ipc-memory-paired.yml`，一元调用的中位延迟、p95 或吞吐退化超过 5% 且超出噪声时先定位，再交维护者裁决。确切候选 `72ffc97b` 在全平台 CI 通过之后跑了[配对基准 42522592085](https://github.com/Dsssyc/c-two/actions/runs/42522592085)，四个平台都没有候选失败，流的对比行也在四个平台上跑完。下表是候选与基线之比，取各组之比的中位数，四客户端一行合并四个客户端计算；每格前一个数是默认 buddy，后一个是关闭 buddy。延迟之比小于 1、吞吐之比大于 1 表示候选更快。

| 平台 | 1 字节 p50 | 1 字节 p95 | 1 字节吞吐 | 四客户端 64 KiB p50 | 四客户端 64 KiB p95 | 4 MiB p50 | 4 MiB p95 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Linux x64 | 1.001 / 0.993 | 0.988 / 1.011 | 1.004 / 1.004 | 1.073 / 0.936 | 1.014 / 0.944 | 1.001 / 0.991 | 1.003 / 0.976 |
| Windows x64 | 0.980 / 0.993 | 1.001 / 0.996 | 1.007 / 0.997 | 0.938 / 1.004 | 0.947 / 0.982 | 0.973 / 1.001 | 0.979 / 0.989 |
| Windows ARM64 | 0.979 / 1.010 | 0.971 / 0.998 | 1.021 / 0.987 | 0.991 / 1.017 | 1.016 / 1.043 | 1.003 / 0.989 | 1.013 / 0.989 |
| macOS arm64 | 0.837 / 0.929 | 0.600 / 0.763 | 1.259 / 1.159 | 1.026 / 0.850 | 1.169 / 0.763 | 1.015 / 1.164 | 0.944 / 1.252 |

在 Linux 与两个 Windows 平台上，1 字节与 4 MiB 调用的 p50、p95 与吞吐都在 3% 以内。超过 5% 的只有 Linux 上四客户端 64 KiB 默认 buddy 的 p50（1.073）：四个客户端各自的组间比值在 0.93 到 1.23 之间，都跨过 1；同一行关闭 buddy 时候选快 6%；IPC 调用路径与确切候选相同的 `d774f992` 上，此前一次运行（[42506334610](https://github.com/Dsssyc/c-two/actions/runs/42506334610)）这一行是 0.952。这是 runner 的波动，不是退化。那次运行里 Linux `idle_small_rpc_buddy_disabled` 的吞吐低 10%，这次是 1.004，也是波动。macOS 的 runner 噪声太大，分辨不出 5%。

## 留给后续的

门禁 5 的“control watch 中断后恢复”在 Host 在线时只断控制连接的情形没有单独测，原因见上文。

补测留下的三处小问题与 Rust SDK 测试在 `/tmp` 下留下的私有根，记在[待办](../design/backlog.md)第 3 项的测试一条。

待办“要核实的事”里与这一批相关的：Windows x64 上 c2-core 的 `server_streams` 一次堆损坏退出；`unlimited_stream_uses_an_operation_slot_until_real_terminal` 一次 `Closed`；负载高时 `tests/repo/test_source_validation.py` 的三个用例超时；Windows x64 上 c2-server 的 `shutdown_preserves_active_reply_refuses_new_calls_and_fences_lifecycle_hook` 偶发读回复超时。

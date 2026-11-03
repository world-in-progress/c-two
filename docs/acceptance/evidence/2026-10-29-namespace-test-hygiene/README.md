# 默认命名空间测试清理与 lease 探测偶发失败

输入提交：`2113550e1d37e8f6ba6a1b6cfdd1a34d64ced3ca`。本次只修改测试；四个 Rust 文件的实际验证内容 SHA-256、命令、退出码与汇总见 [reproduction.json](reproduction.json)。没有修改生产代码、协议、版本号、分支或标签，也没有推送。

`final_lease_stat_io_releases_close_and_drop_leases` 的失败来自测试自己的 lease 锁探测。测试用普通 `File` 打开 lease，成功 `flock(LOCK_EX | LOCK_NB)` 后直接 `drop(File)`。并行 `fork` 会继承同一 open file description；只关闭父进程的 FD 时，子进程尚持有的副本继续保留 flock，紧接着的 reap 因而返回 `Busy`。生产 `Lease::release` 已在关闭 FD 前显式 `LOCK_UN`，这正是该产品所有者类型已经处理的边界（`core/transport/c2-local/src/unix_managed.rs:1208`）。

增加实际返回值诊断后，未改产品与目标断言的整 crate 负载先运行 101 次（首轮 32 线程，其余 16 线程）：27 次退出 0，74 次因默认命名空间的共享协调锁争用失败；目标断言未命中。四线程各 200 次短 fork/_exit/waitpid 的临时负载又运行 100 次，也没有命中。随后让 fork 子进程通过 `poll(NULL, 0, 10)` 保留继承的描述符，并在探测加锁后临时停留 1 ms 放大窗口，首次运行即得到目标断言的 `close=false: Busy`。这些 fork 负载与 1 ms 诊断停留均已移除，没有留在交付测试中。

回归把一个真实 `try_clone()` 副本保留到 reap 之后，稳定表示 fork 继承的同一 open file description。原来的 `drop(unlocked)` 立即稳定失败，实际结果为 `close=true: Busy`；改成 `drop(Lease(Some(unlocked)))` 后，通过现有所有者的显式解锁语义让副本继续打开而不再持锁，close 与 Drop 两个分支都要求 `Reaped` 并通过。相邻三个同类探测点也采用这个清理方式。原来的 EIO、socket/lease 身份与内容保留检查都保留，未增加重试、放宽断言或延长超时。原始失败段落见 [failures.log](failures.log)。

产品的残留和回收路径由当前代码确认：

- `ManagedListener::drop_bounded`（`unix_managed.rs:1360`）只尝试非阻塞协调锁。忙时保留完整 owner record，关闭 listener，并释放其 lease；这属于既有保守行为。
- 同地址重绑经 `bind_at`（`:1435`）先取得协调锁并尝试 lease 锁，再由 `remove_stale_managed_socket`（`:1080`）检查 socket 不再监听、记录属于该地址且身份匹配后移除旧 socket，复用锁住的 lease 写入新记录。缺失或不匹配的所有权不能通过此路径回收。
- `ManagedSweep::next_batch`（`:2110`）遵守条目数与时长预算，调用 `reap_slot_with_directory`（`:1836`）从记录恢复逻辑地址并验证目录与 basename，再由 `reap_locked`（`:1675`）尝试 gate/lease 锁并验证身份、incarnation 后移除 socket 与 lease。协调锁忙时返回 `Busy`，留给后续批次。这里确认的是同地址绑定的自动恢复和调用者驱动的 sweep 路径，没有新增后台机制。

夹具审查覆盖 Core、CLI 与 Rust SDK 的本地 listener 绑定及 `LocalStream::pair()` 调用。改动为：c2-local 的字节交换、重启、Drop 和子进程测试使用测试自己的临时 root，子进程接收同一 context；三个 managed 并发/忙碌测试也移到 `TestNamespace`。c2-ipc 的 `capped_ack_peer`、未协商 buddy reply 夹具使用临时 root 和同一 endpoint，accept 后显式关闭 listener 并检查结果；使用 capped peer 的测试还等待其完成信号。connect-timeout 的 `Peer` 持有临时 root，listener 与 client pool 共享 context，root 活到 stop/join/cleanup 结束。Windows 继续使用其平台用户作用域的 Named Pipe context。

确需默认目录语义的 `managed_public_namespace_lifecycle_converges_over_repeated_runs` 保留默认 context，每轮都显式 `close()` 并要求 `Reaped`。其他直接 listener 夹具（c2-ipc client/pool/control、c2-core owner-control/client-modes、c2-server）已有显式 close 检查；CLI 的原生 Unix listener 使用自建临时 root。公开的 `LocalStream::pair()` 已显式检查 listener close，保留其产品实现。c2-http 与 Rust SDK 的 Host 夹具有显式 shutdown/close 路径。本次另一次内部代理只读复核未发现阻塞问题；这是静态复核，并不替代 Host 验收。

连续三次执行同一命令，使用唯一的 `CARGO_TARGET_DIR=$TASK_TMP/target`，未设置全局 `C2_IPC_ROOT`：

```sh
cargo test --manifest-path core/Cargo.toml --workspace --exclude c2-codegen --features c2-http/relay
```

每次汇总含 45 组单元、集成及文档测试结果；完整运行前后仅通过目录枚举记录条目名，没有读取端点条目内容。

| 运行 | 退出码 | passed | failed | ignored | 秒 | socket 新增 / 删除 | coordination 新增 / 删除 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | 0 | 1895 | 0 | 5 | 92.462 | 0 / 0 | 0 / 0 |
| 2 | 0 | 1895 | 0 | 5 | 73.678 | 0 / 0 | 0 / 0 |
| 3 | 0 | 1895 | 0 | 5 | 74.834 | 0 / 0 | 0 / 0 |

逐次命令、汇总和完整前后条目名见 [run 1](workspace-run-1.json)、[run 2](workspace-run-2.json)、[run 3](workspace-run-3.json)。验证期间四个测试文件的 SHA-256 保持一致。`rustfmt --check` 和 `git diff --check` 通过。

开始诊断时默认目录已有 1 个 socket 与对应 lease。修复前的失败并发负载又留下 18 对，因此三次最终完整运行的基线为 19 个 socket、21 个 coordination 条目（19 个 lease 加 gate、marker）。每次前后都完全相同。初始与诊断后条目名及差集见 [diagnosis-namespaces.json](diagnosis-namespaces.json)。原有和诊断负载生成的这些条目均保留，没有手工 unlink、reap 或 sweep。

仍保留一个独立的压力观察：在同时编译 workspace、以 32 线程运行 c2-local 时，`managed_parallel_first_initialization_shares_one_gate_inode` 发生一次 `WouldBlock: managed endpoint namespace gate is busy`，该次汇总为 156 passed、1 failed、1 ignored。该情形未进一步归因或修复，失败完整保留；不能据此声称所有高压调度下的 c2-local 偶发都已解决。它没有在上述三次完整运行出现。

按照 Host 指定范围，c2-codegen 与跨语言矩阵留给 Host 验收；本检出缺少同级 FastDB 源码。本机验证为 macOS，本次没有运行 Linux、Windows、CLI 或 Rust SDK 独立测试套件。CLI 与 Rust SDK 本次只做了夹具代码审查，没有文件改动。

完整验证结束后立即删除整个唯一构建目录，释放约 3.17 GiB 的磁盘占用（包括二进制、rlib、incremental 与 build 缓存）；检出内没有 target 目录。详细占用和删除后状态见 [cleanup.json](cleanup.json)。证据目录只保留本次命令、汇总、失败段落和目录条目名。

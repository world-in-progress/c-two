# Windows CI 收口：`control::tests::ping_retries_until_endpoint_appears` 夹具事件化

- **日期:** 2026-09-29
- **状态:** 按 Host 审查意见重做夹具（responder setup-ready + 同一次 ping 的首次 `Exchange::Absent` 观测 + 事件驱动 bind + 1 s 探测预算 + 确定性反例）；生产 retry / timeout / admin acknowledgement 语义未改
- **基线:** `4408da580521cb1840bd9ab738b52232bc1ebdb2`
- **输入日志:** `/tmp/c2-memory-win-c666-failed.log`（windows-2025 / full 失败；windows-2022 / full 同 commit 该测试通过）
- **改动:** `core/transport/c2-ipc/src/control.rs`（生产文件内只新增 `#[cfg(test)]` seam，其余都在 `#[cfg(test)] mod tests`）、本报告
- **验证主机:** macOS（darwin，10 核），cargo/rustc 1.91.0；未在 Windows 上执行

## 结论与证据边界

CI 日志能确定的只有：windows-2025 / full 的 panic 文本是 `assertion failed: ping(&address, Duration::from_secs(1)).unwrap()`（`control.rs:285:9`），即 `ping` 在 1 s 预算内返回 `Ok(false)`（不是 `Err`）；同 job 其余控制测试 `ok`，windows-2022 / full 同 commit 该测试 `ok`。`Ok(false)` 只证明这次探测没有在预算内拿到合法 PONG，**不能**据此证明生产 retry 无错，也**不能**排除生产 retry 存在问题的可能。CI 的**原始触发条件没有被直接复现**（本机 macOS 复现不了 runner 的调度/端点创建成本），本报告不使用"本机全绿"作为生产结论。

旧夹具（HEAD）的机制缺陷是时间假设而不是重试实现：端点出现由 responder 线程内的 `sleep(50ms)` 决定；`ping` 用例把 bind 后的 `ready` 通道绑成 `_ready` 从不接收，responder 的线程 / runtime / Windows 端点命名（token、SID/SDDL、`CreateNamedPipeW`）成本可能整体落进 1 s 探测预算；responder 只 accept 一次、只回一次 PONG，一次被放弃的交换就让它永久失去应答能力。本次改动消除这些假设，而不是扩大预算。

## 本次补齐的 Host 审查点

1. **同一次 ping 的首次 `Absent` 有直接观测。** `ping` 在 `Exchange::Absent` 分支调用 `#[cfg(test)] test_seam::note_absent(&address)`；测试在启动探测前用 `watch_first_absent(address)` 注册**精确地址**的 watcher，只有该地址的 ping 会触发且只触发一次。测试收到该事件后才 bind，因此"这次 ping 观察过缺失端点"与"端点在这之后才出现"都是事件事实，不再是"先独立探测 + 随即 bind"的时间推断。
2. **responder 有明确的 setup-ready 事件。** 线程内先建 current-thread runtime、解析 `LocalEndpoint`（Windows 上含 token/SHA 成本），然后发 `ready`，再阻塞在 gate 上；测试 `wait_ready()` 之后才开始探测，夹具初始化不再计入探测时限。
3. **保留 1 s 探测预算。** 测试中的 `ping` / `shutdown` 探针都用 `PROBE_BUDGET = 1s`；`FIXTURE_BUDGET = 10s` 只用于夹具自己的事件等待（ready / bound / 首次 Absent）。这是刻意收紧：端点出现必须发生在探测预算内，不能用 10 s 掩盖。
4. **事件驱动 bind。** bind 由"该 ping 的首次 Absent"触发，完成后 `bound` 事件回到测试；bind 失败或 responder panic 报为独立夹具错误（`the responder never bound the endpoint` / `responder thread panicked`），不会伪装成探测超时。
5. **多次交换应答与丢弃首次交换保留。** responder 持续服务；`shutdown_ack_only_proves_initiation_and_retries_dropped_exchange` 的 `discard_first = true` 分支仍是丢弃首次交换后重试；ping 用例仍是"首次 Absent → 端点出现 → 至少一次后续交换应答"。dropped-exchange 没有替代 absent-to-ready 覆盖。
6. **确定性反例。** 精确地址故障注入 `test_seam::disable_absent_retry(address)` + 新测试 `ping_without_absent_retry_never_reaches_a_late_endpoint`：同一夹具、同一事件序列、同一 1 s 预算，禁用 Absent 重试后结果不是 `Ok(true)`（即正测试的 `assert!(probed, ...)` 在此必失败）。
7. **hook 是地址关联的、且只在 test 构建存在。** watcher 与注入表都以完整 `ipc://...` 地址为键，未注册地址（含同二进制其它测试）既不被观测也不被改行为。`cargo check -p c2-ipc --lib`（非 `cfg(test)`）通过。

## 命令与结果

- 绿（最终产物）：`CARGO_TARGET_DIR=/tmp/c2ctl-target CARGO_BUILD_JOBS=2 cargo test --locked --manifest-path core/Cargo.toml -p c2-ipc --lib -- control::tests` → **exit 0**，`6 passed; 0 failed; 122 filtered out`，`finished in 0.02s`；两次运行结果一致。
- 红（确定性反例，临时把反例断言换成正测试断言后单独运行，随后已还原）：同命令加 `-- --exact control::tests::ping_without_absent_retry_never_reaches_a_late_endpoint` → **exit 101**，`panicked at transport/c2-ipc/src/control.rs:509:9: ping must succeed once the endpoint appears`，`0 passed; 1 failed`，`finished in 0.00s`。还原后 `shasum -a 256 -c` 通过。
- 非 test 构建：`cargo check --locked -p c2-ipc --lib` → exit 0。
- 格式：`rustfmt --edition 2021 --check` 与 HEAD 相同，仅第 8 行既有 import 排序差异（本机 rustfmt 版本差异），新增代码无新增格式差异。
- 负载进程：`ps -eo pid,pcpu,etime,comm` 与 `pgrep -fl 'c2_ipc|c2ctl|c2-ipc|busy|spin'` 显示上一轮 60 个忙进程与测试进程均已退出，无残留；本轮未再启动任何负载进程或重复跑分。
- diff：`git diff --stat` = `control.rs` 255 insertions(+), 30 deletions(-)；`git diff | shasum -a 256` = `8fea288d25eb335b5606d86a7444a1a375db12aa148d931274d07c1531b66871`（本报告为未跟踪新增文件，不计入）。

## 尚未验证 / 需要 Host 决策的边界

1. Windows 2022 / 2025 或 windows-native `full` 未复跑。若仍红，新夹具把原因分清：夹具未就绪（`the responder never became ready` / `the responder never bound the endpoint` / `responder thread panicked` / `the probe never observed the endpoint as absent`）vs 探测预算内没成功（`ping must report absence, not a retry error` + `ping must succeed once the endpoint appears`）。
2. Windows 上"第一次 Absent 之后 bind + 一次往返"能否稳定落在剩余约 0.98 s 内没有实测；这正是 1 s 预算要诚实覆盖的部分。若某 runner 不满足，应视为 Windows 端点创建成本问题，而不是把预算调大。
3. 生产 `ping` 每次尝试 100 ms 硬上限未改（timeout 语义）：若某 runner 每次往返都超过 100 ms，`ping` 仍返回 `Ok(false)`，需 Windows 实测往返分布后单独决策。
4. 同 job `tests.rs:1201` 的两个 5 s 超时失败属另一位 Buddy 的范围，本报告不涉及，也不对其下结论。

## 保留语义

生产 `ping` / `shutdown` 循环、`exchange` 错误分类、每次尝试上限、admin acknowledgement 校验与 `decode_shutdown_ack` 路径均未改；相对 HEAD 唯一的生产代码形状变化是 `Exchange::Absent` 与 `Exchange::NoReply` 拆成两个 match 分支，其中 seam 调用与注入判断都在 `#[cfg(test)]` 内，非 test 构建下两个分支与原合并分支行为一致。未改 `client.rs` / `tests.rs`，未改 `c2-local` / `c2-config` / `c2-local-security`，未 push / merge / release，未派 peer。

Host 已审查最终固定产物 `23eaf4af-1a55-4331-bce9-178075510afa`：首次 Absent 事件确实来自待测 ping，responder setup-ready 先于该探测，1 s 探测预算保持。集成工作树的控制模块 6/6 项通过，日志 `/tmp/c2-ping-host-verification-0929.log`；真实 Windows 结果仍以随后 CI 为准。

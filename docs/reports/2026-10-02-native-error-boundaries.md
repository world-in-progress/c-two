# Native 错误与失效边界：F2 / F3 续修

日期：2026-10-02。固定基线与当前 checkout HEAD 均为 `1c78cd6e5fc49cd97126ec6f5707757cdaf99d6a`。累计修改只涉及 Host 声明的 15 个文件。上一稿输出检查点 `0f35b4380436fef480f5931538afbb34eab867fa` 未获接受；本轮依照续修输入及 `/tmp/c2-full-review-1002/independent-review.md` 修复其新增缺陷，不把之前测试通过当作接受。

本轮完成范围内修改，19 项定向 Rust 纯测试和当前源码新编扩展上的 11 项 Python 纯测试通过，均无失败、skip 或 ignore。native 构建、IPC/server 测试目标编译通过。**真实 SHM/IPC、已注册 Host 生命周期回归，以及与 Host 已导入的 owner incarnation 修复 `5292a99` 的整合审查仍待 Host 完成。** 没有重复已知权限失败、重跑无关全套、改 client close 或 backing release/try_release、改其他 worker 树、派 peer，或执行 push/PR/merge/release。

原始 Host 证据来自 `/tmp/c2-full-review-1002/runtime.md`、`coverage.md`、`probes/result.json`：响应资源已执行一次，客户端容量拒绝却收到 ClientCallResource；shutdown 的 inf/1e300 在 PyO3 实际调用中产生 PanicException。Host 提供的 baseline Python 867、当时当前 904，以及后续整合 Core 1198 / Python 918 / Node 34 全量通过，是上下文证据，不能替代本轮未覆盖交错的回归。

## 独立审查反例：旧候选红、修复绿

这里的“审查 F1/F3”是续修输入的 finding 编号；“runtime F3”仍指原始 shutdown timeout 问题。

在修复生产路径前，先添加两个反例，保持旧候选的 insert/backing admission 和首块分类行为，实际执行：

```bash
CARGO_TARGET_DIR="$PWD/target/native-error-boundaries" cargo test --offline \
  --manifest-path core/Cargo.toml -p c2-wire --lib typed_review_ -- --nocapture
```

旧候选退出 101：**0 passed、2 failed、0 ignored、136 filtered，5.01 秒**。

- `typed_review_file_carrier_callback_and_insert_do_not_deadlock` 用真实 file mmap、完成后的 carrier.with_slice、生产 insert/contains 和确定性 channel/barrier。reader 持 pool read guard；writer 到达 shard 决策之后才放行二者。子进程记录 `real file carrier callback holds pool read guard`、`production insert reached shard decision before pool admission`、`carrier callback entering production contains` 后永久等待；父进程 watchdog 在 5 秒终止子进程，并将终止明确判为失败。没有用模拟 carrier、模拟错误或超时成功掩盖死锁。
- `typed_review_oversized_first_chunk_is_protocol_before_capacity` 使用真实 reply metadata encoder/decoder，传入 size=128、chunks=1、data_len=129。零预算时旧候选实际返回 `Capacity("memory budget cell 'reassembly' rejected a 128 byte reservation: 0 of 0 bytes already in use")`，Protocol 断言失败。

修复后执行同一命令、同一 5 秒 watchdog、同一生产操作：**2 passed、0 failed、0 ignored、136 filtered，0.01 秒**。之后新增 guard/GC 交错测试后的 registry 定向目标为 8 passed；具体命令及计数见下文。未提高 timeout 或串行整套测试。

## 锁次序与原子 admission

旧候选持 shard 阻塞等待 pool.write；完成 carrier.with_slice 持 pool.read，回调调用同 shard 的 contains，形成 shard→pool 与 pool→shard 的 ABBA。

最窄修复保留 shard 决策权威，只改变取得 pool guard 的方式：

```text
锁 shard → 检查 Duplicate / geometry → pool.try_write
  成功：guard 传入 constructor → reserve / allocate → 发布 entry / counters
  竞争：释放 shard → shard 外等待 pool.write → 释放 pool guard → 重新锁 shard 决策
```

不得持 shard 阻塞等 pool，也不得带 pool guard 阻塞取得 shard；这保留已有 feed 的 shard→pool.read 次序。等待后的 retry 重查真实 key 状态，不使用 contains 快照猜测错误。soft-limit GC 仍是原来的 advisory sweep，不把它变成新的容量规则。

`ChunkAssembler::check_geometry()` 返回私有字段的 checked geometry token；`new_with_guard()` 与 `ReassemblyBacking::admit_with_guard()` 消费已经取得的 write guard，不重新取锁。backing 构造在 reserve 前验证 guard 来自传入的确切 pool Arc。reservation 与 storage allocation 仍在同一个 pool critical section 内；拒绝或分配失败的退款行为保留。public standalone constructor 先校验 geometry，再正常取得 pool guard，不保留旧的另一条 registry admission 路径。

本轮 backing 修改限于 admission 构造及其测试；release、try_release、with_slice 与现有 owner 生命周期实现没有改动。guard 的 pool Arc 相等检查不是 owner incarnation 校验的替代品。Host 合并 `5292a99` 时，需在新的 `admit_with_guard()` allocation critical section 中保留该补丁的 owner identity 捕获及后续校验；本树未复制 Host 新 helper，也不声称已验证最终整合结果。

## 权威 typed cause 与响应容量边界

保留 `ChunkAdmissionError::Protocol / Duplicate / Capacity`。geometry、单消息限制、乘积溢出在 reservation 前返回 Protocol；现存重复 key 在同 shard 决策内返回 Duplicate，不尝试 reservation。只有实际 reservation/storage 拒绝返回 Capacity。IPC 只将 Capacity 映射为 `ChunkError::Capacity`；Protocol/Duplicate 映射为 Protocol，不匹配诊断字符串。

`insert_reply()` 在容量操作前拒绝零 size、空首块及**单块 data_len > total_size**。size=128/chunks=1/data_len=129 在零预算与充足预算下都返回 Protocol，reassembly peak/rejected_allocations 均为零；合法 data_len=128 仍进入真实 admission，零预算返回 Capacity。本轮没有要求所有首块长度等于 total_size，也没有扩大多块协议。

Core 对真实响应 admission Capacity 添加已有 canonical ResourceUnavailable/702，同时保留 Transport 原始 source、DispatchUncertain、`fallback_eligible=false`。即使调用方传入 PreDispatch，响应侧错误也不能获得重试资格。规范 details 为：

```json
{"transport":"ipc","stage":"response_reassembly_admission","transport_phase":"dispatch_uncertain","fallback_eligible":"false"}
```

PyO3 沿已有 error_bytes/code/name/details 路径投影，Python 沿既有 CCError 解码路径恢复 ResourceUnavailable；不新增 SDK 分类或重试策略，不重放方法。服务器消费同一 typed cause，Capacity 为 ResourceUnavailable，Protocol/Duplicate/空首块为已有 ProtocolViolation/713；没有新增任意错误码。

## Reservation、duplicate、GC 与旧行为保护

8 项 registry 回归使用真实 MemPool、MemoryBudget、文件 mapping、codec 与 GC。纯测试明确禁用 buddy、SHM budget 为零并走 file，未尝试 shm_open。

- 零 chunks、零 size、空首块、超限 chunks、乘积溢出：Protocol、没有 mapping/charge，预算 rejection 不增长。
- 合法 geometry 的预算拒绝：Capacity，无 mapping/剩余 charge。reservation 成功、backing 拒绝：Capacity、reassembly peak 非零，返回后 charge 归零。
- 满预算重复首块：Duplicate，旧内容和 counters 不变，rejection 不增长；其他 RID 为真正 Capacity；完成及 release 后退款。
- 真正 GC 删除旧 key，另一 RID 消耗退款之后，同 key 重新 admission 为实际 Capacity，旧 contains 观察不能决定结果。
- 新确定性交错 `typed_gc_and_duplicate_are_rechecked_after_pool_contention`：持住真实 pool.write；production insert 的测试 gate 只在释放 shard 后触发。GC 删除过期 RID11，registry active 从 2 变 1，但 storage release 尚等 pool，charge 仍为 128。释放 pool 后 GC 退款至 64；另一 production insert 发布 RID33 填满预算；原等待 insert 放行、重试后得到 Duplicate，rejection=0，RID22/RID33 各保留、active=2、bytes=128，cleanup 后 charge=0。
- 真实完成 carrier 的回调重入 contains 与同 shard insert 可以共同完成；内容正确，完成 carrier 的 charge 不随 registry 移除消失，abort/release 各退款一次。

上一稿“constructor 等 pool 时 GC 必须等待 shard”的测试断言正好强制了审查 F1 的错误锁设计，因此用上述真实交错替换。保留其 GC 到期、实际退款、新 admission 可用的保护，并保留另外的 GC/fresh-capacity、duplicate/fullbudget 断言；没有修改生产语义来迎合旧错误断言。所有旧协议、cell/size/limit、数据、trim、release/drop 断言保留，5 项旧 file backing 生命周期回归亦实际通过。

新增 backing guard 测试将另一个真实 pool 的 guard 传给 constructor，验证在 reservation/allocation 之前拒绝，两个 pool 的 peak/charge/rejection 都为零。测试 gate 仅在 `cfg(test)` 下存在，按真实 production step 调度，不替换任何 admission 结果。

Core 两项测试经实际 codec→registry→IPC mapper→Core normalization：非法 128/1/129 保持 Protocol；合法单块 128/1/128 与多块 128/2/64 的 budget/backing 拒绝保持 702、DispatchUncertain、不允许 fallback、规范 wire roundtrip、guard 退款。未手工构造错误变体作为生产分类证据。

receiver 的 `encoded_invalid_reply_geometry_is_protocol_and_ping_stays_usable` 增加 raw 128/1/129，保留旧 geometry 断言；非法 RID 及时清理 pending/assembly、预算 rejection=0，然后 ping 可用，其他 RID 的合法单块及多块获得真实 Capacity。满预算 duplicate、malformed later chunk、超大 chunk 等旧 receiver 断言保留。该真实 receiver 目标已编译，动态执行交 Host。

Python 公共反例保留 reassembly=0 / backing=0 两行，服务端强制 256 KiB chunk response：canonical 702/DispatchUncertain/禁止 fallback、资源执行恰好一次、无 replay、ping/小 CRM 可用、charge/lease 归零；backing 行检查非零 reassembly peak。真实执行仍待 Host。

## Runtime F3：shutdown 参数先校验

`checked_shutdown_timeout()` 先校验有限、非负，再用 `Duration::try_from_secs_f64()` 验证可表示性；在 route_names 检查、host.take、registration clear、bridge/native shutdown 等所有状态改变之前调用，坏参数稳定 ValueError。没有扩大 hosted shutdown 各阶段时限语义。

2 项 Rust 与 11 项新扩展 PyO3 纯回归覆盖 inf、-inf、NaN、负数、微小负数、1e300、2**64，以及 0、负零、0.125、5。已注册 Host 的回归保留：逐次坏参数后 identity/registration 不变，原资源可通过真实 IPC 调用，hook 未执行；正常 cc.shutdown 后 hook 恰好一次，再 shutdown 不重复。该真实 Host 测试仍待 Host 动态验收。

## 当前源码验证收据

以下是本轮实际执行，Rust 合计 19 passed、0 failed、0 ignored；Python 11 passed、0 failed、0 skipped。仅运行新增和受影响的目标：

```bash
CARGO_TARGET_DIR="$PWD/target/native-error-boundaries" cargo test --offline \
  --manifest-path core/Cargo.toml -p c2-wire --lib typed_ -- --nocapture
# 8 passed，0 failed，0 ignored，131 filtered
CARGO_TARGET_DIR="$PWD/target/native-error-boundaries" cargo test --offline \
  --manifest-path core/Cargo.toml -p c2-wire --lib \
  admission_guard_must_belong_to_carrier_pool_before_charging -- --nocapture
# 1 passed，0 failed，0 ignored，138 filtered
CARGO_TARGET_DIR="$PWD/target/native-error-boundaries" cargo test --offline \
  --manifest-path core/Cargo.toml -p c2-wire --lib file_back -- --nocapture
# 5 passed，0 failed，0 ignored，134 filtered
CARGO_TARGET_DIR="$PWD/target/native-error-boundaries-core-continuation" cargo test --offline \
  --manifest-path core/Cargo.toml -p c2-core --lib native_boundary_tests -- --nocapture
# 2 passed，0 failed，0 ignored，34 filtered
CARGO_TARGET_DIR="$PWD/target/native-error-boundaries-core-continuation" cargo test --offline \
  --manifest-path core/Cargo.toml -p c2-ipc --lib \
  response_admission_is_dispatch_uncertain_and_not_retry_safe -- --nocapture
# 1 passed，0 failed，0 ignored，131 filtered；整个 IPC lib 测试目标编译
CARGO_TARGET_DIR="$PWD/target/native-error-boundaries-server-continuation" cargo test --offline \
  --manifest-path core/Cargo.toml -p c2-server --lib --no-run
# exit 0，仅编译，未执行 SHM/IPC 用例
FASTDB_PAYLOAD_LINK_MODE=system \
FASTDB_PAYLOAD_SYSTEM_LIB_DIR=/tmp/c2-memory-fastdb-sdk/lib \
CARGO_TARGET_DIR="$PWD/target/native-error-boundaries-python" cargo build --offline \
  --manifest-path sdk/python/native/Cargo.toml
# exit 0，6.64 秒；mem_ffi.rs 之前 E0277 阻塞已修正
FASTDB_PAYLOAD_LINK_MODE=system \
FASTDB_PAYLOAD_SYSTEM_LIB_DIR=/tmp/c2-memory-fastdb-sdk/lib \
DYLD_LIBRARY_PATH=/tmp/c2-memory-fastdb-sdk/lib \
CARGO_TARGET_DIR="$PWD/target/native-error-boundaries-python" cargo test --offline \
  --manifest-path sdk/python/native/Cargo.toml shutdown_timeout_tests -- --nocapture
# 2 passed，0 failed，0 ignored，0 filtered
```

本轮获授权后，仅将 mem_ffi.rs 唯一 constructor 投影改为 `.map_err(|error| PyRuntimeError::new_err(error.to_string()))?;`，保留低层 RuntimeError/诊断行为。构建使用只读已有 `/tmp/c2-memory-fastdb-sdk/lib` 的命令级 system link，避免重复只读 FastDB Cargo cache 的已知 CMake 写入失败；没有修改全局配置或发布依赖，此构建不是包分发证据。

将新编 `target/native-error-boundaries-python/debug/libc2_ffi.dylib` 复制到 checkout 内的 ignored Python 验证 staging，然后执行：

```bash
cp target/native-error-boundaries-python/debug/libc2_ffi.dylib \
  target/native-error-boundaries-python/python/c_two/_native.so
PYTHONDONTWRITEBYTECODE=1 PYTEST_DISABLE_PLUGIN_AUTOLOAD=1 C2_RELAY_ANCHOR_ADDRESS= \
DYLD_LIBRARY_PATH=/tmp/c2-memory-fastdb-sdk/lib \
PYTHONPATH="$PWD/target/native-error-boundaries-python/python:/Users/soku/Desktop/codespace/WorldInProgress/c-two/.venv/lib/python3.13/site-packages" \
python3 -m pytest --confcutdir=sdk/python/tests/unit \
  sdk/python/tests/unit/test_runtime_session.py \
  -k 'native_shutdown_rejects_invalid_timeout or native_shutdown_accepts_zero_and_normal_timeout' \
  -q --timeout=30 -p pytest_timeout -p no:cacheprovider
# 11 passed，35 deselected，0 skipped，0.38 秒
```

只读取既有 pytest/包 metadata 依赖。独立 import 核对 c_two 和 _native 均从本 checkout 的 staging 加载；未用旧 extension 冒充本轮 native 验证。dylib 与复制的 _native.so SHA256 相同：`88e768017732b6900e7348dfd63b07609a93b8c2f8b75c61843b0dd12599711e`。

代码输入（14 个累计源码/测试文件，不含本报告）指纹：`efb0bedfbb0edd5965d49b179119c790dc3991601d6ffcf60076acd37b8e48f7`。计算方式为按相对路径排序，对每个文件生成 `path + NUL + SHA256(file) + LF`，再取整体 SHA256。Host 的最终整合 SHA 应另行记录，不能沿用本指纹。

两份 Python 回归的 Python 3.10 AST 检查、git diff --check、15 文件 scope/固定 HEAD 核对通过。九份 Rust 文件使用 `rustfmt --edition 2024 --config skip_children=true --check` 通过；server.rs、mem_ffi.rs、backing.rs 保留范围外现有格式，backing diff 核对仅 admission/import/新增 admission 测试，无 release 变化。首次 rustfmt 递归检查提示 backing 两处原有格式，未为了格式检查扩大修改。

## Host 精确后续命令与验收

请先合并本补丁，保留 Host 的 `5292a99` owner identity 捕获/校验及另一 worker 的 close/release 修改，检查新的 guard constructor 是否完整承接 owner incarnation；然后在有 SHM/IPC 权限且固定 FastDB 输入已准备的 Host 环境执行下列定向命令，记录实际源码 SHA、构建来源、完整日志与 pass/fail/skip/ignore 计数：

```bash
uv sync --reinstall-package c-two
C2_RELAY_ANCHOR_ADDRESS= uv run pytest \
  sdk/python/tests/integration/test_memory_capacity_errors.py \
  sdk/python/tests/unit/test_runtime_session.py -q --timeout=30 -rs
cargo test --manifest-path core/Cargo.toml -p c2-wire --lib typed_ -- --nocapture
cargo test --manifest-path core/Cargo.toml -p c2-wire --lib \
  admission_guard_must_belong_to_carrier_pool_before_charging -- --nocapture
cargo test --manifest-path core/Cargo.toml -p c2-wire --lib file_back -- --nocapture
cargo test --manifest-path core/Cargo.toml -p c2-ipc --lib \
  chunk_reply_admission_tests -- --nocapture
cargo test --manifest-path core/Cargo.toml -p c2-server --lib \
  chunked_request_admission_failure_writes_correlated_error_reply -- --nocapture
cargo test --manifest-path core/Cargo.toml -p c2-core --lib native_boundary_tests -- --nocapture
cargo test --manifest-path core/Cargo.toml -p c2-ipc --lib \
  response_admission_is_dispatch_uncertain_and_not_retry_safe -- --nocapture
PYO3_PYTHON="$PWD/.venv/bin/python" cargo test \
  --manifest-path sdk/python/native/Cargo.toml shutdown_timeout_tests -- --nocapture
```

验收须同时满足：watchdog 子进程正常完成，carrier 内容/预算生命周期保留；无 shard→阻塞 pool 或 pool→阻塞 shard 新倒置；GC/duplicate/fullbudget 原子决策与退款断言通过；非法 raw 128/1/129 始终 Protocol，合法容量拒绝 canonical 702/DispatchUncertain/无重放且资源恰好一次、其他 RID/ping 可用、guard 清理；坏 timeout 后真实 Host 仍可调用、正常 shutdown hook 恰好一次；Host owner incarnation 保护完整保留。将最终整合与真实执行收据追加本报告。当前不宣称最终整合或真实 SDK/IPC 验收完成。

## Host 整合定向验证（2026-10-03）

Host 将本稿与 carrier 身份修复和关闭修复合成：admit_with_guard 中 allocation 与 owner incarnation 捕获共用同一 guard，release_storage_with_pool 中先核对 incarnation/authority，再释放与退款。合并前定向 gate 为 wire145、真实 IPC reply6、Core2；合并关闭修复后完整 wire149、IPC151、Core边界2 均通过。收据 /tmp/c2-full-review-1002/admission-integrated-result.json 与 close-integrated-result.json。最终 native/完整 facade 回归和 Windows 验证将绑定后续源码提交，当前不声明全部交付。

## Host 最终验收（2026-10-03）

固定实现 `15578b5`：Core all-features 1211、Rust SDK 10、native 2、Python 定向 53 / 完整 918 项及三个原始故障探针通过，严格 18/12 收据有效。Windows 验证源码 `2d360875` 的 2022/2025 各 23 项完整门禁、两套基础门禁及不可变 ZIP/内部 hash/两种 wheel 消费清理/c3 字节核验全部通过；与本机仅有验证 workflow 触发条件差异。最新 sealed artifact 已在 Host 真实合成树验收，细节与边界见 [总报告](2026-10-03-memory-audit-validation.md) 和精简证据 JSON。正式发布与 PR 不由这些测试自动授权。

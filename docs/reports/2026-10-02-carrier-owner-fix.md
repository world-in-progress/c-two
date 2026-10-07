# Carrier owner 修复与 abort 旧保护恢复

日期：2026-10-02。输入 HEAD：`1c78cd6e5fc49cd97126ec6f5707757cdaf99d6a`。
只修改 `core/foundation/c2-mem/src/pool.rs`、`core/protocol/c2-wire/src/chunk/backing.rs`、`core/protocol/c2-wire/src/assembler.rs` 及本报告。未 push、操作 PR、merge、release，也未修改 Host、原始 checkout、其他 worker 或全局设置。

## 问题与证据来源

本轮处理 `/tmp/c2-full-review-1002/runtime.md` 的 F4 和 `coverage.md` 的 F1。Host 已在真实环境执行 `/tmp/c2-full-review-1002/probes/carrier-incarnation.py`，其日志与 `probes/result.json` 记录输入 HEAD 的失败：

```text
old_copy=Ok(Some(34)); old_release=Ok(()); replacement_allocs=0
F4: old carrier read replacement bytes
```

旧、新 pool 的首个 buddy 坐标均为 seg0/gen1/offset0。旧 carrier 写入 `0x11`，替换池写入 `0x22`，随后旧 carrier 读取了替换池的数据并释放了替换池的 allocation。`Arc<RwLock<MemPool>>` 只持有可变容器，不能单独证明其内实体仍是原 owner。空 replacement 的写入还会落入 `handle_slice_mut()` 的 `expect`，违反 `write_at()` 的 Result 错误界面。

上述 red 证据由 Host 提供；本轮遵守不重复已知 SHM 权限失败的约束，未重新执行真实反例。Host 的 baseline Python 867 项和当前 904 项均零 skip 通过是已有 suite 的覆盖背景，不是本轮修复的 green 证据。

## 修改及错误语义

carrier 在 admission 的同一个 pool 写锁临界区，捕获 allocation owner 的完整 prefix（含构造器生成的 PID/UUID incarnation）。新增 `MemPool::validate_owner_incarnation()` 是非破坏检查：它只比较身份和 owner/peer 状态，不打开映射、不读 payload、不分配、不释放、不改变预算；没有改变已有 MemPool allocation、handle 或 free API，也没有新增 unsafe API。

SHM carrier 的 `write_at()`、`copy_bytes()`、`with_slice()` 和 `release()` 均先检查原 owner incarnation 与 authority，再检查 handle 的 generation/跨度。校验与实际操作共用同一次 pool 锁，不留实体替换窗口。`write_at()` 显式传播 handle 校验失败，避免空 replacement 经由 slice accessor panic。不同 owner 返回 `pool owner incarnation mismatch`；即使 prefix 相同，peer cache 也返回 `peer pool is not an owner authority`。carrier 添加操作上下文；release 仍含 `validation`，保留既有错误断言。这里使用现有低层 `Result<_, String>`，没有改动 RPC canonical 错误分类。

失败的显式 release 不取走 state、不退款、不 free replacement，重复失败仍可重试。调用者保留并恢复原 MemPool 实体后，carrier 可以重新读取原内容、正确 free 原 allocation，并在 storage release 成功后退款一次。Drop 复用同一路径；若 owner 已永久丢失或无法验证，仍按原合同保留 reservation，不用强行退款隐藏失去 owner。这个异常 Drop 不能承诺最终退款。本修复提供身份栅栏，不另建不可替换的 owner 持有抽象；若未来要求永久替换也自动释放原池，需要另行设计所有权模型。

FileSpill 的 mmap 和 file-backing guard 由 handle 自持有，不通过 pool 坐标寻找 storage，因此不要求容器内仍是原 pool。它继续验证自身映射跨度，释放顺序仍是关闭 handle 再退款。`trim_to()`、长度、tier 和容量查询仅改变或观察 carrier 元数据，不解引用池内 allocation；异常 owner 时仍保留这些诊断能力。本边界不承诺让已故意取出的原始指针随 owner 替换机械失效。

## 回归保护

保留原有失败 release、幂等 release、assembler charge 和其他旧用例断言；只增补保护。新增测试直接调用实际 carrier 与 MemPool：

| 测试 | 必须捕获的行为 |
| --- | --- |
| `colliding_buddy_replacement_cannot_read_write_or_release` | 同标签、不同 incarnation 的真实 buddy 坐标碰撞；旧内容 `0x11`、新内容 `0x22`；拒绝所有 storage 操作；重复 release 失败仍留 charge；新 allocation/count/bytes 不变；恢复原 owner 后正确 release |
| `colliding_dedicated_replacement_cannot_read_write_or_release` | dedicated 首坐标碰撞下同样的 owner、字节、计费、重试保护 |
| `empty_replacement_write_returns_error_and_restored_owner_can_release` | 空 replacement 写入必须 Err；原内容与 charge 保留，恢复 owner 后可释放 |
| `peer_with_same_incarnation_cannot_replace_owner_authority` | 真正打开原 buddy 的 peer，prefix 和 handle 坐标校验均有效，仍不能取代 carrier 的 owner authority |
| `drop_with_colliding_replacement_does_not_free_or_refund` | Drop 不误 free replacement、不提前退款；detached owner 仍有原 allocation |
| `self_owned_file_survives_pool_replacement_and_releases_its_charge` | 零 SHM tiers 的真实文件路径在 pool 替换后可读写、trim、release；保留全容量 charge，最终文件与预算释放；写跨度错误受控 |
| `owner_incarnation_validation_is_non_destructive` | 无映射的 owner/replacement/同 prefix peer 检查及 owner 移动后身份不变 |

`test_abort_releases_handle_and_charge` 保留原来 4×4096 字节 admission 及 reassembly budget 归零断言，并保留 `pool.clone()`。配置只允许一个 64 KiB buddy，保留该唯一映射，dedicated 上限为 0、文件预算为 0、禁用压力启发式。在实际 assembler 的 buddy allocation 上先断 alloc_count 为 1，abort 后断为 0；再从同池申请完整 64 KiB，要求 buddy 成功且 expansion 计数不变、dedicated/file allocation 计数仍为 0。最后 release 新 handle 再断 allocation 和 reassembly charge 为 0。此测试能发现仅退款却未释放原 block，不能由扩容或 fallback 掩盖。

## 本轮实际验证

所有构建输出放在 checkout 内忽略的 `target/carrier-owner-check`，使用 offline Cargo，未运行无关全套或提高 timeout。先新增碰撞/abort 回归，完成旧实现下的 `c2-wire --no-run` 编译；真实 red 执行使用 Host 已有证据，未声称本轮完成 SHM red→green。

以下命令均执行并检查输出，exit 0：

```sh
cargo test --offline --manifest-path core/Cargo.toml -p c2-wire -p c2-mem \
  --no-run --target-dir target/carrier-owner-check
cargo test --offline --manifest-path core/Cargo.toml -p c2-mem \
  pool::tests::owner_incarnation_validation_is_non_destructive \
  --target-dir target/carrier-owner-check -- --exact
target/carrier-owner-check/debug/deps/c2_wire-66c991babd1e1cbb file_
target/carrier-owner-check/debug/deps/c2_wire-66c991babd1e1cbb \
  --exact assembler::tests::budget_rejection_leaves_no_mapping_and_reports_cell_and_size
target/carrier-owner-check/debug/deps/c2_wire-66c991babd1e1cbb \
  --exact assembler::tests::test_zero_chunk_size_rejected_before_charge_or_allocation
target/carrier-owner-check/debug/deps/c2_wire-66c991babd1e1cbb \
  --exact assembler::tests::test_zero_chunks_rejected
rustfmt --check --edition 2024 core/foundation/c2-mem/src/pool.rs \
  core/protocol/c2-wire/src/chunk/backing.rs core/protocol/c2-wire/src/assembler.rs
git diff --check
```

共 **10 个不同的相关纯/文件测试通过，0 failed、0 ignored**：owner helper 1 项，文件 carrier/registry 6 项，admission 3 项。文件测试包含真实 mmap、内容、finish/abort/GC、held charge 及 cleanup。二进制名只标识本轮本机编译产物，不要求 Host 复用。初次 helper 过滤误写成 `tests::... --exact`，输出 0 tests；已纠正为上面的 `pool::tests::...` 并实际运行 1 项，未把零测试结果算作通过。完整测试编译已验证，真实 SHM 用例尚未运行。

## Host 接手的精确命令与验收

请在有 SHM 权限的修复源码 checkout 执行，保留实际 source SHA、补丁 SHA256、命令、测试数、exit 与完整日志。以下不要求重跑完整 Python suite：

```sh
cargo test --offline --manifest-path core/Cargo.toml -p c2-wire
cargo test --offline --manifest-path core/Cargo.toml -p c2-mem \
  pool::tests::owner_incarnation_validation_is_non_destructive -- --exact
python3 /tmp/c2-full-review-1002/probes/carrier-incarnation.py
```

期望本修复单独应用时：c2-wire **136 项**通过（原 130 项加 6 项，含严格 abort 和旧 release 用例），helper 实际运行 **1 项**；原 Host probe exit 0，打印 `old_copy=Err(...)`、`old_release=Err(...)`、`replacement_allocs=1`。不接受 0 tests、skip、权限失败或历史 suite 回执代替真实执行。若 Host 已整合其他补丁导致测试总数变化，需记录差异并确认上述所有命名用例实际执行。

对新增回归的敏感性，还需在 Host 独立的临时源码副本中各验证一次、随后恢复修复源码：

1. 暂时将 `validate_owner_incarnation()` 改成无条件 Ok，以下命令必须因读取 replacement、错误 Drop 或接受 peer authority 而非权限错误失败：

   ```sh
   cargo test --offline --manifest-path core/Cargo.toml -p c2-wire \
     chunk::backing::tests::colliding_buddy_replacement_cannot_read_write_or_release -- --exact
   cargo test --offline --manifest-path core/Cargo.toml -p c2-wire \
     chunk::backing::tests::peer_with_same_incarnation_cannot_replace_owner_authority -- --exact
   cargo test --offline --manifest-path core/Cargo.toml -p c2-wire \
     chunk::backing::tests::drop_with_colliding_replacement_does_not_free_or_refund -- --exact
   ```

2. 在临时副本中仅省略 `release_storage()` 的 buddy `free_at()`，仍保留 state take 与 reservation 退款，以下命令必须在 abort 后 `alloc_count == 0` 断言失败，证明恢复的旧保护能抓仅退款未释放：

   ```sh
   cargo test --offline --manifest-path core/Cargo.toml -p c2-wire \
     assembler::tests::test_abort_releases_handle_and_charge -- --exact
   ```

这些 mutation 命令是待 Host 执行的验收条件，不是本轮已运行的证据；不得将 mutation 保留或整合。建议固定回执 `/tmp/c2-carrier-owner-host-validation.json` 与日志 `/tmp/c2-carrier-owner-host-validation.log`。回执应证明所有实际操作、原 owner 恢复、失败 release 预算留存、严格 abort 重分配及 mutation red 条件，之后由 Host 做最终审查。本轮停在实现可审查、纯验证已完成、真实验证与最终审查待接手的状态。

## Host 实际验收

Host 审阅固定产物 `e5a99680-b170-4450-80ef-05f6433d5634` / `88fc42016c439210d32b5ccb172fe9afc9902fca` 的完整实现 diff 后，在主整合树运行真实 SHM 验证。`c2-mem` 155 项、`c2-wire` 136 项及 1 项文档测试全部通过，0 ignored；原公开 carrier 反例现在 exit 0，旧 carrier 读与释放均受控拒绝，replacement allocation 保留为 1。日志位于 `/tmp/c2-full-review-1002/carrier-mem-wire.log` 和 `carrier-public-probe.log`，机器收据为 `native-repo-result.json`。

在专属 `c2-close-audit` 探针工作树完成上述 4 个 mutation：去掉 owner 校验分别使 buddy 碰撞、peer authority、Drop 保护失败；只退款不 free 使严格 abort 测试在 allocation count 断言失败。4 次均为预期的单项断言失败（exit 101），没有编译或权限错误。源码在 finally 中逐字恢复，mutation 未导入整合树。机器收据 `/tmp/c2-full-review-1002/carrier-mutation-fixture-result.json` 记录命令、结果及日志。此验收只关闭 carrier 与 abort 覆盖问题，不代表本次完整内存改动已交付。

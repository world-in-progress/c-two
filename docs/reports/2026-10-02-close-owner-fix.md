# 关闭所有权修复（2026-10-02）

基线 `1c78cd6e5fc49cd97126ec6f5707757cdaf99d6a`，HEAD 未变；本报告对应 allocated checkout 内的未提交累计差异。续办输入 `c4f628f01b1babd249975e3e40e550bb34b84937`、独立审查 `/tmp/c2-full-review-1002/independent-review.md` F2；assembler 最小接口已获授权并接入。

**本地 file 反例已红转绿，候选等待 Host 整合及真实 IPC/SHM 验收。** 没有端到端硬实时或 OS 调度承诺，没有提高原 timeout、删除失败断言、串行整套测试或重跑无关全套。

## 行为与所有权

- 前轮 F1 请求池 detach 保留：关闭确认后去掉 transport-owned slot 的引用，不本地 free/reuse 已可能发布的 buddy 块；真实 RequestBlock、retire job、carrier 保有确切池和 charge；injected pool 不接管。同预算新连接使用新 incarnation。partial-frame 原断言及新增“保留关闭 client、真实 owner 消失后预算归零、同预算重新 acquire”断言保留。
- backing 的阻塞 release 与 try-release 共用 `release_storage_with_pool`，在同一 write guard 内验证、释放，真实 storage release 后才退款。assembler 只新增委托的 `try_release` 和相邻回归；未改 constructor/admission/feed/finish。
- registry try-cleanup/try-abort/try-GC 只尝试 shard/pool 锁。争用或错误时原 assembler、reservation、generation、entry 和 counters 原地保留；只有实际释放成功才移除。没有“探锁后解锁再同步 Drop”的竞态。receiver cleanup guard 和 close retry 使用同一 registry/conn_id；finished carrier 独立于这些 entries。
- receiver footer、正常 completion、发送失败/取消后的 request settlement 共用非阻塞 pending 清理。已 dispatch token 的自动 Drop 不再强行等待池锁；pending 保留最后的 release state/permit。dedicated 成功结算后仍由原有 bounded retire executor 等待真实 read_done，未改变两个线程、256 个 permit 的上限。
- 无人接收的 response 整体保留在 pending；最多暂存 receiver 当前一帧，释放忙时以异步等待暂停下一帧，不新增后台 worker 或无界清理队列。close 可取消该等待，原 entry 继续持有 carrier。新调用跳过已占用 RID，避免覆盖清理权威。maintenance 只结算已结束/取消的 waiter，活跃 waiter 不动。
- maintenance 对 shard、registry pool、request pool、pending 及其状态槽使用非阻塞尝试，忙则跳过该 tick。close 对无法完成的清理及时 false，保留重试状态并禁止提前 reconnect；成功后才 detach。

ClientPool 的既有 retired ticket 在 false 时保留 client Arc（UnconfirmedIdle），故上述 registry/pending 仍由现有 close owner 持有；无需新增清理线程。普通用户 owner 的显式 release/最终 RAII Drop 仍使用既有同步释放语义，不从本轮测试推导 OS unmap/unlink 的时间上限。

## 本轮实际红绿证据

所有 Cargo 命令在本 checkout，使用 `CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR="$PWD/core/target" C2_ENV_FILE= C2_RELAY_ANCHOR_ADDRESS=`，加 `--offline --locked --manifest-path core/Cargo.toml`；测试线程未改为串行。

| 命令 | 实际结果 |
| --- | --- |
| 修复前 `-p c2-ipc --lib client::tests::close_aborted_receiver_file_cleanup_is_retryable -- --exact --nocapture` | exit 101：50ms close 耗时 **1.011628792s**，confirmed=false；未完成 assembly 已同步释放，file/reassembly 各仅 8192B 留存，200ms elapsed 断言失败 |
| 修复后 `-p c2-ipc --lib client::tests::close_ -- --nocapture` | **16 passed，0 failed/ignored**；最新同一反例 **1.454375ms、false、receiver_terminal=true、active=1、file/reassembly 各 24576B**。解锁后 retry true，finished carrier 内容有效，显式 release 后失效及最终预算归零 |
| `-p c2-wire --lib file -- --nocapture` | **9 passed，0 failed/ignored**；包括 assembler retry、真实 carrier callback 下的 registry cleanup、busy shard/pool 的 GC 重试，以及原有 file finish/abort/hold/error/admission 保护 |
| `-p c2-ipc --lib client::tests::bounded_close_keeps_stalled_maintenance_join_reachable_for_retry -- --exact --nocapture` | **1 passed，0 failed/ignored**，原未终态句柄及重试断言保留 |
| IPC lib tests 编译（no-run，后续 close 组重新编译） | 成功；macOS 当前 **149** 项均编译，未运行全 149 项 |
| `cargo check -p c2-wire -p c2-ipc -p c2-core` | exit 0，无编译警告 |
| 范围/HEAD/旧代码与测试断言校验、增量 rustfmt 检查、`git diff --check` | exit 0；未触碰 admission 和旧测试，backing admission/access 原文保留；只格式化新增 wire 片段，保留其他 worker 的边界 |

共 26 项不同的纯/file 用例实际通过。16 项包含 cancelled/unknown response 的完整 carrier 保留、RID 不覆盖、maintenance callback/请求池争用、关闭锁与取消 closer 的旧保护。真实 LocalStream + recv_loop 对照和新的 dedicated completion/footer/read_done 用例仅编译；没有重复已知 shm_open/IPC 权限失败。

续办输入中的 Core 1198、Python 918、partial-buddy 历史记录未在本轮重跑，也不代替本补丁验收。Python lifetime 文件和原断言未改。

## 范围与合并边界

仅改冻结范围中的 assembler/backing/registry、client/sync_client/tests 和本报告；不改 Host、原始树、其他 worker、全局设置、budget 或 c2-local。registry 只新增 cleanup/drain 与测试；原 insert/admission 内容保留。Host 最新 carrier incarnation 验证未复制到本 checkout；合并时必须把它保留在 `release_storage_with_pool` 的同一 guard 内，不能先采样再释放。其他 worker 的 F1/F3 admission 修复由 Host 整合。

原 partial-frame、dedicated-retire、held、selection/close 断言及 timeout 未删除或放宽。未执行 push、PR、merge、release，未派 peer。源码范围、未触碰的 admission/旧测试、格式和 diff 检查由本轮最终检查确认。

## Host 精确验收

先在整合后的真实环境运行两个新关键对照，再跑相关 component 的新旧保护；不重跑无关完整 Core/Python 套件：

```bash
C2_ENV_FILE= C2_RELAY_ANCHOR_ADDRESS= cargo test --locked --manifest-path core/Cargo.toml -p c2-ipc --lib client::tests::owner_aborted_real_receiver_file_cleanup_is_retryable -- --exact --nocapture
C2_ENV_FILE= C2_RELAY_ANCHOR_ADDRESS= cargo test --locked --manifest-path core/Cargo.toml -p c2-ipc --lib client::tests::owner_receiver_request_completion_and_footer_are_retryable -- --exact --nocapture
C2_ENV_FILE= C2_RELAY_ANCHOR_ADDRESS= cargo test --locked --manifest-path core/Cargo.toml -p c2-wire --lib
C2_ENV_FILE= C2_RELAY_ANCHOR_ADDRESS= cargo test --locked --manifest-path core/Cargo.toml -p c2-ipc --lib
uv sync --reinstall-package c-two
C2_ENV_FILE= C2_RELAY_ANCHOR_ADDRESS= PYTHONPATH="$PWD/sdk/python/src" uv run python /tmp/c2-full-review-1002/probes/partial-buddy.py
C2_ENV_FILE= C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/integration/test_memory_budget_lifetime.py -q --timeout=30 -rs
```

记录实际合成源码/扩展来源、具名测试数、exit code、elapsed/confirmed 和预算。验收必须同时满足：持锁时及时 false、receiver 已终态、未完成 carrier/request 仍有权威与 charge；解锁重试 true；held 内容不失效，实际 release 后退款；dedicated peer 尚未 read_done 时仍可读且 charge 保留，read_done 后才归零；原 12 字节 partial-buddy 探针资源执行 0、无 held、关闭且保留 proxy 不占孤立预算；lifetime 原 5 个参数展开用例零 skip。不能仅检查 false、仅观察 JoinHandle 或删掉预算检查。

Host 负责整合和最终审查。请将上述真实验收记录写入本报告或 `/tmp/c2-close-owner-host-1002/result.json`；当前本地证据不充当接受或发布批准。

## Host 整合定向验证（2026-10-03）

Host 已审阅合成后的释放、pending、receiver 与 maintenance 路径，保留同一 guard 内的 incarnation 校验和类型化 admission。完整 c2-wire 149 项、c2-ipc 151 项（含真实 LocalStream/SHM/dedicated completion/footer 对照）及 Core 错误边界 2 项已在真实环境通过，0 ignored。机器收据 /tmp/c2-full-review-1002/close-integrated-result.json。新 try-release 也补入 owner/peer 替换回归，要求错误而非提前退款。最终 native/SDK/完整 Python 与 Windows 门禁仍待后续固定提交验证。

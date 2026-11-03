# R17c：末次 lease stat I/O 原因保留验收

R17c 在实现候选 `1e74b4c9463071ae6162619fc20eca4f55634b75` 上验收完成，固定基线为 `f4ab1c7bf9e629a3d2e22df236d0ed7285c12670`。本子项只调整 Unix `unlink_verified` 的末次 lease stat 错误分类：真实 I/O 错误进入既有 `IoError`，对象身份不符仍拒绝删除。两次目录 pin 短路、socket 后续核验与删除顺序保持。

基线验证在旧分类下得到 2 个目标反例失败、2 个控制通过。候选隔离运行中，4 个新增场景和 10 个相关既有用例通过。Linux 与 macOS 完整 Core 测试日志分别包含以下 4 个新增场景并均报告通过：

- `final_lease_stat_rollback_keeps_initialization_error`
- `final_lease_stat_short_circuits_directory_and_keeps_ownership_denials`
- `final_lease_stat_io_releases_close_and_drop_leases`
- `final_lease_stat_io_preserves_stale_bind_reap_and_sweep`

同源 [Source Validation run 40462218729](https://github.com/Dsssyc/c-two/actions/runs/40462218729) attempt 1 的 run 状态为 `completed/success`，`.github/workflows/ci.yml`；run/source/workflow SHA 均为 `1e74b4c9463071ae6162619fc20eca4f55634b75`。10/10 jobs 均为 completed/success。六条平台验证线的 receipt 均核对 C-Two 与 FastDB 的 expected/actual SHA 一致且 verified；Linux x64、macOS arm64、Windows 2022 x64、Windows 2025 x64 和 Windows 11 ARM64 的 full scope 分别通过 25、25、27、27、27 个 gate，共 131 个。Python 3.14t 独立 scope 再通过 16 个 gate，合计 147 个适用 gate。完整 relay、Rust/Python/Node 跨语言路径、Windows 标准用户消费者和平台专属检查均包含在这些通过的门禁中。两个聚合 test jobs 也均通过。精简后的字段核对见[机器证据](evidence/2026-10-27-r17c-final-lease-stat-acceptance.json)。

生产差异为 +7/−1，净 +6 行；新增测试场景 241 行及既有 `cfg(test)` 故障注入点 14 行，测试合计 +255 行，非文档净增 261 行。无性能提升声明。本轮记录的 14 个隔离端点目录均已消失，baseline 与 candidate 专属 Cargo target 均已删除；这些记录只证明本轮自建隔离资源清理完成，不代表历史默认 IPC 目录为空。独立只读审查未发现阻塞问题。

本批未发版。专用 fork CI 分支已按候选 SHA 校验后删除；本地分支与标签未改动。R17c 仅验收末次 lease stat；`gate_entry_is_current`、`gate_still_named`、目录 pin 分类、`NamespaceDirectories::same_as`、sweep 目录失效与 owner context 映射仍单列，R17 整体保持部分完成。

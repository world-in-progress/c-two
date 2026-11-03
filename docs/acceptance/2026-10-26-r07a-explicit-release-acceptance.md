# R07a 显式失败释放所有权验收

2026-10-26，R07a 按[批准边界](2026-10-26-r07-failed-release-ownership-evaluation.md)完成。基线 `0e7db94e2b63d71196e2c1f64ce69efd5db732b9`；主体实现 `d18028062fbe52f0392cb32615d8e01cdac4c081`，最终实现与测试来源 `b059c02dd78729e38e9c375612580af72c30374f`。本记录及后续文档提交不作为构建来源，未 bump 版本或发版。完整 R07 保持部分完成；Core/Held、consuming 和最终 Drop 的接管责任继续单列。

`MemPool::release_handle` 借用原句柄完成校验与释放，成功才消费，错误返回包含原 `MemHandle` 和原因的 `ReleaseHandleError`。`SyncClient::pool_alloc_and_write/fill` 返回 `RequestPreparationError`，只私有持有清理失败的原 `RequestBlock`；可查询主错误、首次清理错误及待清理状态，也可显式清理，不能取出或发布半填充块，不重放 fill。data_ptr 回滚在退出原 pool 锁后才构造该块，`pool_free` 如实返回释放 Result。两个 transport lease 复用原 Option，成功后才 take，保留 storage-before-reservation 事务。

这四处低层 Rust 返回签名不留兼容入口，两个 lease 的签名保持、失败后保留所有权。原池身份、代际、allocation id、预算及错误/释放顺序保持；dedicated 原缺槽位成功无操作不顺改。竞争解除后可由调用者再次清理，poison/dead-holder 不恢复、不偷锁，也不新增后台重试。Python/正式 Rust SDK 签名、IPC 14、SHM 3、C/FFI ABI 6 和业务帧未改；MemHandle 自身仍不延长 MemPool 寿命。

| 分类 | 新增 | 删除 | 实际净量 |
| --- | ---: | ---: | ---: |
| 生产代码 | 184 | 50 | +134 |
| 测试 | 452 | 25 | +427 |
| 文档与验收证据 | 294 | 7 | +287 |

按 Git 行数统计，包含注释和空行；内嵌 cfg(test) 单独计入测试。生产与测试合计净增 561 行，作为正确性修复，不计作瘦身收益。生产净增在原 90–160 行估算内；测试超出原 180–300 行估算，其中新增真实双向 SHM 往返用例 115 行，实际值取代估算。复用既有 pool、callback、lease 和编译夹具，没有新测试框架或运行时管理器。

旧实现的四条反例在 macOS arm64 / Rust 1.91.0、Windows 11 x64 build 26200 / Rust 1.99.1 均稳定失败：真实竞争和 poison 被报成成功；request/response 重组 lease 再释放报成功但仍有活分配及重组 charge。反例在断言前清理本批映射并确认原生重开失败。候选两端各 61 项通过，包括同 id/generation/容量返还、错误池拒绝、trim 到 0/1、file/dedicated、重组再 admission、准备主/清理双错误与 fill 一次、失败块私有字段的 compile-fail，以及原有 21 场景外部所有权编译门禁。

真实 SHM RPC 在 task-owned endpoint context 上验证双向 carrier：各自首次释放因真实共享锁竞争失败，快照仍可复制；解锁后实际释放，重复释放幂等，双方 alloc_count 归零。Unix socket 与 lease 关闭后均不存在；Windows 关闭后原管道不可连接。映射 charge 可以按原缓存/GC 策略保留，不把它与应归零的重组 reservation 混算。

最终 [Source Validation 40272071925](https://github.com/Dsssyc/c-two/actions/runs/40272071925) 对 `b059c02d` 在 attempt 1 全部 10/10 通过。已读取全部六份原始 run-evidence，逐项核实 147 个适用门禁均 passed/exit 0、进程退出、来源及 workflow SHA 精确匹配；FastDB 来源为 `abba785ef2a7d3fbee1bbc608393a3e6c7b042c4`。每线严格验证 Rust/Python 18 行与 Node 12 行 development 矩阵；五条完整源码线的原生日志各确认七个新增 R07a 用例通过，完整 relay、跨语言、IPC memory 及 Windows 普通用户门禁均通过。

| 平台线 | 适用门禁 | Rust/Python | Node |
| --- | ---: | ---: | ---: |
| Linux x64 / Python 3.12 | 25 | 18/18 | 12/12 |
| macOS arm64 / Python 3.12 | 25 | 18/18 | 12/12 |
| Windows 2022 x64 / Python 3.12 | 27 | 18/18 | 12/12 |
| Windows 2025 x64 / Python 3.12 | 27 | 18/18 | 12/12 |
| Windows 11 ARM64 / Python 3.12 | 27 | 18/18 | 12/12 |
| Linux x64 / Python 3.14t | 16 | 18/18 | 12/12 |

验证过程中保留三项纠正记录：Host 起初写错一个既有 Display 前缀，只把测试修正为原 `Pool error`，未改生产文案；Windows 首次混用 Cargo target 导致候选误用旧基线二进制，该结果弃用，改为独立 target；随后发现 Windows 专属关闭探针漏传原生 connect 的 Duration，修正后针对剩余/受影响项验证，生产未改。首轮 CI `40271664346` 因该修正取消，五条源码线已进入共享验证但未完成，不计入验收；此前“尚未进入运行时测试”的进度表述未获逐门禁证据支持，以这里保留的真实时间线为准。最终候选重新执行确切来源的完整门禁，本机和开发机不受影响的通过记录直接复用。

独立生产审查 run `3218a4fc-53fa-4abd-8bfb-fbbee1bec7da`、artifact `20fb0ecc-d21a-44e7-aac3-f31e25fdbbb5`，固定输入 `5811200c`；测试及 API 文档审查 run `dd22dc78-e0bb-4f9d-b553-8d505b47fb23`，固定输入 `d1802806`，均无阻断发现。它们只做静态审查，没有独立运行测试。Host 核对最终六个生产文件除注释外与审查输入一致，最终测试差异仅补 Windows 原生期限参数，并由 Host 及真实 Windows 运行复核。两份报告均接受、工作树回收。GLM-5.3 run `bb842283-2428-4935-aff6-e1a4cefb3482` 仅交付基线测试，Host 拆开竞争/poison、增加原 owner 与真实 RPC 覆盖并执行红绿；该调整后的 artifact 也已接受和回收。

实际开销：macOS arm64 上，pool 释放 Result 的类型大小从 24 到 104 B，准备 Result 从 96 到 200 B；这是 Rust 类型布局，不能解释为每次调用的额外堆分配，也不是稳定 ABI。成功路径没有新增锁、worker 或 payload 复制；错误对象保留原 pool/分配/预算，既有 Drop 可能多做一次有界 allocator 等待。未测 RPC 性能差异，不声明提速；最终析构仍不能保证接管失败 owner。

本机已清理 target 544680280 B、传输归档 2075476 B。Windows 三次 target 分别清理 420459245 / 289792670 / 411582307 B；最终 temp 仅剩两个空 spill 目录，经清单确认后用原生非递归删除，target/temp/归档均不存在。文本证据与源码留在 `~/.codex/diagnostics/c2-r07a-implementation-1_sjflpb/`；没有保留 wheel、DLL 或 ZIP，没有 inspect/reap 或删除历史目录。本地分支和标签保持；仅本批 fork CI 分支在验收后按确切 SHA 清理。

[机器记录](evidence/2026-10-26-r07a-explicit-release-acceptance.json)保留来源、artifact digest、内容哈希核查、平台/审查/清理摘要。只按 Range 下载文本，核 ZIP CRC 和 run-evidence 中的逐文件 SHA-256；GitHub 整包 digest 仅记录，没有宣称对未下载的完整 ZIP 重新计算哈希。R07 的 Core/Held owner 与 pin、consuming/构造回滚及最终 Drop 保持未完成；C04、R35 和两份控制会话重构继续暂缓。

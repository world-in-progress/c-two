# R17b lease 核验与读取错误保真验收

R17b 按[批准边界](2026-10-26-r17b-lease-io-repair.md)完成，基线 `41c90f04557deaa76a38035e77e1d25783dffcdd`，实现与测试来源 `172350276e2f7f769c608a655e5fca74d5c2cf26`。验收文档提交不是构建来源。本批未 bump 版本或发版，R17 保持部分完成。

两个 crate 内部 helper 改为既有 NamespaceError：verify_lease_entry 保留 fstat/stat 的真实错误，read_lease_record 保留 verify_slot/stat/open/核验/read 的原始 I/O 原因。复用原有 inspect/reap/bind 转换器及 sweep 计数；文件缺失、已识别的类型/属主/权限/身份拒绝和坏记录保持原顺序。bind 的策略拒绝仍为 InvalidData 和原文案，不变成 AddrInUse。没有新增 helper、错误类型、后台任务、重试或兼容入口。

显式 close 和 Drop 保持原 listener/lease/gate 顺序。失败的 lease 退役保留记录、归还 flock；bind 的回滚仍只清理本次新建且再次核验通过的 lease，不清理已有对象。服务端 close 的既有映射因而从 Config 进入 ServerError::Io，Failed 终态和释放时点不变。watch 仍只结构化保存 bind 原因，本批不扩展关闭错误的 SDK 投影。公开签名、凭据 JSON、IPC 14、SHM 3、C/FFI ABI 6 及 Windows 生产路径不改。

| 分类 | 新增 | 删除 | 净量 |
| --- | ---: | ---: | ---: |
| 生产代码 | 45 | 45 | 0 |
| 测试 | 261 | 1 | +260 |
| 文档及验收证据 | 293 | 3 | +290 |

统计含注释、空行，cfg(test) 语句和专属测试文件独立计入测试。非文档净增 260 行，属于正确性修复，不计瘦身收益。生产和测试均在原估算内；新增测试包含 38 行定点故障注入、5 个原 fault 枚举成员及真实生命周期/权限覆盖，没有增加测试框架。正常路径没有新增 syscall、锁、worker 或 payload 操作，未测性能差异，不声明提速。

macOS arm64 的旧分类基线先验证四组反例：fstat/stat 故障在 inspect/reap/close 丢失 errno；bind 在新旧 lease 上均改报 InvalidData；sweep 的五类故障均计作 unverified；双目录 verify_slot 的真实 EACCES 也被折叠。五个 one-shot 注入替换实际 syscall 返回的 Result，再经过旧分类分支。所有测试在红断言前完成本批端点清理；已有不安全 mode 与后续 stat 故障的优先级控制也先通过。

候选 23 项不同的 macOS 定向测试通过，覆盖新增四组及 R17a、未知/坏记录、身份替换、失败初始化、Busy、重复句柄、双目录和 native reap。根据独立审查建议，只在原用例追加 22 行真实 Drop 控制，补做旧分类基线和候选的该项回归：Drop 后记录保留、独立 fd 能取得 flock，当前凭据可回收，socket/lease 为零；原错误断言未放宽。macOS 双目录权限测试先恢复权限、关闭 listener 并确认仅剩 coordinator，再断言原 EACCES。

Windows 开发机复用 R17a 已通过的 10 项凭据测试及原生观察探针，没有重复构建。Host 核对 Windows/local-security/config/公开入口等源码与原已验证来源 f64afd9e 一致，并核对 Windows 文件哈希；本批没有在开发机新执行 R17b 测试，也没有声称 Windows 重现 Unix 故障。最终源码的 Windows x64/ARM64 验证由本批 Actions 完成。

[Source Validation 40322814485](https://github.com/Dsssyc/c-two/actions/runs/40322814485) 对 `172350276e2f7f769c608a655e5fca74d5c2cf26` 首轮 attempt 1 全部 10/10 通过，没有重跑 CI。六条平台线逐项核对源码/workflow SHA 与固定 FastDB 来源，147 个适用门禁 passed/exit 0、进程退出；每线严格验证 Rust/Python 18 行、Node 12 行矩阵。完整 relay、跨语言、原生 memory、Python/CLI 公开投影和 Windows 普通用户消费者门禁通过。Unix 原始日志逐项确认新增用例通过，macOS 包括真实双目录权限反例。

| 平台线 | 门禁 | Rust/Python | Node |
| --- | ---: | ---: | ---: |
| Linux x64 / Python 3.12 | 25 | 18/18 | 12/12 |
| macOS arm64 / Python 3.12 | 25 | 18/18 | 12/12 |
| Windows 2022 x64 / Python 3.12 | 27 | 18/18 | 12/12 |
| Windows 2025 x64 / Python 3.12 | 27 | 18/18 | 12/12 |
| Windows 11 ARM64 / Python 3.12 | 27 | 18/18 | 12/12 |
| Linux x64 / Python 3.14t | 16 | 18/18 | 12/12 |

仅按 Range 下载文本，核 ZIP CRC 和 run-evidence 中的逐文件 SHA-256；GitHub 整包 digest 仅记录，没有宣称重新计算未下载的完整二进制归档。最后一份证据下载发生一次 SSL 中断，只恢复了文本下载；六线齐全后重新核验通过，没有重跑测试。

独立核查 run `3df776e0-a78b-4d80-a7d3-a45d179ec9cb`、artifact `f6b067ad-7ed8-4792-aa16-6af798c31318` 核查原调用链与历史；实现审查 run `24d3fd48-f730-4af7-8f14-1e57f8ceee2f`、artifact `d815b896-c0a6-4c16-bf1c-342e28738e36`，输入 `26d8504c`，无阻断，提出非阻断 Drop 覆盖建议；Host 补齐后由 run `e8e48ec3-bbce-41f8-8ab5-c26b3a574a69`、artifact `2ecf51af-6fd5-4af7-bac8-2d293fd50ac2` 复核输入 `fdc97d02` 的增量并通过。最终候选树与最后审查输入一致。三次均只读、未独立运行测试，报告已接受，工作树已回收。

Host 未采纳首次核查中“bind I/O 失败应保留本次新建 lease”的测试建议：该建议与原 BindLeaseGuard 在瞬时故障解除后重新核验并撤回新 lease 的行为不符。基线与候选都验证了新 lease 撤回、已有 lease 保留，未改变原回滚条件。

本机两次基线 target 分别清理 157934096 / 157939296 B，候选 target 清理 157940360 B；只保留源码、日志与文本收据，未下载 CI wheel、DLL 或包归档。Windows 未重复构建，无本批新增构建产物。所有新增故障夹具均在分类断言前确认 socket/lease 零残留。历史目录未执行维护，本地分支/标签保持，专用 fork CI 分支按确切 SHA 删除。原始文本证据位于 `~/.codex/diagnostics/c2-r17b-implementation-bk8xm7ar/`。

[机器记录](evidence/2026-10-26-r17b-lease-io-acceptance.json)保存来源、反例、计数、审查、Windows 证据复用及 CI 原始记录核查。R17 的 gate_still_named/gate_entry_is_current、目录 pin 验证等错误折叠继续另批；R07 剩余所有权、C04、R35 和两份控制会话重构保持原暂缓边界。

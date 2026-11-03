# Claude 反馈与新版基础复核

2026-10-22，审查当前完整工作树快照 `c209e6c542932d7e536d085316402bd8fa2962e1`。结论：连接通知、准入竞态修复、所有者接管有三系统实现与验收证据，未发现新的静态阻塞；本地 bind/connect 结构化 OS 原因仍未实施，不能称全部 Claude 反馈已经闭环。两份控制会话重构继续暂缓。

| 反馈 | 当前实现与证据 | 结论 |
| --- | --- | --- |
| 无在途调用时双方获知连接终态 | 复用 receive/heartbeat/close 发布；Python 的双方 kill、正常关闭、GIL 等待与订阅测试适用 Windows；原生区分 Windows EOF/reset。A 候选 `7bd74912`、后续 C 全量矩阵均覆盖 | 已完成；通知只表示连接事实 |
| owner_missing 后迟到 relay final 重开准入 | token 绑定 server、route UID/revision 和准入代次；关闭使旧 token 失效，恢复不能复活；真实 IPC 回调的迟到 final 反例跨平台 | R04 已完成，清单原“待评估”过期 |
| OwnerBound 宽限期内能力接管 | 发行/撤销、防重放、旧 owner 命令/EOF 失权、准入恢复及期限竞态均有现有测试；Windows 使用原生 Named Pipe/IOCP | 已完成；Unix 的三项目录身份用例按平台限定 |
| 非 Hello 拒绝期限 | `3312a3d6` 将分支移入原 setup；macOS、Linux、Windows 11 开发机及 Windows 2022/2025 CI 定向回归和完整 relay 通过 | 已完成，生产净增 0 行 |
| bind/connect 结构化系统原因 | IpcError 未转发 source；Failed(String) 丢失原因；CoreError 未投影三个字段；Unix 目录包装也丢 errno | 待批准实施，见[具体方案](2026-10-21-local-io-cause.md) |

Host 本次重新查询 [A 的 CI 39569384042](https://github.com/Dsssyc/c-two/actions/runs/39569384042)、[C 的 CI 39618680376](https://github.com/Dsssyc/c-two/actions/runs/39618680376) 与[期限修复 CI 39712853886](https://github.com/Dsssyc/c-two/actions/runs/39712853886)，各自要求的 job 均处于 completed/success。前两次各 9 个 job，包含 Linux/macOS 与 Windows 2022/2025 的完整 source gates；后一次是 Windows 的四组 owner/relay 定向门禁，实际检出 `3312a3d6`，不能与 workflow SHA 混用。

C 的完整验收源为 `ea511c7a`。此后生产只有 Unix owner 激活状态收敛和非 Hello 期限移动：前者有 macOS/Linux 原生与生命周期测试以及 Windows 9 项补验；后者有三系统 owner/relay 验收。当前 core/sdk 与 `3312a3d6` 一致，差异仅为验收文档。本轮核对历史日志/收据、当前源码与后续差异，没有将其说成当前快照重跑全矩阵；也未重复构建已经通过且未受影响的门禁。Windows 范围为 x64，不将 FastDB ARM64 发行包支持等同于 C-Two ARM64 实测。

新版 hey-my-buddy 独立只读审查 `36ec26bf-9574-4755-b75a-2d9eae849aeb` 得出同样结论。Host 核对固定快照、空补丁和证据边界后验收，审查工作树已回收。运行证据继续绑定[连接通知记录](evidence/2026-10-21-connection-observation-fastdb023.json)、[接管记录](evidence/2026-10-21-owner-takeover.json)与[期限修复记录](evidence/2026-10-22-owner-setup-deadline.json)。

原 39 个 R 条目现为完成 12、部分完成 13、待处理 13、明确保留 1；这里只纠正 R04 状态，未新增生产或测试改动。先补齐上述 OS 原因缺口，再恢复普通瘦身筛选。历史目录、本地分支和标签未操作，本轮没有生成构建产物。

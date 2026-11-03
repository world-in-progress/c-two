# Runtime 会话换代待修方案：Opus 审查记录

状态：只读方案审查已完成；方案有阻塞，尚未批准或实施。接受本次 review 产物只表示记录并核对了意见，不表示接受生产实现。

## 来源与复核边界

- 源码基线：`9e8da3aacbfe64f704279a61a1c7f7198574d6c0`，生产来源 `e0b5af2d`。
- 工具：hey-my-buddy skill 0.55.2；用户指定 `claude / anthropic / claude-opus-5-5 / high`。回执 requested、resolved、observed model 均相符，未修改全局路由偏好。
- 宏任务：`macro-d6108004-b1c3-4861-89b5-06b2d786b7c7`；微任务：`07bcd77b-b0db-48bb-8de0-ac0fbad194ed`。
- 固定只读源码/初稿快照：`340ebdffb8c4681c74f259cf14c9ee5390836a7b`。最终复核另由 Host 输入完整二稿，SHA-256 `9071b1d5fa20e2d78d649078ff042b8f69bbe7fdf52e0a8ca6834e0d35c74a13`；不能把初稿工作树误记为最终二稿。
- 最终 attempt：`1fd52588-7e36-45ab-9441-e6abe86226aa`；固定 artifact：`d891d3f7-09da-4390-9685-3539227fcfab`。输出 commit 与输入一致，changedPaths 为空，补丁 SHA-256 为 `e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855`，Host 已核对实际空补丁。
- 一次恢复会话读错工作树路径被权限边界拒绝，改用既有 checkout 内路径继续，未放宽权限；该恢复没有完整继承修订输入，后续已重新提供完整二稿复核。
- 全部结论为静态分析；本轮没有构建、运行测试或重跑既有 CI。既有真实反例仍以[原报告](2026-10-27-runtime-concurrent-shutdown-followup.md)为准。

## 结论及 Host 处置

Opus 最终结论为“有条件接受。修完一处阻塞后，二稿可以交用户确认”。Host 接受其阻塞结论，未把有条件接受记为方案通过。

| 项目 | 审查判断与处置 |
| --- | --- |
| SDK 退役引用 | 认可：仅 native stopped 状态不能覆盖 Python 会话发布窗口，也不能覆盖 hostless。引用由原 Runtime 锁保护，不持锁排空或运行 hook。 |
| Native 预准备对象 | 删除：初稿预选目标再执行会引入额外所有权和锁边界；只复用现有纯 timeout 校验，不制造命令对象。 |
| hook 归属 | 换代与唯一消费在原锁内完成，hook 在锁外执行；从 outcome 的 safe route 取 slot。Runtime 与 Bridge 都不再传 Python 快照名单，低层显式名单接口保留。 |
| setter 范围 | 收窄：只拦截会实际更换 session 的 transport-policy 分支。其他 setter 保留原冻结/重置/继承规则，重叠 setter 先于 shutdown 的既有重置不能直接判为新故障。 |
| 新准入错误 | 用现有 RuntimeError，避免新增异常类或借用资源业务调用层的 ResourceUnavailable。 |
| timeout | 推荐一次原生数值提取：公开 Runtime 有 Host 时不再接受数值字符串。这是需用户确认的语义收窄；同一 Rust 无状态校验在 preflight/native 入口各执行一次，规则只有一份。 |
| 迟到 connect | **实施阻塞**：close_all 后 pool 可重开，terminate 只还引用。审查建议私有 Native cache-close 投影；Host 同时要求补齐 pending/零剩余预算下的实际 owner 与继续收尾责任，不能仅依靠 Drop。 |

Host 追加核实：relay 解析后选 IPC 的分支同样走 `acquire_relay_ipc → acquire_ipc_client_with_deadline`，与直连复用 Runtime pool。审查中“shutdown 摘掉 pool”的早期表述已更正为只克隆 pool；正常 Native shutdown 不持 Host 锁等待或清表。记录只采用修正后的结论。

## 独立记录的静态线索

- unregister 先移除 Native Registration、Python 因尚不 safe 保留 slot，旧名单校验可能挡住后续 shutdown；取消名单校验能移除该障碍，但 hook 是否最终有结果，要检查 Server journal。Core 会读取完整 journal，不能直接声称“永远没有 outcome”。尚未做隔离复现。
- 首次 register 失败回滚会在 Runtime 锁内调用 Bridge.shutdown；若已有 slot 且 hook 重入，可能自锁。触发前提尚未实测，不与已复现的换代缺口混写。
- 独立低层 NativeSession/Server 的并发重启、私有 default reset/atexit 的重入，不在本次局部草案的保证内。

[待修方案](../reference/runtime-lifecycle.md)已经记录上述阻塞、API/语义取舍、三端回归范围及下一步基线验证；生产改动需待方案补齐并由用户确认。

> **⚠️ 历史档案（Frozen Archive）**
>
> 本目录内容反映决策时的术语体系（ICRM / Component / I-prefix / `@cc.runtime.connect` 等）。
> 这些术语在 v0.x **Terminology Cleanup** 之后已变更：
>
> - `ICRM` / `@cc.icrm` → `CRM 契约` / `@cc.crm`
> - `CRM`（实例语义）→ `Resource`
> - `Component` → 取消该术语；调用 `cc.connect(...)` 的代码即 *client*
> - `I` 前缀约定废弃（`IGrid` → `Grid`）
> - `@cc.runtime.connect` 已移除，请使用 `with cc.connect(...) as x:`
> - Error 类 `CRM*` / `Compo*` → `Resource*` / `Client*`
>
> 最新术语以 [`docs/vision/endgame-architecture.md`](../../vision/endgame-architecture.md) 附录 A 为准。
>
> 本目录内文档**不再修订**，仅作决策档案留存。

> **目录规则**
>
> 本目录按日期保存实施计划。当前权威为 [`2026-07-24 portable-payload contract composition plan`](./2026-07-24-portable-payload-contract-composition.md)、其后续 [`2026-07-24 Rust SDK and portable-payload local release candidate plan`](./2026-07-24-rust-sdk-portable-payload-local-release-candidate.md)，以及 [`docs/roadmap.md`](../../roadmap.md) 明确指向的后续文档；更早文档是历史档案，并可能反映旧术语体系（ICRM / Component / I-prefix / `@cc.runtime.connect` 等）。
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
> 历史文档只可增加 superseded/authority 标记或事实勘误，不得重新解释为当前 API。

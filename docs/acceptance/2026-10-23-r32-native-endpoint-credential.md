# R32：端点凭据单一原生投影方案

状态：已实施并验收，实现提交 `6065660318fd74161ca873eb99e6a74b5e384a1f`。实际生产净减 66 行、测试净增 14 行，合计净减 52 行；完整回归、Windows 开发机、GitHub Actions 10/10（含 Windows ARM64）、端点零残留及独立审查见[验收记录](2026-10-23-r32-native-endpoint-credential-acceptance.md)。固定分析输入为 `548710a8617a33755d5ad73f62b3271452429874`，即 0.8.2 发布后同步了验收记录的主分支。本次只收敛 R32 的 Python 端点凭据投影；结构化错误转换、R17 的 inspect/reap I/O 分类及两份控制会话重构继续独立保留。

## 调用链和原设计

基线的 `cc.inspect_endpoint` 经 `transport/endpoint.py` 进入 `endpoint_ffi.rs::inspect_endpoint_endpoint`，由 Core 重导出的 `c2-local::inspect_endpoint` 返回 `EndpointInspection::Present(EndpointCredential)`。PyO3 随后将此凭据编码成 JSON；Python 的 `_wrap_credential` 再调用 `EndpointCredential.from_json`，经 PyO3 返回同一个 Rust codec 重新解析、派生 context 和 endpoint，最后套入持有 `_native` 的 Python 对象。后续 `cc.reap_endpoint` 又取出 `_native` 交给 Rust。

`def93646` 在 2026-10-06 引入 Python 维护面。原报告明确要求 Rust 独占字段校验、路径派生和回收权，Python 提供门面；JSON 则服务于持久记录与跨进程传递。未发现同进程、同次 inspect 的 JSON 往返是独立安全边界的设计承诺。`5ed04149`、`b0031040` 后续把捕获的 context 和最终目录身份带入凭据，并没有让 Python wrapper 获得额外 authority。当前原生 `PyEndpointCredential` 已经提供全部 metadata、`from_json`、`to_json` 和 repr；Python 类只再转发一次。`LocalEndpointContext`、连接观察和所有者控制类型已经采用原生类直接公开的方式，本项复用这一现有模式。

实际回收仍须经过 `reap_endpoint_credential` 的 address/context 比较，以及 `c2-local` 当时的 gate、lease、目录和实例身份核验。JSON 重解析不证明存活，也不授予删除权。删除内部重解析不删除这些实时核验。

## 替换范围与预计净量

| 范围 | 替换内容 | 预计物理行净变化，含注释与空行 |
| --- | --- | --- |
| `sdk/python/src/c_two/transport/endpoint.py` | 删除 54 行 `EndpointCredential` 转发类和 9 行 `_wrap_credential`；直接导出唯一原生类型；inspect 消费原生对象，reap 直接传递同一对象。保留入口参数检查与顺序。 | 净减约 65–70 行 |
| `sdk/python/native/src/endpoint_ffi.rs` | 复用现有 `PyEndpointCredential`，Python 名称改为 `EndpointCredential`，module 指向公开门面；Present 分支直接移动原凭据。保留公开 `document` 参数名和 Python repr 转义；将必要类型说明移到唯一类型。 | 净增约 5–10 行 |
| 现有 Python 端点测试 | wrapper 断言改为原生类型、不可手工构造/替换/继承断言；补 `document=` 和 repr 边界；两处 native reap 调用去掉 `_native` 解包。合并到现有用例，不另建夹具或框架。 | 净增约 10–20 行 |
| 生产与测试合计 | 去掉一个 Python 对象层及一次 JSON encode/decode、endpoint 重建链。 | 预计净减约 35–55 行 |

上表保留确认时的估算，实际 numstat 已在验收记录中分开统计生产、测试和文档。可确定的运行成本收益是每次 Unix `present` 少一次编码、一次解码和一个 wrapper 分配；没有测量延迟或吞吐提升。Windows 的 inspect 本来返回 kernel-managed 状态，本项不会给 Windows 凭空制造凭据。

## API、协议与语义

保留 `cc.EndpointCredential` 和 `c_two.transport.endpoint.EndpointCredential` 公开名称，以及 `from_json(document)`、`from_json(document=...)`、`to_json()`、`address`、`context`、`protocol`、`platform`。`repr` 保持公开类名及 Python `!r` 的引号和 Unicode 转义，不能直接沿用 Rust `Debug` 格式。外部 JSON 仍由唯一 Rust codec 严格接纳，schema、canonical 字节、32 KiB 上限、未知字段/错误版本拒绝及错误优先级保持。

需要接受的 Python API 变化：类型实现成为 frozen 原生类；删除 `EndpointCredential(native)` 手工包装入口、Python 子类化和可替换的 `_native` 槽；删除旧低层 `_native.PyEndpointCredential` 名称，不留 alias 或双入口。低层 `_native.inspect_endpoint_endpoint` 的 present 结果携带对象，不再携带 JSON 字符串。方法描述符等类型内省会随原生实现改变；旧 wrapper 的 `copy.copy` 槽浅复制也退出，调用方可直接共享不可变凭据引用，显式跨进程交接仍使用 JSON，不补复制兼容方法。`from_json` 非字符串仍抛 TypeError，文案由唯一原生参数接纳生成；不为保留旧 Python 文案再增加检查层。原生方法参数也改名为 `document`，不保留旧私有入口的 `json=` 读法。

`inspect` 改为报告已观察的 typed credential，不再附带验证其 JSON 导出可行性；需要存盘或传进程时显式调用 `to_json()`，原 codec 的失败仍会原样报告。不能笼统声称每个 typed credential 必定可导出：OwnerRecord 的 1024-byte 限制只约束 address，context 本身没有 root 总长度上限，现有 `long_escaped_directory_and_address_obey_the_same_encode_decode_limit` 已覆盖合法 typed 值导出 TooLarge。本次未在真实 Present 对象上复现超限；该纯构造反例不能描述为 inspect 故障。用户已确认此边界调整；不继续编码后丢弃 JSON，也不新增大小估算器维持隐含检查。

Rust 公开 API、C/FFI ABI 5、IPC 12、SHM 3、owner control 1、业务帧、CRM 哈希及凭据 JSON 格式不变。配置冻结、身份校验、回收/关闭和生命周期策略不变；不更改 socket/协调记录位置，不接纳历史对象，不扩大 sweep 范围。公开 re-export 继续指向唯一类型，不保留旧 wrapper 作为兼容层。

## 回归风险与验收边界

主要风险是 Python 类替换造成的公开调用形状变化，或迁移测试时误删跨 context、旧 incarnation 和异常退出覆盖。将正常 metadata/JSON round-trip、`document=`、引号/非 ASCII repr、跨 context 拒绝和正常关闭覆盖先在基线确认；类型、构造、复制退役断言在候选执行。保留现有未知字段/版本/平台、尺寸边界、地址失配、目录替换、异常退出及重新绑定后的精确回收断言。

优先修改 `tests/unit/test_endpoint_maintenance.py`、`tests/unit/test_endpoint_context.py` 和 `tests/integration/test_endpoint_context.py` 中已有用例。Unix 的真实 Present 对象直接传入 reap，导出的 JSON 再导入后同样传入 reap，覆盖两种真正的入口；Windows 执行 Named Pipe kernel-managed、Windows credential JSON、错平台和错地址分支，不能用 Unix skip 代替。真实 connection observation 的子进程清理也要通过原公开路径。

端点定向回归、完整 relay、真实跨语言、三系统及独立实现审查均已完成。Windows 先在开发机验证；由于 Python API 删除，确切候选同时通过 GitHub Actions 现行跨平台门禁，包括 Windows ARM64 与 3.14t。本批隔离端点 socket/lease 零残留验收及构建产物清理均已完成，详细范围见验收记录。已经通过且不受本批影响的历史候选没有重跑。

当前 Python 使用/端点生命周期中英文说明及未发布迁移条目已同步，说明唯一类型和显式 JSON 交接；历史验收报告保持原事实。本凭据子项已完成，R32 整项仍标为部分完成，剩余内部错误转换另行评估。

## 本轮分析证据

Host 已检查上述完整调用链、全部 Python/native credential 引用、引入提交及现有回归。hey-my-buddy 0.51.1 的独立只读复核已完成，run `771d4450-4006-4ddb-b619-4d8b21488fb1`，artifact `add90cee-9b53-4b30-95de-c9e5855e355c`，固定输出仍为分析输入提交，patch 为空。复核认可本项范围及净量估算；Host 已采纳 `document=`、Python repr、浅复制变化及区分纯构造与真实 inspect 反例的意见，验收分析交付并回收审查 worktree。分析阶段只编辑方案和清单；用户随后确认了完整实施边界。实施前扩展既有 round-trip 用例，在原生产代码上运行端点单元及集成回归：122 passed、5 个 Windows 专属 skip，隔离端点 socket/lease 残留为零。

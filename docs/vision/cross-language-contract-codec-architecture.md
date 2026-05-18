# 资源优先 CRM 契约与 Codec 架构

> **Status:** Vision（当前方向）
> **Date:** 2026-05-18
> **Scope:** 定义 C-Two 如何从已有资源类投影出可远程调用的 CRM 契约，并通过 opaque `CodecRef` 与第三方 codec provider 支撑 py-arrow、fastdb、Protobuf、Arrow 等 payload 格式，而不把任何具体格式写入 C-Two core。
> **Audience:** C-Two 核心开发者、未来 SDK 作者、fastdb 贡献者、py-arrow/Arrow codec 作者，以及接手 roadmap 工作的 agents。

## 决策摘要

C-Two 的长期方向是资源优先：已有 GIS、科学计算、模型和工具库中的资源类是事实来源，CRM contract 是这些资源对远程世界暴露的稳定投影，payload codec 是参数和返回值的 ABI。C-Two runtime 负责 route、transport、scheduler、buffer、relay、contract matching 和 adapter 调用；它不理解 fastdb schema、py-arrow schema、Protobuf descriptor 或 Arrow IPC 内部结构。

跨语言能力的核心 artifact 应分成两类：`c-two.contract.v1` 描述可调用资源边界，`CodecRef` 描述每个参数和返回值所使用的 payload ABI 身份。`CodecRef` 是 opaque identity：C-Two 可以验证它结构稳定、hash 确定、能力声明合法，但不能把某个 codec family 的字段布局或 schema 规则硬编码进 core。

普通资源作者不应该在每个 CRM 方法上手写 serializer wrapper。主路径应是 codec provider 自动识别类型并生成 `CodecRef`，`cc.bind_codec(Type, Codec)` 只作为歧义和自定义格式的 escape hatch，`@cc.transfer(...)` 保留为方法级覆盖机制。Python pickle 可以继续服务 Python-only 快速开发，但 portable contract export 必须在遇到 unresolved codec 或 pickle-only wire ref 时 fail-fast。

## 当前实现状态

本文档是当前方向和阶段计划，不表示所有示例 API 都已经完整可用。今天已经存在的基础包括 Python CRM、`@cc.transferable(abi_id=..., abi_schema=...)`、`@cc.transferable(codec_ref=...)`、`CodecRef`、`cc.use_codec(...)`、`cc.bind_codec(...)`、provider 在单参数、返回值和 `list[T]` 场景下优先于 pickle fallback 的解析、resource/CRM conformance check、`build_contract_descriptor(..., portable=True)` 对 pickle-only wire ref 的拒绝、`cc.export_contract_descriptor(...)`、`cc.infer_crm_from_resource(...)`、`python -m c_two.cli.contract export/infer module:Class`、Rust `c2-contract` 对 `c-two.contract.v1` 的 portable descriptor 结构校验、Rust `c2-codegen` 的 TypeScript skeleton 生成、`c3 contract validate/export/infer/codegen typescript`、`c-two.control.json.v1` primitive/control envelope、descriptor hashing、custom ABI 引用、Python-only pickle fallback、direct IPC、relay、可复用 `c_two.providers.arrow` py-arrow provider、grid 示例中由 provider 生成的 Arrow IPC payload refs、fastdb 侧可选 C-Two wrapper provider 冒烟，以及 fastdb 侧基于 `fastdb.schema.v1` 的 provider-owned TypeScript helper stub 生成。后续仍需要 fastdb provider 的可复用打包，以及把真实 TypeScript/WASM payload codec runtime 接到已生成的 helper stub 后面。

## 背景

C-Two 与 gRPC 的主要差异是入口顺序不同。gRPC 通常从 service IDL 开始，再生成实现 skeleton；C-Two 面向大量已经存在的 GIS/科学计算资源，目标是基于资源构造可达服务，而不是为了服务定义资源。CRM 与资源类解耦的意义正是在这里：资源类可以保持领域模型和本地库形态，CRM 则承诺哪些方法、类型和 ABI 会成为远程稳定边界。

当前 Python SDK 已经具备一部分雏形：`@cc.transferable(abi_id=..., abi_schema=...)` 能声明 custom ABI，descriptor path 会把 custom hooks 变成 ABI 引用，默认 pickle transferable 支撑 Python-only 工作流。问题是这些概念仍围绕 Python transferable class 展开，缺少明确的 `CodecRef`/provider 分层，也缺少 portable export 的 fail-fast 规则。

静态语言 SDK 不能依赖 Python 资源类，也不能在运行时猜测 Python serializer。C++、Rust、TypeScript 和浏览器客户端必须消费确定的 contract descriptor 和 codec identity；服务端则在 `cc.register(CRM, resource)` 时证明资源实现符合 CRM 投影。

## 分层模型

资源类是事实来源。它可以是已有 Python 类、C++ binding、GIS 模型对象、科学计算工具类或下游框架封装。C-Two 不要求资源继承 CRM，也不要求资源为了 RPC 改写内部 API。

CRM contract 是远程投影。它声明 remotely callable 方法、参数和返回值类型、method access、buffer mode、namespace、version、route matching identity 和错误边界。静态 SDK、relay route validation 和 codegen 只面向 CRM contract，不面向资源类实现。

`c-two.contract.v1` 是导出的语言中立 descriptor。它应由 Rust `c2-contract` 负责 canonical validation 和 hashing，Python 只作为 exporter/facade。descriptor 可以从 Python CRM 派生，但不能包含 Python-only class names 作为 portable ABI 的事实来源。

`CodecRef` 是 payload ABI identity。它应包含 codec id、version、schema hash 或 opaque ABI hash、buffer capabilities、portable/Python-only 标记和可选 media type。C-Two 只比较和传播这些 identity，不解析 codec 内部 schema。

Codec adapter 是 SDK/第三方包里的实际 encode/decode/from_buffer 实现。py-arrow provider 可以把 Python/Arrow 类型映射到 Arrow IPC codec；fastdb provider 可以把 `@feature` schema 映射到 fastdb codec；Protobuf provider 可以识别 generated message 类型。C-Two core 不 import 这些包。

## 资源优先工作流

已有资源类可以先存在：

```python
class Rasterizer:
    def rasterize(self, points: PointCloud, style: Style) -> Tile:
        ...
```

C-Two 支持从资源类推断 CRM 投影 descriptor，当前要求开发者显式选择方法、命名空间和版本：

```bash
c3 contract infer mypkg.raster:Rasterizer --method rasterize --namespace gis.raster --version 0.1.0 --name RasterizerCRM --out contracts/rasterizer.contract.json
```

推断命令只能生成显式选择方法的投影，不能自动把所有 public 方法变成稳定远程 API。公共边界必须由人确认，避免把资源内部偶然 API 固化成远程契约；遇到 varargs、keyword-only 参数、缺失注解或 unresolved/pickle-only codec 时必须失败。

确认后的 CRM 才是跨语言边界：

```python
@cc.crm(namespace="gis.raster", version="0.1.0")
class RasterizerCRM:
    def rasterize(self, points: PointCloud, style: Style) -> Tile:
        ...
```

服务端注册时执行 conformance check：

```python
cc.register(RasterizerCRM, Rasterizer(), name="rasterizer")
```

注册检查应验证资源方法存在、参数兼容、返回值兼容、codec 可解析、buffer mode 合法，并在需要时要求显式 adapter。静态 SDK 不依赖资源类；它只依赖导出的 CRM descriptor。

## Codec Provider 工作流

主路径是启用 provider，而不是逐类型手写绑定：

```python
import c_two as cc
import fastdb_c2

cc.use_codec(fastdb_c2.provider)
```

导出或注册时，C-Two 询问 provider：这个类型是否有 portable codec candidate。provider 返回一个或多个 `CodecRef` 和对应 adapter。没有 candidate 时，Python-only 调用可以退回 pickle；portable export 必须失败并给出修复建议。

手写绑定只处理歧义：

```python
cc.bind_codec(PointCloud, FastdbColumnarCodec)
```

方法级覆盖仍然保留：

```python
@cc.transfer(input=PointCloudFastdbCodec, output=TileArrowCodec)
def render(self, points: PointCloud) -> Tile:
    ...
```

这个层级让普通 CRM 作者只关心业务类型，codec 包作者承担 schema/adapter 细节，C-Two 负责把结果变成可验证的 `CodecRef`。

py-arrow provider 把示例中的手写 adapter 压缩成记录类型声明：

```python
from c_two.providers import arrow

@arrow.record
class GridAttribute:
    level: int
    global_id: int
    activate: bool
```

provider 是可选 Python 模块，不由 `import c_two as cc` 隐式导入，也不把 py-arrow 依赖推进 Rust core、CLI 或 SDK 顶层。它负责把被 `@arrow.record` 标记的 dataclass 生成 Arrow IPC `CodecRef` 和 transfer adapter，并负责为 `list[Record]` 生成批量 adapter；`@arrow.record` 是完整 opt-in，会标记 record 并为当前进程注册默认 Arrow provider，不再要求额外的 provider 开关。默认 schema identity 在 CRM 解析绑定 transferable 时由 `crm_namespace.crm_name.record_name.arrow-ipc.vcrm_version` 生成，canonical schema text 同时包含 CRM namespace/name/version、record name、single/batch mode、字段名、字段类型和 nullable 信息。CRM 作者不需要再为了 `list[GridAttribute]` 手写 `GridAttributeBatch`、方法级 `@cc.transfer(output=...)`、record 级 `schema_id` 或 record 级版本；`@arrow.record(name=...)` 只用于覆盖公开 record 名，`@arrow.record(schema_id=...)` 只留给少数跨 CRM 共享 payload 的显式 identity。不在 provider 支持范围内的 annotation 要么继续走 `c-two.control.json`，要么在 portable export 时明确失败，不能静默回退为 portable-looking pickle。

## Portable Export 规则

`c3 contract export` 必须生成确定的 `c-two.contract.v1`，其中每个参数和返回值的 wire entry 都有 resolved `CodecRef`。如果某个类型只能走 `python-pickle-default`，portable export 必须失败。

错误应直指问题：

```text
Cannot export portable contract:
  RasterizerCRM.rasterize(points: PointCloud)

No portable codec found for PointCloud.
Available:
  - python-pickle-default: Python-only, not portable

Hints:
  - install fastdb-c2
  - enable cc.use_codec(fastdb_c2.provider)
  - add explicit cc.bind_codec(...)
```

descriptor hash 必须把 callable contract 和 codec identity 纳入 ABI hash，但不能把 provider implementation details、runtime caches、Python object ids 或 performance-only plans 纳入 hash。

## Codegen 位置

codegen 的正确顺序是 resource infer -> confirmed CRM -> contract export -> SDK codegen。codegen 不应该替开发者决定公共 API，也不应该先于 descriptor 稳定化。

SDK codegen 应从 `c-two.contract.v1` 生成 client stubs、server skeletons、adapter skeletons、contract hash constants 和 codec diagnostics。当前 TypeScript 切片已经能生成 dependency-neutral client skeleton、contract hash constant 和 codec requirement declarations；某个 SDK 只应暴露它能支持的 codec，如果 descriptor 引用的 codec 在该语言不可用，生成器应失败或生成明确的 opaque/unsupported stub。

## fastdb 与 py-arrow 的角色

fastdb 是第一个 domain-specific provider pilot，而不是 CRM IDL。fastdb 应提供 `fastdb.schema.v1`、ColumnEngine/ObjectEngine capability profiles、codec adapters 和可选 codegen plugin。C-Two 只看到 `CodecRef(id="org.fastdb.columnar", schema_sha256="...")` 这样的 opaque identity。

py-arrow 是第一个 mature external format provider pilot。C-Two examples 已经把 grid Arrow IPC payload 收敛为可复用 `c_two.providers.arrow` provider，控制参数也已经能通过 `c-two.control.json.v1` 脱离 pickle；grid 现在只声明领域 payload 类型，Arrow schema identity 由 CRM context 默认生成，provider/codegen 自动组合能力仍保留为后续清晰边界。

`c_two.providers.custom` 是 opaque custom codec 的主路径。它装饰业务 payload 类型，要求开发者声明稳定 `CodecRef` identity 和 schema hash/text，并手写 Python runtime 的 `serialize`、`deserialize`、可选 `from_buffer`；portable descriptor 仍导出 `portable=true` 的 codec ref，TypeScript 等 SDK codegen 只生成类型表面、codec requirement 和 runtime implementation stub，不假装能自动实现未知 wire format。

Arrow 的 wire ABI 与 Python 侧 materialization policy 分开处理。`list[@arrow.record]` 是正常 batch wire path：普通调用仍返回物化后的 Python list，`cc.hold(proxy.method)(...)` 则保留底层 response lease 并可返回 `ArrowBatchView`，同时 `HeldResult.buffer` 暴露原始 Arrow IPC wire buffer 供高级用户自行用 PyArrow/NumPy 等解析。`arrow.Batch[Record]` 只保留为“普通调用也返回 Arrow-backed view”的显式 annotation，不再代表另一套 schema identity。

Protobuf、FlatBuffers、Avro、JSON Schema、GeoArrow、WKB 等都应作为 provider families 进入，不应让某一种格式成为 C-Two core 的内置 worldview。

## 实现顺序

1. 已完成：文档落地资源优先 CRM 投影、opaque `CodecRef`、provider 自动解析、portable export fail-fast 和 pickle Python-only 边界。
2. 已完成：在现有 Python transferable/descriptor 基础上引入最小 `CodecRef` 表达和 provider resolution，不改变底层 transport。
3. 已完成：给 `cc.register(CRM, resource)` 增加服务端 conformance check，确保资源实现符合 CRM 投影。
4. 已完成：增加 portable contract export，先能稳定导出 Python CRM 的 language-neutral descriptor 并拒绝 unresolved codecs。
5. 已完成：在 fastdb 侧实现 `fastdb.schema.v1` 与 provider pilot，验证 C-Two 不 import fastdb。
6. 已完成：把 grid 示例中的 Arrow IPC payload 迁移为显式 `CodecRef`，并用 thread-local、direct IPC 和 relay 冒烟验证 payload codec 路径。
7. 已完成：把 `c-two.contract.v1` 的结构校验移入 Rust `c2-contract`，Python exporter 在返回 JSON 前调用 Rust 校验，`c3 contract validate` 使用同一 Rust validator。
8. 已完成：实现 primitive/control portable envelope，使 JSON-safe primitives、lists、dicts、tuples、optionals 和默认值可以在远程控制参数中脱离 pickle，同时继续要求大 payload 通过显式 provider codec。
9. 已完成：实现 resource-to-CRM infer，允许从已有资源类生成显式方法子集的 CRM 投影 descriptor，并把生成结果交给 Rust validator。
10. 已完成：实现 `c3 contract export/infer`，其中 Python 只负责 import/reflection，Rust 负责 portable descriptor validation 和 hash。
11. 已完成：实现首个 SDK codegen 切片，优先 TypeScript，因为 fastdb 已有 TS 工具链；生成器只生成 contract skeleton、typed call surface、contract hash constants 和 codec requirement declarations，不伪造未知 codec 的实现。
12. 已完成：在 fastdb 侧实现 provider-owned codegen helper，把 `fastdb.schema.v1` 生成 TypeScript payload codec helper stub 或明确的 unsupported diagnostic，应用层组合 c-two RPC skeleton 与 fastdb payload helper。
13. 已完成：用 grid 资源导出、Rust 校验、生成 TypeScript artifact，并继续验证 Python thread-local、direct IPC 和 relay 三条运行时路径。
14. 已完成：把 py-arrow provider 从 grid 示例手写 adapter 升级为可复用 `c_two.providers.arrow` 可选模块，支持记录类型和 `list[record]` 批量 payload，并让默认 Arrow schema identity 从 CRM namespace/name/version 与 record 信息生成，不让 C-Two core 理解 Arrow schema 内部结构。
15. 已完成：新增 `c_two.providers.custom` 作为手写 runtime codec 的 portable stub 路径，并把 py-arrow `list[record]` 收敛为单一 batch wire identity：普通调用物化为 list，`cc.hold(...)` 返回 `ArrowBatchView` 并通过 `HeldResult.buffer` 暴露 retained wire buffer；`arrow.Batch[Record]` 保留为普通调用也返回 view 的显式入口。
16. 下一步：把 fastdb provider 从可选 wrapper 升级为可复用 package，并为 TypeScript/WASM payload codec runtime 补齐真实 encode/decode/from-buffer 实现。

## 非目标

不要把 fastdb schema、py-arrow schema、Protobuf descriptor 或 Arrow IPC internals 放进 C-Two core。

不要把 `cc.bind_codec(...)` 变成普通用户的主路径。

不要把 Python pickle fallback 保留为 portable ABI。

不要自动暴露资源类的所有 public 方法。

不要在没有真实可运行 slice 前创建投机性 SDK 目录。

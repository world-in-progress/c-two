# Unix 持久身份与服务自身端点凭据合并方案

状态：已按批准基线 `8ef1ce21` 实施并验收，最终实现/测试来源为 `9f9d4de3`，见[验收记录](2026-10-27-unix-namespace-identity-acceptance.md)。核查基线为 `f4ab1c7bf9e629a3d2e22df236d0ed7285c12670`；R17c 的独立候选为 `1e74b4c9463071ae6162619fc20eca4f55634b75`，不属于本方案待批内容。本文合并持久命名空间身份修复和 Python 自身凭据查询，两项进入同一后续发行版本；具体发行版本号和发版时机由用户决定。本轮不发版。

用户已接受不同磁盘格式不得显式共用物理 root 的边界。本版按最新反馈保留固定魔数、删除 record 重复协议字节、只用现有生命周期 watch 投影凭据，并将无法核验的原因限定在绑定 cause；按批准范围实施。此前只读审查中关于字段、错误顺序和最后一次现场 stat 的核查继续适用，弱存活标记方案已撤销。下文保留批准时的设计与估算；实际验证、差异和清理以验收记录为准。

本版已再次完成独立只读复核，未发现阻塞设计问题；绑定拒绝分支须保留具名 source，不能在到达 Server 的生命周期快照之前将 reason 字符串化丢失。

## 目标与身份边界

跨重启存储的 Unix 身份去掉设备号：marker 记录 gate 的 inode 和随机 nonce；owner record 与端点凭据记录 socket 的 inode、ctime 和记录的 incarnation。直接收窄现有私有 `UnixSocketIdentity`，不保留占位 device、不在设备号变化时重写记录、不增加新旧格式双读。

同一时刻的比较继续使用 dev+ino：`same_file`、已打开目录与路径的核对、gate/lease 文件描述符与目录项的核对、绑定失败回滚中的现场身份核对均保留。需要从持久证明进入 unlink 时，先匹配持久字段，再把本次已取得的 `libc::stat` 传给既有 `unlink_verified`，用 dev+ino 及 ctime 完成原删除事务的复核；四个内部调用点一起改，不增加一个“现场身份”包装类型，也不增加 stat。不能因收窄持久身份而顺带删掉现场设备号校验。

owner `context_digest` 的输入和 `c2.owner.unix-context.v1` domain 保持原样。它绑定仍存活的 OwnerBound 宿主；接管要求该宿主处于 OwnerMissing 且尚在原宽限期内，机器重启后不存在可由旧票据恢复的原宿主。它属于同一生命周期内的现场身份，不属于本次持久身份迁移。

## 各版本号的职责

| 标识 | 当前值 → 目标值 | 职责 |
| --- | --- | --- |
| 端点协议元数据字符串 | `managed-v2` → 不变 | 表示现有 managed Unix 后端；不新增另一个协议名称。Windows `named-pipe` 不变。 |
| 固定魔数 | `c2mgatv2` / `c2mgrec2` → 不变 | 末位 `2` 标识 managed-v2 后端，不是磁盘格式版本，不作为通配符。 |
| namespace 磁盘格式 | `2` → `3` | gate、marker 和 lease record 使用同一当前版本；marker 和 record 删除 device。gate nonce 本体内容不变，但声明同一 namespace 格式。 |
| record 内的协议编号 | `RECORD_PROTOCOL=2` → 删除 | 后端已由固定魔数和命名空间决定；格式 3 不再存储或验证这个重复字节。 |
| 默认目录代号 | 无版本后缀 → `3` | 直接取 namespace 磁盘格式版本，不再维护独立目录版本。 |
| Unix 凭据 JSON `schemaVersion` | `3` → `4` | 删除 `device`，保留 root、namespaceId、incarnation、inode 和 ctime；拒绝全部 schema 3。 |
| Windows 凭据 JSON | `1` → 不变 | 内核管理端点，不含 Unix 文件身份。 |
| IPC / SHM / C-FFI ABI | `14 / 3 / 6` → 不变 | 本批不修改握手、业务帧、SHM、C 结构布局或 owner 控制消息。 |

上表的“不变”相对于本批源码基线；已发布的 0.9.0 使用 IPC 13，待发候选中更早完成的 IPC 14 变更仍须在最终发布说明中保留，不能据此声称新发行版与 0.9.0 握手兼容。

历史提交 `da5ca3be` 引入 `c2mgatv2`、`c2mgrec2` 时，`GATE_VERSION` 与 `RECORD_VERSION` 均为 1，证明魔数末位与格式版本是两回事。本批魔数原字节保持，只把独立格式版本字节设为 3。版本只在一处声明，由现有 `c2-config` 的 Unix 命名空间常量同时供应默认目录派生和 `c2-local` 的磁盘格式写入。

格式识别通则是“魔数相同、版本字节不是当前值 → unsupported-namespace-format”。namespace 入口只对 gate/marker 做这一判断，保留原文件类型、属主、目录与 gate 身份校验顺序，在采用已有协调器或写入初始化记录之前完成；不为格式 2 单列特例，不读取旧正文、不扫描 lease。可辨认的异版本与截断、未知魔数、替换身份分别处理；后者保留对应损坏/身份原因。通过入口后，marker/lease 解码只接受当前格式；当前 namespace 内按槽访问到的旧/损坏 lease 仍是 `invalid-record`。首次初始化的写回核验继续保留。

格式 3 的 gate 保持 25 字节（8 字节魔数、1 字节版本、16 字节 nonce）；marker 从 41 字节变为 33 字节，删除 device。lease record 删除 device 和 RECORD_PROTOCOL，固定头从 60 字节变为 51 字节，顺序为魔数、版本、地址长度、incarnation、inode、ctime 秒、ctime 纳秒，后接地址字节；原整数端序和地址长度上限保持。写入、解码、长度常量与独立手写 golden 同批替换。

默认目录遵循一条规则：**名称包含 namespace 磁盘格式版本，且只在磁盘格式改变时改名**。本批 Unix socket 默认 root 为 `/tmp/c2-v3-<uidhex>`；macOS 记录目录为 `confstr(_CS_DARWIN_USER_DIR)/c2-ipc-coordination-v3`，Linux 记录仍与 socket 同目录。自定义 root 永远使用调用方给定的位置，不追加版本名。

同一用户的全部 C-Two 程序共用默认命名空间，可能运行不同磁盘格式版本；版本化默认目录让各版本使用自己的记录与 socket，不互相解释或修改。它不消耗 socket 地址长度预算：自 0.7.3 起 bind/connect/探活按目录描述符定位，交给内核的是短名。默认路径别名继续按当前目录身份汇合；不能改成只比较路径字符串。

## 已确认的默认目录升级与共存边界

不同磁盘格式各用自己的默认目录；应用不得通过显式自定义 root 或路径别名让不同格式共用同一个物理目录，这种配置不受支持。新版默认目录没有同目录旧布局的迁移职责。

违反该边界的具体情形是旧版显式把自定义 root 指向新版默认 socket 目录：旧版同目录 gate/lease 与 macOS 新版独立 records 目录可能并存。双方仍不能凭对方的记录删除对方端点：没有自己租约的 socket 拒绝为 missing-ownership，对方监听存活时报 AddrInUse；旧同目录 lease 不会通过新协调目录被删除。计数和错误原因可能不准确，因此不能把这种混用视作受支持的跨格式互操作。新版不为它增加阻断文件、第二套锁、旧格式解码或 fallback。

同批删除四处被取代的冷切换处理：

1. `NamespaceDirectories::open` 中检查默认 socket 目录旧 `.gate/.gate.marker` 的分支。
2. `verify_slot` 及所有调用点。
3. `next_batch` 的条目处理链中，注释为 “Historical colocated leases belong to the cold-switch boundary” 的专用分支。
4. `split_preserves_legacy_lease_and_refuses_slot_adoption` 测试。

生命周期文档的同名目录冷切换段落改写成版本化默认目录升级说明。已存在的持久身份核对、租约锁和原生精确 reap 继续决定删除权限；删除上述四项不增加历史对象的收养或删除路径。

## 无法核验的绑定错误

`remove_stale_managed_socket` 使用现有 `socket_unverified` 和 reason 枚举，与 inspect 对齐，不再把拒绝归为地址占用：

| 绑定事实 | 结果 |
| --- | --- |
| 探活成功；现有 lease 被另一持有者锁住 | 保留 `AddrInUse`。 |
| socket 为 symlink、其他类型或外来属主 | `InvalidData` + `symlink / unexpected-object / foreign-owner`。 |
| 旧 socket 没有既有 lease，本次才创建空 lease | `InvalidData` + `missing-ownership`；复用既有 `created` 事实，不增加一次检查；失败仍按原回滚规则撤回本次新建 lease。 |
| 既有 lease 无有效 record | `InvalidData` + `invalid-record`。 |
| 地址或持久 socket 身份与 record 不匹配 | `InvalidData` + `record-mismatch`。 |
| `unlink_verified` 返回 `Unverified(reason)` 或 `StaleTarget` | 原 reason；后者在 bind 中映射为 `record-mismatch`。 |
| namespace/lease 核验失败、绑定前 gate 身份发生变化 | `InvalidData` + 原 reason；gate 变化为 `coordinator-replaced`。 |
| coordinator gate 的有界等待耗尽 | 原等待次数与预算不变，报 `WouldBlock`；不再谎报地址占用，不增加自动重试。 |
| 真实 stat/open/unlink/bind 等 OS 错误 | 原 kind/errno 原样传播。内核 bind 自己返回的 `EADDRINUSE` 也照实保留，不能伪造身份原因。 |

现有 `try_lock_lease` 实际只产生 Busy 或 IoError。将其返回型收窄为既有 `io::Result<()>`，在两个调用边界分别把真实 WouldBlock 投影为 AddrInUse / Busy，去掉虚假的其他结果兜底；初始化回滚的 `.is_err()` 判定保持。这里不新增错误枚举或通用映射框架。

原因只放在绑定专用的 cause 上。`EndpointIoError { kind, raw_os_error }`、其转换和 Rust SDK 导出保持原形状，sweep 的 `last_io_error` 不增加永远为 None 的字段。新增一个 `EndpointBindCause` 作为绑定原因的唯一载体，由 Server 的失败快照和 Core 的 Bind 分支共用；不另设错误缓存或平行原因字段。

公开 Rust 目标形状如下。`EndpointBindCause` 定义在 `c2-local`，Core 与 Rust SDK 按现有方式导出；`ServerFailure` 统一使用字段 `bind_cause`，同一绑定原因只用一种名称；`LocalIoCause::error()` 仍返回 `EndpointIoError`，从 Bind 取 `cause.error`，从 Connect 直接取原值。

```rust
pub struct EndpointIoError {
    pub kind: std::io::ErrorKind,
    pub raw_os_error: Option<i32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EndpointBindCause {
    pub error: EndpointIoError,
    pub unverified_reason: Option<EndpointUnverifiedReason>,
}

pub struct ServerFailure {
    pub message: String,
    pub bind_cause: Option<EndpointBindCause>,
    pub startup_timeout: Option<ServerStartupTimeout>,
}

pub enum LocalIoCause {
    Bind(EndpointBindCause),
    Connect(EndpointIoError),
}
```

原因沿现有链传递：原绑定 `io::Error` 的具名原因 → `EndpointBindCause::from(&error)` → `ServerFailure.bind_cause` / 生命周期 watch → `LocalIoCause::Bind` → `CoreError.endpoint_unverified_reason`。普通 OS 绑定失败的 reason 为 None；Connect、sweep 等路径不携带该绑定 reason。Python 新字段在类上声明固定 Literal 取值，其他错误为 None；值与 inspect.reason 相同。Rust 原生错误为 InvalidData，Python `io_kind` 保留现有粗粒度词表（该类映射为 `other`），不再是 `address_in_use`；以新增 reason 字段程序化区分，不从消息解析。真实 errno 不造假，策略拒绝 raw_os_error 为 None。

所有绑定无法核验分支统一把现有 `EndpointUnverifiedReason` 作为可 downcast 的具名 `io::Error` source 保存；绑定 cause 在 `ServerFailure` 的既有转换边界提取一次，Core 与 PyO3 只投影同一值。不先转成裸字符串，不通过消息重新推断原因。

原因字符串由现有原生枚举提供一处映射，inspect、CLI 和 CoreError 共用；删除重复的 Python native/CLI 枚举到字符串表。公开 Rust 变化为新增 `EndpointUnverifiedReason` 枚举项、`EndpointBindCause` 类型，以及 `ServerFailure.bind_cause` 和 `LocalIoCause::Bind` 的载荷变化；不保留旧载荷兼容入口。`EndpointIoError` 不变，没有 C ABI 或协议改动。更广的 gate/context I/O 折叠继续列为 R17 剩余工作，不借这次重构所有错误机制。

## 自身监听凭据接口

新增公开 Python `cc.server_credential() -> EndpointCredential | None`，与 `server_address()` / `server_id()` 并列。复用现有原生 `EndpointCredential` 类与 JSON codec，不增加 dict 包装、别名或第二种凭据。

来源为成功绑定的 `LocalListener::credential()`。现有调用链为 `Server::run` 局部 listener → Core `Host` → PyO3 `RuntimeSession` 已有 host 槽 → Python registry → 顶层函数。`Runtime` 目前不拥有 Host，不为查询新增 Runtime host 缓存，也不根据 `server_address` 推导凭据。

凭据只跟现有生命周期 watch 走：私有消息携带原 lifecycle state 与 `Option<EndpointCredential>`，公开 `ServerLifecycleState` 形状保持。凭据随 Ready 一起发布；Ready → Stopping 保留；`begin_start_attempt` 清除旧值；close 完成后的 Stopped/Failed 清除。运行时强行丢弃后，由已有 `finalize_runtime_stopped` 把残留的 Starting/Ready/Stopping 收成 Stopped，并同步清除凭据。

不增加每次绑定的 `Arc<()>`、Weak、引用计数查询、Drop guard 或 close 闭包持标记规则。从 run 被取消到现有生命周期收尾之间，getter 允许返回上一个监听者的凭据快照；它不证明存活、不授予权限，reap 仍按现有身份规则判断。若未来发现该窗口影响实际生命周期保证，应修复生命周期状态发布，不能为凭据单建一套存活机制。

Core Host 新增只读 `endpoint_credential()`，Server 提供对应投影；PyO3 getter 只读取已有 host，不调用 `ensure_host`。SDK 只转发，不调用 inspect、不做文件 I/O、不获取 namespace gate、不冻结配置，也不触发 readiness 等待。

| 时机/平台 | 查询结果 |
| --- | --- |
| 初始进程未启动监听；绑定或 owner arm 失败 | None；查询不创建服务。仅存在预先生成的 server_address 不代表有凭据。 |
| 注册成功、listener 已发布 Ready | Unix 返回 schema 4 的凭据，内容与同一 listener 的 inspect present 结果一致。 |
| 低层显式启动了 Host，但还没有注册路由 | 返回该 listener 的凭据；是否存在路由不参与判断。 |
| 注销部分或全部路由，但 listener 仍存在；OwnerMissing；仍持有 listener 的排空阶段 | 同一监听凭据；它不证明业务准入或资源存在。 |
| 生命周期进入终态 Stopped/Failed，包括清理错误仍有文件残留 | 新查询为 None；应用先前保存的凭据仍是可用于后续精确 reap 的描述。 |
| run 已取消，但既有生命周期收尾尚未发布终态 | 可以返回上一个监听者的凭据快照；不表示仍在监听，也不改变 reap 的身份核验。 |
| Persistent 重新启动 | 只发布新 listener 的新 incarnation；旧凭据不能回收新实例。 |
| Windows 已绑定 listener | 返回原 `LocalListener::credential()` 的 kernel-managed schema 1 凭据；inspect 仍是 not-applicable，reap 仍是 NotApplicable。没有 Unix 文件回收承诺。 |

getter 返回的是查询时捕获的凭据，不保证下一时刻仍在监听；它与 inspect 一样不授予删除或接管权。Unix 上仍存活时 reap 返回 Busy，正常关闭后为 AlreadyAbsent，确认异常退出后才按现有精确回收规则处理。`inspect_endpoint` 仍只 try-lock，协调锁忙时返回 `io-error / WouldBlock / retryable=True`。

## 替换范围、测试与成本

生产涉及 `c2-config/src/local.rs` 的默认目录与版本常量；`c2-local` 的 Unix 持久身份、marker/record、namespace 入口、凭据 codec、错误类型和字符串投影；`c2-server/src/server.rs` 的现有生命周期消息；`c2-core/src/host.rs` 的只读投影；Python native 的 runtime session、endpoint/error 投影；Python registry、顶层导出和错误类型；CLI 的 reason 映射。Windows 管道、安全主体、映射和 owner 实现不改；Node 继续通过现有原生配置路径派生端点，不复制路径算法。

纯比较测试构造 stat，只改变 device、inode、ctime、nonce/incarnation 中的一项；验证持久比较忽略 device，现场 dev+ino 与 owner context digest 仍受 device 影响。替换旧 schema 3 golden 和 codec 测试，独立手写 schema 4 golden；从 wire/document 两侧删除 device 字段，schema 4 带 device 也必须按未知字段拒绝，保留其他严格边界检查；旧 schema 全拒。namespace 测试使用相同固定魔数搭配多个非当前版本字节，证明没有格式 2 特例；改变魔数末位仍按未知魔数拒绝，不能把末位当通配符。独立手写格式 3 record golden，确认 RECORD_PROTOCOL 字节已删除、其后字段偏移和长度一致。损坏/截断、目录替换、别名、初始化失败、异常退出和原生精确回收仍覆盖。

绑定反例覆盖表中全部拒绝分支，与 inspect 的 reason 对照；真实 I/O 原因、活监听、lease 锁和 gate 竞争分别验证。Core/Python 测试必须通过真实注册失败的生命周期 watch 取得 reason，不只测试字符串函数；确认 Connect 和 sweep 的 OS 错误形状与投影保持原样。用本批隔离夹具验证不支持的显式共根情况下，对方 socket/lease 未被修改；不要求计数与原因成为跨格式兼容合同。

自身凭据使用真实子进程持有 `.gate`：以进程间信号确认已拿锁后，服务端查询必须在锁释放之前返回；同一时段 inspect 仍为 retryable。比较直接凭据与锁释放后的 inspect present；验证存活 Busy、正常关闭 AlreadyAbsent、异常退出精确 reap、重启新 incarnation、未启动监听/绑定失败/终态 None，以及无路由但已绑定时返回凭据。取消测试保留现有生命周期窗口：收尾前允许旧快照，`finalize_runtime_stopped` 后必须为 None；错误或取消不改变凭据 reap 的身份与释放约束。复用现有生命周期/跨进程夹具，不以放宽期限或重跑规避竞态。

本机 macOS 只用隔离根与现有双目录夹具；Linux 通过 C-Two 的完整 Source Validation；Windows 读代码确认文件身份修复不涉及管道实现，并在既有 Windows x64/ARM64 源码门禁验证新查询与 kernel-managed 返回。完整 relay、真实 Rust/Python/Node 互操作和跨语言矩阵在同一最终实现候选跑一轮；相关新用例须在 CI 日志中实际执行。完成 socket/lease 零残留验收和独立审查后接受候选；不把纯读码称为某平台已实测。

主要回归风险有四项：持久身份缩减时误删现场 dev 校验；namespace 版本识别被错误放到创建之后或覆盖更早的类型/属主拒绝；凭据没有随 Ready/Stopping/新启动/终态正确发布或清除；SDK/relay/Node 升级不齐或自定义根仍存旧格式。以上各项分别用字段反例、入口写入前后证据、生命周期用例和跨语言矩阵验收。新 getter 仅公开既有快照，不增加绕过身份校验、回收或 owner 接管的授权能力，也不承诺比现有 lifecycle watch 更强的存活观察。

静态估算：持久身份/格式/目录改动净 −60 至 +50 行，绑定 cause 与重复映射收敛净 +60 至 +110 行，自身凭据链净 +50 至 +90 行；生产合计约 +50 至 +250 行。测试替换旧 golden/冷切换用例后预计净 +220 至 +380 行。它是正确性修复与缺失能力补齐，不宣称净减量或性能提升；实现后按生产、测试、文档分别报告真实增减。新增查询每次仅读取既有 watch 并复制小凭据，不新增 syscall、存活标记或 Arc 控制块；持久格式每条 marker 少存 8 字节、每条 lease record 少存 9 字节，不增加扫描任务。

## 文档与发布说明草稿

同批更新 AGENTS.md、生命周期/本地端点指南、Python API 与错误字段文档、README 相关入口、c3 维护说明和待发布说明。把“默认路径不带版本”和旧同名目录冷切换文字替换成磁盘格式版本规则；历史验收报告保持历史事实，不改写成新版已经验证。

发布说明至少包含：

1. Unix 持久身份不再保存重启后可能变化的设备号；现场身份与 owner 恢复票据绑定保持原规则。无法核验的绑定错误改为 InvalidData，并能从 CoreError.endpoint_unverified_reason 读取与 inspect 相同的原因。
2. 同一机器上的 Python/Rust SDK、c3 relay、Node 原生插件及其运行进程一起升级。IPC 握手号未因本批改变，但新旧默认目录互相看不见，混用表现为找不到端点，不是新的握手版本错误。普通默认升级不需要人工清目录。
3. 旧 `/tmp/c2-<uidhex>` 和 `c2-ipc-coordination` 一律不读、不删、不自动迁移。确认没有旧版进程或遗留子进程、并禁止服务管理器再次拉起旧版本后，用户可自行归档或删除旧目录。
4. 自定义 root 遇到 `unsupported-namespace-format` 时，先停止所有使用该物理 root 及路径别名的客户端、监听者、relay、owner 控制/维护进程，确认进程退出并关闭自动重启；确认目录仅属于该应用的 C-Two 命名空间后，才能离线整体归档或清空、按原权限预建并由新版初始化。不能仅凭 connect 失败、文件年龄或某一个 PID 不存在就清理；不能只删 gate/marker 后继续复用旧 socket/lease。
5. 不同磁盘格式不得通过显式自定义 root 共用同一物理目录。违反时，缺少本方租约、活监听及身份不符仍阻止删除对方 socket/lease，旧 lease 不会通过新协调目录被删除；计数和错误原因可能不准，这种配置不受支持。
6. 所有 Unix schema 3 凭据一律拒绝，无 device 忽略兼容。应用须从升级后的 listener 重新取得 schema 4；旧凭据不能用于新目录实例。Windows schema 1 不变。
7. `cc.server_credential()` 直接返回现有生命周期 watch 持有的监听凭据快照；它不证明 listener 仍存活、不授予权限，终态后为 None。服务进程无需 inspect 自己来获取它；第三方观察仍使用 inspect，忙时的 retryable 语义保持。Windows 返回 kernel-managed 凭据，精确文件回收为 not-applicable。
8. 低层 Rust 新增 reason 枚举项、绑定专用 `EndpointBindCause` 与只读凭据 getter；`ServerFailure.bind_cause` 和 `LocalIoCause::Bind` 改用该载荷，不保留旧载荷兼容。`EndpointIoError` 与 sweep OS 原因形状保持；无 C/FFI ABI、IPC、SHM、业务帧或 owner 控制消息变化。列明实际验证平台、实现来源与 CI 链接，未运行的实验照实标明。

本方案不触碰 hey-my-buddy 仓库或其用户状态，不读取/清理历史 IPC 目录，不新增后台清扫。构建结束清理本批可再生成产物，保留源代码、日志和验收记录。R17c 独立完成原有门禁；C04、R35、两份会话重构和更广 R17 剩余项继续单列。

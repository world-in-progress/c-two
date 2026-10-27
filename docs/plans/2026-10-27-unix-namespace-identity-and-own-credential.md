# Unix 持久身份与服务自身端点凭据合并方案

状态：待用户确认，尚未实施。核查基线为 `f4ab1c7bf9e629a3d2e22df236d0ed7285c12670`；R17c 的独立候选为 `1e74b4c9463071ae6162619fc20eca4f55634b75`，不属于本方案待批内容。本文合并持久命名空间身份修复和 Python 自身凭据查询，两项进入同一后续发行版本；具体发行版本号和发版时机由用户决定。本轮不发版。

独立只读审查已完成；按审查补明了最后一次现场 stat、无路由但已绑定的 Host、格式识别边界和并发查询不续活监听投影。旧版显式自定义 root 指向新版默认目录的共存边界仍待用户确认。本文没有新的动态验证或实现完成声明。

## 目标与身份边界

跨重启存储的 Unix 身份去掉设备号：marker 记录 gate 的 inode 和随机 nonce；owner record 与端点凭据记录 socket 的 inode、ctime 和记录的 incarnation。直接收窄现有私有 `UnixSocketIdentity`，不保留占位 device、不在设备号变化时重写记录、不增加新旧格式双读。

同一时刻的比较继续使用 dev+ino：`same_file`、已打开目录与路径的核对、gate/lease 文件描述符与目录项的核对、绑定失败回滚中的现场身份核对均保留。需要从持久证明进入 unlink 时，先匹配持久字段，再把本次已取得的 `libc::stat` 传给既有 `unlink_verified`，用 dev+ino 及 ctime 完成原删除事务的复核；四个内部调用点一起改，不增加一个“现场身份”包装类型，也不增加 stat。不能因收窄持久身份而顺带删掉现场设备号校验。

owner `context_digest` 的输入和 `c2.owner.unix-context.v1` domain 保持原样。它绑定仍存活的 OwnerBound 宿主；接管要求该宿主处于 OwnerMissing 且尚在原宽限期内，机器重启后不存在可由旧票据恢复的原宿主。它属于同一生命周期内的现场身份，不属于本次持久身份迁移。

## 各版本号的职责

| 标识 | 当前值 → 目标值 | 职责 |
| --- | --- | --- |
| 端点协议元数据字符串 | `managed-v2` → 不变 | 表示现有 managed Unix 后端；不新增另一个协议名称。Windows `named-pipe` 不变。 |
| namespace 磁盘格式 | `2` → `3` | gate、marker 和 lease record 使用同一当前版本；marker 和 record 删除 device。gate nonce 本体内容不变，但声明同一 namespace 格式。 |
| record 内的协议编号 | `RECORD_PROTOCOL=2` → 不变 | 对应现有 managed 后端，与磁盘格式版本分开。 |
| 默认目录代号 | 无版本后缀 → `3` | 直接取 namespace 磁盘格式版本，不再维护独立目录版本。 |
| Unix 凭据 JSON `schemaVersion` | `3` → `4` | 删除 `device`，保留 root、namespaceId、incarnation、inode 和 ctime；拒绝全部 schema 3。 |
| Windows 凭据 JSON | `1` → 不变 | 内核管理端点，不含 Unix 文件身份。 |
| IPC / SHM / C-FFI ABI | `14 / 3 / 6` → 不变 | 本批不修改握手、业务帧、SHM、C 结构布局或 owner 控制消息。 |

上表的“不变”相对于本批源码基线；已发布的 0.9.0 使用 IPC 13，待发候选中更早完成的 IPC 14 变更仍须在最终发布说明中保留，不能据此声称新发行版与 0.9.0 握手兼容。

版本只在一处声明：由现有 `c2-config` 的 Unix 命名空间常量同时供应默认目录派生和 `c2-local` 的磁盘格式写入。gate/marker 与 record 的魔数族分别为 `c2mgatv*`、`c2mgrec*`。namespace 入口仅识别 gate/marker 的同族非当前版本，统一拒绝为 `UnsupportedNamespaceFormat`，公开 reason 为 `unsupported-namespace-format`；不扫描 lease。当前格式 namespace 内访问到的旧/损坏 lease 是 `invalid-record`。不为 v2 写专门分支，也不读取旧记录正文来转换。

格式识别保留原文件类型、属主、目录与 gate 身份校验顺序，在采用已有协调器或写入初始化记录之前完成。同族的可辨认异版本与截断、未知魔数、替换身份分别处理；后者保留对应损坏/身份原因。通过入口后，marker/lease 解码只接受当前格式；夹杂在当前格式 namespace 中的损坏 lease 仍是 `invalid-record`，不扫描或迁移整个目录。首次初始化的写回核验继续保留。

默认目录遵循一条规则：**名称包含 namespace 磁盘格式版本，且只在磁盘格式改变时改名**。本批 Unix socket 默认 root 为 `/tmp/c2-v3-<uidhex>`；macOS 记录目录为 `confstr(_CS_DARWIN_USER_DIR)/c2-ipc-coordination-v3`，Linux 记录仍与 socket 同目录。自定义 root 永远使用调用方给定的位置，不追加版本名。

同一用户的全部 C-Two 程序共用默认命名空间，可能运行不同磁盘格式版本；版本化默认目录让各版本使用自己的记录与 socket，不互相解释或修改。它不消耗 socket 地址长度预算：自 0.7.3 起 bind/connect/探活按目录描述符定位，交给内核的是短名。默认路径别名继续按当前目录身份汇合；不能改成只比较路径字符串。

## 旧冷切换代码：一处需确认的共存边界

普通升级中，新默认目录从未承载旧默认布局，可以删除原同名目录冷切换分支、`verify_slot`、`split_preserves_legacy_lease_and_refuses_slot_adoption`，并将文档改为版本化默认目录的升级说明。

源码仍允许一条例外路径：旧版程序显式设置自定义 root 为新版的 `/tmp/c2-v3-<uidhex>`（或其目录别名）。旧版会在该 socket 目录写同目录 gate/marker/lease；macOS 新版会按目录身份把它识别为默认 root，选择独立协调目录。旧版 `LocalEndpointContext::with_unix_root` 没有保留名字限制，因此不能把这条路径写成不可达。入口检查仍可挡住已存在的同目录 gate/marker；`verify_slot` 还会拒绝新协调目录已初始化后出现在 socket 目录中的孤立旧 lease。现有两项检查也不能阻止旧版在新版完成检查之后再创建自己的 gate。

建议确认的边界是：不同磁盘格式通过各自默认目录隔离；应用不得通过显式自定义 root 将不同格式重新指向同一物理目录。接受这一共存边界后，按用户要求删除上述三项，不为旧进程新增阻断文件、第二套锁或兼容读取。若要求这种显式混用也受支持，本次不能直接宣布旧检查无用，须先重新确定同一物理目录的排他约束。本项等待用户确认，不擅自删改。

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

原因沿现有链传递：原 `io::Error` 的具名原因 → `EndpointIoError` → `ServerFailure.bind_error` / 生命周期 watch → `LocalIoCause` → `CoreError.endpoint_unverified_reason`。后者在 Python 类上声明固定 Literal 取值，其他错误为 None；值与 inspect.reason 相同。Rust 原生错误为 InvalidData，Python `io_kind` 保留现有粗粒度词表（该类映射为 `other`），不再是 `address_in_use`；以新增 reason 字段程序化区分，不从消息解析。真实 errno 不造假，策略拒绝 raw_os_error 为 None。

原因字符串由现有原生枚举提供一处映射，inspect、CLI 和 CoreError 共用；删除重复的 Python native/CLI 枚举到字符串表。公开 Rust `EndpointUnverifiedReason` 增加一项，`EndpointIoError` 增加可选原因字段，属于需要说明的 Rust 源码接口变化；没有 C ABI 或协议改动。更广的 gate/context I/O 折叠继续列为 R17 剩余工作，不借这次重构所有错误机制。

## 自身监听凭据接口

新增公开 Python `cc.server_credential() -> EndpointCredential | None`，与 `server_address()` / `server_id()` 并列。复用现有原生 `EndpointCredential` 类与 JSON codec，不增加 dict 包装、别名或第二种凭据。

来源为成功绑定的 `LocalListener::credential()`。现有调用链为 `Server::run` 局部 listener → Core `Host` → PyO3 `RuntimeSession` 已有 host 槽 → Python registry → 顶层函数。`Runtime` 目前不拥有 Host，不为查询新增 Runtime host 缓存，也不根据 `server_address` 推导凭据。

复用 Server 的现有 lifecycle watch：私有消息携带原 lifecycle state，以及可选的“listener 凭据值 + 弱存活标记”，公开 `ServerLifecycleState` 形状保持。run 为本次 listener 持有该标记唯一的 `Arc<()>`，在 Ready 发布时一起原子发布；Ready → Stopping 保留该项，begin_start_attempt 与终态清除旧项，新 Ready 只发布新监听的值。正常收尾将标记随 listener 移进现有 close 任务；异常取消或展开也随局部 owner 析构。查询只检查弱标记的强引用计数并复制凭据，绝不 upgrade/保留强引用，避免并发查询把已经退出的监听投影续活。标记不拥有 socket、lease 或 Host，只表示该次监听作用域尚未退出；外部获得的凭据是独立元数据。没有新增锁、线程、后台任务、计时器或生命周期状态机；每次绑定增加一个小的 Arc 控制块。

现有 `spawn_blocking` close 闭包必须显式持有该标记，并在 `listener.close()` 返回后释放，再由既有路径发布终态。标记不能留在等待 close 的 async future 中；后者被取消时，实际 close 任务仍可能正在退役监听。

Core Host 新增只读 `endpoint_credential()`，Server 提供对应投影；PyO3 getter 只读取已有 host，不调用 `ensure_host`。SDK 只转发，不调用 inspect、不做文件 I/O、不获取 namespace gate、不冻结配置，也不触发 readiness 等待。

| 时机/平台 | 查询结果 |
| --- | --- |
| 初始进程未启动监听；绑定或 owner arm 失败 | None；查询不创建服务。仅存在预先生成的 server_address 不代表有凭据。 |
| 注册成功、listener 已发布 Ready | Unix 返回 schema 4 的凭据，内容与同一 listener 的 inspect present 结果一致。 |
| 低层显式启动了 Host，但还没有注册路由 | 返回该 listener 的凭据；是否存在路由不参与判断。 |
| 注销部分或全部路由，但 listener 仍存在；OwnerMissing；仍持有 listener 的排空阶段 | 同一监听凭据；它不证明业务准入或资源存在。 |
| close/Drop 完成，包括 close 返回清理错误而残留文件仍在 | 新查询为 None；应用先前保存的凭据仍是可用于后续精确 reap 的描述。 |
| Persistent 重新启动 | 只发布新 listener 的新 incarnation；旧凭据不能回收新实例。 |
| Windows 已绑定 listener | 返回原 `LocalListener::credential()` 的 kernel-managed schema 1 凭据；inspect 仍是 not-applicable，reap 仍是 NotApplicable。没有 Unix 文件回收承诺。 |

getter 返回的是查询时捕获的凭据，不保证下一时刻仍在监听；它与 inspect 一样不授予删除或接管权。Unix 上仍存活时 reap 返回 Busy，正常关闭后为 AlreadyAbsent，确认异常退出后才按现有精确回收规则处理。`inspect_endpoint` 仍只 try-lock，协调锁忙时返回 `io-error / WouldBlock / retryable=True`。

## 替换范围、测试与成本

生产涉及 `c2-config/src/local.rs` 的默认目录与版本常量；`c2-local` 的 Unix 持久身份、marker/record、namespace 入口、凭据 codec、错误类型和字符串投影；`c2-server/src/server.rs` 的现有生命周期消息；`c2-core/src/host.rs` 的只读投影；Python native 的 runtime session、endpoint/error 投影；Python registry、顶层导出和错误类型；CLI 的 reason 映射。Windows 管道、安全主体、映射和 owner 实现不改；Node 继续通过现有原生配置路径派生端点，不复制路径算法。

纯比较测试构造 stat，只改变 device、inode、ctime、nonce/incarnation 中的一项；验证持久比较忽略 device，现场 dev+ino 与 owner context digest 仍受 device 影响。替换旧 schema 3 golden 和 codec 测试，独立手写 schema 4 golden；从 wire/document 两侧删除 device 字段，schema 4 带 device 也必须按未知字段拒绝，保留其他严格边界检查；旧 schema 全拒。namespace 测试覆盖同魔数族的多个非当前版本，证明没有 v2 特例；损坏/截断、目录替换、别名、初始化失败、异常退出和原生精确回收仍覆盖。

绑定反例覆盖表中全部拒绝分支，与 inspect 的 reason 对照；真实 I/O 原因、活监听、lease 锁和 gate 竞争分别验证。Core/Python 测试必须通过真实注册失败的生命周期 watch 取得 reason，不只测试字符串函数。

自身凭据使用真实子进程持有 `.gate`：以进程间信号确认已拿锁后，服务端查询必须在锁释放之前返回；同一时段 inspect 仍为 retryable。比较直接凭据与锁释放后的 inspect present；验证存活 Busy、正常关闭 AlreadyAbsent、异常退出精确 reap、重启新 incarnation、未启动监听/绑定失败/终态 None，以及无路由但已绑定时返回凭据；关闭失败、run 取消和并发查询不能让投影保留旧 owner。复用现有生命周期/跨进程夹具，不以放宽期限或重跑规避竞态。

本机 macOS 只用隔离根与现有双目录夹具；Linux 通过 C-Two 的完整 Source Validation；Windows 读代码确认文件身份修复不涉及管道实现，并在既有 Windows x64/ARM64 源码门禁验证新查询与 kernel-managed 返回。完整 relay、真实 Rust/Python/Node 互操作和跨语言矩阵在同一最终实现候选跑一轮；相关新用例须在 CI 日志中实际执行。完成 socket/lease 零残留验收和独立审查后接受候选；不把纯读码称为某平台已实测。

主要回归风险有四项：持久身份缩减时误删现场 dev 校验；namespace 版本识别被错误放到创建之后或覆盖更早的类型/属主拒绝；凭据发布与 listener close、失败、取消不同步；SDK/relay/Node 升级不齐或自定义根仍存旧格式。以上各项分别用字段反例、入口写入前后证据、生命周期并发用例和跨语言矩阵验收。新 getter 仅公开既有描述，不增加绕过身份校验、回收或 owner 接管的授权能力。

静态估算：持久身份/格式/目录改动净 −40 至 +70 行，原因传递与重复映射收敛净 +40 至 +90 行，自身凭据链净 +70 至 +130 行；生产合计约 +70 至 +290 行。测试替换旧 golden/冷切换用例后预计净 +220 至 +380 行。它是正确性修复与缺失能力补齐，不宣称净减量或性能提升；实现后按生产、测试、文档分别报告真实增减。新增查询每次仅读取既有内存同步点并复制小凭据，不新增 syscall；持久格式每条 marker/lease 少存 8 字节，不增加扫描任务。

## 文档与发布说明草稿

同批更新 AGENTS.md、生命周期/本地端点指南、Python API 与错误字段文档、README 相关入口、c3 维护说明和待发布说明。把“默认路径不带版本”和旧同名目录冷切换文字替换成磁盘格式版本规则；历史验收报告保持历史事实，不改写成新版已经验证。

发布说明至少包含：

1. Unix 持久身份不再保存重启后可能变化的设备号；现场身份与 owner 恢复票据绑定保持原规则。无法核验的绑定错误改为 InvalidData，并能从 CoreError.endpoint_unverified_reason 读取与 inspect 相同的原因。
2. 同一机器上的 Python/Rust SDK、c3 relay、Node 原生插件及其运行进程一起升级。IPC 握手号未因本批改变，但新旧默认目录互相看不见，混用表现为找不到端点，不是新的握手版本错误。普通默认升级不需要人工清目录。
3. 旧 `/tmp/c2-<uidhex>` 和 `c2-ipc-coordination` 一律不读、不删、不自动迁移。确认没有旧版进程或遗留子进程、并禁止服务管理器再次拉起旧版本后，用户可自行归档或删除旧目录。
4. 自定义 root 遇到 `unsupported-namespace-format` 时，先停止所有使用该物理 root 及路径别名的客户端、监听者、relay、owner 控制/维护进程，确认进程退出并关闭自动重启；确认目录仅属于该应用的 C-Two 命名空间后，才能离线整体归档或清空、按原权限预建并由新版初始化。不能仅凭 connect 失败、文件年龄或某一个 PID 不存在就清理；不能只删 gate/marker 后继续复用旧 socket/lease。
5. 所有 Unix schema 3 凭据一律拒绝，无 device 忽略兼容。应用须从升级后的 listener 重新取得 schema 4；旧凭据不能用于新目录实例。Windows schema 1 不变。
6. `cc.server_credential()` 直接返回当前 listener 的凭据；服务进程无需 inspect 自己来获取它。第三方观察仍使用 inspect，忙时的 retryable 语义保持。Windows 返回 kernel-managed 凭据，精确文件回收为 not-applicable。
7. 低层 Rust 新增 reason 枚举项、EndpointIoError 原因字段及只读凭据 getter；无 C/FFI ABI、IPC、SHM、业务帧或 owner 控制消息变化。列明实际验证平台、实现来源与 CI 链接，未运行的实验照实标明。

本方案不触碰 hey-my-buddy 仓库或其用户状态，不读取/清理历史 IPC 目录，不新增后台清扫。构建结束清理本批可再生成产物，保留源代码、日志和验收记录。R17c 独立完成原有门禁；C04、R35、两份会话重构和更广 R17 剩余项继续单列。

# 精简候选回归风险复核

日期：2026-10-09。审查源码：`4cc5b4d229d21f00fc25619e8490169b2246edd1`。本轮逐项复核[初次精简报告](2026-10-09-implementation-slimming.md)，沿调用者、状态变化、公开能力和测试寻找反例。生产代码未修改；这里明确可做的范围与实施前门禁，不把基线通过当成未来 patch 无回归的证明。

结论已收紧：无效参数和重复字段转换仍适合先清理；Node 工厂直接替换、删除 SHM 填充 helper、删除 Relay 缺省域推断都不能按原建议直接实施。当前 Relay 的主要一致性机制有明确用途，本轮没有找到可以整块删除它们的依据。

2026-10-10 的[历史闲置故障复核](2026-10-10-idle-eviction-regression-audit.md)进一步发现并复现：Rust 精确 token acquire 可接受已删除路由的正缓存；TS 持久直连看不到后注册路由，默认重连还可接受同名替换。共享连接语义错误清理仍需并发证明，FallbackDenied 的原始 IPC cause 也有丢失。本报告的已有测试通过不能解释为这些场景全部安全；涉及 acquire、连接池与 Node transport 的精简以该补充报告的前置修复和回归门禁为准。

## 逐项结论

| 原候选 | 复核结论与最小安全范围 | 必须保留或先补的证明 |
| --- | --- | --- |
| 1. 旧 codec / Python 协议门面 | segment/buddy announce 与 consumed 编解码没有找到当前生产或生成消费者，可以退役对应实现、PyO3 导出和常量。协议门面应按具体符号清理。 | 保留仍使用的 `FLAG_CTRL`、`MethodTable`、descriptor/fingerprint/release-ref 投影。逐项映射 Python 畸形输入覆盖，不能整份删除 `test_security.py`。`into_inline_bytes()` 的测试改为显式匹配 Inline，不能无条件 materialize 而隐藏 carrier 变化。 |
| 2. 空 allocator / 重复投影 | 固定传 None、最终丢弃的 allocator 参数可整链删除；两套 outcome 字典 helper 和重复 `ConfigFrozen` arm 可收敛。 | release closure 只共享每次 dispatch 的构造，保留 view/borrowed 的不同释放时刻、先释放 memoryview exporter 再释放请求、恰好一次清理，以及主异常不被投影失败覆盖。补解码、执行、序列化和清理失败的运行覆盖。 |
| 3. 注册 descriptor 快照 | 有重复工作，但属于元数据采集时机重构，不能认定为纯等价删除。 | `get_type_hints()` 会求值字符串 annotation；保留实例/bridge 构造时机、方法顺序/access、shutdown 排除和重复注册错误优先级。portable 取 canonical fingerprints，pickle 保留两种不同 hash 投影。补求值次数、构造/hook 次数和动态类修改测试，不加全局缓存。 |
| 4. Node 两套适配器 | 直接互换已被反例否定。可研究共享内部缓冲/错误处理，先保留自定义连接能力。 | 模板允许较弱的 socket 结构；binding 需要 `off/end/destroy/destroyed` 等。callback-only 错误已有实际行为差异。旧 ABI3 降级是明确能力，退役需同步 TS 与 C addon 的符号检查；Unix `connectUnix` 要求不能强加到 Windows。 |
| 5. SHM 响应准备/发送 | 可以共享内部 allocation/fill、坐标组帧和发送后处理，不能直接删除公共直接填充能力。 | `CrmCallback` 可接收 response pool 并返回 `ShmAlloc`。owned bytes 指针失败允许回退，公共填充 helper 失败返回错误；保留这种分类。现有 closed-writer 测试不足以证明部分发送取消，需补对应阶段与填充 panic 的清理证明。 |
| 6. 分块/帧编码 | 可以合并纯编码，不能顺带合并完整发送循环。 | 普通 chunk 调用整批持 writer 锁，流来源逐帧持锁；请求 u16 chunk 与响应 u32 chunk/u64 总长度不同。保留首块 control、首字节后 deadline、流源失败和部分帧取消后的连接隔离。新增编码逐字节对照与溢出边界证明。 |
| 7. Relay 域信息和包装类型 | 仅类型表达可以先整理，HTTP 形状和行为保持。撤回直接删除缺省域分支。 | 当前 TS 默认 relay-local IPC 路径依赖无 namespace header 的默认域处理。保留本地域匹配、非本地 anchor 的 HTTP 行为、冲突 metadata 抑制 IPC、身份和完整合同校验。消费者全部迁移并证明相同路径之后才可退役旧表达。 |
| 8. 发行体积与依赖 | strip 有实测收益，可作为单独发布工程变更；工具依赖拆分暂不进入等价精简范围。 | 保留异常展开、导出符号、必要签名/调试能力和三平台安装消费。FastDB codegen、HTTP、四个 workspace 的链接方式都有真实用途；不能删除 `cc.compile_contract_artifacts` 或把 source/System CoreSDK 两种构建当作重复。Tokio feature 裁剪需查实际生效 feature 图。 |
| 9. 执行器 / TS 通用运行时 | 两项都保留为独立设计与实验，不直接合并。 | 三个 runtime 的同步 `block_on`、接收任务、finite detach、Unlimited 和初始化失败语义不同。TS 还拥有调用串行链、Promise/关闭行为、provider allocator、观察数据与 JS payload owner；新增 Core 投影必须先证明这些能力及复制量、并发行为。 |
| 10. CI / 活动文档 | 可以统一准备步骤和修正过时文档，不删除实际覆盖或改变发行顺序。 | Windows local/full 的 feature、链接、消费者范围需逐项对齐；c3 在可能重链接的 CLI 测试后才留存，随后测试/收据使用同一字节。保留非管理员消费、清理、18/12 行矩阵、受信 main 验证和候选原字节发布。历史冻结收据保留原文。 |

### 已运行的 Node 反例

直接导入本次源码中的两个 TypeScript 模块，向各自工厂注入一个符合两边结构要求的 socket：`write()` 返回 false，随后只调用 `callback(new Error(...))`，不发送 error/close 事件。模板适配器拒绝写入，binding 适配器却成功返回。原因是前者检查 callback error，后者使用 `this.socket.write(bytes, () => finish())` 丢弃该参数。

这证明两条实现不能等价替换；它不等同于声称所有真实 Node socket 都会静默丢错。实验使用 Node v25.8.1 的 TypeScript 转换，未连接真实端点或加载 FFI；输入、脚本哈希及结果保存在[复核证据](../reports/evidence/2026-10-09-slimming-regression-review.json)。位置：[模板 write helper](../../core/foundation/c2-codegen/assets/typescript_transport.ts) `1228–1261`、[binding write](../../core/foundation/c2-mem-ffi/bindings/typescript/src/index.ts) `957–985`。

独立审查提出“缺少长路径实际连接覆盖”，Host 核对后排除了这一说法：[endpoint-context-transport 测试](../../core/foundation/c2-mem-ffi/bindings/typescript/tests/c2-mem-ffi-endpoint-context-transport.test.mjs) 已创建至少 512 UTF-8 字节、包含中文和空格的目录，启动真实 Rust listener 并执行字节往返。应保留这项证明；本次新发现的缺口是合并所需的自定义 socket 与 callback 错误等价性，而非重复补一个已有长路径测试。

### SHM 与分发不能只看代码相似

[公共 CrmCallback](../../core/transport/c2-server/src/dispatcher.rs) 接收 response pool，[relay forwarding 测试](../../core/transport/c2-http/src/relay/forwarding_tests.rs) 通过 helper 真正构造 prepared buddy/dedicated 响应。即使目前 Python/Core callback 主要返回 Inline，直接填充能力仍存在。可以替换其实现，不能只删入口和证明它的测试。

dedicated 的 owner free 只记录 `freed_at`，peer free 写入 `read_done`，GC 再依据消费证明或 crash timeout 退休。未发送、部分发送、发送成功、调用方已超时属于不同阶段。将这些路径统一成“退出函数就 Drop 释放”会破坏现有所有权。

Python view 输入解码后即可释放；borrowed 输入须存活到资源执行和输出序列化结束，先 invalidate payload，再释放 memoryview 与请求。`CoreRequestBuffer.release()` 会拒绝尚有 exporter 的释放，因此两份相同 closure 可以共享构造，却不能把两种调用时机合并。位置：[分发](../../sdk/python/src/c_two/transport/server/native.py) `585–630`、[请求 exporter](../../sdk/python/native/src/core_ffi.rs) `499`、[dedicated 释放](../../core/foundation/c2-mem/src/pool.rs) `1491`。

## Relay 必须保留的能力与一致性约束

复核沿 route table、authority、peer 协议、background anti-entropy、connection pool、upstream watch、forwarding 和 SDK resolve/call 串起状态变化。不能把多个 ID、revision 或重试检查按字段相似度合并。

| 机制 | 为什么需要 | 本轮已通过的代表性测试 |
| --- | --- | --- |
| `(name, relay_id)` 路由键与完整合同过滤 | 相同名称可在不同 relay 存在；本地优先、peer 顺序和合同匹配是不同规则。 | `resolve_matching_filters_by_full_crm_tag`、`resolve_local_before_peer`、`resolve_peers_sorted_deterministically` |
| tombstone 与单调事件时间 | tombstone 保留期间阻止旧公告覆盖删除；旧删除不能覆盖新注册，重启后的本地注册需胜过旧 tombstone；snapshot 先应用删除状态。 | `tombstone_blocks_stale_route_announce`、`old_tombstone_cannot_delete_newer_route`、`local_register_after_restart_outranks_old_tombstone` |
| digest 的公共字段集合 | 本地 IPC 地址和私有 owner 信息在 peer 侧被剥离，不能放入共享 digest 造成永久不一致；路由/合同、状态和删除 revision 必须被 hash 绑定。 | `route_digest_has_fixed_active_and_tombstone_golden_vectors`、`route_digest_stable_when_only_ipc_only_identity_changes`、`route_state_validator_rejects_digest_diff_hash_replay` |
| sender 所有权与缺失路由的权威删除 | peer 只能发布自己的权威变更；无权删除本地路由。owner 已没有而对端仍保存的路由，需以 tombstone 修复，不能只同步现存条目。 | `peer_withdraw_cannot_remove_local_route`、`digest_exchange_only_advertises_sender_owned_routes`、`digest_exchange_repairs_stale_peer_route_with_tombstone` |
| owner 删除 revision 与本地 catalog revision 分离 | 跨节点 owner 事件序号不能拿来裁剪本机事件日志；日志溢出要求重新列举，不能顺便删活动路由。 | `peer_tombstone_gc_compacts_local_revision_not_owner_removed_revision`、`relay_watch_history_overflow_compacts_without_dropping_routes` |
| endpoint connection 与 route binding 分离 | 一条连接承载多个 route；idle eviction 只丢连接。后注册路由需向原生 authority 获取 token，不能依赖握手快照。 | `relay_late_route_survives_existing_endpoint_and_idle_eviction`、`concurrent_acquire_after_eviction_shares_one_reconnect`；手动 eviction 覆盖不包含精确 token 正缓存、TS 持久直连或所有显式关闭路径，见 2026-10-10 补充报告。 |
| slot 身份、owner generation 与 lease epoch | 旧 acquire/lease drop/evict 不能作用到已替换或重连的新 slot；同一 owner 续租也使旧替换证据失效。 | `stale_lease_drop_after_reinsert_does_not_touch_new_slot`、`replacement_proof_is_rejected_after_same_owner_lease_renewal`、`replacement_proof_cannot_replace_reconnected_owner_slot` |
| candidate 证明之后的最终 owner 探测 | 初次探测和 candidate attestation 之间旧 owner 可能恢复；少一次探测可能抢走仍活跃 owner。提交仍需在锁内验证 token、lease epoch 和 active requests。 | `final_replacement_confirmation_rejects_silent_recovered_owner` |
| 传输错误、容量拒绝与语义错误分类 | EOF、timeout、容量不足或 watch 传输失败不能作为撤路由依据；identity mismatch 的作用域是对应 endpoint 实例。 | `semantic_withdrawal_reason_separates_authority_from_transport`、`relay_reply_capacity_preserves_shared_connection_and_inflight_call`、`watch_transport_failure_does_not_withdraw_route` |
| 域、loopback、server instance 与 route token | 本地域相同只是 IPC 候选条件，不能替代 server/instance 与合同验证。peer 路由的 IPC 私有信息不能投影为本机地址。 | `default_relay_namespace_keeps_legacy_loopback_resolution`、`custom_relay_suppresses_ipc_for_legacy_and_other_namespace_clients`、`two_roots_same_owner_keep_attestation_pool_watch_and_http_in_frozen_namespace` |
| HTTP waiter 与 upstream 工作的独立生命周期 | HTTP 断开后原生工作仍可能使用请求；晚响应必须完成对应 carrier 释放，不能直接 abort 让输入或 dedicated/file 生命周期丢失。 | `h1_disconnect_reclaims_late_dedicated_response`、`h1_disconnect_reclaims_late_file_reassembly_response` |

主要实现位置：[route table](../../core/transport/c2-http/src/relay/route_table.rs) `381、549、884、941`，[peer digest](../../core/transport/c2-http/src/relay/peer_handlers.rs) `261`，[最终替换确认](../../core/transport/c2-http/src/relay/authority.rs) `568`，[slot fence](../../core/transport/c2-http/src/relay/conn_pool.rs) `737`，[语义撤路由分类](../../core/transport/c2-http/src/relay/router.rs) `2123`，[late-result ownership](../../core/transport/c2-http/src/relay/forwarding.rs) `92`。

上表验证的是已有路由状态传播/收敛与本机事务约束。gossip、digest 和 full sync 处理不同恢复路径，不能为了统一为一个“同步函数”删除其中的协议验证、sender 约束、私有字段剥离或 tombstone 应用顺序。

tombstone 有有限保留期，GC 后的陈旧状态还依赖 owner 的 authoritative-missing tombstone 与后续同步修复。本轮没有将这套最终收敛机制描述成无限期阻断一切历史消息；因此也不能删掉 GC 后的修复路径，或为了减少代码将过期、删除和 owner 不可达合成同一种状态。

### 缺省域分支仍有当前消费者

生成 TS 的 [resolve 路径](../../core/foundation/c2-codegen/assets/typescript_transport.ts) `712、3690` 使用用户提供的 headers，没有自动投影 native endpoint namespace；`normalizeRelayRouteInfo()` 也不读取该 namespace。[实际矩阵夹具](../../sdk/python/tests/fixtures/typescript_real_call.mjs) 的 `relay-aware-local-ipc` 模式只注入 `nodeRuntime.connect` 等 IPC 能力，没有附加域 header。

Relay [router](../../core/transport/c2-http/src/relay/router.rs) `1410–1415` 的缺省域分支因此仍维持当前本地 IPC 快路。直接改成“缺 header 一律 HTTP”会损失该路径；这不是只退役未知旧客户端。后续若统一类型，只能先保持现有 JSON 数组、header/JSON 冲突处理、默认/自定义域及纯 HTTP 行为，等当前 TS、Rust、Python 和 CLI 都明确投影域并通过原路径断言后再考虑删分支。Windows 使用登录 SID 域，不套 Unix 路径规则。

## 本轮实际验证

- Rust HTTP/Relay：`CARGO_BUILD_JOBS=2 RUST_TEST_THREADS=4 C2_ENV_FILE= C2_RELAY_ANCHOR_ADDRESS= cargo test --manifest-path core/Cargo.toml --locked --offline -p c2-http --features relay --lib`，397 passed、0 failed、0 ignored。本轮为 macOS；其中 30 个实际通过案例已按约束分组保存在证据 JSON。
- Mesh 集成：运行现有 `test_relay_mesh.py`，7 passed、0 failed、0 skipped，覆盖注册/解析、双 Relay gossip、撤回、双向发现和 owner relay URL。使用已校验的公开 C-Two 0.7.3 wheel 与 c3 0.3.2；34 个 wheel Python 源文件与当前 checkout 逐字节相同。
- Node 反例：现有两个适配器对同一个 callback-only 错误得到不同结果，否定直接替换假设。
- 上轮 strip 副本补查：原件与副本 `codesign --verify --strict` 均成功，Mach-O 全局导出相同；仍未执行完整 stripped-wheel RPC 或 Windows/Linux 产物验证。

本地可编辑 Python 环境的旧原生扩展缺少 `NativeOwnerReceiver`，无法导入当前 Python 源码。本轮保留该错误证据，并在 `/tmp` 解包已核验的正式 wheel 作为 mesh 运行输入，没有重建或替换原始 checkout 的扩展。pytest 的依赖仍来自本地虚拟环境；这不是重新做完整隔离安装验收。artifact source 是 `b0031040a8d1a75d9f089f85b10568e7b33c6343`，文档 HEAD 与它的生产源码一致。

证据包括日志哈希、命令、产物身份和反例输入，见[复核证据](../reports/evidence/2026-10-09-slimming-regression-review.json)。未重复执行整仓测试或 Windows CI；已有 Node 长路径证明作为当前测试覆盖核对，未冒称本轮再次运行。

## 实施前的约束

每个精简 patch 先列出删除的接口、仍承诺的能力、原测试到保留/替代测试的映射。涉及 Node、SHM、descriptor、Relay 域或 executor 的项目，先让上文缺口测试在当前实现上明确刻画行为或复现问题，再改实现；不能只把原断言改成新行为。

Relay 一致性相关改动必须保留上表约束，同时运行 HTTP/Relay 全套、真实 mesh、受影响 SDK 与跨语言矩阵。实际触及端点/内存/共享配置或发布产物时，保留 Windows 2022/2025、非管理员消费和精确字节验收。相同功能的多个实现合并后，错误分类、超时后的行为、释放顺序和调用路径都需要与基线比较。

当前建议的首批范围限于空参数、重复字典转换/不可达 arm、完成覆盖映射后的旧 codec 清理及活动文档同步。其余项目保留为有明确前置条件的重构，不与 0.7.4 根目录权限调整混成一项大改。

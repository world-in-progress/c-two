# 本地 IPC 端点生命周期与残留回收方案

方案日期：2026-10-06。2026-10-07 收口：尚未发布的新版本使用唯一原生端点。Unix 自动采用托管 v2.2 命名空间和可退休端点租约，Windows 自动采用当前登录 SID 的 Named Pipe 与内核监听租约。旧 Unix 映射、永久每地址锁机制及公开协议选择开关从当前实现移除；SDK、relay 和 CLI 只接受逻辑地址，并共享 Rust 地址派生权威。凭据格式元数据不再表示可选的运行模式。

默认 Persistent 和专用子进程 OwnerBound 各自承担常驻与受控服务用途。2026-10-21 的当前开发源码以双向 owner session 和显式恢复票据替换初始单向关系，接管合同见 [C 方案](../plans/2026-10-21-connection-observation-owner-takeover.md#c统一-owner-session限定新增控制面)。已安装 hey-my-buddy 的调用方接入、正式发布和 Windows 11 桌面验证分别需要实际证据；本次文档迁移不表示这些步骤或三平台 CI 已完成。代码清理没有授权删除旧目录数据。

## 所有权与关闭

端点属于服务实例，业务连接只拥有自身流。Persistent 服务在业务断连后继续可用；OwnerBound 只用于单控制器独占整个 Runtime 的专用子进程。`owner_control_pair()` 返回 `OwnerControl` 和 Rust `OwnerBootstrap` / Python `NativeOwnerBootstrap`；可信 launcher 只向目标子进程转交一次性 bootstrap，子进程显式 adopt，Core 在 readiness 前 arm。启动及配置冻结保留原生权威；消耗 bootstrap 后不能 late attach，终态重启需要新的 Runtime 和 bootstrap。普通断连、relay 重连、idle 驱逐或请求取消不能结束所有权。

`OwnerControl.close()` / drop 结束关系，原生观察到 EOF 后进入 `OwnerMissing` 并暂停同一主机 gate；`request_shutdown(timeout)` 则由当前 session 请求 drain，ACK 只证明 initiate。OwnerBound 拒绝普通 admin shutdown，握手前或重复 initiate 也返回既有负 ACK；Persistent admin 和可信进程内 shutdown 保持原语义。`OwnerMissing` 期间已发布路由仍为 Ready，保留 server instance、route UID/revision 和 relay 登记，connect 可成功但调用返回既有 `ResourceUnavailable`；已准入工作及 payload lease 保持原规则。恢复只打开主机 gate，不重开关闭/注销的路由，也不恢复旧 pending 许可或迟到登记 token 的权限。

仅当前有效 session/epoch 可通过 `recovery_ticket(timeout)` / `revoke_recovery(timeout)` 发行或幂等撤销唯一 grant；重复取票返回同一 grant，撤销后再发才更换 secret。票据 repr 脱敏、不隐式 pickle，显式 `export()` 后由应用在控制器之外保存；Rust 用 `OwnerRecoveryTicket::import()`，Python 用 `cc.OwnerRecoveryTicket.from_bytes()`。`Runtime.prepare_owner_takeover(ticket)` / `cc.prepare_owner_takeover(ticket)` 使用并冻结既有 endpoint 配置，失败也冻结；原生 identity 接受同目录别名，拒绝错 root/instance 和目录替换，凭据不指示目录采用。按 prepare、inspect、保存 successor、一次 commit 接入；Python 的 `successor_ticket` / `recovery_ticket` 是属性，Rust 是方法，commit 成功返回新 `OwnerControl`，inspect 仅观察 identity/epoch/phase，不是成功收据。

接管只允许 `OwnerMissing` 且单调时钟 `now < 原 grace deadline`；失败、半包、竞争和 timer 迟到都不能延长期限。提交原子消费旧 grant、安装 successor、提升 epoch 并转移监督，旧票据、旧 epoch 命令/EOF/timer 失权；到期进入原 drain，WatcherIoError/Draining/Finished 不可恢复。每 session 一个在途请求和原生绝对预算，串行槽 waiter 超时不影响其他操作；setup 取原生 5 秒与原 grace deadline 的较早者。明确区分 `NotSent` / `Indeterminate`（Python `not_sent` / `indeterminate`）和确定拒绝；发送后失去确认会关闭候选，只能由应用显式选取已保存票据建立新准备对象，不自动 old/new-key 回退、重放业务或降级为普通 admin/HTTP/relay。

每个shutdown deadline只限制该调用者观察，不取消原生事务。未完成时保留Session、bridge、路由及pending hook；完成结果消费一次。监听器关闭、工作排空与retained lease分别报告，端点回收不替代held/borrowed/FastDB实际所有者释放。

## 唯一派生与精确回收

c2-config::LocalEndpoint独占逻辑地址到OS名称的派生，SDK、relay及CLI不拼路径、不探测旧目录、不选择协议。Unix使用有界摘要文件名及私有目录，gate/marker格式2核对设备号、inode和nonce；监听者固定原gate inode到租约结束。绑定、重启、退出和维护遵守同一交接协议，完整登记的socket及每端点lease可退休，固定协调文件长期保留。Windows使用真实Named Pipe/内核租约，不模拟Unix文件。

inspect不创建未知锁，也不证明对象存活。reap要求精确原生凭据；旧incarnation不能删除新实例。持协调保护、取得目标租约、核对完整记录及当前对象后才能退休。年龄、PID、连接失败或shutdown initiate ACK不能替代所有权与排空证明。

未知socket、损坏记录、符号链接、替换gate/marker及未完成初始化保守拒绝。强杀收敛限于完整登记实例，不承诺任意持久化窗口崩溃都能自动恢复。旧目录数据清理需要单独维护范围。

## 有预算维护与运行器

Rust持有迭代器及进程级单个在途lease。每条目均计入预算并前进，包含busy/unverified/I/O失败；预算用尽让出，EOF才标记完成。维护协调与短gate分离，不能持gate遍历全目录；中断/命名空间变化不伪造覆盖。默认64项/10ms，时间预算为调度目标。

运行器记录本次地址及原生凭据。停止派发、请求关闭并观察原生完成；强杀后必须OS wait，再请求精确reap。维护只选择本次逻辑地址，不按mtime/目录树清其他运行。无人运行时没有额外daemon自动回收承诺。

## 接口与验收

Python 接口为 `inspect_endpoint(address)`、`reap_endpoint(address, credential)`、`sweep_endpoints(*, addresses=..., max_entries=..., max_ms=...)`；Rust/Core/CLI 共用唯一派生。凭据保留平台/格式元数据，Unix incarnation 必填。当前 IPC 握手 12、SHM 3、C/FFI ABI 5 和业务帧、调用/回复表示不因接管改变，addon 仍在调用前核验 ABI。owner 独立使用 version 1、最多 4 KiB 的控制消息并复用既有帧；旧单向接口、格式和别名直接移除，不保留兼容路径。

删除只验证被移除协议/开关的测试，逐项迁移有效覆盖：活跃监听与满backlog、重复绑定唯一赢家、旧流仍活时重启、完整登记后强杀回收、旧凭据阻断、未知reap不创建lease、权限/符号链接/替换/损坏记录、有界预算/续扫/中断/显式范围。Persistent、OwnerBound、relay精确instance/route UID/revision注销、内存各级回退及held/borrowed不提前释放继续验证。

独立审查固定diff和删除/替代映射后，运行正常并行的完整Rust/SDK/Python/repo及真实Linux、Windows2022/2025门禁。严格18行Rust/Python、12行TypeScript收据、普通/非管理员wheel消费及账号/进程/目录清理须有效；ZIP、内部文件、c3及wheel字节匹配。平台、源码及未执行环境分别记录；PR/合并/发布和已安装Buddy接入另行决定。

owner 低层 API 替换与 grace 行为变化仍须固定实现候选的 Linux/macOS/Windows GitHub Actions 全部门禁通过，并保留 source commit 和 run URL 后才能验收；接口说明不替代运行证据。接入用法见[中文指南](../local-endpoint-lifecycle.md)及[英文指南](../local-endpoint-lifecycle.en.md)，原有 A 连接通知、B 准入修复各自保留独立验证边界。

原始方案和此前显式选择行为保留于 Git 历史及[旧源码验收报告](../reports/local-endpoint-final-validation.md)，不再作为当前接口指南。[唯一端点验收记录](../reports/canonical-local-endpoint-validation.md)保留此前端点实施的源码和运行范围，不证明本次 owner 变更已通过平台验收；当前用法见[接入说明](../local-endpoint-lifecycle.md)。

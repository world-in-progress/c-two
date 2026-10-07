# 本地 IPC 端点生命周期与残留回收方案

方案日期：2026-10-06。2026-10-07 收口：尚未发布的新版本使用唯一原生端点。Unix 自动采用托管 v2.2 命名空间和可退休端点租约，Windows 自动采用当前登录 SID 的 Named Pipe 与内核监听租约。旧 Unix 映射、永久每地址锁机制及公开协议选择开关从当前实现移除；SDK、relay 和 CLI 只接受逻辑地址，并共享 Rust 地址派生权威。凭据格式元数据不再表示可选的运行模式。

默认 Persistent 和专用子进程 OwnerBound 各自承担常驻与受控服务用途；OwnerBound 的私有继承通道仍不可重连或接管。已安装 hey-my-buddy 的调用方接入、正式发布和 Windows 11 桌面验证分别需要实际证据。代码清理没有授权删除旧目录数据。

## 所有权与关闭

端点属于服务实例，业务连接只拥有自身流。Persistent 服务在业务断连后继续可用；OwnerBound 只用于单控制器独占整个Runtime的专用子进程。私有继承能力必须在原生就绪前准备，EOF后关闭准入并排空，宽限期不提供重连或接管。普通断连、relay重连、idle驱逐或请求取消不能结束服务。

每个shutdown deadline只限制该调用者观察，不取消原生事务。未完成时保留Session、bridge、路由及pending hook；完成结果消费一次。监听器关闭、工作排空与retained lease分别报告，端点回收不替代held/borrowed/FastDB实际所有者释放。

## 唯一派生与精确回收

c2-config::LocalEndpoint独占逻辑地址到OS名称的派生，SDK、relay及CLI不拼路径、不探测旧目录、不选择协议。Unix使用有界摘要文件名及私有目录，gate/marker格式2核对设备号、inode和nonce；监听者固定原gate inode到租约结束。绑定、重启、退出和维护遵守同一交接协议，完整登记的socket及每端点lease可退休，固定协调文件长期保留。Windows使用真实Named Pipe/内核租约，不模拟Unix文件。

inspect不创建未知锁，也不证明对象存活。reap要求精确原生凭据；旧incarnation不能删除新实例。持协调保护、取得目标租约、核对完整记录及当前对象后才能退休。年龄、PID、连接失败或shutdown initiate ACK不能替代所有权与排空证明。

未知socket、损坏记录、符号链接、替换gate/marker及未完成初始化保守拒绝。强杀收敛限于完整登记实例，不承诺任意持久化窗口崩溃都能自动恢复。旧目录数据清理需要单独维护范围。

## 有预算维护与运行器

Rust持有迭代器及进程级单个在途lease。每条目均计入预算并前进，包含busy/unverified/I/O失败；预算用尽让出，EOF才标记完成。维护协调与短gate分离，不能持gate遍历全目录；中断/命名空间变化不伪造覆盖。默认64项/10ms，时间预算为调度目标。

运行器记录本次地址及原生凭据。停止派发、请求关闭并观察原生完成；强杀后必须OS wait，再请求精确reap。维护只选择本次逻辑地址，不按mtime/目录树清其他运行。无人运行时没有额外daemon自动回收承诺。

## 接口与验收

Python接口为inspect_endpoint(address)、reap_endpoint(address,credential)、sweep_endpoints(*,addresses=...,max_entries=...,max_ms=...)；Rust/Core/CLI共用唯一派生。凭据保留平台/格式元数据，Unix incarnation必填。C ABI升至3，旧addon在调用前拒绝。

删除只验证被移除协议/开关的测试，逐项迁移有效覆盖：活跃监听与满backlog、重复绑定唯一赢家、旧流仍活时重启、完整登记后强杀回收、旧凭据阻断、未知reap不创建lease、权限/符号链接/替换/损坏记录、有界预算/续扫/中断/显式范围。Persistent、OwnerBound、relay精确instance/route UID/revision注销、内存各级回退及held/borrowed不提前释放继续验证。

独立审查固定diff和删除/替代映射后，运行正常并行的完整Rust/SDK/Python/repo及真实Linux、Windows2022/2025门禁。严格18行Rust/Python、12行TypeScript收据、普通/非管理员wheel消费及账号/进程/目录清理须有效；ZIP、内部文件、c3及wheel字节匹配。平台、源码及未执行环境分别记录；PR/合并/发布和已安装Buddy接入另行决定。

原始方案和此前显式选择行为保留于Git历史及[旧源码验收报告](../reports/local-endpoint-final-validation.md)，不再作为当前接口指南。当前实施及实际执行状态见[唯一端点验收记录](../reports/canonical-local-endpoint-validation.md)，用法见[接入说明](../local-endpoint-lifecycle.md)。

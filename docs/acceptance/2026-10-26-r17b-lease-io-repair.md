# R17b：lease 核验与读取错误保真

状态：R17b 已按批准边界完成；生产 45 行增/45 行删，净量 0；测试净增 260 行。基线 `41c90f04`，确切来源 `17235027` 首轮 Actions 10/10，23 项 macOS 定向回归、完整 relay/跨语言、三系统、端点零残留及独立审查见[验收记录](2026-10-26-r17b-lease-io-acceptance.md)。Windows 开发机复用源码未变的既有证据，最终源码另经 x64/ARM64 CI；本批未发版，产物已清理，R17 剩余 gate/context 分类继续单列。IPC 14、SHM 3、C/FFI ABI 6 保持，历史目录、本地分支与标签不动。

## 边界与调用链

`verify_lease_entry` 的 fstat/stat 错误目前折叠为 InvalidOwnership，调用者包括 `retire_owned`（显式 close 与 Drop）、`BindLeaseGuard::drop`、bind、`read_lease_record`、凭据 reap 和 inspect。`read_lease_record` 又把 verify_slot、stat、open、read 的真实 I/O 错误折叠，生产调用者为 sweep 的 `reap_slot_with_directory`。两者从固定 gate/lease 实现 `da5ca3be` 起使用只承载身份拒绝的返回类型；descriptor 与路径二次核对用于防止替换，核验职责保留。

最小替换复用 `NamespaceError::Io / Unverified` 及既有 inspect/reap 适配。保持 syscall 和分支顺序，fstat/stat/open/read 的 Err 保留原 kind/errno；缺失、类型/属主/权限不符、路径身份替换和坏记录继续按原分类拒绝。bind 保留 Unverified 对应的 InvalidData 和原文案，其余通过已有 namespace_bind_error 传递；不会把策略拒绝改成 AddrInUse。Drop 与 BindLeaseGuard 继续只在核验成功时清理，不新增重试或错误缓存。

这两处仅修改 crate 内部 Rust 返回类型，公开签名、SDK 接口、凭据 JSON、ABI 和协议不变。可观察语义变化是错误分类：维护结果不再把真实 I/O 错误记为 unverified，bind 不再把该类原生错误改成 InvalidData；inspect 的既有 retryable 投影随 IoError 改变，但不自动重试。close 仍先关闭 listener，再按原顺序归还 lease/gate；回收失败仍保留待 reaper 核验的记录。服务端关闭沿现有转换由 Config 进入 ServerError::Io，失败终态和释放时点不变；readiness watch 仍只结构化保存 bind 原因，本批不扩展关闭错误的 SDK 投影。

`gate_still_named`、`gate_entry_is_current`、目录 pin 比较的错误折叠及 owner context 继续另批；不改 gate/nonce、准入、生命周期或删除授权，不将 R17 整体核销。

## 执行与退出条件

1. 在旧生产分类上扩展既有 one-shot fault 与私有目录夹具，覆盖核验 fstat/stat 和读取 stat/open/read 的原错误；macOS 分离目录补 verify_slot 的真实权限失败。对照缺失、坏记录、身份替换、Busy 和策略拒绝的错误顺序。反例断言前清理本批资源，保存预期失败日志。
2. 替换两个 helper 及必要调用点，覆盖 inspect、reap、sweep、bind 回滚和显式关闭。已有 close/reap 负责恢复后的清理；不增加框架，先前 R17a 的 stat/复合优先级、目录替换与生命周期覆盖继续保留。
3. 针对性回归、完整 relay、Rust/Python/Node 矩阵、三系统及确切来源 GitHub Actions、socket/lease 零残留和独立审查后提交。Windows 生产及公共投影未变时复用已通过的开发机证据并核对源码哈希，最终源码仍通过 Windows x64/ARM64 CI。构建产物及时清理；不受影响的本机验证不重复运行。

生产替换预计净变化约 −10 至 +20 行；测试及故障注入预计净增 150–260 行，以实际统计取代估算。本项是正确性修复，不计作瘦身收益，不声明性能提速。正常路径不增加系统调用、锁、worker 或 payload 操作。

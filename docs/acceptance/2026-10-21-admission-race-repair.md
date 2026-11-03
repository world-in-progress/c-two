# 准入竞态修复 B：本机验收

日期：2026-10-21。基线 `0acf784f6742487cd619a7d419379f693434a337`，候选保留在工作树；没有提交、移动分支/标签、推送或发版。范围为[最小方案的 B](2026-10-21-connection-observation-owner-takeover.md)。

## 修改及边界

`c2-server` 把原 pending map、shutdown generation 和 owner 关闭标志合入已有登记状态锁。最终 commit/open 先取得登记和 dispatcher 锁，再在同步临界区复核代次与身份。owner 关闭和 shutdown 在同一权威上使旧 token 失效；token 绑定具体 Server 状态的 Arc 身份及 route UID/revision。catalog 与 scheduler 的开放一起发布，catalog 失败不会先打开 scheduler。

保留第一次启动前的合法登记；终态后启动换代，不恢复 OwnerMissing 的业务准入。原 reserve 的重复、容量、shutdown、owner 错误顺序不变。业务请求执行路径没有新增锁、轮询、计数或释放状态机。

`c2-core` 的失败回滚复核捕获的 scope 和原 UID/revision，复用既有 Runtime 元数据锁排除新登记提交，随后调用原 unregister。此私有路径只接收从未开放的 provisional route，不能用于活跃路由；`Host::register` 仍持有 ServiceDefinition，所以锁内没有业务回调 drain 或服务析构。普通 unregister 保留原 drain 路径，只清退自己实际注销的 scope；shutdown 仍捕获全部登记 scope。

公开 Rust 声明未变（两生产文件共 134 项文本签名核对），不改 Python API、C/FFI ABI 5、IPC 12、SHM 3、wire、预算、快照、错误码、relay cleanup 顺序或 payload lease 释放。连接通知与所有者接管不在本批。

## 反例与独立审查

先只加入测试，确认两个文件的生产区与基线逐字节相同。基线 6 个测试按目标断言失败：OwnerMissing 后迟到 final 200 执行真实 IPC 回调；open 等 dispatcher 时 owner/shutdown 关闭仍被越过；跨 Server token、同名替换 token 被接受；旧 final 失败误删 Core 登记的新路由。3 个对照通过：正常 final 200、正常 final 409、原失败来源/状态码及 cleanup error 合并。

候选让这些测试全部通过。完整并行测试发现新增端点 inspect 夹具使用共享 gate，可能因其他测试持锁返回 WouldBlock；为这些新增夹具选用每测试独立 Unix root，Windows 保留内核端点。零残留断言和期限保持原样。

hey-my-buddy 0.50.0 独立审查发现一个候选回归：普通 A 注销会清退 B 的 provisional scope，导致新精确回滚跳过 B。复用 HTTP final barrier 增加不同名交错测试，先确认候选失败于 `rollback.local_removed`，再限定普通注销的 scope 清退范围。修后测试和完整 Core 回归通过，独立复审没有新的阻塞项。

审查 run `3166d054-f006-40ad-8021-40f0e8bc1780`；末轮固定源码对象 `33552002676a23f82894ad347e70ffd6380fc234`；artifact `87b16c0c-d09b-412d-a9ba-def10eb8d07d`。Host 核对三份 Rust 文件与末轮输入逐字节相同，末轮输出 patch 为空，并验收该审查。该对象是审查快照，不是分支提交。首轮因 Host 同时调整测试隔离而未完成封存；最终静态审查绑定修正后的固定输入。

## 验证

| 门禁 | 结果 |
| --- | --- |
| c2-server 全套 | 165 通过 |
| 最终 c2-core 全套 | 206 通过；4 个既有子进程夹具入口保持 ignored，由上层用例启动 |
| c2-http relay 全套 | 416 通过；之后生产变更只在 Core scope 清退，transport 源码不再变化 |
| Python 完整 HTTP relay、mesh、graceful shutdown、benchmark，加 owner、registry、multi CRM、readiness、Runtime 生命周期 | 195 通过；Git 元数据检查在真实 checkout 单独通过，共 196 项覆盖 |
| 本次端点 | 隔离默认 socket root、协调目录及任务临时目录中 socket 0、lease 0 |
| 公开声明及格式 | 签名检查、git diff --check 通过；协议/ABI 源码未改 |

实际运行平台仅 macOS arm64 / Python 3.13。未运行 Linux、Windows 或 GitHub Actions；本记录不代表跨平台运行验收。

验证采用 HEAD 的普通导出目录和同一 Cargo target。为避免历史默认目录，导出副本只替换默认 socket namespace、macOS coordinator 名称及相应路径断言；每轮审计全部 847 个源文件，除此以外与候选字节一致。主 checkout 的默认路径源码未变，历史目录未检查、采用或清理。

c3、Python native wheel、Rust owned_child 均由候选构建。FastDB 使用仓库脚本校验的官方 0.2.1 CoreSDK，source `4f99f86a662b0e950a0dd29800c25a1c9fca4def`。初始本机 source 构建缺少 C++ 标准头，随后改用既有 system 构建流程；测试导出目录首次漏放 wheel 附带的 dylib，补齐完整 native package 后再执行测试。两项属于验证环境准备问题，未改依赖版本、生产代码或断言以绕过。

## 增量与清理

相对基线，生产代码 **+122 / -50，净 +72 行**；测试 **+649 / -26，净 +623 行**。包含运行时修复、真实反例和原测试夹具复用，未作为纯瘦身减量申报。本批说明文档净 +50 行、机器证据 +131 行，共 +181 行，不包含之前已存在的方案与复现记录内容。

验证及复审结束后，删除本次 target、隔离 venv、wheel、CoreSDK 和导出源码中的 native 二进制；保留源码、脚本、日志、hash 和审查收据。隔离协调目录中的 gate/marker 是 namespace 记录，不计作 endpoint 残留，也未删除。

机器结果见[验收证据](evidence/2026-10-21-admission-race-repair.json)。本机详细日志位于 `~/.codex/diagnostics/c2-admission-b-20261021-nzws0ls_`，保留首次失败与最终通过记录。

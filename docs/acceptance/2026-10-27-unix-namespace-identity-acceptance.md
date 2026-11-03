# Unix 持久身份与自身监听凭据验收

批准方案为 `8ef1ce21`；生产实现为 `496bf0ae`，最终实现/测试候选为 `9f9d4de3be39ff26410249511fa966d1212714f1`。本批已完成规定验收，未 bump 版本、合入主线或发版。R17c 另按[自己的记录](2026-10-27-r17c-final-lease-stat-acceptance.md)验收，不重复计量。

Unix 磁盘格式 3 保持 `c2mgatv2` / `c2mgrec2` 魔数和 `managed-v2` 后端名称，删除持久 device 与重复 RECORD_PROTOCOL；marker 少 8 字节，record 固定头少 9 字节。默认目录跟随同一个磁盘版本，自定义 root 不加后缀。Unix schema 4 拒绝 schema 3 和 device 字段；Windows schema 1 不变。现场 dev+ino、删除前末次 stat、owner context digest/domain、身份与释放顺序保持。四处旧同名冷切换代码已删除。

绑定无法核验的事实使用 InvalidData 与具名原因；真实 OS 错误原样保留。`EndpointBindCause` 经唯一字段 `ServerFailure.bind_cause`、`LocalIoCause::Bind` 到达 `CoreError.endpoint_unverified_reason`，`EndpointIoError` 的公开形状不变。`cc.server_credential()` 与 `Host::endpoint_credential()` 读取 listener 随 Ready 发布到现有 watch 的快照，Stopping 保留、开始新尝试和终态清除；没有弱存活标记、第二套生命周期、inspect 或 gate 查询。

基线隔离反例包含 3 个预期 Rust 失败：持久 socket 身份的 device-only 变化、marker 的 device-only 变化、不可核验绑定旧分类。旧 Python 基线没有 getter，gate 被独立进程持有时 inspect 返回 WouldBlock/retryable；候选在同一条件下直接取得监听凭据，与释放 gate 后的 inspect 相同。设备号变化用受控字段反例验证，没有把它描述为真实整机重启实验。候选本机 78 项 Python 定向、4 项 Host/生命周期、3 项 CLI 回归通过，另保留格式、目录替换、末次现场身份及双目录清理的 Rust 定向日志。

[Source Validation 40492839892](https://github.com/Dsssyc/c-two/actions/runs/40492839892) attempt 1 为 10/10 jobs 成功。Linux、macOS、Windows 2022/2025、Windows 11 ARM64 和 Python 3.14t 六条线的 C-Two/workflow SHA 均为 `9f9d4de3`，FastDB 为 `abba785e`。六线合计 147/147 适用 gate；每线 Rust/Python portable 为 18/18（直接 IPC 9、relay 9），Node 为 12/12（四条路径各 3）。五条 full 线的 IPC memory matrix 各 9/9。完整 relay、原生与公开 SDK、标准用户消费、安装 wheel 的 native 架构检查均包含在适用门禁内。ARM64 收据明确为 Windows 11 ARM64、win-arm64 和 aarch64 Rust，非 x64 模拟结果。

Unix 三条新增 namespace/gate 回归在 Linux/macOS 实际执行；Windows 仅这三条按精确节点、setup 阶段和固定平台原因跳过。三个 Windows 平台的公共 getter、空 Host、启动失败无凭据、CoreError、生命周期和 native query 均实际通过；Host 又独立核对了每平台 3 个含新凭据断言的公共调用。严格测试入口的 95 项回归通过，1 项为本机不适用的 Windows npm 测试；最后修正后的 32 条平台清单正负例通过。

Windows 开发机另运行两条 c2-server 生命周期用例（各实际 1 项）和真实 Core Host/Named Pipe 凭据探针，均通过。它使用 `63c7fef0`，与最终候选在 core/sdk/cli 上逐字相同；后续只有测试入口清单变化。实机为 Windows 11 build 26200、rustc/cargo 1.99.1；确认 kernel-managed schema 1、reap 不适用与终态凭据 None。本地和远端源归档 SHA256 一致。

前三轮 CI 的失败均保留：`40479799392` 暴露 Node 对旧默认目录的断言；`40482123092` 暴露独立 CoreError 字段清单遗漏和 backlog fixture 仍期待旧 AddrInUse；`40488080595` 暴露 Windows 严格 runner 未登记三条 Unix-only setup skip。分别在 `2ec672e2`、`63c7fef0`、`9f9d4de3` 修正。backlog 在同一 macOS 上对照基线/候选均实际得到 ConnectionRefused，原始 listener 没有 managed lease，因此新合同应为 MissingOwnership/InvalidData；原一秒期限、socket 现场身份和不删除断言均保留。没有通过放宽期限、吞异常、泛化跳过或重复同一候选碰运气验收。

本机中途发现共享 Cargo target 复用了基线测试二进制，那轮结果作废；清空后用独立 candidate-target 重建并得到上述定向证据。CLT 的 C++ 头搜索问题用本次命令级 SDK include 路径处理，未修改全局工具链。Windows 初次执行因 PowerShell 将 Cargo 正常 stderr 当错误中断；改用普通进程重定向后才产生正式通过收据。

独立审查覆盖原生身份/格式、watch 与 Python 投影、固定最终源码、测试合同和文档，未遗留阻塞问题。收尾新增的两处源码差异仅为公开 docstring 与用户作用域注释；Python 去 docstring 后 AST 相同，不改变已验证行为。README、CLI、Rust/Python SDK、API/配置/生命周期、Changelog、未发布说明与清单均已对齐；历史设计加现行文档指引，历史发布事实不改。

相对 `c9298e26`（与批准基线生产代码相同），生产 +376/−285，净 **+91**；测试及验证入口 +866/−173，净 **+693**；文档及验收记录 +299/−47，净 **+252**，合计净 +1,036 行。按 Git 行数包含空行/注释，Rust cfg(test) 模块和验证 runner 计入测试；最后公开 docstring 补充占生产净量 1 行。测试增量高于原方案 220–380 行估算，主要用于独立 golden、持久/现场身份反例、完整绑定分类和真实生命周期/gate 竞争覆盖。本批属于正确性与公开能力补齐，不计纯瘦身收益，不宣称测得性能提升。查询复用 watch，不新增文件 I/O 或存活标记。

本机本轮隔离根 socket/lease 为 0，随后仅删除该根剩余的本批协调文件和目录。Cargo 产物、独立环境、基线临时源码、源码归档与本次生成扩展已清理；Windows 专用目录经进程检查和独立 Test-Path 复核已删除。CI Python 各阶段、Node 与 wheel consumers 均有清理确认。日志和精简[机器证据](evidence/2026-10-27-unix-namespace-identity-acceptance.json)保留，未将历史默认目录作为扫描或删除对象。本批 fork CI 分支按精确 SHA 租约删除，本地分支和标签不动。C04、R35、两份会话重构及更广 R17 项继续暂缓。

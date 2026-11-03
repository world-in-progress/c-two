# R13 inline 响应所有权移交验收

基线为 `3e575acbb4c30414f06caaeb7e6e8c84e503ff3c`，实现提交为 `1ac341506acbafcd28382deae291359abbf03aa9`，对应[批准方案](2026-10-23-r13-inline-response-ownership.md)。本项只移动 Core 已拥有的 IPC inline Vec；SHM 和分块仍走原 ResponseLease。公开 API、C/FFI ABI 5、IPC 12、SHM 3、owner control 1、业务帧及 CRM 哈希均不变。R35 保持暂缓，该独立批次未发版，现已随 [0.9.0](2026-10-25-090-windows-release-acceptance.md) 发布；本记录中的协议编号对应当时的独立实现基线。

## 实际增量与收益

| 范围 | 新增 | 删除 | 净变化 |
| --- | ---: | ---: | ---: |
| 生产 | 15 | 8 | +7 |
| 测试 | 30 | 0 | +30 |
| 生产与测试 | 45 | 8 | +37 |
| 文档，含 R35 暂缓及本记录 | 165 | 6 | +159 |

按基线至本轮收尾的物理行统计，含注释与空行。实现候选的文档净增 101 行，其中 R35 暂缓方案 49 行；其余收尾文档另计。本项是运行时成本优化，不计为代码净减。新增的唯一生产方法是具体的私有响应构造分支，没有新状态机、依赖、配置或框架。每个非空 inline 响应省一次 payload 分配、一次 N 字节复制和一份重复数据；Rust owned、HTTP、SHM/分块不宣称本项收益。

macOS arm64、rustc 1.91.0、CPython 3.13、相同依赖与 release 参数下，基线和候选各自构建；原生库及可执行文件 SHA-256 随诊断保存。先做 4 轮 ABBA，再因真实调用波动追加固定 16 轮 ABBA BAAB ABBA BAAB；每版 8 轮，保留所有结果，没有挑选最快样本。构造测量为 31 × 256 次；显式地址的 IPC 对照中，服务和客户端位于同一进程但实际经过 OS 端点；Rust 每模式 9 × 128 次，Python 每模式 9 × 512 次，均先预热并验收关闭。比较既有 lease/owned 构造器的微基准与实际 Core/Python IPC 分开记录；永久单测另直接调用生产使用的私有构造器。

| inline payload | 基线构造中位数 | 候选构造中位数 | 变化 |
| --- | ---: | ---: | ---: |
| 64 B | 51.9 ns | 14.0 ns | −73.0% |
| 1 KiB | 95.5 ns | 31.2 ns | −67.3% |
| 16 KiB | 555.0 ns | 23.8 ns | −95.7% |

真实非空 IPC 的中位变化为 Rust held +0.5% 至 +6.4%、Python ordinary −1.8% 至 +5.6%、Python held −1.7% 至 +3.7%；未改变分配策略的 Rust owned 对照也有 +3.7% 至 +9.2% 波动。各版运行区间重叠；同一非空 payload 在不同交替区组中既出现变快也出现变慢。首组部分 Python 样本慢 12%–26% 的现象没有在扩大对照中稳定重现。本次没有测得稳定的端到端提速，不能把构造倍率当作 RPC 吞吐收益，也没有测量或宣称 RSS 降幅。确定收益由旧路径失败、新路径通过的非空 Vec 地址连续性和源代码共同证明。

## 回归与审查

基线新增内容、空响应及幂等释放检查通过，既有 lifetime ordering 5 项通过；要求非空 Vec 地址保持后，旧生产路径稳定失败于 `inline bytes must move`。候选同一断言通过。SHM/分块收到响应后立即进入原租约，期间没有新增 await、用户回调或可失败处理；保留请求/响应快照、池身份、预算计费、迟到结果清理及失效 → transport → connection 释放顺序。

| 环境与范围 | 结果 |
| --- | --- |
| macOS Core 新增单测及调用/期限/生命周期 | 1 + 39 passed |
| macOS 原生 Python 快照及逃逸 writer | 4 passed |
| macOS Python 定向 | 29 passed，无 skip |
| macOS Rust/Python 跨语言 | 25 passed，18/18 direct/relay receipt |
| Windows 11 x64 开发机 Core | 1 + 39 passed |
| Windows 11 Python 定向 | 28 项首次通过；CLI 构建完成后单独补跑原先跳过的 1 项，1 passed / 0 skip |
| Windows 11 完整 relay | Rust 412、Python 54 passed |
| Windows 11 Rust/Python/Node 及持久连接 | 41 passed，无 skip；18/18 portable 与 12/12 TypeScript receipt |
| GitHub Actions 确切候选 | [39986881996](https://github.com/Dsssyc/c-two/actions/runs/39986881996)，同一源码第 2 次执行后 10/10 通过；Linux、macOS、Windows 2022/2025/ARM64 及 3.14t |

Windows 开发机使用 Rust 1.99.1、Python 3.12.13、Node 22.23.4，实际 HEAD 与候选一致。29 个日志及 receipt 文件已取回，逐一校验长度与 SHA-256；矩阵通过仓库既有严格 receipt 校验器。完整 CI 使用原测试入口，测试时限和断言均保持。首次 ARM64 运行的 PowerShell 安装器架构识别测试发生 30 秒等待超时，Core、原生桥及完整 Python/relay/跨语言仍通过；依赖该工具测试的内存矩阵未执行。安装器和该测试与基线逐字节相同，也不调用 C-Two。独立分析确认存在内外两个 30 秒期限相撞的诊断/清理窗口问题，但没有证明原内部停顿原因。随后同一 ARM64 runner 对基线/候选的 pwsh、System32、SysWOW64 六个原样入口均通过；首次 SysWOW64 为 15.359 秒，随后 3.671 秒，额外空 host 与分阶段探针也完成。这些对照未复现原 CI 内部卡点，不把它记为已修复。保留首次失败，仅补执行失败 ARM64 任务及汇总检查，其他平台保留原通过结果。该次 ARM64 源码任务最终通过，工具测试为 66 passed / 2 skipped，原先未执行的 IPC memory matrix 也通过；两个汇总门禁随后通过。

本机首次 Rust relay 验证错误地给整组默认命名空间夹具设置了自定义 C2_IPC_ROOT，产生 349 passed / 68 failed：夹具的默认端点与环境解析出的 relay 目录不同，触发连接失败或 namespace 409。该 crate 及其 transport/foundation/protocol 依赖和 Cargo 输入与基线完全一致，且不依赖本次变更的 c2-core。此轮失败不记为通过；完整原样 relay 由干净 CI 和 Windows 开发机验证。此次调用创建并遗留在默认目录的 30 个测试端点已按本轮 PID、逻辑地址、owner record 与原生 credential 精确 reap，socket/lease 为 0；其余 25 个既有端点的身份及时间戳前后相同，没有清理历史对象。后续本机检查只使用本批隔离目录，收尾也为 0。

原生快照的首次私有调用用了错误过滤器，实际收集 0 项；修正过滤器后又发现直接启动嵌入式 Python 缺少解释器路径。最终复用仓库 `tools/dev/test_native.py`，4 项全部实际执行。Windows 的一次跳过源于私有脚本先测后构建 CLI，补齐先决条件后只补跑该项，没有重复已通过测试。这些验证接入错误没有改变生产实现或测试。

hey-my-buddy 0.52.0 独立实现审查绑定同一候选：run `7b938442-8e6a-4f7a-a91d-4fd3d33dfc86`，artifact `f70b39d6-da48-4233-aac8-1d6ac2e847eb`。审查核对完整 diff、取消窗口、owned 等价路径、连接保活、四种载体的迟到释放、Python 投影及地址断言，无 P0–P3 问题。Host 已复核并接受报告，审查 worktree 已回收；测试和性能由 Host 单独验收。

安装器调查的独立只读分析同样使用 hey-my-buddy 0.52.0：run `8901690f-57c8-4d8a-85de-688db2e30e2a`，artifact `0f797807-55dc-4d19-9006-268b523eb09d`；Host 已接受证据边界并回收 worktree。[受控诊断 39996058930](https://github.com/Dsssyc/c-two/actions/runs/39996058930) 只运行安装器对照，未编译 C-Two，也不是完整源码门禁的替代。原安装器超时的内部卡点保留为未定位事项，本批没有改安装器或测试框架。

## 收尾

原始日志、失败记录、性能程序、全部样本及哈希保留在 `~/.codex/diagnostics/c2-r13-inline-6msr2d9q/`。本机本批 Cargo target、临时原生库、FastDB CoreSDK、Go 构建缓存、下载包和隔离端点目录已清理；后续本地原生测试需重建。Windows 本批 D 盘工作区及临时目录已删除，回收 3,657,622,036 个逻辑字节，保留原 Rust 工具链。GitHub 推送范围限于 fork 的本次 CI 候选和受控诊断分支；没有推送 main、修改标签或发布包。实现提交与 CI / 审查候选相同，之后仅补验收文档。

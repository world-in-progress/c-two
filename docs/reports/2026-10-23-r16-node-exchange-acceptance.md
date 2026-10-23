# R16：Node 收发与空闲背压验收

基线为 `764d39b03971119bc73301eb1d4d58465dce7649`，实现候选为 `02be11ea9ed0f8254838c39f42003c75891d86d7`。[Source Validation](https://github.com/Dsssyc/c-two/actions/runs/39892443324) 的全部 10 个作业通过，包括 Linux、macOS、Windows 2022、Windows 2025、Windows ARM64 和 Python 3.14t。运行 attempt 为 1；候选未通过重复重跑取得验收。ARM64 的 26 个验证步骤及安装后的普通/标准用户验收通过，FastDB、C-Two 原生库和 c3 的 PE machine 均为 `0xAA64`。最终提交相对候选仅补充文档和验收记录，生产与测试字节一致。该次独立验收时未发版；随后已随 0.8.2 交付，见[发布记录](2026-10-23-082-client-observation-release-acceptance.md)。

实现复用原帧解析、关闭、chunk 拼接和 SHM 释放。chunked 请求在上传期间接收回复；失败先保存主错误，关闭连接，等待收发双方结束，归还已校验但未交付的 SHM 回复。逐片早停使用已有 connectionPromise 与 callChain，不增加停止状态机。成功连接继续复用；失败调用不重放，下一次调用重新建连并保留 server instance、route UID/revision 校验。Node 仅在 readExactly 需要更多数据时 resume，每次收片后 pause，保留大读取、并发读与 EOF 尾部行为。

用户已接受低层 TypeScript close 必需和 chunked 失败后丢弃连接，以及并行失败保留先确认主错误的语义；注入 Node socket 必须提供 pause/resume。Rust API、IPC 12、SHM 3、C/FFI ABI 5、CRM 哈希和业务帧不变。没有 writableLength 补丁、兼容入口、常驻接收任务、第二个 pending 表或新框架。此项限制空闲预读，不宣称连接或用户持有结果具有总内存硬上限。

## 实际增减

| 分类 | 新增 | 删除 | 净变化 |
| --- | ---: | ---: | ---: |
| 生产 | 152 | 177 | −25 |
| 测试与夹具 | 307 | 88 | +219 |
| 文档与验收记录 | 135 | 7 | +128 |
| 合计 | 594 | 272 | +322 |

生产与测试合计净增 194 行，按正确性及背压修复记账，不计作纯瘦身净减量。生产删除重复发送/观察路径，测试增加提前拒绝、关闭交错、精确释放及真实持久连接反例；原 Node 两项分散读取测试合并为 EOF/显式关闭两组，保留原断言。实验脚本没有进入生产或测试框架。

## 回归与独立审查

先在基线补测并确认失败，再调整生产。覆盖提前拒绝、错误 id/flags、接收先失败、写入失败与成功回复交错、buddy/dedicated 未交付回复精确释放、非法描述符不取得释放授权，以及写回调与 close 同轮成功兑现时不得再发下一片。生成合同 Rust 测试 7 项通过，内含 21 个 Node 收发场景。

| 实测环境 | 官方 Node 全套 | Rust 完整 relay | 跨语言 |
| --- | --- | --- | --- |
| macOS arm64 | 61 通过，1 项既有 Windows 专属跳过 | 416 通过 | 39 通过，真实持久连接 2 通过 |
| Linux arm64 容器 | 61 通过，1 项既有 Windows 专属跳过 | 416 通过 | 同候选 Linux Actions 完整门禁通过 |
| Windows 11 x64 开发机 | 59 通过，3 项既有 Unix 专属跳过 | 411 通过 | Rust/Python 25、Node 14、真实持久连接 2 通过 |

macOS Python relay/helper 共 54 项通过，其中导出源码缺少 .git 而跳过的元数据检查在原工作区单独补齐。Windows relay/helper 为 53 通过、1 项导出源码的 Git 元数据跳过，真实 hey benchmark 已执行；托管作业从真实 Git 检出运行完整检查。各平台新增背压/关闭场景均未跳过。macOS 与 Windows 的收据分别包含完整 18 行 Rust/Python 和 12 行 Node direct/relay 矩阵。

独立审查首次指出：被关闭的 pending write 若同轮成功兑现，发送循环还会尝试下一片。Host 先补出 writesAfterClose=1 的反例，再用原连接状态加逐片 guard 修复。随后两次冻结复核覆盖完整生产差异和最终三处跨平台测试调整，均无 actionable findings；审查 run 分别为 `810bd31d-a30e-49e1-9adc-04a0873c4c51` 与 `5171c930-223a-4248-8439-6d5d9a53be97`。真实连接夹具和性能实验的 buddy 产物均由 Host 核对后验收，工作树已回收。

## 同环境性能

macOS arm64、Node 25.8.1、Python 3.13.3、TypeScript 5.9.3。两组共用同一对 Rust/Python 服务和配置，只替换基线/候选 TS transport 与 Node connector。六轮按 AB/BA 交替，每组每轮预热小请求 100 次、大请求 5 次、拒绝 1 次，正式样本分别为 500/30/8。4 MiB 成功输入采用现有 Python-only pickle 编码，Python 核验完整内容；实验没有新增 codec API。Host 从原始样本独立重算了样本数和中位数。

| 场景 | 每组样本 | 基线 p50 / p95 ms | 候选 p50 / p95 ms | 合并吞吐变化 |
| --- | ---: | ---: | ---: | ---: |
| 小成功调用 | 3,000 | 0.2073 / 0.2938 | 0.2064 / 0.3131 | −6.92% |
| 4 MiB 成功输入 | 180 | 10.4806 / 12.8372 | 10.5861 / 13.2670 | −4.10% |
| 64 MiB 提前拒绝 | 48 | 255.8843 / 295.2206 | 0.3704 / 0.5625 | 不当作成功传输吞吐 |

两组成功调用保持一个连接，写入帧数与字节数相同。拒绝时基线完整完成 16,384 个 chunk 写入，候选完成 2–12 个，均值 4.15，另有一帧在途写入被中止；这些是发送回调完成量，不能冒充对端精确收到的字节。基线拒绝后的第一次调用 48/48 被残留回复污染，第二次显式调用才恢复；候选第一次后续调用 48/48 重连成功，恢复后继续复用。所有调用收尾时在途及关闭后写入均为零，服务端拒绝没有执行资源方法。

正常成功的中位延迟接近基线，但合并吞吐下降、尾延迟增大；各轮方向变化，主机未做 CPU 隔离且同时有其他验证任务，因此不宣称正常路径稳定加速或稳定回退。收益证据集中于提前拒绝、停止无效上传与下一调用恢复。未将这些数据推广到 SHM 性能、HTTP、大响应、并发流水或 Windows 性能。

## 隔离、环境与清理

macOS 全 relay 使用源码导出，仅替换本轮 socket/coordinator 默认命名空间；manifest 确认其余文件与候选一致。最初混用显式根和低层默认构造导致环境失败，并在默认命名空间留下 5 个本轮端点；按本轮日志确认 16 个逻辑地址后，11 个已不存在，5 个凭原生身份凭据精确 reap。历史对象没有作为清理目标。最终本轮 socket/lease 为零，Linux 容器同样为零；Windows C-Two Named Pipe、lease 文件及本轮进程均为零。托管 CI 检验未替换命名空间的确切候选。

Windows 开发机的 Node 22.12 只支持 Node-API 9，不能加载原有 Node-API 10 插件；同一插件换为本轮私有 Node 22.23.4 后加载成功。系统链接的 Python 开发扩展按 DLL 查找规则放置匹配 FastDB DLL；缺失的 hey 按既定 v0.1.4 构建。另确认 LongPathsEnabled=0 时普通路径打不开已存在的 264 字符生成文件，改用本轮短临时目录后 14 项 Node 矩阵通过。均为测试环境调整，没有更改机器全局 Node/长路径策略或生产实现。新增背压测试也改为验证真实 socket.isPaused，避免假定暂停后 Named Pipe 的 bytesRead 仍会增长；修订断言在 macOS/Windows 基线均红、候选均绿。

macOS/Windows 清理的本轮构建、安装副本与临时目录逻辑文件体积约 4.9 GiB，不能等同于物理磁盘释放量；Linux 本轮容器已自动删除。Cargo target、native 库、Node dist/依赖、实验构建和临时安装包已删除，Windows 用户工具链保留。主工作区 .venv 中指向临时导出的 c-two editable 安装已卸载，后续使用 uv sync 重建。源码、命令、失败与成功日志、矩阵收据、性能原始样本及来源哈希保留于 `~/.codex/diagnostics/c2-r16-implementation-7q0v09ad`。

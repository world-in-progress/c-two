# 旧 Python ShmBuffer 投影退役验收

状态：本机回归、GitHub Actions 全部门禁、端点验收与独立审查完成。基线为 `723a9130d9a26d9903d56226b6170f4143a19a29`，固定实现候选为 `fa62f7a3f5227626a49066d1ae51dacb88a2741a`。[Source Validation](https://github.com/Dsssyc/c-two/actions/runs/39905696790) 全部 10 个作业通过，运行 attempt 为 1，未靠重跑取得通过。最终提交相对该候选仅更新方案、清单和验收文档，生产与测试字节一致。该次独立验收未单独发版；随后已随 0.8.2 交付，见[发布记录](2026-10-23-082-client-observation-release-acceptance.md)。

## 实现与边界

删除 335 行孤立的 `shm_buffer.rs` 及三行模块声明/注册，退役低层 Python `c_two._native.ShmBuffer`，不留别名或兼容入口。原构造调用已在 `6e6519a8` 的 Core 迁移中删除，本次未改变运行中的数据路径。

请求继续由 Core 创建快照，再将同一 `Vec` 移入 `CoreRequestBuffer`；原 transport lease 保留到服务返回或展开。响应继续使用 `HeldResponse` / `ResponseBuffer`。`core_ffi.rs` 的生产部分与基线逐字节一致，差异全部在测试区。Rust API、C/FFI ABI 5、IPC 12、SHM 3、CRM 哈希、业务帧与释放语义保持。R06/R07 仍独立保留，旧投影删除不计作其正确性问题已解决。

同步修正 AGENTS.md、当前架构参考、native dispatch 与 peer pool 注释，并在 changelog 记录低层 Python 导出删除。历史 SHM 直映验证保留其阶段语境，不将其表述为现行 Core 适配器的零拷贝证明。没有移除仍有消费者的 Connection 方法或 Cargo 依赖。

## 实际代码增减

| 分类 | 新增 | 删除 | 净变化 |
| --- | ---: | ---: | ---: |
| 生产 | 5 | 343 | −338 |
| 测试 | 26 | 24 | +2 |
| 生产与测试 | 31 | 367 | **−336** |
| 文档与验收记录 | 142 | 73 | +69 |

按物理行计数，含注释和空行；`core_ffi.rs` 的 11 行新增全部计入测试。文档与验收记录另行统计。没有新增运行时抽象、后台任务或状态机；不宣称吞吐、复制次数或二进制体积改善。

## 基线与回归

先在保留旧模块的基线上补测：请求快照 1 项、Python tracker/真实响应 5 项通过。原 request 测试的 Vec 地址、正常/异常返回后 view 寿命断言保留，并加入 retained 统计、导出期间拒绝释放、归零和重复释放。真实 response 用例覆盖小载荷与 1 MiB 载荷，替换一项重复 tracker 用例和一项 hasattr 检查。

| 环境 | Native | Python 定向 | 完整 Rust relay | Python relay/helper | 跨语言及持久连接 |
| --- | ---: | ---: | ---: | --- | --- |
| macOS arm64 / Python 3.13 | 8 | 32 | 416 | 54 | 39 + 2 |
| Windows 11 x64 开发机 / Python 3.12 | 8 | 32 | 411 | 53，1 项导出源码缺少 Git 元数据的检查跳过 | 39 + 2 |

两端均取得完整 18 行 Rust/Python 和 12 行 Node direct/relay 收据。macOS 将导出源码的 Git 元数据检查放回真实 checkout 单独补齐；托管 Windows 门禁从 Git 检出运行该检查。macOS 和 Windows 本地采用经仓库脚本校验的官方 FastDB 0.2.3 CoreSDK；原生完整发行构建以 Actions 收据为准。

已逐份核验六个 CI 源码收据：Linux 与 macOS 各 25 个步骤，Windows 2022、Windows 2025、Windows ARM64 各 26 个步骤，Linux 3.14t 为 16 个步骤，全部通过。来源均为上述 C-Two 候选及 FastDB `abba785ef2a7d3fbee1bbc608393a3e6c7b042c4`。ARM64 的安装后普通/标准用户验收均通过；Python 为 `win-arm64`，FastDB、C-Two 原生库和 c3 的 PE machine 均为 `0xAA64`。

基线首次编译遗漏测试中 `std::sync::Arc` 的限定名，已修正；本机 Rust 嵌入式 Python 随后因未定位标准库而在测试执行前退出，给该命令设置解释器实际 `PYTHONHOME` 后通过。失败日志保留，生产删除在基线通过后才执行。Windows SSH 在最终矩阵期间断开，重新连接确认原进程继续完成并留下 `39 passed` 和 `PASS`，未重跑该矩阵。

## 独立审查

文档由 hey-my-buddy 0.51.1 在独立 worktree 中更新，Host 核对三份完整差异后原样集成并验收。原 buddy 实现审查任务在队列中超过 15 分钟且未启动执行，已取消、收束并回收工作树，不计作审查证据。

新的独立 Codex 审查者对 `723a9130..fa62f7a3` 固定差异只读复核，未发现 actionable findings；确认旧模块无仓内消费者、其他 buffer 的生产状态未改、实际生命周期断言保留。审查者没有亲自运行测试。仓内引用检查不证明仓外无人 import 或反射查询已删除名称，该低层 Python API 变化已获用户确认。

## 端点与清理

macOS 在源码导出中只替换本轮默认 socket/coordinator 命名空间，逐文件哈希确认其余内容与候选一致。默认目录、协调目录和按安装日志确认归属的持久连接目录均无 socket/lease 残留。Windows 使用本轮 D 盘目录和短临时目录，验收时 C-Two Named Pipe、lease 文件与本轮进程均为零。历史目录未作为清理对象。

本机两端的 Cargo target、native 安装副本、Node 依赖/构建、FastDB SDK、Go 交叉构建目录及临时可执行文件/包均已清理，Windows 工具链保留。Mac 工作区 `.venv` 中指向本轮导出源码的 editable c-two 已卸载，下次使用需 `uv sync` 重建。仅保留源码清单、命令、原始日志、矩阵收据、独立审查与清理记录于 `~/.codex/diagnostics/c2-shm-retirement-nu3p19cl`；CI 收据按 HTTP byte ranges 提取，不下载整套构建产物。

# R32 端点凭据原生投影验收

实现基线为 `548710a8617a33755d5ad73f62b3271452429874`，已提交实现为 `6065660318fd74161ca873eb99e6a74b5e384a1f`。本记录对应用户确认的[凭据子项方案](2026-10-23-r32-native-endpoint-credential.md)。该独立批次未改包版本，现已随 [0.9.0](2026-10-25-090-windows-release-acceptance.md) 发布。

## 结果与净量

复用既有 PyO3 凭据类，删除 Python 转发壳及 inspect 的 Rust → JSON → Rust 往返；reap 直接接收同一个原生对象。保留外部 JSON 严格接纳、canonical 字节、32 KiB 限制、metadata、document 关键字和 Python repr。已确认的手工包装、子类、槽替换、浅复制及旧低层名称直接退役，不提供兼容入口。inspect 不再承诺 JSON 可导出。

| 范围 | 新增 | 删除 | 净变化 |
| --- | ---: | ---: | ---: |
| 生产 | 25 | 91 | −66 |
| 测试 | 24 | 10 | +14 |
| 生产与测试 | 49 | 101 | −52 |
| 文档，含方案、迁移及验收 | 108 | 2 | +106 |

物理行含注释和空行，按基线至本轮实现及验收文档的 numstat 统计；实现候选内文档净增 61 行，验收收尾文档再净增 45 行。未增加框架或依赖。确定减少每次 Unix present 的一次 encode、一次 decode 和一个 Python wrapper 分配，没有测量或宣称普通 RPC 吞吐提升。Rust API、C/FFI ABI 5、IPC 12、SHM 3、owner control 1、业务帧及凭据 JSON 格式保持。

## 验证

| 环境与范围 | 结果 |
| --- | --- |
| macOS 基线端点 3 文件，新增 document/repr 断言先运行 | 122 passed，5 个 Windows 专属 skip；socket/lease 为 0 |
| macOS 候选相同端点回归 | 122 passed，5 个 Windows 专属 skip；socket/lease 为 0 |
| macOS c2-local | 134 单元 + 9 集成通过；1 个需 root/第二 UID 的已有用例 ignored |
| macOS 完整 Rust relay | 417 passed |
| macOS 额外真实链 | 连接观察 9、持久 Node IPC 2、Rust/Python 跨语言 25 项通过 |
| Windows 11 开发机端点及连接观察 | 97 passed，39 个 Unix 专属 skip |
| Windows 11 c2-local / 完整 Rust relay | 46 / 412 passed |
| Windows 11 完整 Python relay | 54 passed，无 skip |
| Windows 11 Rust/Python/Node 跨语言及持久连接 | 41 passed，无 skip；严格矩阵 receipts 已生成 |
| 确切候选 GitHub Actions | [Source Validation 39963327070](https://github.com/Dsssyc/c-two/actions/runs/39963327070) 全部 10/10 jobs 成功 |

CI 的 head SHA 与实现提交完全一致；六个源码 lane 为 Ubuntu 24.04 / Python 3.12 与 3.14t、macOS 15 / Python 3.12、Windows 2022 / Python 3.12、Windows 2025 / Python 3.12 和 Windows 11 ARM64 / Python 3.12。ARM64 原生源码门禁及两个汇总门禁均已通过，未用 x64 仿真结果代替。既有完整 relay 和 Rust/Python/Node 互操作门禁保持原测试入口。

Windows 开发机使用 Python 3.12.13、Rust 1.99.1、Node 22.23.4；实际 HEAD 与候选相同。28 个日志/receipt 文件已取回并按 SHA-256 逐个验证。最终本批进程数 0，C-Two Named Pipe 数 0。macOS 各本批隔离目录的 socket/lease 也均为 0。

首次本机完整 Python relay 使用额外的自定义 C2_IPC_ROOT，手写 HTTP 登记用例没有携带该域要求的 namespace，得到 11 failures / 13 setup errors；同一隔离设置下的 TypeScript 矩阵有 3 个预期 IPC 路径断言及其汇总 receipt 失败。其余测试保持原断言。0.8.2 官方 wheel 与候选的受控登记探针均在缺少 namespace 时返回相同 409，加入既有 namespace 头后均返回 201，且两者收尾零残留；登记、选路及生成器源代码没有改动。完整原样默认命名空间回归已在干净 macOS/Linux/Windows CI 通过，没有改断言、添加 skip 或接触本机历史目录来取得通过。本机失败记录保留，不计作通过。

Windows 准备阶段曾将 git 正常 stderr 误作终止异常，已修正私有运行脚本的退出码处理；固定 Go 下载无数据后停止对应进程，改用 hey v0.1.4 源码交叉构建并校验 Windows 可执行文件。Node 官方归档也校验 SHA-256。上述准备调整没有改动候选或测试。

## 独立审查与收尾

hey-my-buddy 0.51.1 的独立实现审查绑定上述候选：run `b9e8aad3-adcc-4d64-a8d4-616576887bb4`，artifact `f9cf60a2-1cd3-4476-8aa6-f1870dbb8052`。审查覆盖直接调用方、公开形状、错误/回收顺序、Windows KernelManaged、文档及原断言，无 actionable findings。Host 已核对固定报告并验收，审查 worktree 已回收。

诊断、脚本和 receipts 保留在 `~/.codex/diagnostics/c2-r32-c09fxvzf/`。Windows 本批隔离根目录及临时目录已删除，回收 2,745,387,816 个逻辑字节，保留原 Rust 工具链；本机 Cargo target、Node 构建目录、Go 临时缓存及包归档已清理。验证所用本地原生库、临时 FastDB CoreSDK 和基线 wheel 环境也已清理；下次本地原生测试需重新构建。实现提交与通过 CI 的候选相同，随后仅补验收文档。历史端点和其他任务 worktree 保持。

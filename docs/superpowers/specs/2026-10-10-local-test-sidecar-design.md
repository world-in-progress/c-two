# macOS / Windows 本地测试旁路模式设计

日期：2026-10-10。目标仓库：`D:\c-two-test\c-two`，基线提交 `8936c5c98745a87f3c8fbb1e08782228585a519b`。

## 目标与推荐方案

为 macOS 与普通 Windows x64 开发账户提供可检查、可准备、可复跑的本机测试入口，并继续使用现有 CI 发布验证流程。这里的“旁路模式”定义为开发者显式调用的本地入口：复用现有测试用例和严格验证逻辑，独立准备开发环境、运行测试与输出报告，不接入发布门禁，也不绕过测试断言。

推荐新增 `tools/dev/test_local.py` 作为共享本地旁路入口，从现有 macOS/Unix 入口 `tools/dev/test_python.py` 提取公共执行与证据校验逻辑，再接入 Windows 平台后端。原 macOS 调用方式和 pytest 插件路径保持兼容。第二种方案是分别维护 macOS 与 Windows 脚本，改动较直观，但前置检查和证据校验容易分叉。第三种方案是将本地与 CI 完全合并，会扩大对发布收据和标准用户验证的影响。本次推荐共享本地引擎与平台后端的方案。

## 现有代码融合边界

现有 `test_python.py` 是 Unix 本地 Python SDK 测试入口，包含普通测试并行、portable 与 TypeScript 矩阵串行、完整收集分组校验、实际执行校验、收据验证和独立证据目录。以上逻辑复用，不能降低 Windows 的验证标准。它当前不负责依赖安装，也不覆盖全部 Cargo/npm/repo 套件；新增入口的准备动作与全套调度需补充实现，不能将现有 Python 入口描述为全仓库测试工具。

公共层负责参数、套件计划、工具能力检查、构建产物散列、执行记录与报告。平台层负责可执行文件定位、动态库环境、资源锁、子进程树启动与清理。macOS/Unix 后端沿用 flock 与进程组；Windows 后端使用 Windows 文件锁与限定到本次启动进程树的 Job Object 管理，不能用宽泛进程名杀进程。Windows 后端清理无法确认时，该阶段不能报告通过。平台实现采用延迟导入，使公共层可以在两个系统导入和测试。

现有 macOS 入口仍要求预建 Python native 与 c3；新增旁路入口明确区分 `check`、`prepare` 与 `run`，不让单纯运行测试隐式安装工具。普通组保留显式 workers 参数，矩阵始终串行；Windows 首次验证采用 workers=1，确认资源隔离后再验证并行模式。平台跳过采用明确用例与原因清单，环境缺失、崩溃、未知 skip 和未完成清理均不能算成功。

## 开发机入口

入口提供三个动作：`check` 输出所选测试套件的前置条件与修复命令；`prepare` 在基础工具已安装的情况下准备项目依赖、官方 FastDB CoreSDK、本地原生扩展、c3 和 TypeScript 测试产物；`run` 串行运行指定套件并写入独立日志与 JSON 汇总。支持 `core`、`cli`、`rust-sdk`、`native`、`python`、`typescript`、`node`、`repo` 和 `all`。默认根据本机选择 macOS/Unix 或 Windows 后端，日志记录实际选择。环境不足时返回非零退出码，并列出无法运行的套件与具体原因。该接口已实现，实际调用与准备路径见 `docs/local-development-testing.md`。

普通测试套件只检查实际需要的依赖。完整套件检查 Windows x64、Python 3.12、Rust MSVC 工具链、uv、Git Bash、CMake、Ninja、匹配的 FastDB 源码与 CoreSDK、Node 22 且 Node-API 至少为 10、TypeScript 5.9.3、Emscripten 5.0.2，以及最低支持版本语法检查所需的 Python 3.10。基础工具缺失时提供安装或启动 Developer PowerShell 的说明；已安装工具可通过显式路径参数或会话环境选择。

构建与测试按依赖顺序执行，避免测试期间并发 relink 或 npm 构建。每次运行使用新的输出与 pytest 临时目录；保存工具版本、实际可执行文件路径、源码提交、构建包散列、各套件命令、退出码、通过/失败/跳过数量及准备阶段错误。报告记录平台跳过的原因，准备失败不会算成测试通过。

## 普通账户与产物一致性

TypeScript 测试使用已有的三个 npm tarball 配置入口，在临时测试工程中安装本地包，避免将目录符号链接作为开发机必需条件。源码模式的 persistent IPC 夹具同样支持普通账户，通过本地打包安装或平台可用的链接方式建立依赖，并保持现有候选包模式。

FastDB WASM 在独立构建暂存目录中生成；JS 与 WASM 必须来自同次构建并一起打包。开发机测试将明确选择这些本地包，不将 Git 中的旧 JS 与新 WASM 混用。准备失败停止依赖它的测试，并保留日志。运行时前置检查在加载原生模块前验证 Node-API 版本，对不兼容版本提供明确错误。

子进程统一采用 UTF-8 输出，清除测试需要隔离的 C2 环境变量，并按 Cargo 工作区选择 FastDB source/system 链接模式。Windows CoreSDK 的 DLL 路径和 macOS 动态库路径仅在测试子进程环境中设置；匹配系统与架构的 SDK 校验复用现有准备脚本。

## 已确认的 Windows 兼容性修复

本地注册表扫描同时覆盖 POSIX 与 Windows 路径表达，并增加反斜杠与混合分隔符回归测试。POSIX 安装脚本与发布流程测试生成的脚本、校验文件明确写入 LF；通过 Git Bash 运行时采用适当的 shell 路径和当前 Python 解释器，避免假设 Windows 上存在 `python3` 或使用 Unix PATH 拼接。这些修复保持 Linux CI 原有断言和错误分支的覆盖。

不将偶发 owner lifecycle 失败直接改成跳过或增加任意 sleep。保留原用例；若在规范环境中再次失败，收集服务状态、进程退出和连接阶段日志，再定位原因。

## CI 支持与验证

现有 `tools/ci/windows_native.py` 的 scope、源码与包散列验证、发布收据、标准用户验证和 required gates 保持其语义。公共 Node-API 检查或测试夹具修复可以被 CI 使用；开发机报告不会被当成正式发布证据。新增本地入口不要求 GitHub Actions 的环境变量或管理员账户。

先增加能复现现有问题的测试，再实现兼容性修复。验证包括：Node 22.11/Node-API 9 被提前拒绝；Node 22.16/Node-API 10 正常执行；普通账户不需符号链接权限即可运行 persistent IPC；修正后的 POSIX 安装与发布流程测试以及注册表扫描测试通过；14 项 TypeScript 真实调用测试通过；共享入口依赖检查、错误退出和报告汇总有自动化覆盖；现有 Windows CI runner 回归测试通过。现有 runner 测试中的纯逻辑用例解除整体 POSIX 限制，平台专用用例分别验证资源锁竞争、超时、取消与孙进程回收。保留原 macOS 命令接口的回归覆盖。

当前可实际执行的机器是 Windows，不能将模拟平台测试或静态检查写成 macOS 实机验证。交付记录区分 Windows 实测、跨平台公共逻辑覆盖、仍待 macOS 实机执行的项目；现有 Unix CI 可运行其后端回归，但不能代替 macOS 实机证据。

实测补充的生命周期规则：MSVC 编译器会启动持续存活的 vctip 辅助进程，不能将任意遗留进程直接忽略。仅当父进程已退出、所有剩余进程均属于本次 Windows Job，并且映像完整路径匹配所选 cl 编译器同目录的 vctip.exe 时，执行限定到该 Job 的回收、确认清空并记录映像/PID；其他遗留进程仍使验证失败。该规则以真实编译复现和四项路径/作用域回归验证为依据，不依赖未经验证的环境变量开关。

## 交付与执行顺序

第一阶段提取现有 macOS/Unix runner 的共享执行与验证层，保留原入口接口和严格校验，以原 runner 回归通过为出口条件。第二阶段完成 Windows 后端、已确认的平台兼容性修复和普通账户依赖准备，以 TypeScript、persistent IPC、资源锁和进程树清理验证通过为出口条件。第三阶段补齐共享旁路入口的套件调度与两个平台的文档，运行全仓库相关套件及 CI runner 回归，明确记录实测范围与剩余问题。实现前将执行步骤保存到仓库的 `docs/superpowers/plans`，实现过程中保持步骤与验证结果同步。

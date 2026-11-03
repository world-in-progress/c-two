# Windows 安装与开发构建

本指南说明 C-Two 原生 Windows 后端的安装、使用、源码构建及验证证据。

[English](windows-native-usage.md) · 简体中文

该后端使用字节模式 Named Pipe 传输 `ipc://` 控制流量，并使用具名文件映射存放共享载荷内存。它跨登录会话使用进程的 TokenUser SID。同一会话的对端可以使用 SHM；不同会话使用自有缓冲区、内联或分片传输。原生连接在发送数据前要求用户一致，且服务端完整性级别至少为 Medium。普通 SDK 代码继续使用逻辑 `ipc://` 地址；不要在 SDK 中构造管道或映射名称。HTTP relay 仍提供网络传输路径。

本指南涵盖 Windows x64 和 ARM64 上的 Python C-Two **0.10.1**、c3 **0.6.0** 和 FastDB **0.2.4**。请配套安装对应版本的 Python 包与 c3；端点行为见 [0.10.0 迁移说明](../releases/0.10.0.md)和[生命周期指南](local-endpoint-lifecycle.md)。

Windows 拒绝 Unix 根目录覆盖值（`cc.endpoint.set_local_endpoint(root=...)`、`C2_IPC_ROOT`、`--ipc-root`）。管道名称由逻辑地址和当前 TokenUser SID 派生。

## 安装 Python 包和 c3

在已激活的 Python 环境中明确指定文档所列版本：

```powershell
uv pip install 'c-two==0.10.1'
```

对于已发布的 [c3 0.6.0 版本](https://github.com/world-in-progress/c-two/releases/tag/c3-v0.6.0)，下载 `c3-x86_64-pc-windows-msvc.exe`（x64）或 `c3-aarch64-pc-windows-msvc.exe`（ARM64）、对应的 `.exe.sha256` 校验文件，以及可选的 `c3-installer.ps1`。运行独立可执行文件前先验证它：

```powershell
$releaseBase = 'https://github.com/world-in-progress/c-two/releases/download/c3-v0.6.0'
$asset = 'c3-x86_64-pc-windows-msvc.exe'
Invoke-WebRequest -UseBasicParsing -Uri "$releaseBase/$asset" -OutFile ".\$asset"
Invoke-WebRequest -UseBasicParsing -Uri "$releaseBase/$asset.sha256" -OutFile ".\$asset.sha256"
$checksum = Get-Content -LiteralPath ".\$asset.sha256" -Raw
if ($checksum -notmatch '(?m)^\s*([0-9a-fA-F]{64})(\s|$)') { throw 'Invalid SHA-256 sidecar' }
$expected = $Matches[1]
$actual = (Get-FileHash -Algorithm SHA256 -LiteralPath ".\$asset").Hash
if ($actual -ne $expected) { throw 'SHA-256 mismatch' }
.\c3-x86_64-pc-windows-msvc.exe --version
.\c3-x86_64-pc-windows-msvc.exe relay --bind 127.0.0.1:8300
```

也可以使用带校验步骤的安装脚本：

```powershell
Invoke-WebRequest -UseBasicParsing -Uri 'https://github.com/world-in-progress/c-two/releases/latest/download/c3-installer.ps1' -OutFile .\c3-installer.ps1
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\c3-installer.ps1 -Version 0.6.0
$env:PATH = "$env:LOCALAPPDATA\Programs\c3;$env:PATH"
c3.exe --version
```

安装脚本支持 PowerShell 5.1，且不需要管理员权限。自动架构检测使用 [`IsWow64Process2`](https://learn.microsoft.com/en-us/windows/win32/api/wow64apiset/nf-wow64apiset-iswow64process2)（Windows 10 版本 1709 / Windows Server 2016 版本 1709 或更新版本），因此在模拟运行的 PowerShell 中仍会选择原生可执行文件。脚本将验证过的可执行文件复制为 `$env:LOCALAPPDATA\Programs\c3` 中的 `c3.exe`，或复制到 `-BinDir` 指定的目录。它不会永久注册 PATH；示例只更新当前会话。脚本选择原生操作系统架构。ARM64 wheel 支持 Python 3.11–3.14 和 3.14t；x64 还支持 3.10。相同选项见 [CLI 安装说明](../../cli/README.md#windows-x64--arm64)。

同机资源调用使用资源进程的 `cc.server_address()`，并传给 `cc.connect(..., address=...)`。直接 IPC 调用不需要 relay。使用 relay 路由时，在资源和客户端进程中配置 `cc.set_relay_anchor('http://127.0.0.1:8300')`。SDK 不会启动或拥有 `c3 relay`。

使用 `cc.serve()` 的 Python 资源服务端接受 Ctrl+C；自动化子进程监管器可以创建新进程组并发送 `CTRL_BREAK_EVENT`。普通应用也可以在所属进程内调用 `cc.shutdown()` 并等待完成。IPC 管理关闭的确认仅表示开始排空，不构成完成屏障。

## 从源码构建

源码开发使用规范的 main 分支。对应 c3 版本的 `rc-manifest.json` 标识发布源码；使用 main 是开发检出的约定。请使用面向原生 x64 或 ARM64 架构的 Developer PowerShell，并确保 MSVC C++ 工具链、CMake、Ninja、Git、Rust 1.95 或更新版本及 uv 可用。Python 和 Rust 从各自的注册源获取 FastDB 0.2.4；Python 需要可选的 `fastdb` extra。Python 扩展与 c3 使用源码模式的原生构建。Rust 绑定还需要匹配的 Core SDK：从该版本中解压 `fastdb-core-0.2.4-x86_64-pc-windows-msvc.tar.gz` 或 `fastdb-core-0.2.4-aarch64-pc-windows-msvc.tar.gz`，并如下配置其 `lib` 目录的绝对路径。完整的源码互操作检查还使用相邻的 FastDB 发布版本检出作为固定输入和 TypeScript 源码；Python 使用者使用已发布的 wheel：

```powershell
git clone https://github.com/world-in-progress/fastdb.git
git -C fastdb checkout v0.2.4
git clone https://github.com/world-in-progress/c-two.git
Set-Location c-two
$env:FASTDB_PAYLOAD_SYSTEM_LIB_DIR = 'C:\path\to\fastdb-core-sdk\lib'
$env:PATH = "$env:FASTDB_PAYLOAD_SYSTEM_LIB_DIR;$env:PATH"
$env:FASTDB_PAYLOAD_LINK_MODE = 'source'
$env:CMAKE_GENERATOR = 'Ninja'
uv sync --python 3.12
cargo build --locked --manifest-path cli/Cargo.toml --bin c3
.\cli\target\debug\c3.exe --version
# Use system mode for subsequent Core/Rust SDK tests.
$env:FASTDB_PAYLOAD_LINK_MODE = 'system'
```

## 验证范围

[0.10.0 发布说明](../releases/0.10.0.md)描述当时的 API 和协议变更，其[验收记录](../acceptance/2026-10-30-0100-release-acceptance.md)将已发布源码与托管的 Windows Server 2022/2025 x64 和 Windows 11 ARM64 检查关联。[0.10.1 验收记录](../acceptance/2026-10-30-0101-release-acceptance.md)将该补丁的源码与托管的 Windows Server 2025 x64 和 Windows 11 ARM64 检查关联。[0.9.0 验收记录](../acceptance/2026-10-25-090-windows-release-acceptance.md)涵盖开发机、第二账户、跨会话和托管 ARM64 主机上的用户范围后端。[0.7.4 发布记录](../acceptance/0.7.4-publication.md)保存该版本的源码配对及完整的 Windows Server 2022/2025 x64 检查，包括严格的 18 行 Rust/Python 与 12 行 TypeScript 记录，以及普通用户、非管理员 wheel 使用者的验证。[FD 后续记录](../acceptance/0.7.4-fd-leak-regression.md)涵盖两台 Windows 运行器上实际的 Named Pipe 关闭与空闲连接替换回归。发布清单标识实际包源码；文档提交与产物源码提交分别记录。

0.8.0 开发记录还包含 Windows 11 x64 开发机测试；托管 Source Validation 要求 Windows Server 2025 x64 与 Windows 11 ARM64。上述结论不涵盖 Windows 服务以及真实的 Windows/Linux relay 链路。发布证据记录准确的源码和通过的原生检查。平台覆盖范围见[实现报告](windows-native-implementation.md)。

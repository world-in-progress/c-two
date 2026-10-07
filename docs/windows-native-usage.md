# Windows installation and development builds

The native Windows backend uses byte-mode Named Pipes for `ipc://` control traffic and named file mappings for shared payload memory. It runs within one logon session under that session's SID. Ordinary SDK code continues to use logical `ipc://` addresses; do not construct pipe or mapping names in an SDK. HTTP relay still provides the network path.

This guide describes Python C-Two **0.7.0** / c3 **0.3.0** on Windows x64.
See [release notes](releases/0.7.0.md) for package and asset availability,
coordinated upgrades and validation progress. FastDB is pinned to 0.2.1.

## Install Python and c3

In an activated Python environment, explicitly select the documented version:

```powershell
uv pip install 'c-two==0.7.0'
```

The coordinated [c3 0.3.0 release entry](https://github.com/world-in-progress/c-two/releases/tag/c3-v0.3.0)
uses `c3-x86_64-pc-windows-msvc.exe`, its `.exe.sha256` sidecar and
`c3-installer.ps1`. The [preparation page](releases/0.7.0.md) records their current
availability; these target URLs do not assert that 0.3.0 is published.
Download and verify the standalone executable before running it:

```powershell
$releaseBase = 'https://github.com/world-in-progress/c-two/releases/download/c3-v0.3.0'
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

Alternatively, use the checksum-verifying installer:

```powershell
Invoke-WebRequest -UseBasicParsing -Uri 'https://github.com/world-in-progress/c-two/releases/latest/download/c3-installer.ps1' -OutFile .\c3-installer.ps1
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\c3-installer.ps1 -Version 0.3.0 -Target x86_64-pc-windows-msvc
$env:PATH = "$env:LOCALAPPDATA\Programs\c3;$env:PATH"
c3.exe --version
```

The installer supports PowerShell 5.1 and needs no administrator privileges.
It copies the verified executable to `c3.exe` in
`$env:LOCALAPPDATA\Programs\c3`, or a directory selected by `-BinDir`.
It does not persistently register PATH; the example updates this session only.
Windows ARM64 has no release exe target. See [CLI installation](../cli/README.md#windows-x64)
for the same options.

For same-host resource calls, use the resource process's `cc.server_address()` with `cc.connect(..., address=...)`. A direct IPC call does not require a relay. For relay routing, configure `cc.set_relay_anchor('http://127.0.0.1:8300')` in resource/client processes. The SDK does not start or own `c3 relay`.

Python resource servers using `cc.serve()` accept Ctrl+C; automated subprocess supervisors can create a new process group and send `CTRL_BREAK_EVENT`. A normal application can instead call `cc.shutdown()` in its owning process and wait for completion. An IPC admin shutdown acknowledgement only starts draining and is not the completion barrier.

## Build from source

Use canonical main for source development. The matching c3 release's `rc-manifest.json` identifies release source; main is a development checkout convention.
Use an x64 Developer PowerShell with the MSVC C++ toolchain, CMake, Git, stable Rust and uv available. Python and Rust resolve FastDB 0.2.1 from their registries. The Rust binding also requires the matching Core SDK: extract `fastdb-core-0.2.1-x86_64-pc-windows-msvc.tar.gz` from that release and configure its absolute `lib` directory below. The full source-based interoperability checks additionally use the sibling FastDB release checkout for fixtures and TypeScript sources, and require SWIG 4.4 or later when building its Python package:

```powershell
git clone https://github.com/world-in-progress/fastdb.git
git -C fastdb checkout v0.2.1
git clone https://github.com/world-in-progress/c-two.git
Set-Location c-two
$env:FASTDB_PAYLOAD_SYSTEM_LIB_DIR = 'C:\path\to\fastdb-core-sdk\lib'
$env:PATH = "$env:FASTDB_PAYLOAD_SYSTEM_LIB_DIR;$env:PATH"
$env:FASTDB_PAYLOAD_LINK_MODE = 'source'
uv sync --python 3.12
cargo build --locked --manifest-path cli/Cargo.toml --bin c3
.\cli\target\debug\c3.exe --version
# Use system mode for subsequent Core/Rust SDK tests.
$env:FASTDB_PAYLOAD_LINK_MODE = 'system'
```

## Source validation boundary

The [canonical report](reports/canonical-local-endpoint-validation.md) records Windows Server 2022/2025 x64 CPython 3.12 validation from C-Two `e49652e85a384f1dd3489f393c13f4d9fff27f9f` plus FastDB `4f99f86a662b0e950a0dd29800c25a1c9fca4def`. Later documentation commits are not artifact sources. This is source evidence, not proof of all 0.7 release ABIs.

The full hosted gate additionally prepares Node 22, Ninja, Git Bash and Emscripten 5.0.2 for generated TypeScript and native Node tests. Run the same workflow for complete evidence rather than treating a local Python import as equivalent coverage.

Non-administrator token execution is verified on both hosted runners. Windows 11 desktop, Windows services, Windows ARM64 and a real Windows/Linux relay link remain separate coverage targets. The coordinated Python ABI candidate matrix is described in the release notes. Their status is recorded in [the implementation report](windows-native-implementation.md); a Windows Server x64 result does not prove those environments.

# Windows installation and development builds

This guide describes installation, use, source builds and validation evidence for the native Windows backend of C-Two.

English · [简体中文](windows-native-usage.zh-CN.md)

The backend uses byte-mode Named Pipes for `ipc://` control traffic and named file mappings for shared payload memory. It uses the process TokenUser SID across logons. Same-session peers may use SHM; different sessions use owned/inline/chunked transfers. Native connections require a matching user and a server integrity of at least Medium before sending. Ordinary SDK code continues to use logical `ipc://` addresses; do not construct pipe or mapping names in an SDK. HTTP relay still provides the network path.

This guide covers Python C-Two **0.10.1**, c3 **0.6.0** and FastDB **0.2.4** on Windows x64 and ARM64. Install the matching Python and c3 versions together; see the [0.10.0 migration notes](../releases/0.10.0.md) and [lifecycle guide](local-endpoint-lifecycle.md) for endpoint behavior.

Unix root overrides (`cc.endpoint.set_local_endpoint(root=...)`, `C2_IPC_ROOT`, `--ipc-root`) are rejected on Windows. Pipe names derive from the logical address and current TokenUser SID.

## Install Python and c3

In an activated Python environment, explicitly select the documented version:

```powershell
uv pip install 'c-two==0.10.1'
```

For the published [c3 0.6.0 release](https://github.com/world-in-progress/c-two/releases/tag/c3-v0.6.0), download `c3-x86_64-pc-windows-msvc.exe` (x64) or `c3-aarch64-pc-windows-msvc.exe` (ARM64), its `.exe.sha256` checksum and optionally `c3-installer.ps1`. Verify the standalone executable before running it:

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

Alternatively, use the checksum-verifying installer:

```powershell
Invoke-WebRequest -UseBasicParsing -Uri 'https://github.com/world-in-progress/c-two/releases/latest/download/c3-installer.ps1' -OutFile .\c3-installer.ps1
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\c3-installer.ps1 -Version 0.6.0
$env:PATH = "$env:LOCALAPPDATA\Programs\c3;$env:PATH"
c3.exe --version
```

The installer supports PowerShell 5.1 and needs no administrator privileges. Automatic architecture detection uses [`IsWow64Process2`](https://learn.microsoft.com/en-us/windows/win32/api/wow64apiset/nf-wow64apiset-iswow64process2) (Windows 10 version 1709 / Windows Server 2016 version 1709 or later), so an emulated PowerShell still selects the native executable. It copies the verified executable to `c3.exe` in `$env:LOCALAPPDATA\Programs\c3`, or a directory selected by `-BinDir`. It does not persistently register PATH; the example updates this session only. The installer selects the native OS architecture. ARM64 wheels support Python 3.11–3.14 and 3.14t; x64 also supports 3.10. See [CLI installation](../../cli/README.md#windows-x64--arm64) for the same options.

For same-host resource calls, use the resource process's `cc.server_address()` with `cc.connect(..., address=...)`. A direct IPC call does not require a relay. For relay routing, configure `cc.set_relay_anchor('http://127.0.0.1:8300')` in resource/client processes. The SDK does not start or own `c3 relay`.

Python resource servers using `cc.serve()` accept Ctrl+C; automated subprocess supervisors can create a new process group and send `CTRL_BREAK_EVENT`. A normal application can instead call `cc.shutdown()` in its owning process and wait for completion. An IPC admin shutdown acknowledgement only starts draining and is not the completion barrier.

## Build from source

Use canonical main for source development. The matching c3 release's `rc-manifest.json` identifies release source; main is a development checkout convention. Use a Developer PowerShell targeting your native x64 or ARM64 architecture with the MSVC C++ toolchain, CMake, Ninja, Git, Rust 1.95 or newer and uv available. Python and Rust resolve FastDB 0.2.4 from their registries; Python requires the optional `fastdb` extra. The Python extension and c3 use source-mode native builds. The Rust binding also requires the matching Core SDK: extract `fastdb-core-0.2.4-x86_64-pc-windows-msvc.tar.gz` or `fastdb-core-0.2.4-aarch64-pc-windows-msvc.tar.gz` from that release and configure its absolute `lib` directory below. The full source-based interoperability checks additionally use the sibling FastDB release checkout for fixtures and TypeScript sources; Python consumers use the published wheel:

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

## Validation scope

The [0.10.0 release notes](../releases/0.10.0.md) describe the current API and protocol changes, and its [acceptance record](../acceptance/2026-10-30-0100-release-acceptance.md) binds the published source to hosted Windows Server 2022/2025 x64 and Windows 11 ARM64 gates. The [0.10.1 acceptance record](../acceptance/2026-10-30-0101-release-acceptance.md) binds that patch's source to hosted Windows Server 2025 x64 and Windows 11 ARM64 gates. The [0.9.0 acceptance record](../acceptance/2026-10-25-090-windows-release-acceptance.md) covers the user-scoped backend on developer, second-account, cross-session and hosted ARM64 hosts. The [0.7.4 publication record](../acceptance/0.7.4-publication.md) retains that release's source pair and full Windows Server 2022/2025 x64 gates, including strict 18-row Rust/Python and 12-row TypeScript receipts and ordinary/non-administrator wheel consumers. The [FD follow-up](../acceptance/0.7.4-fd-leak-regression.md) covers actual Named Pipe close and idle-replacement regressions on both Windows runners. Release manifests identify the actual package source; documentation commits are separate from artifact source commits.

The 0.8.0 development record additionally includes Windows 11 x64 developer-machine tests; hosted Source Validation requires Windows Server 2025 x64 and Windows 11 ARM64. Windows services and a real Windows/Linux relay link are outside those claims. Release evidence records the exact source and passing native lanes. See the [implementation report](windows-native-implementation.md) for platform coverage.

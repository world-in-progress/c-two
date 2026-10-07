# Windows installation and development builds

The native Windows backend uses byte-mode Named Pipes for `ipc://` control traffic and named file mappings for shared payload memory. It runs within one logon session under that session's SID. Ordinary SDK code continues to use logical `ipc://` addresses; do not construct pipe or mapping names in an SDK. HTTP relay still provides the network path.

The [Windows Native workflow](https://github.com/Dsssyc/c-two/actions/workflows/windows-native.yml) builds immutable C-Two/FastDB source pairs on Windows 2022 and 2025, using MSVC x64 and Python 3.12. Its full artifact contains wheels, a standalone `cli/c3.exe`, per-gate logs and `run-evidence.json` with artifact SHA-256 values. Use a run whose required gates actually passed; compilation alone is not an installed-runtime result. The installed-wheel receipt separately proves direct IPC, HTTP relay, a 1 MiB FastDB payload, checked held-view invalidation and resource shutdown outside the source checkout.

## Current status (2026-10-07)

Published [Python c-two 0.6.0](https://pypi.org/project/c-two/0.6.0/) includes
Windows x64 wheels, and [c3 0.2.0](https://github.com/world-in-progress/c-two/releases/tag/c3-v0.2.0)
includes the Windows x64 executable and PowerShell installer. For the published
stack, install Python with `uv pip install c-two` in an activated environment
and obtain the matching CLI from that release. FastDB 0.2.1 supplies official
Windows wheels and a native Core SDK.

Current source targets unpublished Python 0.7.0 / c3 0.3.0. Its
[run 37559074657](https://github.com/Dsssyc/c-two/actions/runs/37559074657)
passed all four jobs on Windows Server 2022/2025 x64, each full job 23/23 gates,
including ordinary and non-administrator installed-wheel consumers and
account/process/temp cleanup. The [canonical report](reports/canonical-local-endpoint-validation.md)
binds C-Two `e49652e85a384f1dd3489f393c13f4d9fff27f9f` plus FastDB
`4f99f86a662b0e950a0dd29800c25a1c9fca4def`. These development wheels still say
0.6.0; later documentation HEADs are not artifact sources. They do not establish
the 0.7 release matrix or publication. Use matching development wheels and c3
for the new [lifecycle mechanisms](local-endpoint-lifecycle.md), and follow the
[coordinated upgrade notes](releases/0.7.0.md).

[Run 36034972057](https://github.com/Dsssyc/c-two/actions/runs/36034972057) and its
[final report](reports/windows-native-final-validation.md) remain historical
evidence for their original source pair; they do not prove the current branch.

## Use a matching artifact pair

Extract one full Windows artifact and verify the files against its `run-evidence.json`. In PowerShell, from that extracted directory:

```powershell
uv venv --python 3.12 .venv
$fastdb = @(Get-ChildItem .\wheels\fastdb\*.whl)
$ctwo = @(Get-ChildItem .\wheels\c-two\*.whl)
if ($fastdb.Count -ne 1 -or $ctwo.Count -ne 1) { throw 'Expected one wheel per package' }
uv pip install --python .\.venv\Scripts\python.exe $fastdb[0].FullName $ctwo[0].FullName
.\cli\c3.exe --version
.\cli\c3.exe relay --bind 127.0.0.1:8300
```

For same-host resource calls, use the resource process's `cc.server_address()` with `cc.connect(..., address=...)`. A direct IPC call does not require a relay. For relay routing, configure `cc.set_relay_anchor('http://127.0.0.1:8300')` in resource/client processes. The SDK does not start or own `c3 relay`.

Python resource servers using `cc.serve()` accept Ctrl+C; automated subprocess supervisors can create a new process group and send `CTRL_BREAK_EVENT`. A normal application can instead call `cc.shutdown()` in its owning process and wait for completion. An IPC admin shutdown acknowledgement only starts draining and is not the completion barrier.

## Build from source

The command below fetches the fork source-validation branch at `e49652e85a384f1dd3489f393c13f4d9fff27f9f`. The release/documentation work remains in the local `socu/local-endpoint-lifecycle` branch and has not been merged into canonical `main`.
Use an x64 Developer PowerShell with the MSVC C++ toolchain, CMake, Git, stable Rust and uv available. Python and Rust resolve FastDB 0.2.1 from their registries. The Rust binding also requires the matching Core SDK: extract `fastdb-core-0.2.1-x86_64-pc-windows-msvc.tar.gz` from that release and configure its absolute `lib` directory below. The full source-based interoperability checks additionally use the sibling FastDB release checkout for fixtures and TypeScript sources, and require SWIG 4.4 or later when building its Python package:

```powershell
git clone https://github.com/world-in-progress/fastdb.git
git -C fastdb checkout v0.2.1
git clone --branch socu/local-endpoint-validation https://github.com/Dsssyc/c-two.git
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

The full hosted gate additionally prepares Node 22, Ninja, Git Bash and Emscripten 5.0.2 for generated TypeScript and native Node tests. Run the same workflow for complete evidence rather than treating a local Python import as equivalent coverage.

Non-administrator token execution is verified on both hosted runners. Windows 11 desktop, Windows services, ARM64, the complete 0.7 Python ABI candidate matrix and a real Windows/Linux relay link remain separate coverage targets. Their status is recorded in [the implementation report](windows-native-implementation.md); a Windows Server x64 result does not prove those environments.

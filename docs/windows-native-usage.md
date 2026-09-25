# Windows development builds

The native Windows backend uses byte-mode Named Pipes for `ipc://` control traffic and named file mappings for shared payload memory. It runs within one logon session under that session's SID. Ordinary SDK code continues to use logical `ipc://` addresses; do not construct pipe or mapping names in an SDK. HTTP relay still provides the network path.

The [Windows Native workflow](https://github.com/Dsssyc/c-two/actions/workflows/windows-native.yml) builds immutable C-Two/FastDB source pairs on Windows 2022 and 2025, using MSVC x64 and Python 3.12. Its full artifact contains wheels, a standalone `cli/c3.exe`, per-gate logs and `run-evidence.json` with artifact SHA-256 values. Use a run whose required gates actually passed; compilation alone is not an installed-runtime result. The installed-wheel receipt separately proves direct IPC, HTTP relay, a 1 MiB FastDB payload, checked held-view invalidation and resource shutdown outside the source checkout.

[Run 36034972057](https://github.com/Dsssyc/c-two/actions/runs/36034972057) passes all required gates on both runners, including separate non-administrator wheel consumers. Its [final validation report](reports/windows-native-final-validation.md) identifies the exact tested source pair, downloadable artifacts and hashes. The artifact API reports expiry on 2026-10-08 UTC.

These C-Two artifacts are development builds. [FastDB 0.2.1](https://github.com/world-in-progress/fastdb/releases/tag/v0.2.1) now supplies official Windows wheels and a native Core SDK; the current C-Two branch consumes its published packages. C-Two's own 0.6.0 Windows distribution is still being prepared. The historical run above retains its original source pair and does not prove the updated dependency pair or a C-Two publication. Candidate wheels must not be uploaded over existing releases.

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

Use an x64 Developer PowerShell with the MSVC C++ toolchain, CMake, Git, stable Rust and uv available. Python and Rust resolve FastDB 0.2.1 from their registries. The Rust binding also requires the matching Core SDK: extract `fastdb-core-0.2.1-x86_64-pc-windows-msvc.tar.gz` from that release and configure its absolute `lib` directory below. The full source-based interoperability checks additionally use the sibling FastDB release checkout for fixtures and TypeScript sources, and require SWIG 4.4 or later when building its Python package:

```powershell
git clone https://github.com/world-in-progress/fastdb.git
git -C fastdb checkout v0.2.1
git clone --branch dev-feature https://github.com/Dsssyc/c-two.git
Set-Location c-two
$env:FASTDB_PAYLOAD_LINK_MODE = 'system'
$env:FASTDB_PAYLOAD_SYSTEM_LIB_DIR = 'C:\path\to\fastdb-core-sdk\lib'
$env:PATH = "$env:FASTDB_PAYLOAD_SYSTEM_LIB_DIR;$env:PATH"
uv sync --python 3.12
cargo build --locked --manifest-path cli/Cargo.toml --bin c3
.\cli\target\debug\c3.exe --version
```

The full hosted gate additionally prepares Node 22, Ninja, Git Bash and Emscripten 5.0.2 for generated TypeScript and native Node tests. Run the same workflow for complete evidence rather than treating a local Python import as equivalent coverage.

Non-administrator token execution is verified on both hosted runners. Windows 11 desktop, Windows services, ARM64, Python 3.10/3.14t native Windows wheels and a real Windows/Linux relay link remain separate coverage targets. Their status is recorded in [the implementation report](windows-native-implementation.md); a Windows Server x64 result does not prove those environments.

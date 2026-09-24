# Windows development builds

The native Windows backend uses byte-mode Named Pipes for `ipc://` control traffic and named file mappings for shared payload memory. It runs within one logon session under that session's SID. Ordinary SDK code continues to use logical `ipc://` addresses; do not construct pipe or mapping names in an SDK. HTTP relay still provides the network path.

The [Windows Native workflow](https://github.com/Dsssyc/c-two/actions/workflows/windows-native.yml) builds immutable C-Two/FastDB source pairs on Windows 2022 and 2025, using MSVC x64 and Python 3.12. Its full artifact contains wheels, a standalone `cli/c3.exe`, per-gate logs and `run-evidence.json` with artifact SHA-256 values. Use a run whose required gates actually passed; compilation alone is not an installed-runtime result. The installed-wheel receipt separately proves direct IPC, HTTP relay, a 1 MiB FastDB payload, checked held-view invalidation and resource shutdown outside the source checkout.

These are development artifacts. The existing C-Two 0.5.1 and FastDB 0.2.0 registry releases do not contain these Windows wheels. The pinned FastDB MSVC repair is tracked by [FastDB PR #37](https://github.com/world-in-progress/fastdb/pull/37); producing a new official Windows distribution requires new release versions and their publication gates. Candidate wheels must not be uploaded over the existing releases.

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

Use an x64 Developer PowerShell with the MSVC C++ toolchain, CMake, SWIG, Git, stable Rust and uv available. Keep the repositories as siblings because development Cargo and uv dependencies use the FastDB checkout:

```powershell
git clone https://github.com/world-in-progress/fastdb.git
git -C fastdb checkout 6f03b1c9a0ffc8d9c205f9dcebb54ce698b9dff4
git clone --branch socu/windows-native-ipc https://github.com/Dsssyc/c-two.git
Set-Location c-two
uv sync --python 3.12
cargo build --locked --manifest-path cli/Cargo.toml --bin c3
.\cli\target\debug\c3.exe --version
```

The full hosted gate additionally prepares Node 22, Ninja, Git Bash and Emscripten 5.0.2 for generated TypeScript and native Node tests. Run the same workflow for complete evidence rather than treating a local Python import as equivalent coverage.

Windows 11 desktop, non-administrator token execution, Windows services, Python 3.10/3.14t native Windows wheels and a real Windows/Linux relay link are separate coverage targets. Their status is recorded in [the implementation report](windows-native-implementation.md); a Windows Server x64 result does not prove those environments.

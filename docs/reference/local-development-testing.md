# Local development test sidecar

This guide owns shared macOS/Unix and Windows development test preparation, execution, and retained evidence.

`tools/dev/test_local.py` shares the local Python executor with macOS/Unix and Windows. It is an explicit development entry, separate from `tools/ci/source_validation.py` and the release workflows. Local reports are development evidence; they do not certify publication, standard-user release installation, or hosted CI success.

## Tools and platform preparation

Use an installed Python 3.12 interpreter, uv, Git and a Rust 1.95 or newer toolchain for the target. Windows builds need an MSVC Developer PowerShell (or the environment from `vcvars64.bat`); macOS builds need the Xcode command-line tools. Repository installer checks also require PowerShell (`pwsh` on macOS/Unix, or Windows PowerShell). The official FastDB SDK preparation script chooses a published host target and checks pinned manifests and archives. An unsupported host target fails with its available-target list.

The Windows commands below show x64 tools. For ARM64, follow the [native Windows prerequisites](windows-native-usage.md): use ARM64 Python, Rust, MSVC and Node. The x64 Emscripten build tool does not substitute for native ARM64 test consumers.

Python/TypeScript payload preparation additionally needs Node 22 with Node-API at least 10, CMake, Ninja and an activated Emscripten 5.0.2 SDK. A Node version number alone is insufficient: Node 22.11 has Node-API 9 and is rejected before native code is loaded. Use `--tool-path DIRECTORY` repeatedly to select tools only for this run. Providing all three prebuilt npm tarballs avoids the WASM preparation prerequisites. Node-only preparation installs/builds the c2-mem package without requiring FastDB WASM.

The FastDB source sibling must be clean and pinned to `f82fa13adeae2b87a295bf984821c85c44b55a3d`. Preparation clones it into a new staging directory and builds the JS loader and WASM together there; it does not overwrite the sibling's tracked loader. Temporary npm packages avoid requiring Windows directory-symlink privileges. CLI/Python native builds use FastDB source linking; Rust portable tests use the verified CoreSDK in system mode. Dynamic library paths apply to child processes.

## Prepare, check, run

Run from the repository root. Each `--output` directory must be new; preparation also creates a sibling directory ending in `-inputs`. The commands below are examples: replace the tool, interpreter, SDK and output paths with the paths on your machine.

Windows, from Developer PowerShell after activating Emscripten:

```powershell
python tools/dev/test_local.py prepare --suite all --python C:/Python312/python.exe --emsdk D:/emsdk --output D:/test-evidence/prepare --tool-path D:/emsdk/node/22.16.0_64bit/bin

.venv/Scripts/python.exe tools/dev/test_local.py check --suite all --output D:/test-evidence/check --coresdk-lib D:/test-evidence/prepare-inputs/coresdk/lib --repo-python D:/test-evidence/prepare-inputs/repo-venv/Scripts/python.exe --release-fixtures D:/test-evidence/prepare-inputs/release-fixtures --fastdb-package D:/test-evidence/prepare-inputs/packages/fastdb4ts-0.2.4.tgz --c2-mem-package D:/test-evidence/prepare-inputs/packages/c-two-c2-mem-ffi-0.1.0.tgz --typescript-package D:/test-evidence/prepare-inputs/packages/typescript-5.9.3.tgz --tool-path D:/emsdk/node/22.16.0_64bit/bin

.venv/Scripts/python.exe tools/dev/test_local.py run --suite all --output D:/test-evidence/run --coresdk-lib D:/test-evidence/prepare-inputs/coresdk/lib --repo-python D:/test-evidence/prepare-inputs/repo-venv/Scripts/python.exe --release-fixtures D:/test-evidence/prepare-inputs/release-fixtures --fastdb-package D:/test-evidence/prepare-inputs/packages/fastdb4ts-0.2.4.tgz --c2-mem-package D:/test-evidence/prepare-inputs/packages/c-two-c2-mem-ffi-0.1.0.tgz --typescript-package D:/test-evidence/prepare-inputs/packages/typescript-5.9.3.tgz --tool-path D:/emsdk/node/22.16.0_64bit/bin
```

When using an existing verified SDK, supply `--coresdk-lib` to preparation. The `prepared_inputs` section of `run.json` records actual staging/package paths and hashes. Preparation installs project dependencies via uv and creates a separate repository-test environment containing pytest, xdist, timeout, packaging, PyYAML, maturin and puccinialin. `run` directly executes the selected prepared interpreters and does not install dependencies.

macOS/Unix uses the same actions and flags with Unix paths and `.venv/bin/python`; the repository-test interpreter is `prepare-inputs/repo-venv/bin/python`. The existing macOS command also remains available for an already prepared environment:

```bash
.venv/bin/python tools/dev/test_python.py \
  --python "$PWD/.venv/bin/python" --c3 "$PWD/cli/target/debug/c3" \
  --output /absolute/new/evidence --workers 2
```

The existing command requires the CoreSDK environment and prebuilt TypeScript inputs, as before. It does not perform installation. The shared sidecar defaults to one worker; `--workers 2` or `4` parallelizes ordinary Python files only. Portable and TypeScript matrices always run serially.

## Suites and evidence

Available suites are `core`, `cli`, `rust-sdk`, `native`, `node`, `repo`, `python`, `typescript` and `all`. `all` runs the four Rust suites, Node bindings, repository checks, and the complete Python SDK including both portable and TypeScript matrices. The focused `typescript` suite still validates exact execution and its development receipt. c3 defaults to the source debug binary; `--c3` selects an explicit prebuilt binary.

`check.json` records tool paths, versions, capabilities and missing prerequisites. `run.json` and each `step.json` record commands, source provenance, link modes, finite timeouts, cleanup status and suite outcomes. Cargo summaries, Node TAP summaries and repository JUnit counts are checked. Python keeps its collected-nodeid partition, exactly-once execution, platform skips only through the `platform` marker, package/native hashes and matrix receipt validation. Missing dependencies, unknown skips, filtered tests, crashes, failed builds and unconfirmed cleanup fail validation. The two Rust HTTP child entrypoints marked ignored are invoked by their parent tests and are explicitly identified in the local report policy.

Repository tests that validate official FastDB release archives need `--release-fixtures`; `prepare --suite repo` or `all` retains the four pinned official target archives. Missing fixture files do not become allowed platform skips. The original synthetic CI SDK tests continue to run independently of these archives. The local skip policy names specific platform tests and reasons; it does not accept arbitrary skips with a matching reason.

Unix manages sessions/process groups and resource leases through flock; Windows assigns suspended children to a kill-on-close Job Object before resuming them and uses Windows file locks. Resource leases coordinate these local entries, not unrelated manual Cargo/npm commands. Do not run a manual rebuild while a local suite is using its artifacts. Timeout/cancellation stops subsequent dependent stages and retains failure evidence. Windows compiler helper cleanup applies only after the parent exits and every remaining Job-owned process is the exact `vctip.exe` beside the selected `cl` compiler. It terminates this owned Job, confirms that it is empty and records the helper image/PID. Unknown helpers, foreign paths, live parents and mixed residual children cannot yield a passing result.

Windows children also set `MSBUILDDISABLENODEREUSE=1` so completed CMake/MSBuild builds do not leave reusable worker processes behind. This setting is documented by [MSBuild](https://github.com/dotnet/msbuild/blob/main/documentation/wiki/MSBuild-Tips-%26-Tricks.md) and is confined to the test environment; it does not relax process cleanup checks.

The initial implementation is verified on a Windows development machine. macOS command compatibility and the Unix backend have regression coverage, but actual macOS execution must be recorded separately; Windows results do not prove a macOS run.

The initial Windows per-suite results and retained failure investigations are recorded in [the verification report](../acceptance/2026-10-10-local-test-sidecar-windows.md).


## Review fixes: source identity and preparation failures

The sidecar records `source_identity_before` before preflight and retains that starting `source_sha`. At completion it compares HEAD, tracked diff and tracked/untracked file hashes against `source_identity_after`; a detected change fails validation. Newly created output and preparation input directories are excluded from file inventory. This is a beginning/end comparison, so an external edit reverted before the final capture is outside its guarantee. The aggregate stays running until final input and source checks complete.

Every accepted new output directory gets `run.json`, including validation, preflight, planning and cancellation failures. The report includes `phase`, error and completion time. Existing evidence directories are never overwritten. Commands launched by the sidecar resolve against the child tool PATH to absolute executables; reports record the actual selected executable.

`prepare --suite core` and `all` install locked npm dependencies with scripts disabled in the actual sibling `../fastdb/ts/fastdb4ts` consumed by the Rust Core TypeScript contract tests. This requires Node/npm and the sibling package manifest and lockfile. `--fastdb-source` continues to select the separate source for staged WASM preparation; it does not change the Core tests' sibling path. Core check/prepare/run require this actual sibling to be a clean Git checkout at the pinned FastDB commit, with the payload source entrypoint present. Linked Git worktrees are supported; index flags `assume-unchanged` and `skip-worktree` are rejected because they can hide source edits. Preflight records the checkout path, HEAD, tracked diff/content hashes, worktree status, index flags and payload entrypoint hash; final validation records and compares the source identity again. Core run preflight also verifies TypeScript 5.9.3 and fingerprints the compiler package. Sibling leases protect capture, preparation and Core execution; source identity and compiler inputs are checked before/after Core consumption so a change after preflight cannot produce passing evidence.

## Endpoint namespace isolation

`C2_IPC_ROOT` is configuration, not an isolation boundary for the complete Rust relay suite: some fixtures explicitly derive the platform-default namespace while others resolve the environment. Do not override it globally to isolate those fixtures. When the host's historical namespace must remain untouched, run that suite in a clean runner; local targeted fixtures must explicitly use the same task-owned context on both ends. `LocalStream::pair()` also derives the platform default through `LocalEndpoint::from_address()` and ignores `C2_IPC_ROOT`; an isolated probe must explicitly pass its task-owned `LocalEndpointContext` into both endpoints rather than treating that environment variable as sufficient.

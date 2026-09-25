# Windows native final validation

The Windows development implementation passed its hosted acceptance gates on **2026-09-25 (Asia/Shanghai)**. [Windows Native run 36034972057](https://github.com/Dsssyc/c-two/actions/runs/36034972057) completed successfully on Windows Server 2022 and 2025, each with a local-platform job and a full job. Downloaded artifacts were independently checked after execution. The verification result was `PASS: 0 failure(s)`.

## Exact source inputs

- C-Two: [`5cf96eafc4674f7961beb15b7a976cfee7f238d0`](https://github.com/Dsssyc/c-two/commit/5cf96eafc4674f7961beb15b7a976cfee7f238d0), branch `socu/windows-native-ipc`.
- FastDB: [`6f03b1c9a0ffc8d9c205f9dcebb54ce698b9dff4`](https://github.com/world-in-progress/fastdb/commit/6f03b1c9a0ffc8d9c205f9dcebb54ce698b9dff4), the unreleased MSVC repair in [PR #37](https://github.com/world-in-progress/fastdb/pull/37).
- All four evidence envelopes report these same expected and actual source revisions, verified source checks and the C-Two workflow revision above. Subsequent documentation commits are not the source of these binaries.

The [machine-readable validation record](evidence/windows-native-36034972057-validation.v1.json) retains artifact identities, runner image versions, gate results, receipt hashes and every recorded member hash. Older failed runs and historical portable receipts remain unchanged.

## Executed coverage

| Gate or evidence | Windows 2022 | Windows 2025 |
| --- | --- | --- |
| Full required gates | 22/22 passed | 22/22 passed |
| Local-platform tests | 513 passed | 513 passed |
| Full Core workspace | 954 passed | 954 passed |
| Python suite | 829 passed, zero skips | 829 passed, zero skips |
| Rust/Python portable suite and strict receipt | 25 tests; 18/18 rows | 25 tests; 18/18 rows |
| Generated TypeScript suite and strict receipt | 13 tests; 12/12 rows | 13 tests; 12/12 rows |
| User-facing Rust SDK | 10 passed | 10 passed |
| FastDB Rust | 22 passed | 22 passed |
| CLI | 36 passed | 36 passed |
| Native Node memory bindings | 33 passed; six-file package check passed | 33 passed; six-file package check passed |
| Wheel test harness | 4 passed | 4 passed |
| Installed-wheel consumers | Runner account and non-administrator account passed | Runner account and non-administrator account passed |

Counts describe their named gates, not disjoint totals: local-platform tests are also exercised in the Core workspace, and the explicit portable gate repeats relevant integration coverage. The native Python extension gate compiled and passed its Cargo test command, which contained zero standalone Rust tests; its behavior is exercised by the Python and installed-wheel gates.

The 513 local-platform tests cover config (67), IPC (58), local endpoint/stream/listener behavior (11), logon security (1), memory (99), memory FFI (19) and wire (117). The platform tests include listener restart/cancellation, duplicate ownership, stale mapping generation rejection, held-allocation retirement refusal, killed allocator-holder refusal and killed-owner spill cleanup. Core regression also passes the corrected parallel proxy-environment tests and deterministic never-found 404 / removed-route 410 tests.

Each installed-wheel consumer created a fresh environment outside the checkout, installed the exact downloaded C-Two and FastDB wheels, and checked distribution versions and wheel source hashes. Both host and client checked resolved Python/native-library origins inside that environment. Direct calls observed `ipc`; relay calls observed `http`. Host and client used separate processes, transferred a 1 MiB FastDB payload, invalidated checked held views on release, and verified four borrowed inputs became invalid after callbacks. Resource shutdown, child-process exit and temporary-directory deletion passed. The non-administrator wrapper additionally verified an unprivileged token and successfully removed its temporary account, owned processes and workspace.

The normal runner receipts retain raw `C:\Users\RUNNER~1` module paths alongside a resolved `C:\Users\runneradmin` environment prefix. Their native `origins()` check resolves both on Windows before enforcing containment. Offline review corroborated only that exact 8.3 spelling difference and the full random temporary-directory suffix; it did not attempt to resolve Windows filesystem aliases on macOS. The dedicated non-administrator receipts use matching path spellings.

## Download and byte provenance

All four downloaded ZIP sizes and SHA-256 values match the GitHub artifact API. All 37 recorded members in each full archive and the one recorded local-platform log in each local archive match their evidence-envelope sizes and hashes.

| Archive | Artifact ID | ZIP bytes | ZIP SHA-256 |
| --- | --- | --- | --- |
| [windows-2022 / full](https://github.com/Dsssyc/c-two/actions/runs/36034972057/artifacts/10825710829) | 10825710829 | 11561573 | `b61f3c71eb2d9a6c75ebf38c4b953cb5d3ebf73909f2ed3c307110de4c973b30` |
| [windows-2025 / full](https://github.com/Dsssyc/c-two/actions/runs/36034972057/artifacts/10825532067) | 10825532067 | 11550285 | `d05035067d79f1ac387a2b3eaab58822cbc703c9745d00b756fc2ecdc6bc46da` |
| [windows-2022 / local-platform](https://github.com/Dsssyc/c-two/actions/runs/36034972057/artifacts/10825130065) | 10825130065 | 10703 | `1284898d14397770eb846ee770f96f83c163387995619443b0c5491908076a19` |
| [windows-2025 / local-platform](https://github.com/Dsssyc/c-two/actions/runs/36034972057/artifacts/10824350873) | 10824350873 | 10701 | `827847dd26d79142f4a32b799c988703d48e4eaf10d23daf339454096126261a` |

The API reports these archives expire on **2026-10-08 UTC**. They are temporary development downloads, not a permanent registry release. ZIPs, extracted binaries and raw logs were retained only under `/tmp`; no binaries were added to Git.

| Runner | Downloaded binary | SHA-256 |
| --- | --- | --- |
| windows-2022 | `cli/c3.exe` | `bb202d8a4d76b099918766824ad4629f8847eece83d6fb5ca228952838ace2cf` |
| windows-2022 | `wheels/c-two/c_two-0.5.1-cp312-cp312-win_amd64.whl` | `d9641f3dfa99d68baee7d0df84065fe92960ce3e2d82b98617ede3c5e20bd128` |
| windows-2022 | `wheels/fastdb/fastdb4py-0.2.0-cp312-cp312-win_amd64.whl` | `b06fddaf617c60f75daf064e62e86207fbcc5b386441494dfff9303b19ef4c61` |
| windows-2025 | `cli/c3.exe` | `ad43ee89f3569833cf44e76eb76bb04aaf91a9fc9cd13acd9bdd0a5be9e05af2` |
| windows-2025 | `wheels/c-two/c_two-0.5.1-cp312-cp312-win_amd64.whl` | `4892eb0bb4854a00c1cca858a80047dfb5c732da3c8940eb34b8568580a5e16f` |
| windows-2025 | `wheels/fastdb/fastdb4py-0.2.0-cp312-cp312-win_amd64.whl` | `27d2fa628d5373ee98154ad52aac096ca76d5da0f30cc17fa6e5a713ba76af93` |

Within each runner's artifact, `cli/c3.exe` has exactly the hash recorded by the Rust/Python matrix, TypeScript matrix, runner-account wheel consumer and non-administrator wheel consumer. The matrix and wheel receipts therefore refer to the downloadable executable bytes. The two runner builds have different hashes; no cross-run reproducible-build claim is made. Each installed wheel likewise matches that runner's retained wheel bytes.

## Delivery boundary

The development implementation and the declared hosted Windows x64 acceptance are complete. Named Pipes, mapping/spill ownership, lifecycle cleanup and Python/Rust/Node projections are implemented; see [implementation](../windows-native-implementation.md) and [development usage](../windows-native-usage.md).

This result does not publish official C-Two or FastDB Windows packages. Candidate wheel versions remain C-Two 0.5.1 and FastDB 0.2.0; they must not overwrite existing releases. The FastDB repair is still a separate source input requiring upstream integration and a future release. No main/dev-feature merge or registry publication is part of this validation.

Windows 11 desktop, Windows services, ARM64, native Windows Python 3.10 or free-threaded 3.14 wheels, browser runtime, and an actual Windows/Linux two-machine relay remain unexecuted here. Python 3.10 syntax coverage passed; the native wheel consumers use CPython 3.12. All relay matrices in this run execute on one Windows runner. Checked payload-owner invalidation is proven; direct response-SHM construction and zero-copy decoding are not claimed.

The separately agreed configurable memory fallback policy remains follow-up work. These artifacts do not change the existing fallback contract or claim that `pool_enabled=False` prevents every pool allocation.

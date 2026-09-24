# Windows native implementation

Status: resumed at the user's request on 2026-09-24 after FastDB 0.2.0 publication and the OIDC migration completed. The target architecture and evidence boundaries are recorded in [the analysis](reports/2026-09-24-windows-native-ipc-analysis.zh-CN.md). Implementation continues on `socu/windows-native-ipc`. The first baseline used FastDB's official release source `ceebed2edbef580ba0a42dcd28dadf9628894523`; the next run pins the MSVC repair source `6f03b1c9a0ffc8d9c205f9dcebb54ce698b9dff4` from [FastDB PR #37](https://github.com/world-in-progress/fastdb/pull/37). That patch is an unreleased source input, not a replacement for the immutable 0.2.0 release.

## Sequence and acceptance

| Stage | Scope | Required evidence | Status |
| --- | --- | --- | --- |
| W0 | Native Windows CI; immutable C-Two/FastDB inputs; per-command run evidence | Actual hosted Windows result, including truthful compilation failures | In progress |
| W1 | Extract `c2-local`; migrate Unix endpoint, listener, stream, control and readiness | Core, SDK and direct/relay regression on Unix; no Unix types in common call paths | In progress |
| W2 | Windows byte-mode Named Pipes; shared current-logon security | Native connect, duplicate listener, bounded cancellation/close and ordinary-user access | In progress |
| W3 | Windows mapping, spill, process and memory handling; segment incarnation | Native mapping lifecycle, idle reclaim/regrow, outstanding lease and stale-reference tests | In progress |
| W4 | Complete Python/Rust/Node runtime and artifact projections | Actual portable matrices, package installation and DLL loading | Pending |
| W5 | Second Windows runner, minimum/free-threaded Python, ordinary users and cross-OS relay | Evidence for each declared supported environment and path; Unix regression | Pending |

Each stage is completed only when its required execution evidence exists. A green compiler check cannot complete a runtime or distribution stage. Failed baseline runs are recorded separately from the existing success-only portable receipts.

## Segment identity contract

The wire transport carries a `u32` buddy backing generation with every SHM coordinate; handshake version changes as a clean cut. Each owner pool creates a fresh incarnation prefix. A slot keeps its generation counter even after the backing is reclaimed. Recreating the slot increments the generation with checked overflow and uses a different bounded OS object name. Dedicated indices never truncate into the wire's `u16` range.

Peer caches identify a concrete slot/generation and reject older generations. They may replace a cached backing only when its shared allocation count is zero. Releasing the last valid block also releases an idle peer cache mapping, while an outstanding held/borrowed lease keeps its allocation live. This provides the retirement boundary through the existing allocation ownership rather than an unbounded wait for a new ACK. Connection loss does not revoke a still-live local held owner. A stale generation never reinitializes or frees a newer backing.

This contract must be proven by a test retaining an old peer mapping while the producer reclaims and regrows the same slot, plus tests holding an active block across attempted retirement. If these invariants cannot be maintained through every receive/release path, the implementation must add an explicit retirement protocol before claiming this stage complete.

## Evidence log

- Baseline macOS `cargo test --manifest-path core/Cargo.toml -p c2-mem --lib`: 77 passed, before implementation edits.
- FastDB baseline source was unavailable by SHA on GitHub; its isolated development branch was pushed so hosted builds can retrieve the exact source. No release or tag was created.
- Work stopped with Windows workflow/runner files staged, platform crate and mapping files partially written, and the new buddy-incarnation regression test added. The `c2-local` manifest currently has no library source target; the attempted focused regression run stopped at workspace loading, so no fail-first or post-change passing result is claimed. Resume by completing the extraction and re-running the recorded gates after the FastDB release dependency is fixed.
- On resumption, the local transport target was restored. The six spill tests pass on macOS after introducing an owner that closes the mapping before its backing file. Windows cross-compilation exposed an incorrect security constant import; no Windows runtime pass is inferred.
- The buddy-incarnation regression now executes and fails as intended: reclaim/regrow returns the same SHM name for the retained old mapping and new allocation. This is the fail-first baseline for the generation change.
- The generation change passes 87 local `c2-mem` tests, including stale-reference rejection, active-block retirement refusal, peer cache release, generation/index exhaustion, and killed-owner file spill cleanup. `c2-mem --tests` also passes Windows target checking; this is compilation evidence only.
- [Windows baseline 35987505275](https://github.com/Dsssyc/c-two/actions/runs/35987505275) executed on both Windows 2022 and 2025. Each envelope records 1 passed, 10 failed and 3 not-run commands. FastDB's MSVC builds fail on shadowed iterator names (C4456) and unreachable return (C4702); the initial C-Two source still has the pre-migration FastDB dependency constraint. Neither baseline is a runtime pass.
- After integration fixes, the full Python suite passes 867/867 without skips, including the Rust/Python portable matrix. The separate generated TypeScript gate passes 13/13 tests and all 12 direct/relay rows, with checked cleanup. These runs are macOS execution evidence; Windows results remain separate.
- Unix endpoint ownership uses a persistent rendezvous lock and a fixed socket identity record. Startup only reclaims an orphan whose recorded identity matches. Unknown, corrupt, or older unrecorded socket files are conservatively rejected and require explicit external cleanup; a full listen backlog must never be mistaken for permission to unlink an active socket.

## Outstanding environment coverage

Windows Server hosted jobs, Windows 11 desktop behavior, ordinary-user tokens, and a real Windows/Linux two-machine relay are separate evidence targets. Until executed, none is inferred from another target's result. No Windows support claim is made merely by adding configuration or compiling from macOS.

# c2-ipc cancellation and retirement test review

Review scope: candidate `da8cfb407994a129d883eb00848de501fa40a4ab` against semantic baseline `c66617019a63f53473a71f11f88dbd4382f4e2e6`, limited to `client.rs`, `tests.rs`, and this report. The correction below changes test-only observation and assertions. It does not change production retention limits or the permit handoff that the Host already integrated.

## Findings and correction

- The original fault-injection write guard and production-path read guard used two distinct function-local static locks. The candidate makes both use one lock. This prevents injected capacity or spawn failure from overlapping guarded production-path scenarios while ordinary guarded tests may still run concurrently.
- The cancellation fixtures stalled six and four callbacks respectively, but inherited a worker count derived from host parallelism. The candidate states those worker counts in the fixtures. The production default is unchanged.
- The process-wide permit baseline could be affected by other tests. The candidate waits for this pool's retained jobs, counting queued jobs and worker-popped jobs. A test-only `Weak` entry is installed under the executor lock at `pop_front` and removed after `permit.release()`, so observation does not retain a pool or omit the in-flight interval.
- The candidate's new in-flight regression had a false-failure race: it read `returned=false`, then independently read the pool count. The worker could legally return the permit and remove the entry between those reads. The regression now reads the flag first and the count while holding the executor lock. A `false, 0` sample is a real premature disappearance: removal needs that same lock, and removal completed before lock acquisition would already have stored `returned=true`. The test-only removal guard also checks the returned flag before removing the entry for this fixture; this checks the transition even if a transient zero falls between polling samples. It skips that assertion during unwinding to preserve the original panic and still removes the entry.

The scoped observer reads the executor's job queue and weak in-flight pointers without acquiring a pool lock. The worker pops and registers under one executor lock; retirement uses the pool lock, releases it, then returns the permit under the executor lock; guard removal takes the executor lock afterward. The new snapshot introduces no pool/executor nested lock order. The regression waits for a popped, unread backing, signals peer `read_done`, checks the returned flag and scoped count, then requires the scoped count, in-flight count, awaiting-retirement state, and budget charge to reach zero. The cancellation scenarios still check pending waiters, connection health, mapped and charged backings before `read_done`, worker bound, permit lower bound, and full cleanup after release. A queue-only zero cannot satisfy their retention-slot waits.

## Evidence and boundary

Earlier local investigation recorded Linux's two cancellation waits and Windows' dedicated capacity failures at the baseline; a targeted fault-injection overlap reproduced the capacity rejection, and a four-worker local build reproduced the callback starvation. Those are local observations from the earlier candidate review. The fixed candidate's earlier 128-test c2-ipc run and queue-only negative control are historical evidence, not validation of this correction.

Final correction validation:

```text
CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/private/tmp/c2-ipc-final-2802 \
  C2_ENV_FILE='' C2_RELAY_ANCHOR_ADDRESS='' \
  cargo test --manifest-path core/Cargo.toml -p c2-ipc --lib \
  pool_scoped_retention_slot_stays_visible_while_the_job_is_in_flight -- --nocapture
  => compiled; test failed before its assertions at the real dedicated allocation:
     shm_open failed: Operation not permitted (os error 1); exit 101.

CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/private/tmp/c2-ipc-final-2802 \
  C2_ENV_FILE='' C2_RELAY_ANCHOR_ADDRESS='' \
  cargo test --manifest-path core/Cargo.toml -p c2-ipc
  => one full run: 57 passed, 71 failed. Failures include shm_open and
     server startup Operation not permitted. Output:
     /private/tmp/c2-ipc-final-2802-full.log
     The zsh wrapper exited 1 because it assigned the read-only `status`
     variable after cargo finished; cargo's exact exit code was not captured.

git diff --check => exit 0.
```

The local failures above are a sandbox permission limit, not a passing runtime gate. The Host then ran the same correction in an integration tree with SHM permission, plus an already accepted control probe test. The Host's `/tmp/c2-combined-final-0929/result.json` identifies source `4e94fe6ff999355cefa3df0aeb35fbd122a54f93` and records:

```text
target-retire  exit 0   target-retire.log: 1 passed, 0 failed
ipc-full       exit 0   ipc-full.log:      129 passed, 0 failed
cli-build      exit 0
portable       exit 0   portable.log:      25 passed
typescript     exit 0   typescript.log:    14 passed
```

The two c2-ipc counts were read from the named logs; the extra control probe explains the increase from this candidate's 128 tests to 129 in the Host tree. The Host also reports strict 18/12-row receipts and one rebuilt `c3` SHA-256 `6cdc59752012aced8ff4faecce4c09f44db479d613b17f80d1ea2029250f4938`; those receipts are Host evidence and were not independently inspected in this turn. Windows and Linux hosted CI remain for the Host after push and are not claimed as passing. The earlier Windows 2025 `control::tests::ping_retries_until_endpoint_appears` failure was outside this write scope. A prior local `pool::tests::close_shared_aborts_blocked_receiver_within_deadline` timeout under a loaded simulation was observed but not attributed or fixed here.

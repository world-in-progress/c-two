# Relay proxy test environment isolation

Windows run 36017573157 exposed a shared-process test race in `c2-core/tests/client_modes.rs`: the stale-route test read `C2_RELAY_USE_PROXY=not-a-bool` while another test intentionally published that value. Only the writers held `ENV_LOCK`; unrelated relay-configuration readers did not.

The Host reviewed the timed-out Flash task's unsealed working diff after shutdown was confirmed. Its broad closure wrapping was not imported. The integrated correction adds the same lock at the start of each of the nine configuration-reading tests and reuses it in the existing invalid-value helper. The guard outlives each test's runtimes and servers. Two pure error/release-identity tests remain independent. The test harness retains normal parallelism; no global `RUST_TEST_THREADS` setting or CI serialization is introduced. Invalid-value assertions and production parsing are unchanged, and lock poisoning from a failed test does not cause unrelated cascading failures.

Host validation: `cargo test --locked --manifest-path core/Cargo.toml -p c2-core --test client_modes` passed 13 tests, zero failures or ignored tests, with the normal parallel harness. Actual Windows confirmation remains the next hosted run. The cancelled Flash attempt has no accepted final artifact; this narrowed integration is Host-owned.

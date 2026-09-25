# Shared allocator crash safety

The shared buddy allocator must refuse further mutations if its previous holder dies or panics inside a critical section. Stealing that lock could resume partly updated bitmaps. A failed acquisition now returns a bounded error rather than hanging or panicking from the lock layer.

## Runtime contract

Segment header version 2 contains a 64-bit lock word at offset 32. It preserves the complete lower 32-bit process identifier; bit 63 records holder-owned panic poison. The header is 56 bytes, with allocation count at offset 40 and free bytes at offset 48. Compile-time assertions pin these offsets. Attaching an older layout fails before reading shifted fields. This is a clean cut alongside transport handshake version 11.

A death observation returns `SpinlockError::DeadHolder` without modifying the shared word. An OS probe and a PID-only compare-and-swap cannot distinguish an old holder that released cleanly before exiting from a new holder that reused its PID. Avoiding that write prevents a delayed observer from permanently poisoning the new holder. A process that actually dies while holding the lock leaves a nonzero word, which continues to refuse allocation and retirement. If its PID is reused, the acquire can instead exhaust its contention budget; no observer takes over the lock.

A caught panic is different: the holder still owns the critical section and marks its own word poisoned before resuming the original unwind. Under panic-abort the process leaves a held word and follows the dead-holder refusal path. Unknown or permission-denied liveness results never prove death. No acquisition counter or incarnation field is added, so there is no new counter-exhaustion policy.

Every failed acquisition iteration consumes the spin budget, including losing a compare-and-swap after observing an unlocked word. `BuddyAllocator::alloc` returns `None` on refusal and `free` returns a descriptive error. The unused recovery API and dead-holder poison reason have been removed. Panic-poisoned or dead-held backings stay unavailable until their owning pool is destroyed. Normal reclamation and peer generation replacement inspect the zero-allocation counter while holding the same SHM lock. This retirement check attempts only one CAS, with no spin, yield, or liveness probe. A separate unlocked-word load followed by a counter load would allow the peer's last free to publish zero before completing its bitmap merge. Peer last-free cache eviction uses the same protected check.

## Verification

The killed-holder regression starts a real child test process, opens the same shared mapping, acquires the allocator lock, and reports readiness with an atomic release/acquire flag. The parent kills and reaps that child, requires unsuccessful process termination, and verifies refusal, unchanged lock word and counters, and rejected retirement. A child guard also kills and reaps the process on timeout or assertion unwind. No test assumes a hard-coded Windows PID is unassigned. A controlled liveness probe reproduces clean release, PID reuse, and a delayed death observation; it verifies that the new holder remains usable. Other regressions cover busy zero-counter retirement, panic poisoning, complete `u32` PID encoding, contention budgets, old-header rejection and poison visibility through another attached mapping.

Buddy produced the allocator change in an isolated checkout. Host review required revisions for complete PID encoding and acquisition-budget accounting. Integration review additionally made readiness atomic, guaranteed child cleanup, removed the unused recovery API, protected the retirement snapshot with the SHM lock, and removed observer writes after PID-only death probes.

On macOS arm64, the integrated `cargo test --manifest-path core/Cargo.toml -p c2-mem --lib --locked` passes **100 tests**. `cargo check --manifest-path core/Cargo.toml -p c2-mem --tests --target x86_64-pc-windows-msvc --locked` also passes. The latter is compile evidence; actual Windows execution is tracked separately by the [native implementation report](../windows-native-implementation.md).

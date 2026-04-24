# C-Two Journey PPT Self-Contained Report

> Audience: mixed technical/report audience
> Target deck size: 13–16 slides
> Narrative mode: results first

## Slide 1. Title

**Takeaway**
C-Two is a resource-oriented RPC framework for Python that pushed from file-based IPC to SHM-first IPC and finally to decentralized relay-based discovery.

**Key points**
- Focus: stateful resources, not stateless endpoints
- Focus: large scientific / GIS-style payloads
- Focus: transparent local/IPC/relay access path

**Suggested visual**
Three-step timeline: file-based IPC → SHM-first IPC → relay discovery, with a resource object at the center.

**Speaker notes**
For technical listeners, name the resource model and transport path changes; for non-technical listeners, translate that into fewer copies, less setup, and easier local-to-remote use.

## Slide 2. Executive results

**Takeaway**
C-Two combines faster data movement with lower RPC overhead, zero-copy-friendly paths, and relay-based name resolution.

**Key points**
- `memory://` to IPC v3 moved the transport from polling and filesystem I/O to UDS + SHM
- README benchmark representative points at 1K / 100K / 1M rows: 0.07 / 0.38 / 3.7 ms vs Ray's 6.1 / 9.8 / 58 ms
- Relay Mesh removes address coupling by resolving `name -> route -> direct connection`

**Suggested visual**
Results dashboard with three columns: transport evolution, benchmark uplift, and relay discovery flow.

**Speaker notes**
Frame the deck around outcomes first: resource identity for app owners, SHM/relay mechanics for systems readers, and the practical payoff for everyone — less copying, less wiring, and easier local-to-remote use.

## Slide 3. Positioning: C-Two vs Ray vs iceoryx2

**Takeaway**
Ray is the closest Python comparison, while iceoryx2 is the closest transport ideal; C-Two sits between them by pairing Python resource semantics with SHM-aware transport.

**Key points**
- Ray: strong distributed actor/runtime story, but higher fixed overhead and different center of gravity
- iceoryx2: strong zero-copy middleware model, but not a Python resource RPC framework
- C-Two: stateful resource objects + Python ergonomics + SHM data path + relay-based discovery

**Suggested visual**
Three-way positioning chart. X-axis: Python resource ergonomics → transport middleware primitives. Y-axis: application semantics / statefulness → transport-only focus. Place Ray upper-left, iceoryx2 lower-right, and C-Two near the middle with a slight bias toward Python resource semantics and SHM-aware transport.

**Speaker notes**
Use workload language, not generic platform language: C-Two is built for Python resources that carry state and large payloads.

## Slide 4. Why C-Two exists

**Takeaway**
The project started from a real pain point: Python resource objects with large payloads were awkward to move across process boundaries without losing state, structure, or performance.

**Key points**
- GIS / scientific workflows combine object state with large numeric data
- Traditional RPC is endpoint-centric, not resource-centric
- The goal was location transparency without giving up object identity and data efficiency

**Suggested visual**
Pain-point diagram: a stateful Python object plus large arrays crossing process boundaries, with the old path showing copies and glue code.

**Speaker notes**
For technical listeners, connect the origin story to process boundaries and payload size; for non-technical listeners, explain that the goal was to keep a Python object usable without turning it into copies and glue code.

## Slide 5. The earliest memory:// IPC

**Takeaway**
memory:// started as filesystem-mediated IPC: the client wrote a request file, the server polled for it, and the response came back as another file.

**Key points**
- The client serialized each request and wrote it through the filesystem
- The server and client both relied on polling to discover request and response files
- Each call was a file exchange, which made the transport simple to prototype but tied it to disk-backed artifacts
- The transport was already shaped by the filesystem rather than by IPC-native mechanics

**Suggested visual**
Two-process diagram with a shared folder in the middle: client writes request file, server polls and writes response file, client polls again to read it.

**Speaker notes**
Explain this as the first transport experiment: it proved the resource model could cross process boundaries, but it did so by turning each call into files on disk.

## Slide 6. Why memory:// hit a wall

**Takeaway**
memory:// hit an architectural bottleneck: polling, serialization, and filesystem I/O imposed a fixed latency floor even for tiny payloads.

**Key points**
- Both sides relied on polling, so even small calls paid a constant waiting cost
- Serialization and file I/O dominated the round trip instead of the application work
- There was no SHM-backed zero-copy path to remove the extra copies
- The bottleneck was architectural, not just an implementation detail to tune away

**Suggested visual**
Layered latency bar showing polling at the base, then serialization and file I/O stacked above it, with a visible floor line that does not shrink for small payloads.

**Speaker notes**
Use this slide to make the core lesson explicit: the transport was not merely slow in practice, it was designed around the wrong mechanism for low-latency IPC.

## Slide 7. IPC v2: transition architecture

**Takeaway**
IPC v2 was the first clear transition away from file polling and toward real IPC by adding a Unix Domain Socket (UDS) control path.

**Key points**
- It introduced a UDS control path that replaced file polling for request and response handling
- It proved the direction was right: move toward real IPC rather than pseudo-IPC over files
- It moved request handling onto an explicit IPC channel instead of temp files and watchers
- It was a transition architecture, not yet the end state

**Suggested visual**
Bridge diagram from a folder-and-polling icon on the left to a slimmer control-path icon on the right, with the old path crossed out and the new path highlighted.

**Speaker notes**
Frame IPC v2 as a necessary proof point: it showed the project should keep moving toward IPC-native mechanics, but it still carried Python `multiprocessing.SharedMemory` pool overhead and pickle-based payload handling.

## Slide 8. IPC v2: results and remaining problems

**Takeaway**
IPC v2 was necessary but incomplete: it improved the control path, but it still did not unify lifecycle, large-payload handling, and memory behavior into one clean transport story.

**Key points**
- The story was still split across the UDS control path, the `multiprocessing.SharedMemory` pool, and pickle-based payload handling
- Lifecycle management was not yet unified end to end
- Large payload handling still lacked a single clean model
- Memory behavior remained more complicated than the final architecture would need

**Suggested visual**
Checklist graphic with one box checked for control-path improvement and several unchecked boxes for lifecycle, large payloads, and memory behavior.

**Speaker notes**
Make the transition explicit here: IPC v2 confirmed the direction, but its gaps are exactly why the later IPC design had to go further.

## Slide 9. IPC v3: architecture

**Takeaway**
IPC v3 made the transport split explicit: a UDS control plane handles coordination while SHM carries the payload, with a Rust buddy allocator managing the shared pool.

**Key points**
- The UDS control plane keeps coordination low-latency and small, so request/response signaling stays cheap even when payloads are large
- The SHM data plane moves the actual bytes, which removes filesystem I/O from the hot path and keeps large transfers off the socket
- A Rust buddy allocator is the core pool manager, so allocation, reuse, and crash recovery live in one place instead of being spread across Python objects
- Block reuse is a steady-state optimization: when the next response fits the existing block, IPC v3 can reuse that allocation instead of paying a fresh alloc/free cycle
- This architecture is designed for resource RPC, not transport demos, so the control path and data path are split by role

**Suggested visual**
Two-lane diagram: a narrow UDS control lane on top for metadata and coordination, and a wide SHM lane below for payload bytes, with a buddy allocator shown as the pool behind the SHM lane.

**Speaker notes**
Emphasize that IPC v3 is not “just faster sockets.” The point is to keep signaling tiny and deterministic while moving bulk data through shared memory, and then to make the steady state cheaper by reusing blocks instead of constantly reallocating them.

## Slide 10. IPC v3: benchmark results

**Takeaway**
IPC v3 delivered the measured speedup, but the meaningful result is the end-to-end round trip: transport-only intuition is too optimistic unless serialization and application work are included.

**Key points**
- On the realistic benchmark, IPC v3 reached a **10.7× geomean P50 speedup** for payloads at or above 10 MB, with **5.03 GB/s** peak throughput around 100 MB
- Small payloads benefited most from eliminating polling, with 64B latency dropping from **31.1 ms** to **0.12 ms**
- Larger payloads still paid for packing, unpacking, and checksum/mutation work, so the measured result is an end-to-end application result rather than a pure transport micro-benchmark
- That distinction matters: a transport-only mental model would expect the SHM path to dominate everything, but the real curve also includes serialization cost and memory-copy cost
- The practical conclusion is stronger than a micro-benchmark headline: IPC v3 is fast enough that the application work becomes visible again
- The 1GB case still improved materially, but it also shows where large-buffer copies and materialization begin to matter again, which sets up the remaining-issues slide

**Suggested visual**
Results table with three callouts: small-payload latency collapse, peak throughput at 100 MB, and a note box that labels the numbers as end-to-end benchmark results rather than transport-only intuition.

**Speaker notes**
Lead with the numbers, then explain the caveat. The transport is clearly faster, but the deck should make it obvious that the benchmark includes real serialization and payload handling work, so the speedup is not a misleading socket-only comparison.

## Slide 11. IPC v3: remaining issues

**Takeaway**
IPC v3 solved the common case, but remaining issues still showed up under large payload stress, memory pressure, and receiver-side reassembly limits.

**Key points**
- Large payload stress exposed that even a fast transport still has to move and materialize enough data to matter at 500 MB and 1 GB
- Memory pressure remains a real limit when the receiver must rebuild a full payload in memory before it can hand the result back up the stack
- Receiver-side reassembly is not free: chunk tracking, ordering, and final buffer assembly all add work that is separate from the SHM transport itself
- The limitations are therefore about end-to-end payload handling, not about the UDS control plane or the buddy allocator alone
- These are the places where a fallback architecture has to take over cleanly instead of turning a large call into a hard failure

**Suggested visual**
Stress-path diagram that shows a very large payload splitting into chunks, then reassembling on the receiver side, with warning markers for memory pressure and large-payload limits.

**Speaker notes**
Use this slide to separate wins from limits. IPC v3 is the right default, but it does not make large payloads free, and it still needs a deliberate fallback story when memory pressure appears.

## Slide 12. Comprehensive fallback strategy

**Takeaway**
The fallback story is a tiered safety net: buddy SHM first, dedicated SHM next, then file-spill, so the system can keep working under pressure instead of failing the request.

**Key points**
- The dedicated SHM path is the first fallback when the shared buddy pool is exhausted or a payload needs an isolated segment
- The file-spill path is the last resort when memory pressure is severe and shared memory allocation is no longer the right answer
- This fallback chain preserves service continuity for large payload stress cases instead of forcing a single failure mode
- Receiver-side reassembly must understand which tier produced the data so it can open, read, and release the right backing store safely
- Crash recovery and lifecycle safety matter at every tier: stale allocations must be recoverable, ownership must be clear, and cleanup must not depend on perfect shutdown
- The design goal is graceful degradation, not silent loss of performance: each fallback is slower, but each one is still usable and explicit

**Suggested visual**
Layered ladder diagram showing buddy SHM, dedicated SHM, and file-spill as descending tiers, with lifecycle arrows for open, read, reassemble, release, and recovery.

**Speaker notes**
Explain the fallback chain as an operational guarantee. Under normal load the buddy pool wins; under pressure, the transport can move down the ladder without breaking correctness, and cleanup remains safe even if a process dies mid-flight. The exact thresholds between buddy SHM, dedicated SHM, and file-spill are implementation-defined tuning choices, so the slide should be read as a guarantee of graceful fallback rather than a fixed cutoff map.

## Slide 13. Relay Mesh: overview

**Takeaway**
C-Two’s Relay Mesh removes address coupling by turning `name -> route` lookup into a local control-plane step, so clients can connect by resource name instead of tracking process addresses.

**Key points**
- Address coupling is the wrong default because it forces every client to know where every CRM lives, even when processes move, restart, or replicate
- The local relay is checked first for name resolution: same process, then local IPC route, then peer route if the resource lives on another node
- Seed relays provide bootstrap only; they are just starting points for join, not a permanent coordinator or single point of control
- The mesh is a resource-discovery layer: it decides where to connect, then the actual RPC still goes directly to the chosen relay and CRM
- Standalone mode is intentional fallback for the no-seed case, so a node can run locally without treating mesh absence as failure

**Suggested visual**
A three-stage flow: client asks local relay for `name`, relay resolves to local or peer route, client connects directly. Add a small side branch showing seed relays only for bootstrap.

**Speaker notes**
Make the main message explicit: Relay Mesh does not replace the IPC data plane. It removes manual address wiring by giving C-Two a decentralized discovery layer that resolves names to routes, then hands the call back to the existing transport path.

## Slide 14. Relay Mesh: detailed mechanisms

**Takeaway**
C-Two keeps relay discovery consistent by storing route state inside each relay, spreading updates with gossip, and repairing gaps with anti-entropy.

**Key points**
- `RouteTable` is relay-internal state, not a CRM resource; it lives in Rust and tracks routes, peers, and route metadata without scheduler or serialization overhead
- Join works through seed relays: a new relay contacts any reachable seed, receives a full snapshot, then merges that state into its local route table
- Gossip is the dissemination mechanism for route updates, while anti-entropy is the correction mechanism that reconciles missed updates after delay or partition
- Deterministic ordering keeps resolution stable: relays sort competing peer routes the same way, so every node converges on the same first choice
- The consistency model is practical, not magical: local state is authoritative for a node, replicated peer state converges over time, and ordering is framed to be repeatable even when message arrival is not

**Suggested visual**
A relay mesh diagram with three labeled layers: local RouteTable state, gossip fan-out between relays, and periodic anti-entropy digests that patch missing or stale routes. Show ordered route entries to visualize deterministic selection.

**Speaker notes**
Walk from control-plane state to convergence. First explain that each relay owns its own route table, then show how gossip spreads changes, and finally show how anti-entropy repairs anything that gossip missed. Close by emphasizing that deterministic ordering matters because discovery must not become random when multiple relays advertise the same name.

## Slide 15. Final summary

**Takeaway**
C-Two’s journey is a move from pseudo-IPC to real IPC and then from address-based wiring to resource discovery, without losing Python resource semantics.

**Key points**
- The durable value is the combination of Python resource semantics, SHM-aware transport, and decentralized resolution
- Pseudo-IPC gave way to real IPC, and then the system added discovery so clients could address resources by name instead of by location
- The relay mesh completes the user story by making discovery decentralized and self-contained rather than dependent on a hard-coded host list
- The boundary is honest: C-Two is not trying to replace every distributed runtime; it is focused on Python resources, efficient data movement, and practical discovery for that workload
- The final shape is a system that keeps the application model, keeps the fast transport, and removes the last manual wiring step

**Suggested visual**
Closing arc diagram: file-backed IPC → real UDS/SHM IPC → relay-based resource discovery, with a callout box for “Python resource semantics + SHM-aware transport + decentralized resolution.”

**Speaker notes**
End on scope and value. The story is not “we built a universal distributed platform”; it is “we built a narrower system that matches the workload well.” That honesty makes the technical result stronger: C-Two now has a coherent path from resource identity to transport to discovery.

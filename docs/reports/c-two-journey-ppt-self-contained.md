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
- Hold-mode / SHM zero-copy, same-machine, same NumPy payload, end-to-end representative points at 1K / 100K / 1M rows: 0.07 / 0.38 / 3.7 ms vs Ray's 6.1 / 9.8 / 58 ms
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
- IPC v2 proved the UDS control path could replace file polling for request and response handling
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
IPC v3 made the transport split explicit: a UDS control plane handles coordination, SHM carries the fast-path payload, and chunked streaming covers the oversized or SHM-unavailable path, with a Rust buddy allocator managing the shared pool underneath.

**Key points**
- The UDS control plane keeps coordination low-latency and small, so request/response signaling stays cheap even when payloads are large
- The SHM data plane moves the actual bytes, which removes filesystem I/O from the hot path and keeps large transfers off the socket
- IPC v3 is not one transport path but a selector: small calls stay inline, larger ones prefer buddy SHM, and payloads beyond the chunk threshold fall back to chunked streaming
- Chunked streaming is negotiated as a capability in the handshake, and the first chunk carries route/method control while later chunks are data-only
- A Rust buddy allocator is the core pool manager, so allocation, reuse, and crash recovery live in one place instead of being spread across Python objects
- Block reuse is a steady-state optimization: when the next response fits the existing block, IPC v3 can reuse that allocation instead of paying a fresh alloc/free cycle
- This architecture is designed for resource RPC, not transport demos, so the control path and data path are split by role

**Suggested visual**
Three-path diagram: inline frame for small calls, buddy SHM fast path for large calls, and chunked streaming path for oversized or SHM-failed transfers. Keep a narrow UDS control lane above them and show the buddy allocator behind the SHM path.

**Speaker notes**
Emphasize that IPC v3 is not “just faster sockets.” The point is to keep signaling tiny and deterministic while choosing among inline, SHM, and chunked paths based on payload size and allocator state. Chunked streaming matters here because it prevents one giant frame from becoming the only fallback once the preferred SHM route is unavailable.

## Slide 10. IPC v3: benchmark results

**Takeaway**
IPC v3 delivered the measured speedup, but the meaningful result is the end-to-end round trip: transport-only intuition is too optimistic unless serialization and application work are included.

**Key points**
- On the realistic benchmark, IPC v3 reached a **10.7× geomean P50 speedup** over `memory://` for payloads at or above 10 MB, with **5.03 GB/s** peak throughput around 100 MB
- Small payloads benefited most from eliminating polling, with 64B latency dropping from **31.1 ms** to **0.12 ms**
- Larger payloads still paid for packing, unpacking, and checksum/mutation work, so the measured result is an end-to-end application result rather than a pure transport micro-benchmark
- That distinction matters: a transport-only mental model would expect the SHM path to dominate everything, but the real curve also includes serialization cost and memory-copy cost
- The practical conclusion is stronger than a micro-benchmark headline: IPC v3 is fast enough that the application work becomes visible again
- The 1GB case still improved materially, but it also shows where large-buffer copies and materialization begin to matter again, which sets up the remaining-issues slide

**Suggested visual**
Results table with three callouts: small-payload latency collapse, peak throughput at 100 MB, and a note box that labels the numbers as end-to-end benchmark results rather than transport-only intuition.

**Speaker notes**
Lead with the numbers, then explain the caveat. The transport is clearly faster, but the deck should make it obvious that the benchmark includes real serialization and payload handling work, so the speedup is not a misleading socket-only comparison. This slide is the payload-size sweep vs `memory://`; Slide 2 is the row-based NumPy application comparison vs Ray.

## Slide 11. IPC v3: remaining issues

**Takeaway**
IPC v3 solved the common case, but remaining issues still showed up under large payload stress, memory pressure, and receiver-side reassembly limits.

**Key points**
- Large payload stress exposed that even a fast transport still has to move and materialize enough data to matter at 500 MB and 1 GB
- Memory pressure remains a real limit when the receiver must rebuild a full payload in memory before it can hand the result back up the stack
- Chunked streaming avoids one monolithic frame, but it is still bounded reassembly rather than true incremental consumption: chunk tracking, ordering, and final buffer assembly all add work of their own
- The limitations are therefore about end-to-end payload handling, not about the UDS control plane or the buddy allocator alone
- These are the places where a fallback architecture has to take over cleanly instead of turning a large call into a hard failure

**Suggested visual**
Stress-path diagram that shows a very large payload splitting into chunks, then reassembling on the receiver side, with warning markers for memory pressure and large-payload limits.

**Speaker notes**
Use this slide to separate wins from limits. IPC v3 is the right default, and chunked streaming helps it survive larger transfers, but the current design still reassembles the full payload before handing it upward. That means “streaming” here is a transport-level chunked path, not an application-level incremental processing model.

## Slide 12. Comprehensive fallback strategy

**Takeaway**
The fallback story is not one generic ladder: sender-side allocation and receiver-side reassembly take different paths, but both degrade explicitly from fast SHM toward slower dedicated or file-backed storage instead of failing the call outright.

**Key points**
- Transport fallback and storage fallback are separate layers: if the preferred buddy SHM request/reply path cannot be used, the system can fall back to chunked streaming first, and then the reassembly buffer still chooses buddy SHM, dedicated SHM, or file-spill underneath
- Sender-side SHM allocation prefers existing buddy segments first, then reclaims trailing idle segments and lazily opens new buddy segments before it gives up on the shared pool
- Dedicated SHM is the oversized or no-buddy-capacity path: one payload gets its own segment, plus a small `read_done` header so creator-side GC can wait for peer consumption or crash timeout
- Receiver-side reassembly does not just mirror the sender: `alloc_handle` can land directly in buddy SHM, dedicated SHM, or file-spill depending on size, free space, and current RAM pressure
- File-spill is not a visible temp-file protocol; it is an unlink-on-create mmap fallback used when low-memory heuristics say “do not create more SHM mappings” or when SHM creation fails
- After chunk reassembly completes on file-spill, the code still tries to promote the finished buffer back into SHM, so disk is a survival path rather than the preferred steady state
- Release rules differ by tier: buddy frees immediately and marks segments idle, dedicated frees are deferred until reader completion or timeout, and file-spill is reclaimed by dropping the mmap after the backing file is already unlinked
- The guarantee is graceful degradation with explicit slower tiers, not a magical zero-copy path that works the same way under all pressure conditions

**Suggested visual**
Two-level diagram: top level is transport selection (`buddy reply/request or chunked streaming`), bottom level is backing-store selection during reassembly (`buddy/dedicated/file-spill -> optional promote back to SHM -> release`). Add callouts for `read_done` on dedicated SHM and unlink-on-create semantics for file-spill.

**Speaker notes**
Explain this slide as two related fallback paths, not one generic memory tier chart. First comes transport choice: SHM pointer transfer when possible, chunked streaming when the request or reply cannot stay on the preferred SHM path. Then comes reassembly storage choice: the receiving side may rebuild into buddy SHM, dedicated SHM, or file-spill, and if it ends on file-spill it can still try to promote the completed buffer back into SHM. Dedicated SHM also has stricter lifecycle tracking than buddy SHM: the reader marks completion, and creator-side GC waits for that signal or a crash timeout. The exact thresholds are implementation-defined tuning choices, so the point of the slide is the guarantee of explicit, recoverable degradation rather than one fixed cutoff table.

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

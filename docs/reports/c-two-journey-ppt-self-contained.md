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
memory:// started as filesystem-mediated IPC: the client wrote a request artifact, the server polled it, and the response came back as another file.

**Key points**
- The client serialized each request and wrote it through the filesystem
- The server and client both relied on polling to discover request and response artifacts
- There was no SHM-backed zero-copy path; file I/O and serialization carried the work
- It was a workable first transport, but it was already shaped by the filesystem

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
IPC v2 was the first clear transition away from file polling and toward real IPC.

**Key points**
- It removed the most obvious file-polling pain on the control path
- It proved the direction was right: move toward real IPC rather than pseudo-IPC over files
- It made the transport feel more like a transport and less like a filesystem workflow
- It was a transition architecture, not yet the end state

**Suggested visual**
Bridge diagram from a folder-and-polling icon on the left to a slimmer control-path icon on the right, with the old path crossed out and the new path highlighted.

**Speaker notes**
Frame IPC v2 as a necessary proof point: it showed the project should keep moving toward IPC-native mechanics, but it still carried legacy complexity.

## Slide 8. IPC v2: results and remaining problems

**Takeaway**
IPC v2 was necessary but incomplete: it improved the control path, but it still did not unify lifecycle, large-payload handling, and memory behavior into one clean transport story.

**Key points**
- The control path was better, but the overall transport story was still split
- Lifecycle management was not yet unified end to end
- Large payload handling still lacked a single clean model
- Memory behavior remained more complicated than the final architecture would need

**Suggested visual**
Checklist graphic with one box checked for control-path improvement and several unchecked boxes for lifecycle, large payloads, and memory behavior.

**Speaker notes**
Make the transition explicit here: IPC v2 confirmed the direction, but its gaps are exactly why the later IPC design had to go further.

## Slide 9. IPC v3: architecture

**Takeaway**

**Key points**

**Suggested visual**

**Speaker notes**

## Slide 10. IPC v3: benchmark results

**Takeaway**

**Key points**

**Suggested visual**

**Speaker notes**

## Slide 11. IPC v3: remaining issues

**Takeaway**

**Key points**

**Suggested visual**

**Speaker notes**

## Slide 12. Comprehensive fallback strategy

**Takeaway**

**Key points**

**Suggested visual**

**Speaker notes**

## Slide 13. Relay Mesh: overview

**Takeaway**

**Key points**

**Suggested visual**

**Speaker notes**

## Slide 14. Relay Mesh: detailed mechanisms

**Takeaway**

**Key points**

**Suggested visual**

**Speaker notes**

## Slide 15. Final summary

**Takeaway**

**Key points**

**Suggested visual**

**Speaker notes**

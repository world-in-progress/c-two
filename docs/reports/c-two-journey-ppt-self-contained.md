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
Open with the end state: one Python-facing resource model, multiple transport paths, and a clear progression in transport efficiency.

## Slide 2. Executive results

**Takeaway**
The project's main outcome is a step-change in transport efficiency and usability: lower fixed overhead than conventional Python actor RPC, zero-copy-friendly structured data paths, and relay-based name resolution.

**Key points**
- `memory://` to IPC v3 moved the transport from polling and filesystem I/O to UDS + SHM
- README benchmark shows hold-mode NumPy workloads substantially ahead of Ray on the tested single-host path
- Relay Mesh removes address coupling by resolving `name -> route -> direct connection`

**Suggested visual**
Results dashboard with three columns: transport evolution, benchmark uplift, and relay discovery flow.

**Speaker notes**
Frame the deck around outcomes first: faster data movement, simpler client usage, and less wiring between names and addresses.

## Slide 3. Positioning: C-Two vs Ray vs iceoryx2

**Takeaway**
Ray is the closest Python comparison, while iceoryx2 is the closest transport ideal; C-Two sits between them by pairing Python resource semantics with SHM-aware transport.

**Key points**
- Ray: strong distributed actor/runtime story, but higher fixed overhead and different center of gravity
- iceoryx2: strong zero-copy middleware model, but not a Python resource RPC framework
- C-Two: stateful resource objects + Python ergonomics + SHM data path + relay-based discovery

**Suggested visual**
Three-way positioning chart with axes for Python ergonomics and transport efficiency, placing C-Two between runtime and middleware styles.

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
Explain the origin story in one sentence: the framework was shaped by workloads where object identity and data volume matter at the same time.

## Slide 5. The earliest memory:// IPC

**Takeaway**

**Key points**

**Suggested visual**

**Speaker notes**

## Slide 6. Why memory:// hit a wall

**Takeaway**

**Key points**

**Suggested visual**

**Speaker notes**

## Slide 7. IPC v2: transition architecture

**Takeaway**

**Key points**

**Suggested visual**

**Speaker notes**

## Slide 8. IPC v2: results and remaining problems

**Takeaway**

**Key points**

**Suggested visual**

**Speaker notes**

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

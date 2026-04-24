# C-Two Journey PPT Self-Contained Report Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Create a new self-contained markdown file that can be used directly to build a 13–16 page C-Two journey PPT without relying on old appendices or scattered design references.

**Architecture:** Keep the existing historical report intact and create a new report file dedicated to slide production. The new file will follow a fixed slide-by-slide template, use a results-first narrative, and synthesize validated material from the existing report, README, benchmark reports, fallback design, and relay mesh design into one presentation mother-script.

**Tech Stack:** Markdown, repository design docs, benchmark reports, ripgrep, git

**Spec:** `docs/superpowers/specs/2026-04-24-c-two-journey-ppt-self-contained-design.md`

---

## File Structure

| File | Responsibility |
|------|----------------|
| `docs/reports/c-two-journey-ppt-self-contained.md` | New self-contained PPT source document; the main deliverable |
| `docs/reports/c-two-journey-ppt-report.md` | Existing long-form historical report; read-only source material |
| `README.md` | Current public positioning and benchmark summary source |
| `docs/logs/ipc-v3-optimization-report.md` | IPC v3 architecture and benchmark source |
| `docs/reports/2026-04-18-kostya-benchmark.md` | Ray comparison and structured payload benchmark source |
| `docs/superpowers/specs/2026-03-30-unified-memory-fallback-design.md` | Fallback chain source |
| `docs/superpowers/specs/2026-07-20-relay-mesh-resource-discovery-design.md` | Relay mesh mechanism source |

## Task 1: Create the new report skeleton

**Files:**
- Create: `docs/reports/c-two-journey-ppt-self-contained.md`

- [ ] **Step 1: Create the document header and slide skeleton**

Create the new file with this top-level structure:

```md
# C-Two Journey PPT Self-Contained Report

> Audience: mixed technical/report audience
> Target deck size: 13–16 slides
> Narrative mode: results first

## Slide 1. Title
## Slide 2. Executive results
## Slide 3. Positioning: C-Two vs Ray vs iceoryx2
## Slide 4. Why C-Two exists
## Slide 5. The earliest memory:// IPC
## Slide 6. Why memory:// hit a wall
## Slide 7. IPC v2: transition architecture
## Slide 8. IPC v2: results and remaining problems
## Slide 9. IPC v3: architecture
## Slide 10. IPC v3: benchmark results
## Slide 11. IPC v3: remaining issues
## Slide 12. Comprehensive fallback strategy
## Slide 13. Relay Mesh: overview
## Slide 14. Relay Mesh: detailed mechanisms
## Slide 15. Final summary
```

Immediately under each slide header, reserve the fixed sub-structure:

```md
**Takeaway**

**Key points**

**Suggested visual**

**Speaker notes**
```

- [ ] **Step 2: Verify the skeleton contains all 15 slides**

Run: `rg "^## Slide" docs/reports/c-two-journey-ppt-self-contained.md`
Expected: 15 matches, one for each slide title above

- [ ] **Step 3: Commit the skeleton**

```bash
git add docs/reports/c-two-journey-ppt-self-contained.md
git commit -m "docs: scaffold self-contained c-two ppt report"
```

## Task 2: Write the results-first opening and positioning slides

**Files:**
- Modify: `docs/reports/c-two-journey-ppt-self-contained.md`
- Reference: `README.md`, `docs/reports/2026-04-18-kostya-benchmark.md`

- [ ] **Step 1: Fill Slide 1 and Slide 2**

Add content with this shape:

```md
## Slide 1. Title
**Takeaway**
C-Two is a resource-oriented RPC framework for Python that pushed from file-based IPC to SHM-first IPC and finally to decentralized relay-based discovery.

**Key points**
- Focus: stateful resources, not stateless endpoints
- Focus: large scientific / GIS-style payloads
- Focus: transparent local/IPC/relay access path

## Slide 2. Executive results
**Takeaway**
The project's main outcome is a step-change in transport efficiency and usability: lower fixed overhead than conventional Python actor RPC, zero-copy-friendly structured data paths, and relay-based name resolution.

**Key points**
- `memory://` to IPC v3 moved the transport from polling and filesystem I/O to UDS + SHM
- README benchmark shows hold-mode NumPy workloads substantially ahead of Ray on the tested single-host path
- Relay Mesh removes address coupling by resolving `name -> route -> direct connection`
```

- [ ] **Step 2: Fill Slide 3 and Slide 4**

Add positioning that stays workload-specific:

```md
## Slide 3. Positioning: C-Two vs Ray vs iceoryx2
**Takeaway**
Ray is the closest Python comparison, while iceoryx2 is the closest transport ideal; C-Two sits between them by pairing Python resource semantics with SHM-aware transport.

**Key points**
- Ray: strong distributed actor/runtime story, but higher fixed overhead and different center of gravity
- iceoryx2: strong zero-copy middleware model, but not a Python resource RPC framework
- C-Two: stateful resource objects + Python ergonomics + SHM data path + relay-based discovery

## Slide 4. Why C-Two exists
**Takeaway**
The project started from a real pain point: Python resource objects with large payloads were awkward to move across process boundaries without losing state, structure, or performance.

**Key points**
- GIS / scientific workflows combine object state with large numeric data
- Traditional RPC is endpoint-centric, not resource-centric
- The goal was location transparency without giving up object identity and data efficiency
```

- [ ] **Step 3: Verify the opening has no appendix-style dependency**

Run: `rg "附录|参考|详见|见 docs/|见 README" docs/reports/c-two-journey-ppt-self-contained.md -n`
Expected: no matches in Slides 1–4

- [ ] **Step 4: Commit the opening slides**

```bash
git add docs/reports/c-two-journey-ppt-self-contained.md
git commit -m "docs: add ppt opening and positioning slides"
```

## Task 3: Write the memory:// and IPC v2 story

**Files:**
- Modify: `docs/reports/c-two-journey-ppt-self-contained.md`
- Reference: `docs/reports/c-two-journey-ppt-report.md`, `docs/logs/ipc-v3-optimization-report.md`

- [ ] **Step 1: Fill Slides 5–6 with the earliest memory:// architecture and bottlenecks**

Use these concrete points:

```md
- memory:// worked by writing request and response artifacts through the filesystem
- both sides relied on polling, which imposed a high latency floor even for tiny payloads
- there was no SHM-backed zero-copy path; serialization and file I/O dominated
- the bottleneck was architectural, not just implementation detail
```

- [ ] **Step 2: Fill Slides 7–8 with IPC v2 as a transition, not the final form**

Use these concrete points:

```md
- IPC v2 replaced the most obvious file-polling pain on the control path
- it proved the direction was right: move toward real IPC rather than pseudo-IPC over files
- but it still did not unify lifecycle, large-payload handling, and memory behavior into one clean transport story
- therefore IPC v2 should be presented as necessary but incomplete
```

- [ ] **Step 3: Verify the narrative arc is explicit**

Run: `rg "necessary but incomplete|architectural bottleneck|transition" docs/reports/c-two-journey-ppt-self-contained.md -n`
Expected: matches in the IPC v2 section

- [ ] **Step 4: Commit the legacy transport section**

```bash
git add docs/reports/c-two-journey-ppt-self-contained.md
git commit -m "docs: add memory and ipc v2 slide content"
```

## Task 4: Write IPC v3, benchmarks, and fallback architecture

**Files:**
- Modify: `docs/reports/c-two-journey-ppt-self-contained.md`
- Reference: `README.md`, `docs/logs/ipc-v3-optimization-report.md`, `docs/superpowers/specs/2026-03-30-unified-memory-fallback-design.md`

- [ ] **Step 1: Fill Slides 9–10 with IPC v3 architecture and benchmark outcomes**

The markdown should explicitly cover:

```md
- UDS control plane for low-latency coordination
- SHM data plane for large payload movement
- Rust buddy allocator as the core pool manager
- block reuse / reusable allocation path as an important steady-state optimization
- benchmark wording that distinguishes realistic end-to-end results from transport-only intuition
```

- [ ] **Step 2: Fill Slides 11–12 with remaining issues and the full fallback story**

The markdown should explicitly cover:

```md
- large payload stress
- memory pressure
- dedicated SHM path
- file-spill path
- receiver-side reassembly considerations
- crash recovery and lifecycle safety
```

- [ ] **Step 3: Verify both results and limits are present**

Run: `rg "remaining issues|limitations|fallback|file-spill|memory pressure" docs/reports/c-two-journey-ppt-self-contained.md -n`
Expected: multiple matches across Slides 11–12

- [ ] **Step 4: Commit the IPC v3 section**

```bash
git add docs/reports/c-two-journey-ppt-self-contained.md
git commit -m "docs: add ipc v3 and fallback slide content"
```

## Task 5: Write Relay Mesh and final summary, then verify self-containment

**Files:**
- Modify: `docs/reports/c-two-journey-ppt-self-contained.md`
- Reference: `docs/superpowers/specs/2026-07-20-relay-mesh-resource-discovery-design.md`

- [ ] **Step 1: Fill Slides 13–14 with Relay Mesh overview and detailed mechanisms**

The markdown should explicitly cover:

```md
- why address coupling is a problem
- local relay resolution priority
- RouteTable as relay-internal state
- join/bootstrap through seed relays
- gossip and anti-entropy as dissemination/correction mechanisms
- deterministic ordering / consistency framing
- standalone mode as an intentional fallback, not a failure mode
```

- [ ] **Step 2: Fill Slide 15 with the final summary**

Use this final framing:

```md
- C-Two's journey is a move from pseudo-IPC to real IPC and then from address-based wiring to resource discovery
- the durable value is the combination of Python resource semantics, SHM-aware transport, and decentralized resolution
- the deck should state boundaries honestly: C-Two is not trying to replace every distributed runtime
```

- [ ] **Step 3: Verify the file is self-contained**

Run: `rg "docs/|README|spec|report|附录|参考资料" docs/reports/c-two-journey-ppt-self-contained.md -n`
Expected: no presentation-breaking dependency language; only minimal source notes if truly necessary

- [ ] **Step 4: Verify every slide follows the fixed template**

Run: `rg "^\*\*Takeaway\*\*|^\*\*Key points\*\*|^\*\*Suggested visual\*\*|^\*\*Speaker notes\*\*" docs/reports/c-two-journey-ppt-self-contained.md -n`
Expected: each label appears 15 times, once per slide

- [ ] **Step 5: Commit the completed report**

```bash
git add docs/reports/c-two-journey-ppt-self-contained.md
git commit -m "docs: add self-contained c-two ppt report"
```

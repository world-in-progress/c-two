# C-Two Journey PPT Self-Contained Report — Design Spec

**Date:** 2026-04-24  
**Status:** Draft  
**Scope:** Rewrite the existing C-Two journey PPT markdown into a self-contained presentation source file that can be used to build slides directly, without depending on long reference appendices or process-heavy historical design notes.

## Problem Statement

`docs/reports/c-two-journey-ppt-report.md` contains valuable material, but it is too documentation-heavy for presentation production:

1. It mixes final conclusions with old intermediate design discussions.
2. It appends a large body of references that are useful for engineering archaeology but distracting for slide creation.
3. Its current narrative is closer to a design dossier than a presentation mother-script.

The new deliverable should be a **single self-contained markdown file** that supports direct PPT production for a mixed audience: technical enough to preserve architecture and mechanism detail, but structured like a report, not like an internal design archive.

## Audience

**Primary audience:** mixed audience  
- Technical reviewers and engineers need real architectural detail.  
- Non-specialist reviewers still need a clear problem → solution → result storyline.

## Goals

1. Make the markdown itself sufficient to create a 13–16 page PPT.
2. Use a **results-first narrative** so the audience understands early why the story matters.
3. Preserve the full IPC evolution arc:
   - earliest `memory://` implementation
   - its performance bottlenecks
   - IPC v2 transition
   - IPC v2 results and remaining problems
   - IPC v3 architecture, results, and remaining problems
   - comprehensive fallback strategy
4. Include a detailed but presentation-friendly explanation of **Relay Mesh**:
   - name-based resolution
   - RouteTable
   - join/bootstrap flow
   - gossip
   - anti-entropy
   - consistency and standalone fallback
5. Include comparison framing with **Ray** and, where useful, **iceoryx2**, focused on positioning and pain points rather than generic winner/loser claims.

## Non-Goals

1. Do not preserve the existing long reference appendix structure.
2. Do not turn the new markdown into a full design-history archive.
3. Do not include every benchmark variant or every intermediate design branch.
4. Do not write a PPTX file yet in this phase.

## Proposed Narrative Strategy

The presentation should use a **results-first** structure:

1. Start with the outcome and positioning:
   - what C-Two achieves
   - where it beats conventional Python RPC patterns
   - why Ray / iceoryx2 comparisons matter
2. Then return to the original problem:
   - stateful Python resources
   - scientific / GIS-style large data movement
   - the need for location-transparent resource access
3. Then walk forward through the transport evolution:
   - `memory://`
   - IPC v2
   - IPC v3
   - fallback architecture
4. End with Relay Mesh as the distributed discovery layer that completes the story from local IPC performance to multi-node resource resolution.

This ordering intentionally moves comparative positioning ahead of project origin, because the target deck is a report, not an internal chronology talk.

## Slide-Level Structure

Target size: **13–16 slides**

1. Title slide
2. Executive result slide
3. Positioning slide: C-Two vs Ray / iceoryx2
4. Problem origin: why C-Two exists
5. Earliest `memory://` architecture
6. `memory://` performance bottlenecks
7. IPC v2: what changed
8. IPC v2: results and unresolved issues
9. IPC v3 architecture
10. IPC v3 benchmark results
11. IPC v3 remaining issues
12. Comprehensive fallback strategy
13. Relay Mesh overview
14. Relay Mesh detailed mechanisms
15. Final summary / boundaries / value

If content density requires it, the Relay Mesh overview and mechanism slides may expand to two separate detailed slides while keeping the total within 16.

## Content Format for the New Markdown

Each slide section in the new file should use a fixed, presentation-oriented structure:

```md
## Slide N. Title

**Takeaway**
One sentence describing what the audience should remember.

**Key points**
- 3–5 slide-ready bullets

**Suggested visual**
Recommended figure type: timeline / architecture / benchmark chart / comparison table / flow diagram

**Speaker notes**
Short narration guidance for the presenter; not intended as slide body text.
```

This format ensures the markdown is directly actionable for PPT production and does not require re-mining other documents.

## Content Selection Rules

### Include in the main body

1. The operating model and bottlenecks of the original `memory://` transport.
2. IPC v2 as a meaningful transitional step rather than a discarded footnote.
3. IPC v3 as the main architectural inflection point:
   - UDS control plane
   - SHM data plane
   - buddy allocator
   - reusable block strategy
4. Benchmark numbers that are already stable and presentation-worthy from repository docs and benchmark sources.
5. The two-axis fallback story:
   - sender-side allocation fallback
   - receiver-side reassembly / spill fallback
   - dedicated SHM
   - file-spill
   - memory pressure defense
   - crash recovery and lifecycle safety
6. Relay Mesh mechanics and guarantees.
7. Comparison framing with Ray and iceoryx2 around architecture fit and workload fit.

### Exclude or demote to notes

1. Intermediate design debates that no longer describe the current implementation.
2. Crate-level refactor history unless it directly explains a current performance or reliability property.
3. Large appendices of source references.
4. Benchmark variants that do not materially support the main claims.

## Comparison Framing

### Ray

Ray should be presented as the most relevant comparison point for Python users because it already offers:
- actor abstraction
- distributed execution story
- strong ndarray-friendly path

But the deck should clarify that C-Two focuses on a different center of gravity:
- stateful resource objects rather than distributed task orchestration
- lower fixed overhead on single-host or small-cluster resource RPC
- stronger emphasis on SHM-backed structured data paths and explicit hold semantics

### iceoryx2

iceoryx2 should be used as a systems reference point, not as a like-for-like Python framework competitor:
- strong zero-copy middleware foundation
- excellent transport-layer inspiration
- not itself a Python resource-oriented RPC model

The deck should use iceoryx2 to highlight transport ideals and engineering direction, then explain that C-Two solves the Python-facing object/resource usability gap above that layer.

## Accuracy Rules

The rewritten markdown must clearly separate:

1. **Verified current outcomes** — implemented mechanisms and validated benchmark conclusions.
2. **Remaining problems / limitations** — especially in IPC v2 and IPC v3 discussion.
3. **Positioning claims** — framed as workload-fit statements, not universal superiority claims.

Whenever a number is used, the surrounding text should make its scope obvious:
- workload type
- payload type
- whether it is realistic end-to-end or transport-dominant
- what comparison baseline is being used

## Source Backbone

The rewrite should primarily draw from:

1. `docs/reports/c-two-journey-ppt-report.md`
2. `README.md`
3. `docs/logs/ipc-v3-optimization-report.md`
4. `docs/reports/2026-04-18-kostya-benchmark.md`
5. `docs/superpowers/specs/2026-03-30-unified-memory-fallback-design.md`
6. `docs/superpowers/specs/2026-07-20-relay-mesh-resource-discovery-design.md`
7. benchmark source files under `benchmarks/`

These sources should inform the new file, but the new file itself must remain self-contained.

## Acceptance Criteria

The new markdown is successful if:

1. A presenter can build slides directly from it without consulting a large appendix.
2. The IPC evolution is understandable as a single arc with cause and effect.
3. Relay Mesh is clear as a resource-discovery layer separate from the IPC data plane.
4. Ray and iceoryx2 comparisons help clarify C-Two's niche rather than distract from it.
5. The file reads like a presentation mother-script, not like a design archive.

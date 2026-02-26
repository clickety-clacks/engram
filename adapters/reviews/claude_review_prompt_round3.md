Final adversarial pass. Review only substantive issues in adapters/ADAPTERS.md and adapters/CLAUDE_REVIEWERS.md.
Use same output format; if none say exactly No substantive findings.
--- SPEC START ---
# Engram — Spec

> A causal index over code. Retrieve *why* before changing *what*.

## Product Invariants

These are non-negotiable. If a design decision violates any of these, the decision is wrong.

### P1. Single static binary
One file. No runtime, no VM, no interpreter, no daemon, no sidecar process. `engram` is a CLI tool like `git`. Copy it to a machine and it works.

### P2. Zero-config start
`engram init` in a repo. That's it. No config file required. No account. No server. No environment variables. Sane defaults for everything.

### P3. Local-first, offline-only
Everything works with no network. No server component. No cloud dependency. No telemetry. The data lives next to the code, on the user's machine.

### P4. Fast over gigabytes
Indexing and querying must be fast over repositories with gigabytes of accumulated trace text. Content-addressed storage, compression, and efficient indexing are not optimizations — they are requirements.

### P5. Lightweight install
`curl -sSL ... | sh` or `brew install engram`. No Docker. No Electron. No package manager beyond the OS-native one. Same install story as git circa 2005.

### P6. Non-prescriptive storage
Store raw facts (messages, tool I/O, patches), not interpretations (intent labels, decision categories, summaries). The system does not decide what matters. Future agents do.

### P7. Span-granular provenance
The unit of provenance is a **code region** (span/stanza), not a commit, not a file, not a session. A single line can have evidence from many sessions across time.

### P8. Agents as primary consumer
The primary audience is a future agent that needs to understand why code exists before changing it. Human readability is a nice-to-have, not a design driver.

### P9. Compression-first
All stored text is compressed (zstd). Content-addressed deduplication is built in. The marginal cost of recording a session must be trivially small relative to the codebase size.

### P10. Git coexistence
Engram lives alongside git, not instead of it. `.engram/` next to `.git/`. Git remains the source of truth for code artifacts. Engram is the source of truth for epistemic history.

### P11. Ubiquitous harness integration (non-negotiable)
Engram must have first-class, deterministic ingestion for the dominant agent harnesses (at minimum: Codex CLI and Claude Code). If Engram cannot reliably capture `code.read`/`code.edit` lineage from these harnesses, the product is incomplete regardless of query/index quality.

---

## Core Concepts

### Trace Tape
An append-only chronological log of what happened during an agent session. Compressed JSONL. Contains:
- Raw messages in/out (full text, not summaries)
- Tool calls + results (name, args, stdout/stderr/exit code)
- Patch hunks produced (file, before/after ranges, before/after text or hashes)
- Minimal metadata: model ID, timestamp, repo HEAD at start

Tapes are immutable once closed. Content-addressed by hash.

### Evidence Fragment
A pointer to a specific moment inside a tape:
- `(tape_id, event_offset)`
- Kind tag: `edit` | `read` | `tool` | `message`

This is how the system "points to different parts of transcripts at different conversations."

### Span Anchor
A robust content fingerprint of a code region. Must survive:
- Line number shifts (code above/below changes)
- Small edits to surrounding code
- Code moves within a file
- Moderate refactors

Implementation: winnowed fingerprints over token k-grams (language-agnostic). Details TBD in implementation spec.

### Evidence Index
The reverse index — the heart of the system:
```
anchor → [(tape_id, event_offset, kind, file_path, timestamp), ...]
```
Many-to-many. A span returns many sessions; a session touches many anchors.

Stored in SQLite (single file, no daemon, fast lookups).

### Span Linkage + Tombstones
Lineage is a graph, not just a simple chain. The critical rule:

**Location alone is never enough to link spans.**

A provenance edge requires at least one strong signal:
1. Content fingerprint similarity ≥ `LINK_THRESHOLD` (default `0.30`), or
2. Explicit agent-declared successor link via `span.link` tape event.

Edges below `LINK_THRESHOLD` are stored as `location_only` (forensics only),
excluded from default query output, and do not count as lineage.

Edges at or above `LINK_THRESHOLD` are always stored. The **query-time**
`--min-confidence` flag (default `0.50`) controls which edges are traversed
during `explain`. This is a read-time filter, not a write-time gate — lowering
it reveals more of the stored graph without re-indexing.

When a span is deleted, Engram writes a **tombstone** to the evidence index:
- The anchor hashes of the deleted span
- The tape_id + event_offset of the deletion
- The file path and range at time of deletion
- A timestamp

Tombstones are never erased. They are queryable (`engram explain --include-deleted`).

A new span at the same location starts a **new chain root** by default, and is only
promoted into the old chain if one of the strong-link rules above is satisfied.

**Identical re-insertion:** If deleted code is re-inserted verbatim (similarity ≥0.90),
it links to the old chain. This is correct — the provenance of that text is real
regardless of the deletion gap. The tombstone remains to mark the gap.

This prevents false ancestry when unrelated text is inserted where old text used to be.

#### Edge storage (non-prescriptive)
Each edge stores raw facts only:
- `confidence`: fingerprint similarity score (0.0–1.0)
- `location_delta`: `same` | `adjacent` | `moved` | `absent`
- `cardinality`: `1:1` | `1:N` | `N:1`
- `agent_link`: boolean — true if created via `span.link` event

No interpretive labels (`refactor`, `move_detected`, etc.) are stored.
Downstream consumers derive categories from the raw signals.

Default query-time confidence tiers for `--pretty` display:
- `>=0.90` + `same`/`adjacent` → shows as "edit"
- `>=0.85` + `moved` → shows as "move"
- `>=0.50` → shows as "related"
- `<0.50` → hidden unless `--min-confidence` lowered
- `<0.30` without `agent_link` → `location_only`, hidden unless `--forensics`

If `agent_link` is true, the edge is always included in default traversal
regardless of confidence score.

#### Traversal limits
BFS fan-out is capped at `MAX_FANOUT` (default `50`) edges per node.
When a node exceeds the cap, edges are traversed in descending confidence order
and the remainder is noted in output as truncated. Total traversal budget:
`MAX_EDGES` (default `500`) across the entire BFS.

---

## CLI Commands

Minimal command set. More can be added later; these are the irreducible core.

### `engram init`
Create `.engram/` directory in current repo. No arguments required.

### `engram record <command>`
Run an agent/tool command and record a trace tape. Captures stdin/stdout/stderr, file diffs (before/after), tool invocations. Writes tape to `.engram/tapes/`.

Alternatively: `engram record --stdin` to pipe in a pre-existing session transcript (JSONL).

### `engram explain <file>:<start>-<end>`
**The killer query.** Given a span, return all evidence trails:
1. Compute anchors for selected text
2. Direct index lookup
3. Lineage ancestor traversal (configurable depth)
4. Return ranked evidence fragments grouped by session

Output: structured list of (tape, event, kind, timestamp) — machine-readable by default, human-readable with `--pretty`.

### `engram tapes`
List recorded tapes. Metadata only (timestamp, model, repo head, label, size).

### `engram show <tape_id>`
Dump a tape. Default: compacted view. `--raw` for full event stream.

### `engram gc`
Garbage-collect unreferenced tape blobs. Keep index entries and lineage links.

### `engram search <query>` (future / bolt-on)
Concept search over tape contents. Requires optional vector index.

---

## On-Disk Layout

```
repo/
  .git/
  .engram/
    config.toml          # optional overrides (all have defaults)
    index.sqlite         # evidence index + lineage links
    tapes/
      <hash>.jsonl.zst   # compressed trace tapes
    objects/
      <ab>/<cdef...>     # content-addressed blobs (large tool outputs, snapshots)
```

Everything under `.engram/` is the complete state. Portable — copy it and the provenance travels.

---

## Tape Event Schema (JSONL)

Minimal event vocabulary. Each line is one event:

```jsonl
{"t":"2026-02-15T17:30:00Z","k":"msg.in","role":"user","content":"..."}
{"t":"...","k":"msg.in","role":"system","content":"..."}
{"t":"...","k":"msg.out","role":"assistant","content":"..."}
{"t":"...","k":"tool.call","tool":"cargo test","args":"--lib","cwd":"/src"}
{"t":"...","k":"tool.result","tool":"cargo test","exit":0,"stdout":"...","stderr":"..."}
{"t":"...","k":"code.read","file":"src/api.rs","range":[120,140]}
{"t":"...","k":"code.edit","file":"src/api.rs","before_range":[120,125],"after_range":[120,128],"before_hash":"...","after_hash":"..."}
```

Event kinds:
- `msg.in` / `msg.out` — model messages
- `tool.call` / `tool.result` — tool invocations
- `code.read` — agent read/referenced a code region
- `code.edit` — agent produced a patch hunk
- `span.link` — agent declares explicit provenance successor (see below)
- `meta` — session metadata (model, repo state, label)

No "decision" events. No "plan" events. No taxonomy.

#### `span.link` event
```jsonl
{"t":"...","k":"span.link","from_file":"src/auth.rs","from_range":[42,60],"to_file":"src/auth/session.rs","to_range":[1,25],"note":"extracted to module"}
```
Agents emit this when they know provenance that fingerprinting alone can't capture
(e.g., extracting a function to a new file with heavy restructuring). The indexer
creates an `agent_link: true` edge. The `note` field is optional free text stored
on the edge for human/agent consumption. This is the only prescriptive tape event
— agents are never required to emit it, but it improves lineage when they do.

---

## Indexing Pipeline

On tape close (or incrementally during recording):

1. For each `code.edit` event:
   - Extract before/after text for each hunk
   - Compute span anchors for both
   - Add `after_anchors → evidence_fragment` to index
   - Link before→after only when confidence passes `LINK_THRESHOLD` or agent declares explicit successor
   - If a tracked span is removed, emit a tombstone record (required protocol)

2. For each `code.read` event:
   - Extract referenced text
   - Compute span anchors
   - Add `anchors → evidence_fragment` to index (kind=read)

3. Compress tape, store content-addressed.

---

## Query Algorithm (`engram explain`)

```
INPUT: file path + line range (current working tree)

1. Extract text from current file at given range
2. Compute span anchors for that text
3. DIRECT: lookup each anchor in evidence index → collect matching sessions
4. LINEAGE: BFS backward through lineage links (depth limit, default 10)
   - Excludes edges with confidence < `--min-confidence` (default 0.50)
   - Always includes `agent_link` edges regardless of confidence
   - Respects `MAX_FANOUT` and `MAX_EDGES` traversal limits
   - For each ancestor anchor, collect additional sessions
5. ORDER: sort sessions by (touch count DESC, most recent touch DESC)
6. FOR EACH session: extract transcript window around each touch event
7. OUTPUT: ordered list of raw transcript windows, one block per session
```

Output is raw transcript text — the actual messages and tool I/O from each session
that touched the span. No scoring. No summarization. No interpretation.
The consumer decides what matters.

---

## What Engram Does NOT Do

- Replace git
- Interpret or classify intent
- Score, rank, or weight evidence by kind or recency
- Perform saliency analysis — that is the job of a downstream agent
- Generate documentation (that's a consumer, not the tool)
- Require a server or account
- Mandate a specific agent or IDE
- Decide what's important in a transcript
- Promise deterministic replay

---

## The Saliency Layer (not part of Engram)

Engram's output is raw transcript windows. It does not decide what is relevant
to the specific task at hand — that requires knowing what the task is.

The intended workflow above Engram:

```
engram explain src/auth.rs:42-48
  → raw transcript windows (one block per session that touched the span)

SALIENCY AGENT (separate, not Engram)
  → receives: raw windows + description of the refactor about to happen
  → reads the transcripts, picks out what matters for this specific task
  → outputs: a compact brief

CODING AGENT
  → receives: only the brief
  → context window preserved
  → enters the refactor already informed
```

The saliency agent is explicitly **not** part of this system. It is the intended
consumer of `engram explain` output. It runs in its own context window so the
coding agent's context is not polluted with raw history.

This design keeps Engram non-prescriptive: it makes no assumptions about what
"relevant" means. That judgment belongs to the reading agent, which knows the
task.

---

## Open Questions

- **P0 integration seam (must solve first):** deterministic adapters for Codex CLI + Claude Code that emit/derive `code.read` and `code.edit` events without LLM interpretation.
- **Anchor algorithm specifics**: k-gram size, window size, hash function. Needs benchmarking against real codebases.
- **Lineage depth default**: how many hops back is useful vs noisy?
- **`code.read` capture**: how to detect reads in agents that don't explicitly report them? May need agent-side hooks.
- **Multi-language tokenization**: token k-grams need to handle diverse syntax. Start language-agnostic (whitespace + punctuation split)?
- **Vector search bolt-on**: when to add, what embedding model, local-only constraint.
- **Tape ingestion from existing agents**: adapters for Claude Code, Codex, Aider session formats.
- **`.gitignore` equivalent**: what should `.engram/` exclude? Secrets redaction strategy.

---

## Implementation Language

**Rust.** Single static binary. No runtime. Compiler catches agent mistakes. Fast enough for the tight loops (anchor computation, index scans, compression).

## Crate Structure (preliminary)

```
engram/
  src/
    main.rs              # CLI entry point (clap)
    lib.rs               # public API
    tape/
      mod.rs             # tape recording + reading
      event.rs           # event types + serde
      compress.rs        # zstd compression
    anchor/
      mod.rs             # span anchor computation
      winnow.rs          # winnowed k-gram fingerprinting
    index/
      mod.rs             # evidence index (SQLite)
      lineage.rs         # lineage link storage + traversal
    query/
      mod.rs             # explain algorithm
      rank.rs            # evidence ranking
    store/
      mod.rs             # content-addressed object store
```
--- SPEC END ---
--- ADAPTERS.md ---
# Engram Adapter Contract (Canonical)

Scope: deterministic ingestion adapters that convert external harness logs into Engram tape events.

This file is the canonical contract for adapter behavior. If implementation, tests, or reviews conflict with this file, this file wins.

Upstream authority: this contract must remain consistent with `/Users/mike/shared-workspace/shared/specs/engram.md`. If conflict exists, the Engram spec wins and this file must be updated.

## Product Intent

- Adapters exist only to preserve raw, deterministic provenance from real harness artifacts.
- Adapters must not add interpretation, summaries, or inferred intent labels.
- Keep design lightweight: enforce only what is required for deterministic ingestion and release safety.

## MUST Invariants

1. Deterministic only:
   - Every emitted event must come from explicit on-disk harness facts.
   - Same input bytes must produce byte-identical normalized JSONL output.
2. Raw-fact preservation:
   - Preserve source values needed for auditability (`tool`, args/input, result payloads, timestamps, identifiers).
   - Do not classify or enrich semantics beyond Engram event schema.
3. Coverage declaration:
   - Each adapter must publish guaranteed deterministic coverage for:
     - `meta`
     - `msg.in`
     - `msg.out`
     - `tool.call`
     - `tool.result`
     - `code.read`
     - `code.edit`
     - `span.link`
   - Unsupported kinds must be declared as unsupported or partial, never silently claimed as complete.
4. Explicit partial semantics:
   - If a harness cannot deterministically provide an event kind (for example, free-form shell edits), adapter must emit explicit partial/unsupported status in adapter metadata.
5. Stable failure behavior:
   - Invalid input or unknown schema/version must fail with explicit machine-readable error unless policy allows downgrade mode.
6. No hidden fallback inference:
   - No LLM interpretation.
   - No heuristic that can silently change meaning across runs without version bump and contract update.

## SHOULD Value-Adds

- Retain source IDs for call/result correlation when available.
- Preserve large tool outputs (or references) without truncating semantics.
- Extend required metadata with additional diagnostic fields that are not required by contract.

## OPTIONAL Value-Adds

- Additional deterministic projections that improve utility but do not alter meaning (for example, extracted file lists from patch headers).
- Strict mode toggles that reject ambiguous but currently tolerated inputs.

## Deterministic Ingest Protocol

1. Detect harness family and schema/version from explicit fields.
2. Validate minimal schema requirements for claimed coverage.
3. Normalize to Engram tape events in chronological order, preserving source sequence.
   - If multiple source events have identical timestamps, tie-break deterministically using source order.
   - Never reorder by event type; tape chronology is required.
   - Correlate `tool.call` / `tool.result` by explicit IDs when present.
4. Emit adapter metadata event (or equivalent side metadata) with coverage map and version fields.
   - Minimum required metadata fields:
     - `adapter_id`
     - `adapter_version`
     - `harness_family`
     - `harness_version` (or `unknown`)
     - per-kind coverage map for:
       - `meta`, `msg.in`, `msg.out`, `tool.call`, `tool.result`, `code.read`, `code.edit`, `span.link`
     - downgrade mode/status when applicable
5. If unsupported/ambiguous:
   - fail-fast in strict mode
   - or emit explicit downgraded coverage in permissive mode

## Coverage Semantics

Coverage is per event kind and must be reported as one of:

- `full`: all occurrences deterministically extractable by contract
- `partial`: only a deterministic subset extractable
- `none`: unsupported

Rules:

- `full` may only be claimed with deterministic extraction proofs (tests + fixtures).
- `partial` must document deterministic boundaries (what is included/excluded).
- CI must fail if code claims `full` while fixture tests demonstrate misses.
- `meta` coverage is evaluated against spec-defined meta fields: `model`, `repo state` (for example `repo_head`), and `label`.
  - `full`: adapter deterministically provides all spec-defined meta fields.
  - `partial`: adapter deterministically provides a subset and marks missing fields.

## Integration Seam Contract

Default production seam:

- Adapters produce normalized Engram JSONL suitable for `engram record --stdin`.
- Normalized JSONL must conform to Engram tape event schema (`meta`, `msg.in`, `msg.out`, `tool.call`, `tool.result`, `code.read`, `code.edit`, `span.link`).
- If an adapter is embedded as library code instead of standalone piping, output must remain contract-identical.

## Harness Versioning Policy

### Supported-Version Matrix (required)

Each adapter must maintain a matrix in code/docs:

- harness family (e.g., codex-cli, claude-code)
- supported version ranges
- schema detection fields
- guaranteed coverage profile per version range

### Version/Schema Detection

- Must rely on explicit version/schema fields when present.
- If absent, must use a documented deterministic fingerprint strategy (field presence/signature), not guesswork.

### Unknown-Version Behavior

Default policy:

- Strict mode: hard fail with `unknown_harness_version`.
- Permissive mode: ingest only contract-safe deterministic subset, mark coverage degraded, and emit warning metadata.

### Downgrade/Fail Rules

- Downgrade allowed only when:
  - deterministic subset remains truthful
  - degraded coverage is explicitly emitted
- Hard fail required when:
  - required correlation fields are missing and would make emitted facts misleading
  - parser cannot guarantee deterministic mapping for claimed event kinds

## CI and Release Compliance Gates

Adapters are release-blocking surfaces.

Required CI gates:

1. Fixture conformance tests:
   - version-pinned fixtures per supported harness version range
   - golden normalized output snapshots
2. Determinism test:
   - repeated parse of identical input must produce identical output bytes
3. Coverage assertions:
   - fixture-validated `full/partial/none` status matches declared matrix
4. Unknown-version tests:
   - strict mode fails
   - permissive mode degrades coverage explicitly
5. Contract lint:
   - adapter docs/matrix/code metadata stay in sync (no undeclared version range)

Release gate expectations:

- No adapter release if any required gate fails.
- No expansion of claimed coverage without new fixtures and matrix update.
- No schema heuristic changes without explicit contract changelog entry.

## Enforcement Expectations for Reviews

- Reviewers must reject:
  - unspecced inference
  - silent downgrade behavior
  - unsupported `full` coverage claims
  - version handling that guesses without deterministic basis
- Reviewers should prioritize correctness of deterministic boundaries over feature breadth.
--- CLAUDE_REVIEWERS.md ---
# Claude Reviewer Guidance (Adversarial)

This guidance is for Claude-based reviewers evaluating adapter changes.

## Hard Rule

You MUST review against `adapters/ADAPTERS.md` as the canonical contract.
You MUST also verify `adapters/ADAPTERS.md` remains aligned with `/Users/mike/shared-workspace/shared/specs/engram.md`; if drift exists, flag it as blocking.

## Required Review Focus

1. Determinism:
   - Are emitted events derived only from explicit harness facts?
   - Can same input produce different outputs?
2. Coverage honesty:
   - Do `full/partial/none` claims match tests and matrix?
   - Are unsupported areas explicitly marked?
3. Version governance:
   - Is schema/version detection explicit and deterministic?
   - Are unknown versions handled by documented strict/permissive rules?
4. Release safety:
   - Are required CI gates represented by concrete tests?
   - Is any contract claim untested?

## Blocking Findings Criteria

Mark as blocking if any of the following are true:

- unspecced inference beyond deterministic contract
- silent downgrade or silent data loss
- unsupported `full` coverage claim
- unknown-version behavior diverges from contract
- matrix/docs/code mismatch for supported versions

## Output Format Requirement

- Blocking findings first, ordered by severity, with file references and rationale.
- Then non-blocking findings.
- Then nits.
- If no substantive issues: explicitly say "No substantive findings".

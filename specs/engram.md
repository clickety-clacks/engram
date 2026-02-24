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

### Fingerprint Sources

Engram fingerprints all tape content uniformly — not just code spans from edit/read events, but also message text, tool output, and any other content in the tape. This means if an agent discusses code in a message (quotes it, reasons about it, pastes output containing it), those fingerprints automatically overlap with the code span's fingerprints and the match surfaces through normal query mechanics.

No special "detect code in messages" heuristic is needed. The same fingerprinting mechanism applies to everything. More content fingerprinted = more anchors in the index = better recall when querying provenance.

All tape content is fingerprinted uniformly. There are no "primary" vs "secondary" sources — fingerprinting treats all text equally. Confidence tiers emerge from event classification (when available), not from fingerprinting itself.

When an adapter provides event classification:
- `code.edit` / `code.read` anchors get higher confidence in explain output
- `msg` / `tool` anchors get lower default confidence
- But all participate in matching equally

When an adapter provides only raw text (no classification):
- All anchors are equal confidence
- Provenance still works — just without structured metadata

This means a minimal adapter that dumps raw transcript text gives you working Engram. Better adapters that classify events give you richer output.

### Prior art

The fingerprinting approach is well-established. Engram applies the same winnowed k-gram technique used in:
- **MOSS** (Stanford, 1994) — code plagiarism detection via winnowed fingerprints
- **Winnowing** (Schleimer, Wilkerson, Aiken, SIGMOD 2003) — foundational local document fingerprinting algorithm
- **ssdeep / sdhash** — fuzzy hashing for forensic document similarity

The difference: these tools answer "are these two documents similar?" Engram applies the same math to answer "which transcript produced this code?"


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
**The killer query.** Given a span, return ranked evidence trails with transcript context.

1. Compute anchors for selected text
2. Direct index lookup
3. Lineage ancestor traversal (configurable depth)
4. For each anchor, return a transcript window: `--before N` lines before the anchor event, `--after M` lines after
5. Return ranked evidence fragments grouped by session

Output: structured list of (tape, event, kind, timestamp, transcript_window) — machine-readable by default, human-readable with `--pretty`.

#### Window parameters

Why windowed output? Agent context windows are finite and expensive. Dumping entire transcripts wastes tokens. Instead, `explain` returns a focused window around the anchor point — just enough context for the agent to understand the reasoning. The agent controls how much it sees, and can always expand if the initial window isn't enough.


| Flag | Default | Description |
|------|---------|-------------|
| `--before N` | configurable | Lines of transcript before anchor event |
| `--after M` | configurable | Lines of transcript after anchor event |
| `--brief` | off | Return anchor metadata only (tape_id, offset, confidence), no transcript |

Defaults for `--before` and `--after` are set in `.engram/config.yml` (or equivalent) under `explain.window.before` and `explain.window.after`. Agents can override per-call.

#### Navigation from anchors

Why navigation? The first `explain` window may not contain the full reasoning — especially for decisions that built up over many messages. Rather than guessing a large window upfront (expensive), agents start small and walk backward/forward through the transcript incrementally, pulling only what they need.

After the initial `explain` call, agents can expand context:

```
engram view <tape_id> --at <offset> --before 200
engram view <tape_id> --at <offset> --after 50
```

This allows token-efficient incremental research: start with a default window, expand only where needed.

#### Iterative span expansion for disambiguation

Why expansion? Fingerprinting works by matching small word patterns. A 3-line selection may share patterns with dozens of similar code regions across the codebase, producing noisy results. But the surrounding context (the code above and below the selection) is usually more unique. By expanding the span, the fingerprint becomes more specific and false matches drop away.

Small spans produce many fingerprint matches. Agents can iteratively grow the selected span until results narrow to a useful set.

```
engram explain <file>:<start>-<end> --expand-until N
```

Behavior:
- If initial query returns more than N results, Engram automatically expands the span in both directions (reading more surrounding code from the file).
- Re-queries with the larger span until result count drops to N or below, or a maximum expansion limit is reached.
- Returns the narrowed result set plus the final expanded span range used.

This lets agents start with a precise region of interest and let Engram disambiguate automatically, without manually guessing how much context to include.

When not using `--expand-until`, agents can do this manually:
1. Query small span → too many matches
2. Widen selection → re-query
3. Repeat until results are clean

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

## Conversation Continuation Detection

### Problem

Agent sessions compact, hit rate limits, or get resumed across multiple transcript files. A single logical task can span many tapes. Without detecting continuations, Engram treats each tape as isolated and may miss reasoning that started in an earlier session.

### Approach: fingerprint-based tape alignment

Engram already fingerprints code spans using winnowed k-gram hashes. The same mechanism applies to transcript content within tapes.

When session B is a continuation of session A:
- Session B typically contains carryover content from session A (compaction summaries, repeated assistant text, re-stated context).
- Fingerprinting this content produces anchors that match anchors in session A's tail.
- Overlapping fingerprint anchors between tape A's tail region and tape B's head region constitute a deterministic continuation signal.

### Detection rules

1. **Fingerprint tape content** (not just code spans) during ingest.
2. **Compare tail anchors of earlier tapes against head anchors of later tapes** within the same project/repo scope.
3. **Score continuation confidence** by overlap density:
   - High anchor overlap at tail/head boundary → strong continuation signal.
   - Single shared phrase → weak/no signal.
   - Shared boilerplate (system prompts, AGENTS.md) must be excluded from matching (known-boilerplate filter).
4. **Store continuation edges** between tapes: `tape_a → tape_b` with confidence score.
5. **`engram explain` traversal follows continuation edges** when walking backward through lineage, so the agent can reach reasoning from earlier sessions.

### Harness-specific signals (supplemental, not required)

Some harnesses provide explicit continuation markers:
- **Codex CLI**: `type: "compacted"` events with `replacement_history` payload.
- **Claude Code**: no explicit continuation pointer, but compaction summaries and rate-limit terminal messages are detectable.

When present, these markers can boost continuation confidence but are not required. The fingerprint overlap mechanism works independently of harness cooperation.

### Boilerplate exclusion

Every harness injects repeated scaffolding (system prompts, AGENTS.md, environment context) into every session. This content produces matching fingerprints across unrelated sessions.

Mitigation:
- Maintain a per-harness boilerplate fingerprint set (computed once from known scaffolding content).
- Exclude boilerplate anchors from continuation matching.
- Only non-boilerplate overlap counts toward continuation confidence.

### Query behavior

When `engram explain` walks backward through lineage and reaches the beginning of a tape:
- If a continuation edge exists to an earlier tape, traversal continues into that tape.
- The agent can use `engram view` to navigate backward across the continuation boundary seamlessly.
- Continuation edges are labeled in output so the agent knows it crossed a session boundary.

### Invariants

- Continuation detection must be deterministic (same tapes → same edges).
- False positives (linking unrelated sessions) must be minimized via boilerplate exclusion + confidence thresholds.
- Continuation edges are advisory — they improve recall but do not affect correctness of direct span-anchor linkage.

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

# Engram Design Document

## The Problem

Agents write code. They reason about it, discover constraints, make trade-offs, and produce working implementations. Then the session ends. The reasoning vanishes. The code remains, but the *why* behind every line is gone.

Git tells you *what changed* and *when*. Engram tells you *why this code exists*.

## What Engram Is

Engram is a causal index over code for agent-driven development. It answers a single question: **"Given this span of code, what was the reasoning that produced it?"**

It does this by capturing the full execution trail agents already emit — prompts, tool calls, reads, edits, results — then making that trail queryable at the level of individual code spans.

Engram is designed for a future where most code is written by agents. In this model (sometimes called the "dark factory" of software engineering), every code change flows through an agent session. If every edit is agent-mediated, then the session transcript is a complete causal record. Engram makes that record useful.

## Value Proposition

Modern agents are strong at local reasoning but weak at longitudinal memory. An agent refactoring code today has no access to the conversation that produced that code last week — even if both ran in the same repo.

Engram fixes this by turning prior work into retrievable context:

- Preserve full causal history, not just commit diffs
- Retrieve the exact evidence behind any span before refactoring
- Warm future agent context with real prior decisions, constraints, and tradeoffs
- Reduce repeated mistakes caused by missing historical intent

The intended consumer is an agent, not a human dashboard. Output is machine-readable by default.

## Core Thesis

Every code span has an epistemic history. There was a conversation — a chain of prompts, reasoning, tool invocations, discoveries, and decisions — that caused those exact bytes to exist at that location. Engram preserves that chain and makes it retrievable.

## Design Principles

### P1. Single static binary
One binary. No runtime. No daemon. No external dependencies at install time. `curl | sh` or `brew install engram` and you're running. SQLite is bundled (`rusqlite --features bundled`).

### P2. Non-prescriptive storage
Store raw facts, not interpretations. No "intent" labels. No "decision" categories. No taxonomies. Downstream consumers decide what matters. Engram is an index, not an analyst.

### P3. Agents are the primary consumer
Output is structured for machine consumption by default. Human-readable output is available via `--pretty`, but the default format is optimized for agent context windows.

### P4. Compression-first
Tapes are zstd-compressed. Large tool outputs are content-addressed and deduplicated. Storage should remain manageable even over thousands of sessions on large codebases.

### P5. Append-only tapes
Trace tapes are immutable once written. They can be created and read, never modified or deleted. This is a hard invariant — it enables safe merging, deduplication, and trust in provenance chains.

### P6. Zero-config start
`engram init` creates the `.engram/` directory. That's it. All configuration has sensible defaults. An agent should be able to start using Engram without reading a config file.

### P7. Git coexistence
Engram lives alongside git, not instead of it. `.engram/` next to `.git/`. Git remains the source of truth for code artifacts. Engram is the source of truth for epistemic history.

### P8. Local-first, offline-only
No network required. No accounts. No servers. Everything runs locally. This is a tool, not a service.

### P9. Deterministic core
Given the same inputs (tapes + code), Engram produces the same outputs. No randomness, no LLM interpretation in the core pipeline. Non-deterministic enrichment is allowed but must be labeled and is never required.

### P10. Ubiquitous harness integration
Engram must work with the dominant agent harnesses (at minimum: Codex CLI and Claude Code). If Engram cannot capture provenance from the tools people actually use, the product is incomplete. Integration is not a nice-to-have — it is the product.

## How It Works: A Story

An agent is about to refactor a function. Before touching it, it asks Engram: "Why is this code here?"

1. **Fingerprint the span.** Engram reads the selected code and chops it into small overlapping word patterns (k-grams). Each pattern gets hashed. The result is a bag of hashes — a unique fingerprint of that exact code.

2. **Check the index.** Engram looks up those hashes in its index. The index was built earlier when tapes were ingested. Every time an agent read, edited, or even discussed code, Engram chopped that content into the same kind of word patterns and stored them alongside a pointer to that moment in the transcript.

3. **Find matches.** Results come back: "Tape 7, event 42 has 92% of the same word patterns." That's high confidence — almost certainly the same code. "Tape 3, event 18 has 60% match." That's a partial match — maybe an earlier version before some edits.

4. **Return transcript windows.** For each match, Engram returns a window of transcript around the anchor point. The agent reads: "In tape 7, the previous agent was fixing an auth timeout bug and changed this function because tokens were expiring too fast."

5. **Navigate if needed.** If the window isn't enough context, the agent can expand it — walking backward or forward through the transcript, or following continuation links to earlier sessions.

Now the agent knows *why this code exists* before changing it. It won't accidentally undo the timeout fix.

## Fingerprinting: The Core Mechanism

### What fingerprinting is

Fingerprinting is how Engram connects code spans to transcripts. It works by breaking text into small overlapping word patterns (k-grams), hashing each one, and keeping a representative subset (via winnowing). Two pieces of text that share content will produce overlapping hash sets.

This is not novel — the same technique has been used for decades:
- **MOSS** (Stanford, 1994) — code plagiarism detection via winnowed fingerprints
- **Winnowing** (Schleimer, Wilkerson, Aiken, SIGMOD 2003) — the foundational algorithm for local document fingerprinting
- **ssdeep / sdhash** — fuzzy hashing for forensic document similarity

The difference: these tools answer "are these two documents similar?" Engram applies the same math to answer "which transcript produced this code?"

### Fingerprint everything

Engram fingerprints all tape content uniformly — not just code from edit/read events, but also message text, tool output, and any other content in the tape. If an agent discusses code in a message (quotes it, reasons about it, pastes output containing it), those fingerprints automatically overlap with the code span's fingerprints and the match surfaces through normal query mechanics.

No special "detect code in messages" heuristic is needed. The same fingerprinting mechanism applies to everything. More content fingerprinted means more anchors in the index, which means better recall when querying provenance.

This also means the minimum viable adapter is extremely simple: just emit the raw transcript text. Fingerprinting handles the rest.

### Confidence scores

Confidence is computed from fingerprint overlap, not content interpretation:
- Count matching k-gram hashes between two spans
- Divide by total hashes in the comparison window
- High ratio = high confidence

This is deterministic arithmetic (like Jaccard similarity), not semantic understanding.

### Small spans and disambiguation

A small code selection (3 lines) may share word patterns with many similar regions across the codebase, producing noisy results. But the surrounding context — the code above and below the selection — is usually more unique.

Agents can iteratively expand the selected span until results narrow:

1. Select small span → too many matches
2. Expand selection to include more surrounding code → re-query
3. Repeat until results are clean

Engram supports this directly:

```
engram explain <file>:<start>-<end> --expand-until 3
```

This automatically grows the span in both directions until the result count drops to the target, then returns the narrowed results plus the final span range used.

### Span anchors

A span anchor is a robust content fingerprint of a code region. It must survive:
- Line number shifts (code above/below changes)
- Small edits to surrounding code
- Code moves within a file
- Moderate refactors

Implementation uses winnowed fingerprints over token k-grams (language-agnostic). We use existing Rust winnowing crate implementations rather than writing our own — the algorithm is well-established and the value of Engram is in the system around it, not in reimplementing fingerprint math.

## Tapes: The Raw Record

### What a tape is

A tape is an append-only, immutable JSONL stream of events captured during a single agent session. When the session ends, the tape is zstd-compressed and stored content-addressed in `.engram/tapes/`.

Tapes are the source of truth. The index is a derived artifact that can be rebuilt from tapes at any time. This means you can change calibration parameters (k-gram size, window size, thresholds) and rebuild the index without losing any history.

### Why tapes matter

Agent sessions are ephemeral. After a Claude Code or Codex run ends, its internal conversation — prompts, tool calls, edits — is gone with no persistent record (beyond what the harness chooses to log). Tapes capture this before it vanishes.

### Tape event schema

Minimal event vocabulary. Each line is one JSON event:

```jsonl
{"t":"2026-02-15T17:30:00Z","k":"meta","model":"claude-sonnet-4","repo_head":"a3f91bc"}
{"t":"...","k":"msg.in","role":"user","content":"Fix the auth timeout bug"}
{"t":"...","k":"msg.out","role":"assistant","content":"I'll check the token expiry logic..."}
{"t":"...","k":"tool.call","tool":"Read","args":{"file":"src/auth.rs","range":[42,60]}}
{"t":"...","k":"tool.result","tool":"Read","stdout":"fn verify_token(&self)..."}
{"t":"...","k":"code.read","file":"src/auth.rs","range":[42,60]}
{"t":"...","k":"code.edit","file":"src/config.rs","before_range":[88,96],"after_range":[88,96],"before_hash":"...","after_hash":"..."}
{"t":"...","k":"span.link","from_file":"src/auth.rs","from_range":[42,60],"to_file":"src/auth/session.rs","to_range":[1,25],"note":"extracted to module"}
```

Event kinds and what they capture:

| Kind | What it captures | Key fields |
|------|-----------------|------------|
| `meta` | Session metadata: model, repo state, harness identity | `model`, `repo_head`, `label`, `source` |
| `msg.in` | User/human prompt or task input to the agent | `text` |
| `msg.out` | Agent/assistant response or reasoning output | `text` |
| `tool.call` | Agent invokes a tool (read file, run command, edit, etc.) | `tool`, `call_id`, `args` |
| `tool.result` | Result returned from a tool invocation | `tool`, `call_id`, `stdout`, `stderr`, `exit` |
| `code.read` | Agent reads a specific file/range | `file`, `range`, `anchor_hashes` |
| `code.edit` | Agent modifies a specific file/range | `file`, `before_hash`, `after_hash`, `before_range`, `after_range` |
| `span.link` | Explicit agent-declared lineage between two spans | `from_file`, `from_range`, `to_file`, `to_range`, `note` |

All events carry: `t` (ISO timestamp), `k` (event kind), `source` (harness + session ID).

No "decision" events. No "plan" events. No taxonomy. Raw facts only.

### The `span.link` event

`span.link` is the only "prescriptive" event. Agents emit it when they know provenance that fingerprinting alone can't capture (e.g., extracting a function to a new file with heavy restructuring). The indexer creates an `agent_link: true` edge. Agents are never required to emit it, but it improves lineage when they do.

### Tape immutability

Tapes are write-once. Once a tape file is created, it is never modified or deleted. This is the foundational invariant that enables:
- Safe merging across branches and forks (just copy missing tape files)
- Deduplication by filename/ID
- Trust in provenance chains
- Index rebuilds from the same immutable source

## Evidence Index

The evidence index is the reverse lookup — the heart of the query system:

```
anchor_hash → [(tape_id, event_offset, kind, file_path, timestamp), ...]
```

Many-to-many. A span returns many sessions; a session touches many anchors.

Stored in SQLite (single file, no daemon, fast lookups). The index lives in `.engram-cache/index.sqlite` — it is a derived artifact, never committed to source control, and can be rebuilt from tapes at any time.

### Multiple indices

Because tapes are immutable and the index is derived, you can maintain multiple index profiles (e.g., `default`, `calibration-a`, `calibration-b`) built from the same tapes with different parameters. This enables side-by-side comparison of recall/precision without affecting the source data.

## Span Linkage and Tombstones

Lineage is a graph, not just a simple chain. The critical rule:

**Location alone is never enough to link spans.**

This is important because code at the same file and line number may be completely unrelated to what was there before. If an agent deletes a function and writes an unrelated new function at the same location, those should NOT be linked. An agent researching provenance for the new function should not get transcripts about the old deleted function — that would poison its context with irrelevant history.

A provenance edge requires at least one strong signal:
1. Content fingerprint similarity ≥ `LINK_THRESHOLD` (default `0.30`), or
2. Explicit agent-declared successor link via `span.link` tape event.

### How edges work

Edges below `LINK_THRESHOLD` are stored as `location_only` (forensics only), excluded from default query output, and do not count as lineage.

Edges at or above `LINK_THRESHOLD` are always stored. The query-time `--min-confidence` flag (default `0.50`) controls which edges are traversed during `explain`. This is a read-time filter, not a write-time gate — lowering it reveals more of the stored graph without re-indexing.

Each edge stores raw facts only:
- `confidence`: fingerprint similarity score (0.0–1.0)
- `location_delta`: `same` | `adjacent` | `moved` | `absent`
- `cardinality`: `1:1` | `1:N` | `N:1`
- `agent_link`: boolean — true if created via `span.link` event

No interpretive labels (`refactor`, `move_detected`, etc.) are stored. Downstream consumers derive categories from the raw signals if they want display labels.

If `agent_link` is true, the edge is always included in default traversal regardless of confidence score — the agent knows what it did.

### Tombstones

When a span is deleted, Engram writes a tombstone to the evidence index. Tombstones record:
- The anchor hashes of the deleted span
- The tape_id + event_offset of the deletion
- The file path and range at time of deletion
- A timestamp

Tombstones are never erased. They are provenance data — they tell you "code used to exist here and was deliberately removed." A new span at the same location starts a new chain root by default, and is only promoted into the old chain if the strong-link rules above are satisfied.

**Identical re-insertion:** If deleted code is re-inserted verbatim (similarity ≥0.90), it links to the old chain. This is correct — the provenance of that text is real regardless of the deletion gap. The tombstone remains to mark the gap.

### Traversal limits

BFS fan-out is capped at `MAX_FANOUT` (default `50`) edges per node. When a node exceeds the cap, edges are traversed in descending confidence order and the remainder is noted in output as truncated. Total traversal budget: `MAX_EDGES` (default `500`) across the entire BFS.

## CLI Commands

### `engram init`

Create `.engram/` and `.engram-cache/` directories in the current repo. No arguments required. Zero-config — all settings have sensible defaults. An agent should be able to run this and start using Engram immediately.

### `engram ingest`

Import new harness logs into tapes and rebuild the local index. This is the primary way tapes enter the system during normal development. Adapters detect installed harnesses, parse their logs deterministically, and normalize events into Engram tape format.

Ingest is incremental — it tracks what has already been imported and only processes new content. Running it multiple times with no new harness activity is effectively a no-op (idempotent given unchanged inputs).

### `engram record <command>`

Run a command and capture a live tape. Captures stdin/stdout/stderr, file diffs, tool invocations. Writes tape to `.engram/tapes/`. Alternatively: `engram record --stdin` to pipe in a pre-existing session transcript.

### `engram explain <file>:<start>-<end>`

The killer query. Given a span, return ranked evidence trails with transcript context.

1. Compute anchors for selected text
2. Direct index lookup
3. Lineage ancestor traversal (configurable depth)
4. For each anchor, return a transcript window around the anchor event
5. Return ranked evidence fragments grouped by session

Output is machine-readable by default, human-readable with `--pretty`.

#### Window parameters

Agent context windows are finite and expensive. Dumping entire transcripts wastes tokens. Instead, `explain` returns a focused window around the anchor point — just enough context for the agent to understand the reasoning. The agent controls how much it sees, and can always expand if the initial window isn't enough.

| Flag | Default | Description |
|------|---------|-------------|
| `--before N` | configurable | Lines of transcript before anchor event |
| `--after M` | configurable | Lines of transcript after anchor event |
| `--brief` | off | Anchor metadata only (tape_id, offset, confidence), no transcript |
| `--min-confidence X` | 0.50 | Override confidence threshold for lineage traversal |
| `--all` | off | Include low-confidence and location-only edges |
| `--expand-until N` | off | Auto-expand span until result count ≤ N |

Defaults for `--before` and `--after` are set in `.engram/config.yml` under `explain.window.before` and `explain.window.after`. Agents can override per-call based on their judgment of how much context they need on the first read.

#### Navigation from anchors

The first `explain` window may not contain the full reasoning — especially for decisions that built up over many messages. Rather than guessing a large window upfront (expensive), agents start small and walk backward/forward through the transcript incrementally, pulling only what they need.

```
engram view <tape_id> --at <offset> --before 200
engram view <tape_id> --at <offset> --after 50
```

This allows token-efficient incremental research: start with a default window, expand only where needed.

### `engram tapes`

List recorded tapes. Metadata only (timestamp, model, repo head, label, size, coverage grade).

### `engram show <tape_id>`

Dump a tape's events. Default: compacted view. `--raw` for full event stream.

### `engram gc`

Garbage-collect unreferenced content-addressed blobs. Keeps index entries and lineage links.

### `engram search <query>` (future)

Concept search over tape contents. Requires optional vector index bolt-on.

## Query Algorithm

```
INPUT: file path + line range (current working tree)

1. Extract text from current file at given range
2. Compute span anchors for that text
3. DIRECT: lookup each anchor in evidence index → collect matching sessions
4. LINEAGE: BFS backward through lineage links (depth limit, default 10)
   - Excludes edges with confidence < --min-confidence (default 0.50)
   - Always includes agent_link edges regardless of confidence
   - Respects MAX_FANOUT and MAX_EDGES traversal limits
   - For each ancestor anchor, collect additional sessions
5. ORDER: sort sessions by (touch count DESC, most recent touch DESC)
6. FOR EACH session: extract transcript window around each touch event
7. OUTPUT: ordered list of raw transcript windows, one block per session
```

Output is raw transcript text — the actual messages and tool I/O from each session that touched the span. No scoring. No summarization. No interpretation. The consumer decides what matters.

## Conversation Continuation Detection

### The problem

Agent sessions compact, hit rate limits, or get resumed across multiple transcript files. A single logical task can span many tapes. Without detecting continuations, Engram treats each tape as isolated and may miss reasoning that started in an earlier session.

This is a real problem: an agent might start discussing a design decision in session A, hit a compaction boundary, and implement the decision in session B. Without continuation detection, `engram explain` on the resulting code only finds session B — the implementation. The original reasoning from session A is invisible.

### The solution: fingerprint-based tape alignment

Engram already fingerprints all tape content. The same mechanism detects continuations.

When session B is a continuation of session A, it typically contains carryover content — compaction summaries, repeated assistant text, re-stated context. This carryover content produces fingerprint anchors that match anchors in session A's tail. The overlap is the continuation signal.

This works because assistant messages are highly unique. Unlike user prompts (which may be short or repeated across unrelated sessions), assistant responses contain specific reasoning, specific code references, and specific decisions that are unlikely to appear by coincidence in an unrelated session.

### Detection rules

1. **Fingerprint all tape content** (already done — Engram fingerprints everything).
2. **Compare tail anchors of earlier tapes against head anchors of later tapes** within the same project/repo scope.
3. **Score continuation confidence** by overlap density:
   - High anchor overlap at tail/head boundary → strong continuation signal
   - Single shared phrase → weak/no signal
   - Shared boilerplate (system prompts, AGENTS.md) must be excluded from matching
4. **Store continuation edges** between tapes: `tape_a → tape_b` with confidence score.
5. **`engram explain` traversal follows continuation edges** when walking backward, so the agent can reach reasoning from earlier sessions.

### Boilerplate exclusion

Every harness injects repeated scaffolding (system prompts, AGENTS.md, environment context) into every session. This content produces matching fingerprints across completely unrelated sessions.

Mitigation: maintain a per-harness boilerplate fingerprint set (computed once from known scaffolding content). Exclude boilerplate anchors from continuation matching. Only non-boilerplate overlap counts toward continuation confidence.

### Harness-specific signals (supplemental, not required)

Some harnesses provide explicit continuation markers:
- **Codex CLI**: `type: "compacted"` events with `replacement_history` payload
- **Claude Code**: compaction summaries and rate-limit terminal messages are detectable

When present, these boost continuation confidence. But the fingerprint overlap mechanism works independently — no harness cooperation is required.

### Query behavior across continuations

When `engram explain` walks backward through lineage and reaches the beginning of a tape, it checks for continuation edges to earlier tapes. If found, traversal continues seamlessly into the earlier tape. The agent can use `engram view` to navigate backward across the boundary. Continuation edges are labeled in output so the agent knows it crossed a session boundary.

## Enrichment Model

Engram's core is dumb and deterministic: fingerprint everything, match, return windows. But raw fingerprint matching only tells you "this transcript touched this code." It doesn't tell you "the agent realized its approach was wrong here" or "this is where the key design decision was made."

Enrichment is an optional additive layer that makes output richer without changing core behavior. If no enrichment exists, everything still works. If enrichment exists, explain output gets richer.

### Three enrichment patterns

#### 1. Inline enrichment (harness-provided)

A harness can insert extra anchoring markers or context windows directly into the transcript it emits. Engram fingerprints these like any other content (because it fingerprints everything). When a match hits a recognized enrichment marker, Engram treats it as a navigation hint — not conversation content — and returns the surrounding real transcript instead of the marker itself.

Example: a future OpenAI integration adds an inline anchoring span around every edit with a wider context window. Engram matches it, recognizes the marker format, and uses it to improve anchor precision without returning the synthetic content to the querying agent.

#### 2. Sidecar files (post-hoc or background agent)

Companion files stored alongside tapes in `.engram/sidecars/<tape_id>.sidecar.jsonl`. These contain:

- Annotated code copies with provenance pointers
- Detected conversation start/end boundaries within a tape
- Sub-pointers to notable moments ("agent pivots approach here", "key constraint discovered here")
- Generated interpretation/analysis of conversation segments
- Additional fingerprinted content that, when matched, returns enriched metadata instead of raw transcript

Sidecar files are fingerprinted and indexed like tapes. When an explain query matches a sidecar anchor, the response includes the sidecar's metadata alongside the raw transcript window.

#### 3. Background enrichment agents (async)

Agents that run in parallel with or after ingestion, analyzing tapes and producing sidecar files. These can use LLMs for interpretation — since enrichment is additive and labeled, non-determinism is acceptable here.

Examples:
- **Conversation boundary detector**: identifies where logical conversations start and end within a tape
- **Intent summarizer**: generates one-line summaries of what each conversation segment was trying to accomplish
- **Pivot detector**: identifies moments where the agent changed approach and annotates why

### Enrichment invariants

- Core Engram behavior must not depend on enrichment. Remove all sidecars and enrichment markers → system still works.
- Enrichment content must be labeled as such in query output.
- Non-deterministic enrichment (LLM-generated) must be marked with `source: enrichment` and not treated as ground truth.
- Sidecar files follow the same immutability rule as tapes: write-once, never modified.

## The Saliency Layer (not part of Engram)

Engram's output is raw transcript windows. It does not decide what is relevant to the specific task at hand — that requires knowing what the task is.

The intended workflow above Engram:

```
engram explain src/auth.rs:42-48
  → raw transcript windows from sessions that touched the span

SALIENCY AGENT (separate, not Engram)
  → receives: raw windows + description of the task about to happen
  → reads the transcripts, picks out what matters for this specific task
  → outputs: a compact brief

CODING AGENT
  → receives: only the brief
  → context window preserved
  → enters the refactor already informed
```

The saliency agent is explicitly not part of Engram. This keeps Engram non-prescriptive: it makes no assumptions about what "relevant" means. That judgment belongs to the reading agent, which knows the task.

## Adapter Model

Engram needs to ingest transcripts from the agent harnesses people actually use. This is not a nice-to-have — without working adapters for real harnesses, Engram is a demo, not a product.

### Adapter simplicity

Because Engram fingerprints all content uniformly, the minimum viable adapter is very simple: emit the raw transcript text deterministically. Classification into event kinds (`code.edit`, `msg.out`, etc.) is enrichment that improves quality but is not required for basic provenance to work.

### Compliance levels

**Required (adapter is non-viable without these):**
1. Emit parseable text content deterministically (same input → same output, no LLM interpretation)
2. Identify harness version/schema (or mark unknown)
3. Machine-readable errors on failure

**Enrichment tier (improves quality, not disqualifying if absent):**
1. Event kind classification — enables confidence tiers and structured traversal
2. Structured edit/read spans with file paths, ranges, before/after hashes — enables tombstones and high-confidence anchors
3. Call/result correlation, artifact dereferencing

### Version policy

Each adapter maintains a supported version matrix (min, max-tested, known-bad). Unknown harness versions must either fail (`strict` mode) or ingest a safe subset with explicit degraded coverage (`permissive` mode). No silent degradation.

### Current adapter state

| Harness | Coverage | Notes |
|---------|----------|-------|
| Claude Code | High | Structured read/edit/tool events available in logs |
| Codex CLI | Partial | `apply_patch` edits captured; generic shell reads/edits are gaps |
| OpenCode | Discovery | Adapter scaffolded, needs real-world validation |
| Gemini CLI | Discovery | Adapter scaffolded, needs real-world validation |
| Cursor | Discovery | Adapter scaffolded, needs real-world validation |

See `adapters/ADAPTERS.md` for the full adapter contract and `specs/adapters/*.md` for per-harness details.

## Repository Hygiene

### Folder model

- `.engram/` — **committed** to source control. Contains immutable tapes and optional config.
- `.engram-cache/` — **never committed**. Contains the derived SQLite index, temp files, and rebuild artifacts.

This split means provenance travels with the code (tapes are committed), but the derived index is local and rebuildable.

### Git hook integration

Engram maps 1:1 to Git operations so there's nothing extra to remember:

| Git operation | Engram hook | What it does |
|--------------|-------------|-------------|
| `git commit` | pre-commit | Runs `engram ingest` — captures latest harness logs |
| `git push` | pre-push | Runs `engram ingest` — freshness check |
| `git merge/pull` | post-merge | Rebuilds `.engram-cache/` index from merged tapes |

Hooks are non-destructive — they never overwrite existing repo hooks. Install via `scripts/install-hooks-safe.sh`.

### Branch and merge behavior

On a branch, new tapes accumulate as immutable files. On merge, Git merges tape files like any other files — since tapes are write-once, there are no content conflicts. If the same tape filename exists on both sides, it should be identical (write-once invariant). Optional hash-check emits a warning on mismatch but never blocks.

After merge, run `engram ingest` to rebuild the local index from the combined tape set.

### Fork divergence and convergence

When forks diverge long-term:
- Keep committing tapes in each fork
- When a fork merges upstream, tape files merge as artifacts
- Destination repo rebuilds its index locally
- No SQLite DB merge needed — ever

This stays easy because tapes are immutable and the index is derived. The hard merge work stays in Git where it belongs.

### Idempotency

Running `engram ingest` multiple times with no change in repo state or harness logs produces no new data. Already-seen tapes are deduplicated by file identity. Compute is spent on checking, not on duplicating.

## On-Disk Layout

```
repo/
  .git/
  .engram/
    config.yml             # optional overrides (all have defaults)
    tapes/
      <hash>.jsonl.zst     # compressed trace tapes (immutable, committed)
    sidecars/
      <tape_id>.sidecar.jsonl  # enrichment files (immutable, committed)
  .engram-cache/
    index.sqlite           # evidence index + lineage links (derived, local)
    cursors/               # per-harness ingest state (local)
    tmp/                   # scratch (local)
```

Everything under `.engram/` is the portable source of truth. Copy it and the provenance travels. Everything under `.engram-cache/` is derived and rebuildable.

## What Engram Does NOT Do

- Replace git
- Interpret or classify intent
- Score or weight evidence by meaning
- Perform saliency analysis (that's a downstream agent's job)
- Generate documentation
- Require a server or account
- Mandate a specific agent or IDE
- Decide what's important in a transcript
- Promise deterministic replay
- Require harness cooperation for basic functionality

## Implementation Language

**Rust.** Single static binary. No runtime. Compiler catches agent mistakes. Fast enough for tight loops (anchor computation, index scans, compression). Uses existing winnowing crate for fingerprinting — no need to reimplement well-established algorithms.

## Crate Structure

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
    adapters/
      mod.rs             # adapter trait + registry
      claude_code.rs     # Claude Code adapter
      codex_cli.rs       # Codex CLI adapter
      opencode.rs        # OpenCode adapter
      gemini_cli.rs      # Gemini CLI adapter
      cursor.rs          # Cursor adapter
```

## Open Questions

- **Anchor algorithm calibration**: k-gram size, window size, hash function. Needs benchmarking against real codebases. The thresholds (0.30, 0.50, 0.90) may need tuning, but the architecture is correct regardless of specific numbers.
- **Multi-language tokenization**: token k-grams need to handle diverse syntax. Start language-agnostic (whitespace + punctuation split)?
- **Vector search bolt-on**: when to add, what embedding model, local-only constraint.
- **Secrets redaction**: what should `.engram/` exclude? Redaction strategy for sensitive content in tapes.
- **Incremental ingest cursors**: per-harness state tracking for efficient re-scan (partially designed, not fully implemented).

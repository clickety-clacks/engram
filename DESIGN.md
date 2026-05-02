# Engram Design Document

## Invariants

These are hard constraints. Everything else in this document follows from them.

1. **Tapes are just files.** No central authority, no accounts, no servers. A tape is a zstd-compressed JSONL file. Point Engram at folders of tapes, it indexes them.
2. **Fingerprints are just hashes.** A fingerprint match is a fingerprint match. Engram doesn't know or care what a "project" is — cross-project connections surface automatically because the text overlaps. No one has to wire them up.
3. **Tapes are immutable.** Once written, never modified or deleted. This enables safe merging, deduplication, and trust in provenance chains.
4. **The index is derived.** Delete it, rebuild it from tapes. Change calibration parameters, rebuild it. The index is a cache, not a source of truth.
5. **Provenance is additive.** Start with zero. Add repo tapes. Add orchestrator tapes. Add cross-project tapes. Each layer enriches. None is required. Some > none, always.
6. **Scope is user-defined.** You control what Engram can see by where you run ingest and fingerprint. If you haven't processed it, Engram can't see it. Trade recall for privacy. Your choice, not Engram's.

## The Problem

Agents write code. They reason about it, discover constraints, make trade-offs, and produce working implementations. Then the session ends. The reasoning vanishes. The code remains, but the *why* behind every line is gone.

Git tells you *what changed* and *when*. Engram tells you *why this code exists*.

## What Engram Is

Engram is a system-wide provenance index for agent-driven development. It answers a single question: **"Given this span of code, what was the reasoning that produced it?"**

It does this by capturing the full execution trail agents already emit — prompts, tool calls, reads, edits, results — then making that trail queryable at the level of individual code spans.

Engram is not scoped to a single repository. It is not a git plugin. It is a provenance system that indexes tapes from anywhere — a single repo, a dozen repos, an orchestrator's decision log, a design conversation. Fingerprints don't care where the text came from. If an orchestrator discussed the same function that a coding agent later edited, those fingerprints match, and the causal link surfaces automatically.

Engram integrates well with git (tapes can travel with repos), but git is one deployment pattern, not the assumed model.

## Value Proposition

Modern agents are strong at local reasoning but weak at longitudinal memory. An agent refactoring code today has no access to the conversation that produced that code last week — even if both ran in the same repo.

In compound agent systems, the problem is worse. An orchestrator makes a decision. It dispatches a coding agent. The coding agent produces code. Later, a different agent touches that code with zero visibility into the orchestrator's reasoning — or even the original coding agent's session.

Engram fixes this by turning prior work into retrievable context:

- Preserve full causal history, not just commit diffs
- Retrieve the exact evidence behind any span before refactoring
- Warm future agent context with real prior decisions, constraints, and tradeoffs
- Link orchestrator-tier reasoning to coding-agent-tier implementation automatically
- Reduce repeated mistakes caused by missing historical intent

The intended consumer is an agent, not a human dashboard. Output is machine-readable by default.

## Core Thesis

Every code span has an epistemic history. There was a conversation — a chain of prompts, reasoning, tool invocations, discoveries, and decisions — that caused those exact bytes to exist at that location. That conversation may have happened in one session or across many. It may span a single coding agent or an orchestrator directing multiple agents. Engram preserves that chain and makes it retrievable.

## Design Principles

### P1. Single static binary
One binary. No runtime. No daemon. No external dependencies at install time. `curl | sh` or `brew install engram` and you're running. SQLite is bundled (`rusqlite --features bundled`).

### P2. Non-prescriptive storage
Store raw facts, not interpretations. No "intent" labels. No "decision" categories. No taxonomies. Downstream consumers decide what matters. Engram is an index, not an analyst.

### P3. Agents are the primary consumer
Output is structured for machine consumption by default. Human-readable output is available via `--pretty`, but the default format is optimized for agent context windows.

### P4. Compression-first
Tapes are zstd-compressed. Large tool outputs are content-addressed and deduplicated. Storage should remain manageable even over thousands of sessions across many projects.

### P5. Append-only tapes
Trace tapes are immutable once written. They can be created and read, never modified or deleted. This is a hard invariant — it enables safe merging, deduplication, and trust in provenance chains.

<!-- CHANGED: P6 keeps zero-config auto-create while allowing explicit local init. -->
### P6. Zero-config start
On first invocation, Engram auto-creates `~/.engram/` with a default config and DB.
No setup step is required. `engram init` is optional and creates a local
workspace store (`./.engram/config.yml` with `db: .engram/index.sqlite`) when a
user explicitly wants self-contained local provenance.

### P7. Local-first, offline-only
No network required. No accounts. No servers. Everything runs locally. This is a tool, not a service.

### P8. Deterministic core
Given the same inputs (tapes + code), Engram produces the same outputs. No randomness, no LLM interpretation in the core pipeline. Non-deterministic enrichment is allowed but must be labeled and is never required.

### P9. Ubiquitous harness integration
Engram must work with the dominant agent harnesses (at minimum: Codex CLI and Claude Code) and with orchestrator-tier systems (OpenClaw, custom dispatchers). If Engram cannot capture provenance from the tools people actually use, the product is incomplete. Integration is not a nice-to-have — it is the product.

### P10. No project taxonomy
Engram doesn't know what a "project" is. There are tapes. There are fingerprints. A match is a match. Cross-project connections surface automatically because the text overlaps. Users control boundaries through source configuration, not through Engram's internal model.

<!-- CHANGED: New principle P11. -->
### P11. Config conspicuity
Every command prints its resolved config path and DB path as the first lines of output. The user always knows which config was resolved and which DB is being read from or written to. No silent defaults.

## How It Works: A Story

An agent is about to refactor a function. Before touching it, it asks Engram: "Why is this code here?"

1. **Fingerprint the span.** Engram reads the selected code and chops it into small overlapping word patterns (k-grams). Each pattern gets hashed. The result is a bag of hashes — a unique fingerprint of that exact code.

2. **Check the index.** Engram looks up those hashes in its index. The index was built from tapes — potentially from this repo, from other repos, from orchestrator sessions, from wherever fingerprints have been contributed. Every time an agent read, edited, or even discussed code, Engram chopped that content into the same kind of word patterns and stored them alongside a pointer to that moment in the transcript.

3. **Find matches.** Results come back: "Tape 7, event 42 has 92% of the same word patterns." That's high confidence — almost certainly the same code. "Tape 3, event 18 has 60% match." That's a partial match — maybe an earlier version before some edits. Tape 7 might be from the coding agent that wrote the function. Tape 3 might be from an orchestrator session where the architect discussed the design. Both are just tapes.

4. **Return transcript windows.** For each match, Engram returns a window of transcript around the anchor point. The agent reads: "In tape 7, the previous agent was fixing an auth timeout bug and changed this function because tokens were expiring too fast." And from tape 3: "The orchestrator decided to consolidate token validation into a single module after Flynn flagged the duplication."

5. **Navigate if needed.** If the window isn't enough context, the agent can expand it — walking backward or forward through the transcript, or following continuation links to earlier sessions.

Now the agent knows *why this code exists* before changing it. It won't accidentally undo the timeout fix, and it understands the architectural intent behind the consolidation.

<!-- CHANGED: New section. Replaces the old "Source resolution" subsection in On-Disk Layout
     and the old "Per-repo vs global config" subsection in Scope and Privacy. -->
## Config System

### Walk-Up Resolution

When any Engram command runs, it resolves config by walking up from the current
working directory:

1. Check for `.engram/config.yml` in the current directory
2. Walk parent directories, checking each for `.engram/config.yml`
3. Continue to `~` and include `~/.engram/config.yml` if present
4. Fall back to `~/.engram/config.yml` (system config) if no config file exists in the chain

**Walk-up is a cascading merge.** Engram collects all configs in that chain and
merges keys nearest-to-farthest:
- Nearest config wins for any key it explicitly sets
- Missing keys inherit from the next config up the chain
- Hardcoded defaults apply only when no config in the chain sets a value

**Outside-home edge case:** if the current working directory is outside `~`,
walk-up is skipped entirely and Engram uses `~/.engram/config.yml` directly.

### Auto-Creation

On first invocation, if `~/.engram/config.yml` does not exist, Engram
auto-creates it with explicit defaults:

```yaml
db: ~/.engram/index.sqlite
tapes_dir: .engram/tapes
```

The explicit `tapes_dir` keeps walk-up behavior deterministic and consistent
with `db:` resolution.

### DB Override

The config file specifies where fingerprints are stored via the `db:` key:

```yaml
db: ~/.engram/index.sqlite
```

Any config at any level can override `db:` to point elsewhere. This is the
mechanism for isolation: a repo or shared folder that wants its own segregated
fingerprint DB declares `db:` in its local `.engram/config.yml`.

If no config in the walk-up chain specifies `db:`, the hardcoded default
`~/.engram/index.sqlite` is used.

The "single system DB" experience is not a hard architectural constraint —
it is the emergent result of most directories not overriding `db:`. Everything
falls through to the global default.

### Additional Stores

For explain queries that should search beyond the resolved `db:`, a config
can declare additional stores:

```yaml
additional_stores:
  - /nfs/team/engram/index.sqlite
  - /mnt/shared/engram/index.sqlite
```

`engram explain` queries the primary `db:` plus all `additional_stores:`,
merges and deduplicates results. Ingest and fingerprint commands write only
to the primary `db:`.

### Tapes Output Directory (`tapes_dir`)

Config can optionally override this with:

```yaml
tapes_dir: /path/to/tapes
```

`tapes_dir` follows the same path resolution rules as `db:`:
- `~/...` expands from home
- relative paths resolve from the config base directory

`tapes_dir` also participates in cascading walk-up merge exactly like `db:`:
- nearest config wins if it sets `tapes_dir`
- if omitted, value inherits from parent configs up the chain
- hardcoded fallback applies only if no config in chain sets `tapes_dir`

Auto-generated configs (`engram init` local config and first-run
`~/.engram/config.yml`) always include explicit `tapes_dir: .engram/tapes`.
So inheritance is mainly for manually-authored configs that omit the key.

Motivating use case: source transcripts live on NFS, but compiled tapes should
be written to local disk (for example, eezo-local SSD) to avoid NFS write
amplification.

### Example Configs

**System config (`~/.engram/config.yml`) — typical developer machine:**

```yaml
db: ~/.engram/index.sqlite
tapes_dir: .engram/tapes
```

**Repo override (`.engram/config.yml` in a client project) — isolated DB:**

```yaml
db: .engram/index.sqlite
```

**Team config (`.engram/config.yml` on a shared mount) — team DB:**

```yaml
db: /nfs/team/engram/index.sqlite
```

**Developer who wants to also search a team DB:**

```yaml
db: ~/.engram/index.sqlite
additional_stores:
  - /nfs/team/engram/index.sqlite
```

**Developer ingesting NFS transcripts but writing tapes locally:**

```yaml
db: ~/.engram/index.sqlite
tapes_dir: ~/.engram/local-tapes
```

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

A tape is an append-only, immutable JSONL stream of events captured during a single agent session. When the session ends, the tape is zstd-compressed and stored content-addressed. Where it's stored depends on context — in a repo's `.engram/tapes/`, in `~/.engram/tapes/`, or wherever the adapter writes it. A tape is just a file.

Tapes are the source of truth. The index is a derived artifact that can be rebuilt from tapes at any time. This means you can change calibration parameters (k-gram size, window size, thresholds) and rebuild the index without losing any history.

### Why tapes matter

Agent sessions are ephemeral. After a Claude Code or Codex run ends, its internal conversation — prompts, tool calls, edits — is gone with no persistent record (beyond what the harness chooses to log). Tapes capture this before it vanishes.

In compound agent systems, the problem multiplies. An orchestrator session might dispatch five coding agents across three repos. Each agent's session is ephemeral. The orchestrator's session — which contains the decisions that motivated all five — is also ephemeral. Tapes from all tiers preserve the complete causal chain.

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
| `meta` | Session metadata: model, tier, harness identity, optional repo state | `model`, `repo_head`, `label`, `source`, `tier` |
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
- Sharing provenance by copying files — no protocol needed

## Multi-Tier Provenance

### The problem with single-tier capture

In a single-agent setup (a human running Claude Code directly), one transcript has everything: the user's intent, the agent's reasoning, the code it wrote. Engram indexes the tape and provenance is complete.

In compound agent systems, the causal chain splits across tiers:

- **Orchestrator tier** — contains the WHY: high-level decisions, reasoning about architecture, the user's intent, dispatch logic ("send a coding agent to fix auth in repo X")
- **Coding agent tier** — contains the WHAT: file reads, edits, tool calls, implementation details

An orchestrator decides to consolidate token validation. It dispatches a coding agent. The coding agent refactors the code. Later, someone asks Engram: "Why was this code restructured?" If Engram only has the coding agent tape, it can answer "the agent was told to consolidate token validation and here's how it did it." But the *reason* for the consolidation — the orchestrator's analysis, the user's request, the architectural discussion — lives in a different tape.

### How Engram handles it

Both tiers produce tapes. Engram indexes both. Fingerprints link them automatically.

This works because of text overlap. When an orchestrator dispatches a task, it typically includes context: quoted code, file paths, descriptions of what to change and why. The coding agent's session contains the same code (it reads and edits it) and often echoes the dispatched context. These overlapping text fragments produce matching fingerprints.

No special cross-tier linking is needed. If the orchestrator discussed the same function that the agent later edited, the fingerprints match and `engram explain` returns windows from both tapes — the orchestrator's reasoning alongside the agent's implementation.

In practice:
- Single-agent setup → one tape has everything → works
- Orchestrator + coding agent → two tapes, fingerprints link them → works
- Orchestrator + multiple coding agents across repos → many tapes, fingerprints link what overlaps → works
- Engram doesn't care about the topology

### Dispatch-marker linking (explicit handoff path)

Fingerprint overlap is the default linking mechanism, but some handoffs need an explicit causal marker. Engram supports this with inline dispatch tags:

```text
<engram-src id="f47ac10b-58cc-4372-a567-0e02b2c3d479"/>
```

Plain-language model:
- A sender session emits a UUID when it dispatches work.
- A receiver session includes that UUID in incoming message content.
- Engram ingest records where each UUID was first `received` and where it was later `sent`.
- During `explain`, Engram walks upstream via those markers automatically as part of normal link traversal to recover parent sessions that led to the edit.

Concrete flow:
1. Session A dispatches work and includes `<engram-src id="..."/>`.
2. Session B receives the marker, performs code edits, and optionally forwards the same marker downstream.
3. `engram explain <file>:<start>-<end>` finds Session B via fingerprint, then follows the dispatch marker link upstream to Session A automatically.

<!-- CHANGED: Removed "engram explain --dispatch <uuid>" mode.
     Dispatch markers are followed as part of normal explain link traversal.
     If someone needs to look up a UUID directly, that is a search/lookup
     operation, not an explain operation. -->

This contract is harness-agnostic. Engram only requires the marker text in transcript content; it does not require a specific vendor protocol.

See `specs/core/dispatch-marker.md` for the full dispatch marker specification,
including direction detection via structural nesting depth and causal preceding
UUID traversal rules.

### Tier metadata

Tapes can carry an optional `tier` field in their `meta` event (`orchestrator`, `agent`, or omitted). This is informational — it helps consumers understand the provenance source but does not affect indexing or matching. A tape without a tier field is indexed identically.

## Evidence Index

The evidence index is the reverse lookup — the heart of the query system:

```
anchor_hash → [(tape_id, event_offset, kind, file_path, timestamp), ...]
```

Many-to-many. A span returns many sessions; a session touches many anchors.

Stored in SQLite (single file, no daemon, fast lookups). The index is a derived artifact, never committed to source control, and can be rebuilt from tapes at any time.

The index is built from whatever tapes have been fingerprinted into the resolved DB. It may span a single repo, many repos, orchestrator logs, or any combination. The index doesn't know or care about source boundaries — it's all just anchor hashes pointing at tape events.

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

Create a local Engram workspace rooted at the current directory:
- Creates `./.engram/` directory structure
- Creates `./.engram/config.yml` with:
  - `db: .engram/index.sqlite`
  - `tapes_dir: .engram/tapes`
- Prints conspicuity lines (`config`, `db`) like all commands
- Idempotent: if `./.engram/config.yml` already exists, reports and exits cleanly

This command is optional. The system store at `~/.engram/` is still
auto-created on first command invocation for zero-config startup.

<!-- CHANGED: engram ingest rewritten. Now local-scoped, operates on the folder
     you're in and its subfolders. No global sweep. Creates tapes + fingerprints. -->
### `engram ingest`

Import raw harness transcripts from the current directory and its subdirectories
into tapes and fingerprint them into the resolved DB.

This command is **local-scoped** — it operates on the folder you're standing in,
not on a global source list. Discovery now has two layers:
1. Adapter repo-scoped discovery: each harness adapter can map the ingest
   repo path to that harness's native storage location (for example,
   ~/.claude/projects/<encoded-repo>/...) and return matching transcript files
   for that repo.
2. Local tree fallback: Engram still scans the current directory tree for
   json/jsonl files (excluding .engram) so local transcript drops continue to
   work.

Candidates from both layers are de-duplicated, then parsed via the appropriate
adapter, normalized into tapes, and fingerprinted into the DB resolved via
config walk-up.

```bash
cd ~/src/clawline
engram ingest
# config: ~/.engram/config.yml (walk-up)
# db: ~/.engram/index.sqlite
# ingested: 47 tapes, 3201 fingerprints
```

Ingest is incremental — it tracks what has already been imported and only
processes new content. Running it multiple times with no new harness activity
is effectively a no-op (idempotent given unchanged inputs).

The ingest cadence is local hygiene — each repo or folder decides when to run
it. Typical triggers:
- Git hooks (`post-checkout`, `post-merge`)
- Cron (for shared/mounted transcript folders)
- Manual after a work session

<!-- CHANGED: New command. Indexes existing tapes only, no transcript parsing. -->
### `engram fingerprint`

Index existing tapes in the current directory's `.engram/tapes/` into the
resolved DB. No transcript parsing, no tape creation — just fingerprinting.

This is the command for consuming tapes that arrived from elsewhere:
- Committed `.engram/tapes/` from a cloned repo
- Tapes on an NFS mount from another machine
- Tapes someone sent you

```bash
cd ~/src/colleague-project
engram fingerprint
# config: ~/.engram/config.yml (walk-up)
# db: ~/.engram/index.sqlite
# fingerprinted: 23 tapes
```

Idempotent — skips tapes already fingerprinted in the resolved DB.

To fingerprint tapes in the system store (e.g., tapes dropped into
`~/.engram/tapes/`), run `engram fingerprint` from `~/.engram/`.

### `engram record <command>`

Run a command and capture a live tape. Captures stdin/stdout/stderr, file diffs, tool invocations. Writes tape to the appropriate tape directory. Alternatively: `engram record --stdin` to pipe in a pre-existing session transcript.

<!-- CHANGED: engram explain rewritten. Global by nature (queries resolved DB which
     accumulates all contributions). Removed --dispatch mode. Added config conspicuity.
     Added additional_stores fan-out. -->
### `engram explain <file>:<start>-<end>`

The killer query. Given a span, return ranked evidence trails with transcript context.

**Explain is global by nature.** It queries the resolved DB — which contains
the accumulated fingerprints from every `ingest` and `fingerprint` command
ever run against that DB, from any folder, any repo, any machine that
contributed. The user does not need to know or remember what was configured
or where transcripts came from. The DB is the sum of all local contributions.

If `additional_stores:` is configured, explain also queries those DBs and
merges results.

This is the reasoning behind global explain: on any system, you don't
necessarily know or remember all the transcript locations that were configured
— possibly by an admin, possibly months ago, possibly via automated hooks.
Explain searches everything so you don't have to reconstruct the provenance
surface manually.

Algorithm:

1. Print resolved config path and DB path
2. Compute anchors for selected text
3. DIRECT: lookup each anchor in evidence index → collect matching sessions
4. LINEAGE: BFS backward through lineage links (depth limit, default 10)
   - Excludes edges with confidence < --min-confidence (default 0.50)
   - Always includes agent_link edges regardless of confidence
   - Respects MAX_FANOUT and MAX_EDGES traversal limits
   - Follows dispatch marker links upstream automatically
   - For each ancestor anchor, collect additional sessions
5. ORDER: sort sessions by (touch count DESC, most recent touch DESC)
6. FOR EACH session: extract transcript window around each touch event
7. OUTPUT: ordered list of raw transcript windows, one block per session

Output is raw transcript text — the actual messages and tool I/O from each session that touched the span. No scoring. No summarization. No interpretation. The consumer decides what matters.

Results may include windows from different tiers (orchestrator sessions alongside coding agent sessions) and from different repos. The output identifies each window's source tape, so consumers can distinguish tiers if they choose to.

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

Defaults for `--before` and `--after` are set in config under `explain.window.before` and `explain.window.after`. Agents can override per-call based on their judgment of how much context they need on the first read.

#### Navigation from anchors

The first `explain` window may not contain the full reasoning — especially for decisions that built up over many messages. Rather than guessing a large window upfront (expensive), agents start small and walk backward/forward through the transcript incrementally, pulling only what they need.

```
engram view <tape_id> --at <offset> --before 200
engram view <tape_id> --at <offset> --after 50
```

This allows token-efficient incremental research: start with a default window, expand only where needed.

### `engram tapes`

List recorded tapes. Metadata only (timestamp, model, source path, label, size, coverage grade).

### `engram show <tape_id>`

Dump a tape's events. Default: compacted view. `--raw` for full event stream.

### `engram gc`

Garbage-collect unreferenced content-addressed blobs. Keeps index entries and lineage links.

### `engram search <query>` (future)

Concept search over tape contents. Requires optional vector index bolt-on.

## Agent Tool Surface

### Design principle

Provenance has a topology. Engram's job is to provide tools that let agents navigate that topology. The agent is the intelligence — Engram is navigational infrastructure.

Engram does not interpret, summarize, or understand provenance. It makes the provenance corpus efficiently traversable. The agent does all the thinking — discovering causality, extracting semantics, judging relevance.

### Invariants

1. **Token efficiency**: results include content by default (small default window), but the agent controls sizing to avoid reading more than it needs.

2. **Token exhaustion protection**: if a query would produce a result exceeding a safe threshold, engram truncates the result and tells the agent to constrain its query further.

3. **Useful metadata on large corpora**: when a result is truncated, engram returns actionable metadata about what it didn't return. The agent should never be left guessing about the shape of what it hasn't seen.

### The provenance topology

Provenance is organized as chains: the root is WHY (product decisions, design rationale), descendants are HOW (specs, implementation). The chain is a gradient of abstraction — not fixed layers.

Two dimensions of navigation:
- **Reference depth**: traversal along dispatch-link chains from abstract (root) toward concrete (implementation).
- **Window positioning**: at each node, the agent controls what slice of the session it reads, using absolute line positioning.

#### Edge visibility

Explain returns the full chain structure in metadata — root with all descendant IDs, depths, and dispatch links. The agent has the complete map. Peek reads content at any point on the map without returning chain metadata — it's a content reader, not a query.

#### Filtering

`--grep-filter` winnows which results are returned before the agent reads content. It uses grep pattern syntax and operates on window content only.

### API surface

Three commands. Explain and grep are the query layer (find provenance). Peek is the navigation layer (read content).

#### `engram explain`

Query provenance by code fingerprint.

```
engram explain <file>:<start>-<end>   Find provenance for a code span
engram explain <file>                 Find provenance for an entire file
engram explain "<string>"             Find provenance for arbitrary text
```

| Flag | Default | Description |
|------|---------|-------------|
| `--grep-filter <pattern>` | — | Only include results mentioning this term (grep syntax) |
| `--limit N` | 10 | Max sessions returned |
| `--offset N` | 0 | Skip first N results (pagination) |
| `--min-confidence N` | — | Only results above this match quality (0.0-1.0) |
| `--since <date>` | — | Only sessions after this date |
| `--until <date>` | — | Only sessions before this date |
| `--count` | off | Show result count and metadata only, no content (dry run) |

Default behavior:
- Finds matching sessions via winnow k-gram fingerprinting
- Follows dispatch links upstream to chain roots
- Returns root-first (most abstract → most concrete)
- Each result includes a default content window
- Results include full chain structure in metadata (all descendant IDs, depths)

Per-result metadata:

| Field | Description |
|-------|-------------|
| `session_id` | Stable identifier for this session |
| `confidence` | How strongly this session's fingerprints match the query (0.0-1.0) |
| `timestamp` | When this session occurred |
| `window_start` | First line number of returned content |
| `window_end` | Last line number of returned content |
| `total_lines` | Total lines in session |
| `depth` | Position in chain (0 = root) |
| `parent` | Parent session ID (null at root) |
| `children` | Child session IDs |
| `chain_length` | Total sessions in this chain |
| `files_touched` | Files this session modified |

Truncation header (when result exceeds safe threshold):

| Field | Description |
|-------|-------------|
| `returned` | Number of sessions returned |
| `total` | Total sessions matching |
| `time_range` | Earliest and latest session timestamps |
| `truncated` | true |

#### `engram grep <pattern>`

Query provenance by literal text search across all tapes.

| Flag | Default | Description |
|------|---------|-------------|
| `--limit N` | 10 | Max sessions returned |
| `--offset N` | 0 | Skip first N results |
| `--since <date>` | — | Only sessions after this date |
| `--until <date>` | — | Only sessions before this date |
| `--count` | off | Show result count only, no content |

Same output shape as explain.

#### `engram peek <session_id>`

Read content from a session. Explain gives you the map; peek gives you the content at any point on the map.

| Flag | Default | Description |
|------|---------|-------------|
| `--start N` | — | Absolute line position in the session |
| `--lines N` | config | Number of lines to return from start |
| `--before N` | config | Lines before the anchor point |
| `--after N` | config | Lines after the anchor point |
| `--grep-filter <pattern>` | — | Search within this session; returns matching lines with context |

Default behavior (no flags): returns configured default window anchored at the dispatch link point (if this session was reached via a chain) or head-of-session (if standalone).

Config:

```yaml
explain:
  default_limit: 10
peek:
  default_lines: 40
  default_before: 30
  default_after: 10
  grep_context: 5
```

Error responses:

```json
{"error": "session_not_found", "session_id": "..."}
{"error": "no_results", "query": "..."}
{"error": "invalid_span", "detail": "..."}
```

### Help text

#### `engram --help`

```
Engram indexes agent conversations that produced your code.

Results are organized as provenance chains: the root is WHY
(product decisions, design rationale), descendants are HOW
(specs, implementation). Use explain to find chains, peek to
read them.

COMMANDS:
  explain    Find provenance for code (by fingerprint)
  grep       Find provenance for a term (by text search)
  peek       Read content from a provenance session
  ingest     Import transcripts into the index
  watch      Continuously watch for new transcripts

Run engram <command> --help for details.
```

#### `engram explain --help`

```
Find the conversations that produced this code.

Returns the root of each provenance chain — the highest-level
context explaining WHY this code exists. Results include chain
metadata (children, depth) so you can walk down to HOW with peek.

USAGE:
  engram explain <file>:<start>-<end>   Provenance for a code span
  engram explain <file>                 Provenance for an entire file  
  engram explain "<string>"             Provenance for arbitrary text

OPTIONS:
  --grep-filter <pattern>   Only results whose content matches (grep syntax)
  --limit N                 Max results [default: 10]
  --offset N                Skip first N results (pagination)
  --min-confidence N        Only results above this match quality (0.0-1.0)
  --since <date>            Only sessions after this date
  --until <date>            Only sessions before this date
  --count                   Show counts only, no content (token budgeting)

EXAMPLES:
  engram explain src/server.ts:40-78
  engram explain src/server.ts:40-78 --grep-filter "retry"
  engram explain src/server.ts --since 2026-03-01 --limit 5
```

#### `engram grep --help`

```
Search all provenance sessions for a term.

Unlike explain (which matches by code fingerprint), grep searches
for literal text across all indexed conversations.

USAGE:
  engram grep <pattern>

OPTIONS:
  --limit N       Max results [default: 10]
  --offset N      Skip first N results
  --since <date>  Only sessions after this date
  --until <date>  Only sessions before this date
  --count         Show counts only, no content

EXAMPLES:
  engram grep "maxMessageBytes"
  engram grep "retry logic" --since 2026-03-01
```

#### `engram peek --help`

```
Read content from a provenance session.

Use explain or grep to find sessions, then peek to read them.
By default returns a window around the anchor point (where the
session connects to its parent chain). Use --start/--lines for
absolute positioning.

USAGE:
  engram peek <session_id>

OPTIONS:
  --start N                 Read from this line number
  --lines N                 Number of lines to return [default: 40]
  --before N                Lines before the anchor point [default: 30]
  --after N                 Lines after the anchor point [default: 10]
  --grep-filter <pattern>   Find lines matching this term within the session

EXAMPLES:
  engram peek af156abd
  engram peek af156abd --start 421 --lines 30
  engram peek af156abd --grep-filter "NO_REPLY"
```

### Open questions

1. **Output format**: JSON only (current). Human-readable format deferred unless needed.
2. **Whole-file explain**: may produce broad/noisy results. Help text should note preference for spans.


## Continuation Detection: Open Question

### What we investigated

We actively explored cross-compaction continuation detection — the idea that Engram should recognize when two tapes are segments of the same logical conversation split by a compaction boundary, rate limit, or session restart.

Three approaches were considered:

1. **Fingerprint overlap at tape boundaries.** Compare tail anchors of tape A against head anchors of tape B. High overlap at the boundary suggests continuation — compaction summaries, repeated assistant text, and re-stated context produce matching fingerprints.

2. **Timestamp proximity.** Tapes that end and begin within a narrow time window are continuation candidates. Useful for narrowing the search space but not sufficient alone.

3. **Harness-specific markers.** Some harnesses emit explicit continuation signals — Codex CLI has `type: "compacted"` events with `replacement_history` payload; Claude Code emits compaction summaries and rate-limit terminal messages. When present, these are strong signals.

### What we found

Fingerprint overlap alone is unreliable. Every harness injects repeated scaffolding — system prompts, AGENTS.md, environment context — into every session. This boilerplate produces matching fingerprints across completely unrelated sessions. Filtering boilerplate requires maintaining per-harness fingerprint exclusion sets, which is fragile and harness-version-dependent.

Harness markers work when available but cannot be depended on. Not all harnesses emit them, formats change across versions, and the markers are an implementation detail of each harness — not a stable contract.

Timestamps help narrow candidates but don't confirm anything. Two tapes close in time might be continuations or might be two unrelated agents working concurrently.

### Why continuation detection may not be necessary

The deeper question: does Engram actually need to know that two tapes are "the same conversation"?

Consider what the core query mechanism already does. When an agent asks `engram explain` for a code span, Engram finds all tapes whose fingerprints match that span — regardless of whether those tapes are related to each other. If tape A discussed the code and tape B edited it, both surface. The agent gets context from both. Whether A and B are segments of the same conversation or two completely unrelated agents is irrelevant to the provenance — the evidence is real either way.

The scenarios:
- **Two tapes from the same conversation** — both touched the same code, both surface via fingerprint match. No continuation detection needed.
- **Two tapes from unrelated agents** — both touched the same code, both surface via fingerprint match. Same result.
- **Orchestrator tape + coding agent tape** — orchestrator discussed the code, agent edited it. Both surface via fingerprint overlap on the shared text. No continuation detection needed.

Chronological narrative construction — "first the agent thought X, then it realized Y, then it changed Z" — is a saliency layer concern. Engram returns raw evidence windows. The consuming agent or saliency layer can sort by timestamp and reconstruct narrative if it wants to. Engram doesn't need to pre-compute the narrative structure.

### The one case that might matter

There is one scenario where continuation detection could add value: an agent discusses a design decision in the first half of a session, the session compacts, and then in the resumed session the agent modifies code based on that earlier reasoning — but the reasoning happened before the agent ever read or touched the specific code file.

In this case, the pre-compaction tape contains the reasoning but has no fingerprint anchor to the code (the agent hadn't interacted with the code yet). The post-compaction tape has the code anchors but only the implementation, not the original reasoning. Without a continuation link, `engram explain` on the code only finds the post-compaction tape.

But how likely is this in practice? The agent almost certainly read the code before modifying it. That read event creates fingerprint anchors in the tape. If the read happened before compaction, those anchors already link the pre-compaction tape to the code. If the read happened after compaction, the post-compaction tape has the anchors. The gap only exists if the agent reasoned about code it had never read or quoted — which is unusual for coding agents that work by reading files, reasoning, then editing.

### Facts vs analysis

There is an important philosophical line here that clarifies what Engram should and should not do.

Fingerprint-based continuation analysis — comparing tape boundaries, scoring overlap density, inferring that two tapes are segments of the same conversation — is **analysis**. It is interpretation of content similarity. Engram is an index, not an analyst. This kind of work belongs in the saliency or enrichment layer, not in core Engram. It violates the non-prescriptive storage principle (P2).

However, if a harness provides a deterministic continuation marker — like a Codex `compacted` event that explicitly says "this is a continuation of session X" — then ingesting that marker **is** Engram's purview. It's storing a raw fact, not performing analysis. The harness told us. We record what we were told. This is the same pattern as `span.link`: if the agent declares lineage, we store it. We don't infer it.

The line: Engram stores facts. Inferring continuations from content overlap is interpretation. Storing explicit continuation markers from harnesses is fact storage.

If a harness emits a continuation marker, the adapter should emit a tape event for it and the indexer should store a continuation edge. If no harness emits one, Engram does not guess. An enrichment agent or saliency layer can perform the inference if it wants to — and store results in a sidecar, clearly labeled as derived.

### Current position

We are not implementing continuation inference in core Engram. If harnesses provide explicit continuation markers, adapters will ingest them as raw facts. The core fingerprint mechanism — which already surfaces all tapes that touched a given span, regardless of their relationship to each other — may solve the remaining problem well enough.

We will revisit if real-world usage reveals gaps where important context is unreachable through normal fingerprint matching.

## Scope and Privacy

### The tradeoff

Wider scope means better recall. If Engram can see tapes from all your repos and your orchestrator, it can surface connections that a single-repo index would miss — an architectural decision in one project that motivated a pattern adopted in another.

But wider scope also means more exposure. Orchestrator tapes may contain private discussions. Cross-project tapes may reveal internal decisions about other work.

Engram's answer: **you draw the line, not us.** What gets fingerprinted into the DB depends on where you run `ingest` and `fingerprint`. Anything you haven't explicitly ingested is invisible to Engram. There is no ambient discovery, no automatic expansion, no "helpful" scanning of your filesystem.

<!-- CHANGED: "Per-repo vs global config" subsection replaced with walk-up reference. -->
### Config and scoping

Config walk-up resolution (see Config System section) determines which DB
receives fingerprints and which DB is queried. By default, everything goes
to `~/.engram/index.sqlite`. A repo or folder that needs isolation overrides
`db:` in its local `.engram/config.yml`.

Privacy boundaries are controlled by two mechanisms:
- **Where you run ingest/fingerprint** — only folders you explicitly process
  contribute to the DB
- **DB isolation via config override** — sensitive work can use a separate DB
  that is never queried by default from other directories

### Privacy guarantees

Engram never phones home. It never reads paths you haven't explicitly processed. It never indexes tapes it wasn't pointed at. The index is local. Tapes don't leave your machine unless you explicitly copy them.

If you share a repo that contains `.engram/tapes/`, recipients get those tapes — that's intentional (provenance travels with code). If you don't want that, exclude `.engram/` from distribution or move tapes to `~/.engram/` (home-only, never committed).

## Sharing Provenance

### The model

Provenance sharing is file copying. No protocol. No sync service. No accounts.

Want to share provenance with a collaborator? Zip the tapes. Send them. The recipient drops them in a directory, runs `engram fingerprint` in that directory. Done. The fingerprints enter their resolved DB.

Want to share provenance with a repo? Commit tapes in `.engram/tapes/`. Anyone who clones the repo gets the provenance. They run `engram fingerprint` in the repo and the tapes are indexed.

### Additive layering

Provenance layers stack:

1. **Repo tapes** — baseline. What happened in this repo. Travels with `git clone`. Indexed via `engram fingerprint`.
2. **Cross-project tapes** — enrichment. What happened in related repos. Indexed by running `ingest` or `fingerprint` in those repos.
3. **Orchestrator tapes** — enrichment. Why things happened. Indexed by running `ingest` in the orchestrator's transcript folder.
4. **Shared tapes** — enrichment. What collaborators did. Dropped in a folder, indexed via `engram fingerprint`.

Each layer is optional. Each adds recall. The order doesn't matter — fingerprints match regardless of when tapes were added to the index.

### No merge conflicts

Tapes are immutable files with content-addressed names. "Merging" provenance from two sources means having both in your DB. There is nothing to merge, reconcile, or deduplicate at the tape level. If two sources contain the same tape (same hash), it's the same file. If they contain different tapes, they're different sessions.

The index rebuilds from the combined set. That's the merge.

## Enrichment Model

Engram's core is dumb and deterministic: fingerprint everything, match, return windows. But raw fingerprint matching only tells you "this transcript touched this code." It doesn't tell you "the agent realized its approach was wrong here" or "this is where the key design decision was made."

Enrichment is an optional additive layer that makes output richer without changing core behavior. If no enrichment exists, everything still works. If enrichment exists, explain output gets richer.

### Three enrichment patterns

#### 1. Inline enrichment (harness-provided)

A harness can insert extra anchoring markers or context windows directly into the transcript it emits. Engram fingerprints these like any other content (because it fingerprints everything). When a match hits a recognized enrichment marker, Engram treats it as a navigation hint — not conversation content — and returns the surrounding real transcript instead of the marker itself.

Example: a future OpenAI integration adds an inline anchoring span around every edit with a wider context window. Engram matches it, recognizes the marker format, and uses it to improve anchor precision without returning the synthetic content to the querying agent.

#### 2. Sidecar files (post-hoc or background agent)

Companion files stored alongside tapes as `<tape_id>.sidecar.jsonl`. These contain:

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

Engram needs to ingest transcripts from the agent harnesses and orchestrators people actually use. This is not a nice-to-have — without working adapters for real systems, Engram is a demo, not a product.

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

| Harness | Tier | Coverage | Notes |
|---------|------|----------|-------|
| Claude Code | Agent | High | Structured read/edit/tool events available in logs |
| Codex CLI | Agent | Partial | `apply_patch` edits captured; generic shell reads/edits are gaps |
| OpenClaw | Orchestrator | Discovery | Session logs contain dispatch decisions, agent coordination |
| OpenCode | Agent | Discovery | Adapter scaffolded, needs real-world validation |
| Gemini CLI | Agent | Discovery | Adapter scaffolded, needs real-world validation |
| Cursor | Agent | Discovery | Adapter scaffolded, needs real-world validation |

The OpenClaw adapter is notable because it captures orchestrator-tier provenance — the decisions and reasoning that drive coding agents. Its tapes typically contain dispatched prompts, agent selection logic, and high-level architectural reasoning. These produce fingerprints that overlap with coding agent tapes, enabling the automatic cross-tier linking described in Multi-Tier Provenance.

OpenClaw-specific submitter/header propagation is an example integration pattern, not part of Engram core semantics. Any orchestrator or harness can implement the same dispatch-marker contract by embedding `<engram-src id="..."/>` in deterministic transcript content.

See `adapters/ADAPTERS.md` for the full adapter contract and `specs/adapters/*.md` for per-harness details.

## Git Integration

Engram integrates with git but does not depend on it. For repos that use git, Engram offers a natural pairing: tapes in `.engram/` travel with the code, and git operations provide natural ingestion triggers.

### Folder model (in-repo)

- `.engram/tapes/` — **committed** to source control. Contains immutable tapes.
- `.engram/config.yml` — **committed** if the repo needs config overrides (isolation, custom DB path). Most repos do not have this file.
- `.engram-cache/` — **never committed**. Contains derived artifacts, temp files.

<!-- CHANGED: Clarified that the fingerprint DB is NOT committed. It is derived
     and lives in the system store or a location specified by config. -->
The fingerprint DB (`index.sqlite`) is never committed to the repo. It is
derived and lives at the location specified by the resolved config's `db:`
key — typically `~/.engram/index.sqlite`.

### Integration: file watchers (recommended)

The recommended integration is a **file watcher** on the session directory of your AI harness — not git hooks.

When an agent edits a file, the event appears in the session transcript immediately, before any git operation. A file watcher (e.g. `fswatch`) fires on new/updated session files and calls `engram ingest <file>` in real time:

```bash
fswatch -0 ~/.claude/projects | xargs -0 -r engram ingest
```

This is correct and sufficient. Git events are late (post-commit) and redundant — by the time `git commit` runs, the session transcript already contains the evidence.

### Git hook integration (not recommended)

Git hooks are **not** the right trigger for engram ingestion. The session transcript is the source of truth, not the commit. Do not install engram git hooks. If any exist in your repos, remove them.

### Branch and merge behavior

On a branch, new tapes accumulate as immutable files. On merge, Git merges tape files like any other files — since tapes are write-once, there are no content conflicts. If the same tape filename exists on both sides, it should be identical (write-once invariant). Optional hash-check emits a warning on mismatch but never blocks.

After merge, run `engram fingerprint` to index any newly arrived tapes into the resolved DB.

### Fork divergence and convergence

When forks diverge long-term:
- Keep committing tapes in each fork
- When a fork merges upstream, tape files merge as artifacts
- Recipient runs `engram fingerprint` to index the new tapes
- No SQLite DB merge needed — ever

This stays easy because tapes are immutable and the index is derived. The hard merge work stays in Git where it belongs.

<!-- CHANGED: On-Disk Layout simplified. Removed per-repo index.sqlite.
     Reflects single system DB as default with config-based override. -->
## On-Disk Layout

### System store (auto-created on first use)

```
~/.engram/
  config.yml               # system config: db path, additional_stores, defaults
  index.sqlite             # default fingerprint DB
  tapes/                   # system-level tapes (orchestrator, cross-project, personal)
  sidecars/
    <tape_id>.sidecar.jsonl
```

### Per-repo (inside a git repository, optional)

```
repo/
  .git/
  .engram/
    config.yml             # optional: db override for isolation. Most repos omit this.
    tapes/
      <hash>.jsonl.zst     # compressed trace tapes (immutable, committed)
    sidecars/
      <tape_id>.sidecar.jsonl  # enrichment files (immutable, committed)
  .engram-cache/
    cursors/               # per-harness ingest state (local, never committed)
    tmp/                   # scratch (local, never committed)
```

### Key rule

The fingerprint DB (`index.sqlite`) is **never** stored in a repo. It lives
at the path specified by the resolved config's `db:` key. By default, that
is `~/.engram/index.sqlite`. Tapes are the portable artifact. The DB is
derived and machine-local.

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
- Define or enforce project boundaries
- Automatically discover tapes outside explicitly processed directories
<!-- CHANGED: Added "Run a global sweep" to the not-do list -->
- Run a global ingest sweep — ingestion is always local to the folder you're in

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
    config/
      mod.rs             # config walk-up resolution, db override
    adapters/
      mod.rs             # adapter trait + registry
      claude_code.rs     # Claude Code adapter
      codex_cli.rs       # Codex CLI adapter
      openclaw.rs        # OpenClaw orchestrator adapter
      opencode.rs        # OpenCode adapter
      gemini_cli.rs      # Gemini CLI adapter
      cursor.rs          # Cursor adapter
```

## Usage Metrics

### Why we collect metrics

Engram's default window sizes (how many lines of content to return with explain results and peek calls) are configurable but we don't yet know the right defaults. Too small and the agent has to immediately re-request more. Too big and tokens are wasted on content the agent ignores.

Rather than guess, engram logs minimal per-call metrics so defaults can be tuned from real usage data.

### What is collected

Each CLI call appends one JSON line to `~/.engram/metrics.jsonl`:

```json
{"ts":"2026-03-19T14:30:00Z","command":"explain","target":"server.ts:3398-3412","session_id":null,"window_start":null,"window_lines":null,"total_lines":null}
{"ts":"2026-03-19T14:30:05Z","command":"peek","target":"af156abd","session_id":"af156abd...","window_start":2754,"window_lines":30,"total_lines":7418}
{"ts":"2026-03-19T14:30:08Z","command":"peek","target":"af156abd","session_id":"af156abd...","window_start":2784,"window_lines":30,"total_lines":7418}
```

Seven fields per call:

| Field | Description |
|-------|-------------|
| `ts` | Timestamp for temporal grouping |
| `command` | explain, grep, or peek |
| `target` | What was queried (span, pattern, or session_id) |
| `session_id` | Session being read (null for explain/grep) |
| `window_start` | First line of returned content |
| `window_lines` | Lines returned |
| `total_lines` | Total lines available in the session |

### How to use the metrics

The key insight: `session_id` is the natural correlation key. You don't need a synthetic correlation ID because the session_id links explain results to follow-up peek calls.

**"Is the default window too small?"** Look for multiple peek calls to the same session_id within a short time window. If the agent does `peek X lines 1-30` then immediately `peek X lines 31-60`, the default was too small — the agent needed more than it got.

**"Is the default window too big?"** If explain returns 10 sessions with 30-line windows and the agent only peeks 2 of them, most of that content was wasted. A smaller default would save tokens.

**"How deep do agents navigate?"** Count distinct session_ids peeked per explain target. 1 = stopped at root. 3+ = walked the chain.

An agent (or user) can analyze `~/.engram/metrics.jsonl` periodically and adjust defaults in config:

```yaml
explain:
  default_limit: 10
peek:
  default_lines: 40    # tune this based on metrics
  default_before: 30
  default_after: 10
```

Over time, the metrics reveal the right balance between token efficiency and orientation quality for your specific usage patterns.

### Config

Metrics logging is on by default. To disable:

```yaml
metrics:
  enabled: false
```

To change the log path:

```yaml
metrics:
  log: /custom/path/metrics.jsonl
```

## Open Questions

- **Anchor algorithm calibration**: k-gram size, window size, hash function. Needs benchmarking against real codebases. The thresholds (0.30, 0.50, 0.90) may need tuning, but the architecture is correct regardless of specific numbers.
- **Multi-language tokenization**: token k-grams need to handle diverse syntax. Start language-agnostic (whitespace + punctuation split)?
- **Vector search bolt-on**: when to add, what embedding model, local-only constraint.
- **Secrets redaction**: what should tapes exclude? Redaction strategy for sensitive content.
- **Incremental ingest cursors**: per-harness state tracking for efficient re-scan (partially designed, not fully implemented).

<!-- CHANGED: Removed old open question about index location strategy.
     That question is now answered: walk-up config resolution with overridable db: key.
     Default is single system DB at ~/.engram/index.sqlite. -->

---

## Appendix: Revision 2 Changelog (2026-03-07)

*This appendix summarizes all changes from the original design document,
for impl and review agents.*

### Summary of changes

1. **New section: Config System** — defines walk-up resolution, auto-creation,
   `db:` override, and `additional_stores:`. Replaces the old "Source resolution"
   and "Per-repo vs global config" subsections.

2. **New principle: P11 (Config conspicuity)** — every command prints resolved
   config path and DB path.

3. **P6 (Zero-config start) rewritten** — auto-creation remains, and `engram init`
   is optional explicit local workspace creation.

4. **`engram init` reintroduced as a real command** — creates local
   `./.engram/config.yml` with `db: .engram/index.sqlite`, plus local store dirs.

5. **`engram ingest` rewritten** — now local-scoped (operates on current folder
   and subfolders). No global sweep. Creates tapes + fingerprints.

6. **New command: `engram fingerprint`** — indexes existing tapes only. No
   transcript parsing. For consuming tapes from clones, mounts, or shares.

7. **`engram explain` rewritten** — global by nature (queries resolved DB).
   Removed `--dispatch <uuid>` mode (dispatch markers are followed as normal
   links during traversal). Added config conspicuity. Added `additional_stores`
   fan-out.

8. **On-Disk Layout simplified** — removed per-repo `index.sqlite`. DB lives
   at the resolved config's `db:` path (default `~/.engram/index.sqlite`).

9. **Scope and Privacy updated** — scoping now controlled by where you run
   ingest/fingerprint + config walk-up DB isolation, not by source config lists.

10. **Sharing Provenance updated** — references `engram fingerprint` instead
    of `engram ingest` for consuming shared tapes.

11. **Git Integration updated** — hooks run `ingest` + `fingerprint`. Post-merge
    runs `fingerprint` to index incoming tapes.

12. **Removed open question** about index location strategy (now answered:
    walk-up with overridable `db:`).

13. **"What Engram Does NOT Do" updated** — added "run a global ingest sweep."

### Concepts removed

- Per-repo fingerprint DB as default behavior
- `--global` flag on any command
- `engram init` as a required setup step
- `--dispatch <uuid>` explain mode
- Global source list in config (replaced by local-scoped ingest)
- First-found-stop config walk-up (replaced by cascading merge walk-up)

### Concepts added

- Single system DB as emergent default (not hard constraint)
- Config walk-up resolution (cascading merge, nearest key wins)
- `db:` override at any config level for isolation
- `tapes_dir:` optional override for ingest tape output location
- `additional_stores:` for multi-DB explain fan-out
- `engram fingerprint` command
- Local-scoped ingest (folder you're in, not global sweep)
- Config conspicuity on every command
- Auto-creation of system store on first invocation

### What impl agents need to change

| Area | Before | After |
|------|--------|-------|
| Store init | `engram init` creates `.engram/` in cwd | Keep auto-create `~/.engram/` on first use; `engram init` explicitly creates local workspace config/dirs |
| Config location | Repo `.engram/config.yml` + global `~/.engram/config.yml`, merged | Walk-up resolution with cascading merge (nearest key wins, missing keys inherit) |
| DB location | Per-repo `.engram/index.sqlite` or global | Default `~/.engram/index.sqlite`, overridable via `db:` |
| Ingest scope | Global source list or repo-local | Local to cwd and subfolders |
| New command | N/A | `engram fingerprint` — index existing tapes only |
| Explain scope | Repo-local or `--global` | Always queries resolved DB + `additional_stores` |
| Explain dispatch | `--dispatch <uuid>` mode | Removed; dispatch links followed during normal traversal |
| Output | Silent about config/DB | Every command prints resolved config + DB path |

### What review agents should verify

- `--global` flag is fully removed from CLI parsing and help text
- `engram init` creates local `./.engram/config.yml` + store dirs and is idempotent
- Config walk-up collects all configs from cwd up to `~` and merges them
- If cwd is outside `~`, walk-up is skipped and `~/.engram/config.yml` is used directly
- `engram ingest` only processes files in cwd and subdirectories
- `engram fingerprint` only processes tapes in cwd's `.engram/tapes/`
- `engram explain` queries resolved DB + all `additional_stores`, deduplicates
- `tapes_dir` participates in cascading merge and inherits like `db:`
- auto-generated configs include explicit `tapes_dir: .engram/tapes`
- `tapes_dir` path resolution matches `db:` rules (tilde + relative-to-config-base)
- `--dispatch` flag is removed from explain
- Dispatch marker links are followed during normal explain traversal
- Every command's first output lines show resolved config path and DB path
- No per-repo `index.sqlite` — DB always at resolved `db:` path
- Auto-creation of `~/.engram/` happens on first invocation of any command
- Internal consistency: no remaining references to `--global`, `engram init` as
  required, per-repo DB as default, or `--dispatch` mode anywhere in the document

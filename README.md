# Engram

Engram answers one question: **why does this code span exist?**

## 1. What it is

Engram is a local provenance index for agent-driven software work.

It ingests deterministic transcript/log artifacts (messages, tool calls, reads, edits, results), stores them as immutable tapes, builds a derived evidence index, and lets you query from a code span back to the sessions that caused it.

Core ideas:
- Tapes are immutable files.
- The index is derived and rebuildable.
- Provenance is additive: more sources give better recall.
- Engram is harness-agnostic; it does not require one vendor workflow.

## 2. How you use it

Typical flow:

```bash
# one-time setup in a repo
engram init

# import new transcript/log evidence from configured sources
engram ingest

# ask why a span exists
engram explain src/auth.rs:40-78
```

What the "magic" is:
- `engram ingest` parses configured source files with deterministic adapters, writes normalized tapes, and updates the evidence index.
- `engram explain <file>:<start>-<end>` fingerprints the selected code span, finds matching evidence in tapes, and returns ranked session context and lineage.
- `engram explain --dispatch <uuid>` starts from a dispatch marker UUID and returns sessions/spans tied to that work item.

Global mode (shared index/tapes across repos):

```bash
engram init --global
engram ingest --global
engram explain src/lib.rs:10-20 --global
```

## 3. How you configure it

Engram reads YAML config from repo-level and user-level locations (repo-local and global workflows can be layered).

Minimum schema:

```yaml
sources:
  - path: ~/.codex/sessions/**/*.jsonl
    adapter: codex
  - path: ~/.claude/projects/**/*.jsonl
    adapter: claude
  - path: ~/.openclaw/agents/main/sessions/**/*.jsonl
    adapter: openclaw
exclude:
  - ~/.claude/projects/**/personal-*
```

Field meanings:
- `sources`: list of evidence inputs Engram should ingest.
- `sources[].path`: a file path or glob for transcript/log artifacts.
- `sources[].adapter`: parser to use for that source (`auto|codex|claude|cursor|gemini|opencode|openclaw`).
- `exclude`: globs removed from ingest even if they match a source path.

Practical rule: use `sources` to set your provenance boundary, and `exclude` to enforce privacy/noise boundaries.

## 4. How you install it

Build from source:

```bash
git clone https://github.com/clickety-clacks/engram.git
cd engram
cargo build --release
```

Install the binary for your user:

```bash
cargo install --path .
# or copy target/release/engram into a directory on PATH
```

Verify:

```bash
engram --help
```

## 5. How you link multiple levels of agents together

When work is handed across sessions, include a shared marker in transcript content:

```text
<engram-src id="f47ac10b-58cc-4372-a567-0e02b2c3d479"/>
```

Human model:
1. Upstream session creates a work item UUID and sends a task.
2. Downstream session receives the marker and performs code work.
3. Engram records where that UUID was received/sent in each tape.
4. `engram explain` can walk upstream from the edit to parent sessions.

Concrete chain example:
- Session A (planner) dispatches auth refactor with `<engram-src id="..."/>`.
- Session B (implementer) edits `src/auth.rs` while carrying the same marker.
- Session C (follow-up fixer) keeps the marker for bugfixes.
- Querying B/C edits can recover A -> B -> C causal lineage.

OpenClaw note (example only):
- An OpenClaw submitter can propagate the UUID in a dispatch header and mirror it in message content as `<engram-src .../>`.
- That submitter/header pattern is an integration example, not Engram core behavior. Any orchestrator or harness can implement the same marker contract.

## Specs

- Core event contract: `specs/core/event-contract.md`
- Dispatch marker: `specs/core/dispatch-marker.md`
- Adapter contracts: `specs/adapters/*.md`

# Engram

**Agentic engineering needs memory.**

Engram is a causal index over code for agent-driven development. It captures the full execution trail agents already emit — prompts, tool calls, reads, edits, and results — then makes that trail queryable at span level.

Engram answers: **"why does this code span exist?"**

## Agent integration (recommended default)

This section is intentionally agent-executable. If an agent follows these steps, the repo gets a good-hygiene Engram setup with Git-coupled operations, without overwriting existing repo hooks.

### Repo layout

- `.engram/` → **committed** immutable tape artifacts
- `.engram-cache/` → **local only** derived cache/index (never committed)

### One-time setup

```bash
# from repo root
engram init
mkdir -p .engram .engram-cache .githooks

# install hook scripts shipped in this repo
# non-destructive install: do not overwrite existing hooks
for h in pre-commit pre-push post-merge; do
  if [ -f ".githooks/$h" ]; then
    echo "[engram] .githooks/$h already exists; leaving as-is"
  else
    cp "scripts/hooks/$h" ".githooks/$h"
  fi
done
chmod +x .githooks/pre-commit .githooks/pre-push .githooks/post-merge

git config core.hooksPath .githooks
```

### Git ↔ Engram hygiene mapping

- `git commit` → pre-commit hook runs `engram ingest`
- `git push` → pre-push hook runs `engram ingest` and freshness check
- `git merge` / `git pull` → post-merge hook rebuilds local `.engram-cache` index from `.engram` tapes

### Daily workflow

Use Git normally. Hooks should handle Engram hygiene.

If needed manually:

```bash
engram ingest
engram explain <file>:<start>-<end>
```

### Invariants

- Tapes are write-once immutable.
- Never edit existing tape files.
- Never commit `.engram-cache/`.
- If tape filename already exists during import, skip (optional warning-only hash sanity check).

### Adapter coverage (current)

- Claude Code: deterministic adapter path implemented
- Codex CLI: deterministic adapter path implemented (partial read/edit by harness limits)
- OpenCode: adapter implemented/discovery-backed
- Gemini CLI: adapter implemented/discovery-backed
- Cursor: adapter implemented/discovery-backed

See adapter specs for exact coverage semantics:
- `specs/adapters/claude-code.md`
- `specs/adapters/codex-cli.md`
- `specs/adapters/opencode.md`
- `specs/adapters/gemini-cli.md`
- `specs/adapters/cursor.md`

### Value proposition

Modern agents are strong at local reasoning but weak at longitudinal memory. Engram turns prior work into retrievable context so each new task can start warm instead of cold.

- Preserve full causal history, not just commit diffs
- Retrieve the exact evidence behind any span before refactoring
- Warm future agent context with real prior decisions, constraints, and tradeoffs
- Reduce repeated mistakes caused by missing historical intent

## Status

Early implementation, actively evolving.

Current capabilities include:
- Tape event parsing and storage
- SQLite-backed evidence/lineage/tombstone indexing
- Span linkage model with confidence thresholds
- Query-side traversal primitives for explain flows
- CLI and E2E test scaffolding

## Core model

- **Trace tapes**: append-only event logs (`msg.in`, `msg.out`, `tool.call`, `tool.result`, `code.read`, `code.edit`, `span.link`, `meta`)
- **Span linkage**: lineage edges with confidence + explicit `agent_link` override
- **Tombstones**: deleted spans are preserved as provenance, never erased
- **Query defaults**: machine-first filtering to reduce noise while preserving causality

## Why this exists

Git tells you *what changed*.
Engram is for *why this span is here*.

## Local dev

```bash
# from repo root
cargo test
cargo run -- --help
```

## Roadmap focus

- Complete `engram explain <file>:<start>-<end>` end-to-end UX
- Tune anchor/similarity thresholds against real codebases
- Harden tape ingestion adapters for agent workflows
- Add release-quality docs and examples

## Repo

https://github.com/clickety-clacks/engram

## Specs

- Core event contract: `specs/core/event-contract.md`
- Claude Code adapter: `specs/adapters/claude-code.md`
- Codex CLI adapter: `specs/adapters/codex-cli.md`
- OpenCode adapter: `specs/adapters/opencode.md`
- Gemini CLI adapter: `specs/adapters/gemini-cli.md`
- Cursor adapter: `specs/adapters/cursor.md`

## Slideshow

https://clawline.chat/engram-slides.html

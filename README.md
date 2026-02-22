# Engram

**Agentic engineering needs memory.**

Engram is a causal index over code for agent-driven development. It captures the full execution trail agents already emit — prompts, tool calls, reads, edits, and results — then makes that trail queryable at span level.

Engram answers: **"why does this code span exist?"**

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

## Slideshow

https://clawline.chat/engram-slides.html

https://github.com/clickety-clacks/engram

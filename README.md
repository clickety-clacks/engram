# Engram

A causal index over code.

Engram answers: **"why does this code span exist?"**

Given a file span, Engram retrieves the causal trail (messages, tool calls, reads, edits) that produced it — so agents can understand context before changing code.

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

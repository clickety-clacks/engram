# Engram Event Contract (Core)

Status: Draft (P0)

## Purpose
A harness-independent, deterministic event schema for provenance ingestion.

## Required properties
- Deterministic parsing (no LLM interpretation)
- Stable IDs for event correlation where available
- Explicit coverage signaling when partial

## Event kinds
- `meta`
- `msg.in`
- `msg.out`
- `tool.call`
- `tool.result`
- `code.read`
- `code.edit`
- `span.link`

## Required fields
All events:
- `t` (ISO timestamp)
- `k` (event kind)
- `source.harness` (e.g., `claude-code`, `codex-cli`)
- `source.session_id` (if available)

`tool.call`:
- `tool`
- `call_id` (if available)
- `args` (verbatim serialized input)

`tool.result`:
- `tool`
- `call_id` (if available)
- `exit` (if available)
- `stdout` / `stderr` (or artifact pointer)

`code.read`:
- `file`
- `range` (line or byte range, declare basis)
- `anchor_hashes` (if available)

`code.edit`:
- `file`
- `before_hash` / `after_hash` (deterministic content hashes when possible)
- `before_range` / `after_range` (if available)
- `similarity` (optional deterministic score)

`span.link`:
- `from_file`, `from_range`
- `to_file`, `to_range`
- `note` (optional)

## Coverage grades
Each ingest run MUST emit coverage metadata:
- `coverage.read`: `full|partial|none`
- `coverage.edit`: `full|partial|none`
- `coverage.tool`: `full|partial|none`

Queries MUST surface coverage grade with results.

## Determinism rule
Adapters may only emit events derivable by deterministic transforms from harness artifacts. If uncertain, emit no event and mark coverage partial.

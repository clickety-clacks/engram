# Adapter Spec: Codex CLI

Status: P0 candidate (partial read/edit coverage)

## Artifact locations
- `~/.codex/sessions/YYYY/MM/DD/*.jsonl`
- `~/.codex/history.jsonl` (history only)

## Deterministic mapping
- session metadata -> `meta`
- message items -> `msg.in/msg.out`
- function_call -> `tool.call`
- function_call_output -> `tool.result` (pair by `call_id`)
- `apply_patch` payload parsing -> `code.edit` file touches

## Known gaps
- Generic shell reads (`cat`, `sed`, `rg`, editor opens) are not universally structured as `code.read`.
- Non-`apply_patch` writes/edits are not universally structured as `code.edit`.

## Coverage expectation
- `coverage.tool=full`
- `coverage.read=partial`
- `coverage.edit=partial`

## P0 requirement for parity
Codex-side structured read/edit event emission (or equivalent sidecar stream) is required to reach full span-level lineage guarantees.

# Engram Release Readiness (Lane A)

Date: 2026-02-22
Branch: master

## 1) Spec-Conformance Gaps From Adversarial Review

### Resolved with concrete decisions

1. Gap: `engram record <command>` was stubbed; only `--stdin` worked.
Decision: Implement command-mode recording that executes the command, captures raw `tool.call`/`tool.result`, stores compressed tape, and indexes it.
Status: PASS
Evidence:
- CLI implementation: `src/main.rs` (`cmd_record`, `capture_command_tape`, `record_transcript`)
- E2E tests: `tests/cli_e2e.rs` (`record_command_captures_tool_events_and_persists_tape`, `record_command_keeps_trace_for_failed_process`)

2. Gap: location-only lineage visibility could be interpreted/persisted in a prescriptive way.
Decision: Keep edge storage raw-facts-only; derive `location_only` classification at read-time from `confidence` + `agent_link` + threshold.
Status: PASS
Evidence:
- Index storage/query behavior: `src/index/mod.rs`
- Tests: `index::tests::location_only_edges_are_hidden_without_forensics_even_with_low_min_confidence`, `tests/current_behavior.rs`

3. Gap: event-kind fidelity for unknown events.
Decision: Preserve unknown kinds as `Unknown`/`Other`, never coerce into `Meta`.
Status: PASS
Evidence:
- Parser: `src/tape/event.rs`
- Tests: `unknown_kind_is_not_misclassified_as_meta`, `incomplete_structured_events_fall_back_to_other`

### Explicitly accepted for this phase (rationale)

1. Gap: `record <command>` does not yet produce full file-diff hunks (`code.edit`) automatically.
Decision: Accept for this phase; command-mode still records raw tool I/O and metadata, while full diff extraction remains a follow-on implementation.
Rationale: current scope prioritized reliable tape capture + explain usefulness; no safe/accurate diff extraction path was already present.
Status: OPEN (non-blocking for current phase-drive)

2. Gap: similarity quality depends on available edit similarity/fingerprint material in tape events.
Decision: Accept for this phase; lineage uses provided/derived similarity and preserves location-only forensics when confidence is low.
Rationale: robust anchor/fingerprint benchmarking is an explicit open question in spec.
Status: OPEN (non-blocking)

## 2) CLI Surface vs Spec Intent

### Implemented core commands

- `engram init`: creates `.engram`, index, tape/object dirs.
- `engram record --stdin`: ingests transcript JSONL, stores compressed tape, indexes events.
- `engram record <command ...>`: executes command, records `meta` + `tool.call` + `tool.result`, stores/indexes tape.
- `engram explain <file>:<start>-<end>` and `--anchor` mode with depth/fanout/edge/min-confidence controls.
- `engram tapes`: lists tape metadata.
- `engram show <tape_id>` with `--raw`.
- `engram gc`: removes unreferenced tape blobs while keeping index lineage/evidence.

Status: PASS for intended phase scope.

## 3) Real-Data Validation (Clawline-Style Tape)

Sample input fixture:
- `tests/fixtures/clawline_sample.jsonl`
- Includes `meta`, `msg.in/out`, `tool.call/result`, `code.read`, `code.edit`.

Validation target:
- Verify `engram explain` returns session windows that include conversational/tool context around touches, useful for downstream agents.

Evidence:
- Integration test: `tests/clawline_validation.rs::clawline_style_tape_yields_agent_useful_explain_windows`
- Assertion: explain windows contain at least one of `msg.in`, `msg.out`, `tool.call`, `tool.result` around touched events.

Status: PASS

## 4) Release-Readiness Checklist

- [PASS] Build succeeds
  - Evidence: `cargo test` compiles all targets.

- [PASS] Unit tests green
  - Evidence: 22 unit tests passing.

- [PASS] CLI E2E tests green
  - Evidence: 7 tests in `tests/cli_e2e.rs` passing.

- [PASS] Real-data Clawline-style validation green
  - Evidence: 1 test in `tests/clawline_validation.rs` passing.

- [PASS] Current behavior regression tests green
  - Evidence: 3 tests in `tests/current_behavior.rs` passing.

- [PASS] Span linkage/tombstone/forensics semantics covered
  - Evidence: index + lineage + query tests across thresholds and traversal behavior.

- [PASS] Explain output usable for agent context
  - Evidence: transcript-window assertions in `tests/clawline_validation.rs`.

### Remaining blockers / risks

1. Command-mode diff extraction (`code.edit` from actual filesystem changes) is not yet implemented.
2. Fingerprint/similarity tuning remains an explicit spec open question and may impact lineage precision/recall on large mixed-language repos.

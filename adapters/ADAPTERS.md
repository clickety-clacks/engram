# Engram Adapter Contract (Canonical)

Scope: deterministic ingestion adapters that convert external harness logs into Engram tape events.

This file is the canonical contract for adapter behavior. If implementation, tests, or reviews conflict with this file, this file wins.

## Product Intent

- Adapters exist only to preserve raw, deterministic provenance from real harness artifacts.
- Adapters must not add interpretation, summaries, or inferred intent labels.
- Keep design lightweight: enforce only what is required for deterministic ingestion and release safety.

## MUST Invariants

1. Deterministic only:
   - Every emitted event must come from explicit on-disk harness facts.
   - Same input bytes must produce byte-equivalent normalized JSONL output.
2. Raw-fact preservation:
   - Preserve source values needed for auditability (`tool`, args/input, result payloads, timestamps, identifiers).
   - Do not classify or enrich semantics beyond Engram event schema.
3. Coverage declaration:
   - Each adapter must publish guaranteed deterministic coverage for `meta`, `msg`, `tool`, `code.read`, `code.edit`, `span.link`.
   - Unsupported kinds must be declared as unsupported or partial, never silently claimed as complete.
4. Explicit partial semantics:
   - If a harness cannot deterministically provide an event kind (for example, free-form shell edits), adapter must emit explicit partial/unsupported status in adapter metadata.
5. Stable failure behavior:
   - Invalid input or unknown schema/version must fail with explicit machine-readable error unless policy allows downgrade mode.
6. No hidden fallback inference:
   - No LLM interpretation.
   - No heuristic that can silently change meaning across runs without version bump and contract update.

## SHOULD Value-Adds

- Retain source IDs for call/result correlation when available.
- Preserve large tool outputs (or references) without truncating semantics.
- Emit per-session adapter metadata that records:
  - adapter id/version
  - harness id/version (detected or unknown)
  - coverage status by event kind
  - downgrade/fallback mode used (if any)

## OPTIONAL Value-Adds

- Additional deterministic projections that improve utility but do not alter meaning (for example, extracted file lists from patch headers).
- Strict mode toggles that reject ambiguous but currently tolerated inputs.

## Deterministic Ingest Protocol

1. Detect harness family and schema/version from explicit fields.
2. Validate minimal schema requirements for claimed coverage.
3. Normalize to Engram tape events in stable order:
   - `meta`
   - `msg.in` / `msg.out`
   - `tool.call` / `tool.result` (correlated by explicit IDs when present)
   - deterministic `code.read` / `code.edit` / `span.link`
4. Emit adapter metadata event (or equivalent side metadata) with coverage map and version fields.
5. If unsupported/ambiguous:
   - fail-fast in strict mode
   - or emit explicit downgraded coverage in permissive mode

## Coverage Semantics

Coverage is per event kind and must be reported as one of:

- `full`: all occurrences deterministically extractable by contract
- `partial`: only a deterministic subset extractable
- `none`: unsupported

Rules:

- `full` may only be claimed with deterministic extraction proofs (tests + fixtures).
- `partial` must document deterministic boundaries (what is included/excluded).
- CI must fail if code claims `full` while fixture tests demonstrate misses.

## Harness Versioning Policy

## Supported-Version Matrix (required)

Each adapter must maintain a matrix in code/docs:

- harness family (e.g., codex-cli, claude-code)
- supported version ranges
- schema detection fields
- guaranteed coverage profile per version range

## Version/Schema Detection

- Must rely on explicit version/schema fields when present.
- If absent, must use a documented deterministic fingerprint strategy (field presence/signature), not guesswork.

## Unknown-Version Behavior

Default policy:

- Strict mode: hard fail with `unknown_harness_version`.
- Permissive mode: ingest only contract-safe deterministic subset, mark coverage degraded, and emit warning metadata.

## Downgrade/Fail Rules

- Downgrade allowed only when:
  - deterministic subset remains truthful
  - degraded coverage is explicitly emitted
- Hard fail required when:
  - required correlation fields are missing and would make emitted facts misleading
  - parser cannot guarantee deterministic mapping for claimed event kinds

## CI and Release Compliance Gates

Adapters are release-blocking surfaces.

Required CI gates:

1. Fixture conformance tests:
   - version-pinned fixtures per supported harness version range
   - golden normalized output snapshots
2. Determinism test:
   - repeated parse of identical input must produce identical output bytes
3. Coverage assertions:
   - fixture-validated `full/partial/none` status matches declared matrix
4. Unknown-version tests:
   - strict mode fails
   - permissive mode degrades coverage explicitly
5. Contract lint:
   - adapter docs/matrix/code metadata stay in sync (no undeclared version range)

Release gate expectations:

- No adapter release if any required gate fails.
- No expansion of claimed coverage without new fixtures and matrix update.
- No schema heuristic changes without explicit contract changelog entry.

## Enforcement Expectations for Reviews

- Reviewers must reject:
  - unspecced inference
  - silent downgrade behavior
  - unsupported `full` coverage claims
  - version handling that guesses without deterministic basis
- Reviewers should prioritize correctness of deterministic boundaries over feature breadth.

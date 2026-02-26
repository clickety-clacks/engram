## Adversarial Review: Updated Adapter Governance Docs

Reviewed against the spec provided inline and the round 1 findings in `adapters/reviews/claude_review_output.md`.

### Blocking Findings

None. All round 1 blocking findings (normalization order ambiguity, coverage kind granularity) have been resolved correctly.

### Non-Blocking Findings

**1. Metadata emission sits in both MUST and SHOULD without clear field boundaries**

Deterministic Ingest Protocol step 4 (`ADAPTERS.md:65`) mandates: "Emit adapter metadata event (or equivalent side metadata) with coverage map and version fields." This is a protocol step — normatively required. But the SHOULD section (`ADAPTERS.md:46-50`) lists overlapping metadata fields ("adapter id/version, harness id/version, coverage status by event kind, downgrade/fallback mode") framed as recommended. An implementer reading only the MUST/SHOULD classification would think per-session metadata emission is optional, when protocol step 4 says otherwise. The minimum required metadata fields vs. nice-to-have metadata fields should be clearer.

**2. `meta` event coverage semantics are underspecified**

The spec defines `meta` as "session metadata (model, repo state, label)." The adapter contract requires declaring `full/partial/none` coverage for `meta` (`ADAPTERS.md:25`) but doesn't define what constitutes `full` vs `partial` for this event kind. Unlike `code.edit` (clear: did you extract the hunk?) or `tool.call` (clear: did you capture tool name + args?), `meta` has variable fields. A harness log that includes model ID but not repo HEAD — is that `full` or `partial`? The coverage semantics section (`ADAPTERS.md:70-82`) should note that `meta` coverage is evaluated against the spec's defined meta fields.

### Nits

**3.** `ADAPTERS.md:7` — Upstream authority references an absolute local path (`/Users/mike/shared-workspace/shared/specs/engram.md`). Works for this project's current contributors but won't resolve for anyone else. Consider a repo-relative or URL reference if the audience ever widens.

**4.** `CLAUDE_REVIEWERS.md:40` — "Output format: blocking, non-blocking, nits, or exactly No substantive findings" — the "or" is ambiguous. Reads like the reviewer should pick one of those four words. Suggest: "Output format: blocking findings, then non-blocking findings, then nits. If no substantive issues: state exactly 'No substantive findings'."

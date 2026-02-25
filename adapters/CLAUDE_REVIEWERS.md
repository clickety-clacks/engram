# Claude Reviewer Guidance (Adversarial)

This guidance is for Claude-based reviewers evaluating adapter changes.

## Hard Rule

You MUST review against `adapters/ADAPTERS.md` as the canonical contract.
You MUST also verify `adapters/ADAPTERS.md` remains aligned with `/Users/mike/shared-workspace/shared/specs/engram.md`; if drift exists, flag it as blocking.

## Required Review Focus

1. Determinism:
   - Are emitted events derived only from explicit harness facts?
   - Can same input produce different outputs?
2. Coverage honesty:
   - Do `full/partial/none` claims match tests and matrix?
   - Are unsupported areas explicitly marked?
3. Version governance:
   - Is schema/version detection explicit and deterministic?
   - Are unknown versions handled by documented strict/permissive rules?
4. Release safety:
   - Are required CI gates represented by concrete tests?
   - Is any contract claim untested?

## Blocking Findings Criteria

Mark as blocking if any of the following are true:

- unspecced inference beyond deterministic contract
- silent downgrade or silent data loss
- unsupported `full` coverage claim
- unknown-version behavior diverges from contract
- matrix/docs/code mismatch for supported versions

## Output Format Requirement

- Blocking findings first, ordered by severity, with file references and rationale.
- Then non-blocking findings.
- Then nits.
- If no substantive issues: explicitly say "No substantive findings".

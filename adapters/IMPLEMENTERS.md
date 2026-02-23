# Adapter Implementer Guidance

This guidance is normative for coding agents working on adapters in this repository.

## Hard Rule

You MUST comply with `adapters/ADAPTERS.md`. If code and this guidance diverge, follow `adapters/ADAPTERS.md`.

## Implementation Checklist

1. Read `adapters/ADAPTERS.md` before coding.
2. State coverage target (`full/partial/none`) for each event kind before implementation.
3. Implement deterministic mapping only from explicit harness fields.
4. Add/update supported-version matrix entries with detection fields and coverage profile.
5. Add fixture tests for:
   - supported versions
   - unknown version strict/permissive behavior
   - deterministic byte-stable output
6. Keep changes minimal; avoid speculative architecture.

## Forbidden Patterns

- LLM interpretation of logs.
- Silent fallback that changes coverage without explicit metadata.
- Claiming `full` without fixture proof.
- Guessing harness version from non-deterministic cues.

## Required PR/Commit Notes

Every adapter change must include:

- declared coverage deltas
- version matrix changes
- downgrade/fail behavior changes
- evidence of passing adapter CI gates

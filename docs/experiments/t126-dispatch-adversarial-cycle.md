# T126 Dispatch Marker Adversarial Cycle

This document records adversarial checks run against dispatch marker direction detection and causal traversal.

## Cases and outcomes

1. UUID in both tool body and regular message in same tape
- Test: `dispatch_extraction_handles_same_uuid_in_surface_and_nested_locations` (`src/main.rs`)
- Attack: same UUID appears at depth 0 and nested depth >= 1.
- Result: first-turn direction is `received` when depth-0 appears in that first turn.
- Handling: extractor prefers shallower structural depth when same UUID is seen in the same turn.

2. Session compacted/restarted mid-task, then re-ingested
- Test: `compact_restart_reingest_adds_new_tape_without_duplication` (`tests/dispatch_marker_e2e.rs`)
- Attack: ingest base + worker sessions, run ingest again (no-op), add restart session for same UUID, ingest again.
- Result: unchanged re-ingest is skipped; new restart tape imports exactly once; dispatch query reflects all sessions.
- Handling: ingest cursor/idempotence unchanged; dispatch links are inserted with `INSERT OR IGNORE` keyed by `(tape_id, uuid)`.

3. No received UUID exists before edit turn
- Test: `latest_received_dispatch_before_turn_returns_none_when_edit_precedes_dispatch` (`src/index/mod.rs`)
- Attack: dispatch link exists only after the edit turn.
- Result: traversal seed returns `None`.
- Handling: chain traversal stops cleanly when no qualifying prior `received` marker exists.

4. Two sessions share UUID but both are depth-0 received
- Test: `sent_dispatch_for_uuid_is_none_when_uuid_is_only_received` (`src/index/mod.rs`)
- Attack: no session contains a structural `sent` occurrence for that UUID.
- Result: parent lookup returns `None`.
- Handling: traversal does not invent a parent; chain terminates.

5. Long-running tape with 20+ received UUIDs
- Test: `latest_received_dispatch_before_turn_handles_long_running_tapes` (`src/index/mod.rs`)
- Attack: many received markers; ensure nearest prior marker is chosen.
- Result: selector picks the highest `first_turn_index` strictly before the edit turn.
- Handling: SQL order is deterministic (`ORDER BY first_turn_index DESC`), matching spec's causal-preceding rule.

## Additional chain correctness check

- Test: `explain_dispatch_chain_includes_a_to_b_to_c_and_excludes_sibling` (`tests/dispatch_marker_e2e.rs`)
- Scenario: multi-tier chain `A -> B -> C -> code` with sibling session sharing a UUID but not on the causal path.
- Result: `engram explain <span>` includes `A/B/C` and excludes sibling.

## Notes

- Nested sent markers may arrive with escaped quotes (`id=\\\"...\\\"`) inside shell command strings.
- Direction extraction now normalizes escaped quotes before marker matching to keep structural classification deterministic.

# Engram - Agent Instructions

> The role of this file is to describe common mistakes and confusion points that agents might encounter as they work in this project. If you ever encounter something in the project that surprises you, please alert the developer working with you and indicate that this is the case in the AgentMD file to help prevent future agents from having the same issue.

> This is a greenfield app with no users. Feel free to suggest structural and breaking refactors to help bend this codebase into the right shape.


Native append lineage: dispatch rows retain first occurrences only within each tape. Query selection must fold immutable predecessor segments before choosing a UUID/direction; a repeated marker must not renew its turn. Recovered hops retain context in `session` and the original evidence child in `edit_session`; public chain formatting must use the latter when present.

Legacy Codex suffixes can record an exec result as unknown with no derived exit when the call was in an earlier segment. Recovery must bind the unchanged raw result and unique chronology before filling that annotation; a recorded exit or known tool must never be discarded.

When you build command-peer tape fixtures for `tape_facts`, include a `k: meta` row even when the caller only needs a digest. The owner validates segment metadata before returning `include_digest`, so a tape without that row reports `invalid_tape` and no digest.

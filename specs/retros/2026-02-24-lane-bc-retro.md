## Lane B (index-ingest)
- What changed: hardened ingest/index/query reliability by adding transactional ingest + rollback behavior, idempotent SQLite constraints/migration, location-only forensics gating, event-similarity-driven edge confidence, backward lineage traversal, max_depth default cap (10), and shared FileRange type usage (removed duplicate seam).
- Why it mattered: prevents partial/duplicated index state on failures/retries, keeps lineage/tombstone evidence consistent, and makes explain traversal behavior match spec defaults and confidence semantics.
- Test proof: cargo test passing on lane head with targeted additions for rollback on invalid anchors/similarity, idempotent re-ingest, location-only visibility rules, and traversal depth cap (lineage_traversal_honors_max_depth).
- What surprised you: once confidence stopped being hardcoded, location-only behavior became a real retrieval correctness issue (not just theoretical), and had to be explicitly enforced at read time.
- Recommendation for Lane A: keep event producers emitting code.edit.similarity consistently; if missing, lineage quality drops to forensics-only paths and default explain output becomes sparse.

## Lane C (cli-e2e)
- What changed: implemented engram record <command...> (in addition to --stdin), normalized command-captured tape timestamps to RFC3339, aligned tool.call.args to a string field, and expanded CLI E2E tests for command capture, session ordering, and tape/index recovery lifecycle.
- Why it mattered: CLI moved from partial to practical daily use; users can now wrap real commands directly and still get machine-first JSON outputs and queryable tapes.
- Test proof: cargo test passing with tests/cli_e2e.rs covering command capture (stdout/stderr/exit), explain ordering (touch_count then recency), gc behavior, forensics/tombstones, and file/index reconciliation.
- What surprised you: biggest regression risk was lifecycle coherence (tape blob vs index state) rather than command execution itself; ordering correctness also depended on strict timestamp format consistency.
- Recommendation for Lane A: keep tape/index lifecycle atomicity as a hard invariant in any refactor (especially around ingest/write order and recovery), and preserve the current E2E scenarios as required gates.

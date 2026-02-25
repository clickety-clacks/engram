## Lane C (cli-e2e)
- What changed: implemented `engram record <command...>` (in addition to `--stdin`), normalized command-captured tape timestamps to RFC3339, aligned `tool.call.args` to a string field, and expanded CLI E2E tests for command capture, session ordering, and tape/index recovery lifecycle.
- Why it mattered: CLI moved from partial to practical daily use; users can now wrap real commands directly and still get machine-first JSON outputs and queryable tapes.
- Test proof: `cargo test` passing with `tests/cli_e2e.rs` covering command capture (`stdout`/`stderr`/exit), explain ordering (`touch_count` then recency), gc behavior, forensics/tombstones, and file/index reconciliation.
- What surprised you: biggest regression risk was lifecycle coherence (tape blob vs index state) rather than command execution itself; ordering correctness also depended on strict timestamp format consistency.
- Recommendation for Lane A: keep tape/index lifecycle atomicity as a hard invariant in any refactor (especially around ingest/write order and recovery), and preserve the current E2E scenarios as required gates.

# Real Harness Validation

This validation pass checks discovery and import behavior against realistic
harness-like directory layouts for all implemented adapters:

- Claude (`claude-code`)
- Codex (`codex-cli`)
- Gemini (`gemini-cli`)
- OpenClaw (`openclaw`)
- OpenCode (`opencode`)
- Cursor (`cursor`)

It is deterministic and CI-friendly:
- Uses temp directories only.
- Uses checked-in fixtures only.
- Requires no external auth/login.

## Run

From repo root:

```bash
scripts/real-harness-validation.sh
```

Equivalent direct command:

```bash
cargo test --test real_harness_validation -- --nocapture
```

## What It Validates

For each adapter:

1. Realistic on-disk layout for discovery roots (`~/.claude/...`,
   `~/.codex/...`, `~/.gemini/...`, `~/.openclaw/...`,
   `~/.local/share/opencode/...`, Cursor `workspaceStorage`).
2. Positive discovery for the target repo.
3. Negative discovery for a wrong repo (target repo artifacts are not matched).
4. Importability check:
   - read discovered artifact
   - run adapter conversion
   - parse normalized events
   - require non-empty event stream

## Interpreting Results

- `PASS`:
  - All adapter discovery checks passed.
  - Negative checks passed for every adapter.
  - Conversion+parse path passed for every adapter.
- `FAIL`:
  - At least one adapter’s discovery or importability contract regressed.
  - The failing test name indicates which harness behavior changed.

## Confidence Gaps

- `opencode` discovery still depends on inferred project-id conventions from
  git metadata/cache (`.git/opencode` fallback behavior).
- `cursor` discovery points to `state.vscdb` paths; this validation keeps the
  file content text-based for deterministic adapter conversion checks.
  Real Cursor `state.vscdb` artifacts are SQLite.

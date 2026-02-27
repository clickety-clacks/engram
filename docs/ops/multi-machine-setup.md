# Engram Multi-Machine Setup Runbook

This runbook is operator-focused and command-first.

## 1) EEZO-first ingest with git-cycle hooks (repo-local mode)

Run in each repo that should carry `.engram/` tapes with the code.

```bash
cd ~/src/engram
engram init

mkdir -p .githooks
for h in pre-commit pre-push post-merge; do
  if [ ! -f ".githooks/$h" ]; then
    cp "scripts/hooks/$h" ".githooks/$h"
  fi
done
chmod +x .githooks/pre-commit .githooks/pre-push .githooks/post-merge
git config core.hooksPath .githooks
```

Set repo-local sources in `.engram/config.yml`:

```yaml
sources:
  - path: ~/.codex/sessions/**/*.jsonl
    adapter: codex
  - path: ~/.claude/projects/**/*.jsonl
    adapter: claude
exclude:
  - ~/.claude/projects/**/sessions-index.json
```

Manual first ingest:

```bash
engram ingest
engram tapes | jq '.tapes | length'
```

Validate explain on repo code span:

```bash
engram explain src/store/mod.rs:1-2
```

## 2) TARS global ingest with OpenClaw transcripts

Use one shared global index/tape root on TARS.

```bash
cd ~/src/engram
engram init --global
```

Edit `~/.engram/config.yml`:

```yaml
sources:
  - path: ~/.codex/sessions/**/*.jsonl
    adapter: codex
  - path: ~/.claude/projects/**/*.jsonl
    adapter: claude
  - path: ~/.openclaw/sessions/**/*.jsonl
    adapter: openclaw
exclude:
  - ~/.claude/projects/**/sessions-index.json
  - ~/.openclaw/sessions/personal-*
```

Ingest + query:

```bash
engram ingest --global
engram explain ~/src/engram/src/store/mod.rs:1-2 --global
```

## 3) Optional NFS/shared-tape path model

When multiple machines should read a common tape pool, store source tapes on shared/NFS and keep each machine’s index/cache local.

Example:

```yaml
sources:
  - path: /mnt/engram-shared/codex/**/*.jsonl
    adapter: codex
  - path: /mnt/engram-shared/claude/**/*.jsonl
    adapter: claude
  - path: /mnt/engram-shared/openclaw/**/*.jsonl
    adapter: openclaw
exclude:
  - /mnt/engram-shared/**/sessions-index.json
```

Recommended operator pattern:

1. EEZO/TARS write tape artifacts to shared path.
2. Each machine runs `engram ingest --global` locally.
3. Each machine keeps its own `~/.engram-cache/` and cursor state.

Notes:
- Engram persisted state writes use atomic temp-write + fsync + rename + parent-dir fsync.
- Keep shared path for source artifacts; avoid sharing `~/.engram-cache/`.


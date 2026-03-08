# Engram Multi-Machine Setup Runbook

This runbook is command-first and aligned with DESIGN.md rev2 semantics.

## 1) EEZO-first ingest with git-cycle hooks (local-scoped contribution)

Run in each repo that should contribute provenance.

```bash
cd ~/src/engram
mkdir -p .githooks
for h in pre-commit pre-push post-merge; do
  if [ ! -f ".githooks/$h" ]; then
    cp "scripts/hooks/$h" ".githooks/$h"
  fi
done
chmod +x .githooks/pre-commit .githooks/pre-push .githooks/post-merge
git config core.hooksPath .githooks
```

If no config exists yet, first command invocation auto-creates `~/.engram/config.yml`.

Manual first pass:

```bash
engram ingest
engram fingerprint
engram tapes | jq '.tapes | length'
engram explain src/store/mod.rs:1-2
```

## 2) TARS ingest for OpenClaw transcripts (example path)

Use directory-local config walk-up to point transcript folders at the shared DB.

```bash
mkdir -p ~/.openclaw/.engram
cat > ~/.openclaw/.engram/config.yml <<'YAML'
db: ~/.engram/index.sqlite
additional_stores:
  - /mnt/team/engram/index.sqlite
YAML
```

Run ingest from the transcript root so scope is `cwd + subfolders`:

```bash
cd ~/.openclaw
engram ingest
```

Query from any repo (walk-up resolved DB + additional stores):

```bash
cd ~/src/engram
engram explain src/store/mod.rs:1-2
```

## 3) Optional NFS/shared-tape model

When machines share immutable tapes, index them with `fingerprint` from that folder.

```bash
cd /mnt/engram-shared
engram fingerprint
```

Recommended pattern:

1. EEZO/TARS produce local tapes via `engram ingest`.
2. Copy or sync tape files (`.jsonl.zst`) into shared tape folders.
3. Each machine runs `engram fingerprint` where those tapes are mounted.
4. Keep DB files machine-local unless explicitly operating a shared SQLite path.

Notes:
- Engram persisted state uses atomic write + fsync + rename + parent-dir fsync.
- No `--global` mode in rev2. Scope is controlled by where commands are run and which `db` is selected by config walk-up.

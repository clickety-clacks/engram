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

## 2) Ingest OpenClaw transcripts on their owner

Keep the transcript source and Engram index on the machine where the collector
writes them. A local watch config can point at that machine's OpenClaw directory:

```yaml
watch:
  sources:
    - path: /home/alex/.openclaw
      pattern: "*.jsonl"
```

Run `engram watch` with that local config, or ingest from the transcript root:

```bash
cd ~/.openclaw
engram ingest
```

Do not point two machines at the same SQLite file or put an active SQLite index
on NFS. Each owner reads its own database and tape files.

## 3) Query another machine explicitly

On the owner, declare only its intended default store in the home-only
`~/.engram/topology.yml`:

```yaml
version: 1
self: build-host
exports:
  default:
    db: /home/alex/.engram/index.sqlite
    tape_dirs:
      - /home/alex/.engram/tapes
```

On the caller, add a peer entry to its own `~/.engram/topology.yml`:

```yaml
version: 1
self: laptop-a
peers:
  build-host:
    ssh: alex@build-host
    engram: /usr/local/bin/engram
    exports: [default]
```

The peer entry does not change default query scope. Select the peer on each
command:

```bash
engram explain src/store/mod.rs:1-2 --peers build-host
engram grep "token refresh" --peers build-host
engram topology status --peers build-host
```

Engram uses SSH to launch a temporary `peer-serve --stdio` process on the
owner. It does not mount or copy the remote database. Use a dedicated
restricted SSH key with a forced command if the key should not grant an
interactive shell; see [the multi-machine guide](../multi-machine.md).

If a selected peer fails, the command keeps completed results and reports
partial coverage with the source, failed phase, typed error, and observed
reason. Do not guess a cause that the connection did not report. Use
`--require-complete` with federated `explain`, `grep`, or `show --peers` when a
failed source or incomplete conclusion should make the command exit nonzero.

Before removing a peer or export from your configured query scope, compare its
coverage with the sources that will remain. Changing query scope does not
delete retained indexes or tape files.
Do not expand the default export as part of setup.

Local config walk-up still chooses the database for ingest and local queries;
there is no `--global` query mode. State-file writes use atomic replacement
with file and parent-directory synchronization.

# Engram

Engram answers one question: **why does this code span exist?**

## 1. What it is

Engram is a deterministic provenance index for agent-driven software work.

It stores immutable tapes and indexes their fingerprints in SQLite so a code-span query can return the sessions that causally produced that span.

Core model:
- Tapes are immutable files.
- The DB is derived from tapes and can be rebuilt.
- Ingest/fingerprint are local contribution commands.
- Explain is global retrieval over the resolved DB plus optional additional stores.

## 2. How you use it

### One-shot ingest

```bash
# optional: create an explicit local workspace store
engram init

# from the folder you are working in
engram ingest

# ask why a span exists
engram explain src/auth.rs:40-78
```

### Continuous ingest (recommended)

```bash
engram watch
```

`engram watch` monitors directories listed under the `watch:` key in config.yml, runs ingest on each new or changed file that matches the configured pattern, and logs activity to `watch.log`. This is the recommended integration pattern — file watchers, not git hooks.

### How commands work

- `engram ingest [PATH...]`: local-scoped. Walks the current directory tree (or given paths) for transcript files, converts recognized harness logs into tapes, and fingerprints those tapes into the resolved DB.
- `engram watch`: long-running file watcher. Reads `watch.sources` from the resolved config.yml, watches those directories for new/changed files, debounces, and runs ingest on each matching file. Requires a `watch:` section in config.
- `engram fingerprint`: local-scoped. Indexes existing `./.engram/tapes/*.jsonl.zst` into the resolved DB (no transcript parsing, no tape creation).
- `engram explain <file>:<start>-<end>`: computes anchors for the selected span, queries the resolved DB, follows lineage and dispatch-marker links, and returns evidence sessions/windows.

Dispatch markers are traversed during normal explain:

```text
<engram-src id="f47ac10b-58cc-4372-a567-0e02b2c3d479"/>
```

There is no separate `--dispatch` explain mode.

## 3. How you configure it

### Config resolution

Engram walks up the directory tree from the current working directory looking for `.engram/config.yml`. The first one found wins. No merge between levels. If none is found, falls back to `~/.engram/config.yml`.

On first invocation, Engram auto-creates `~/.engram/config.yml` if missing.

Every command prints the resolved config path and DB path before command output.

### Repo-level vs global config

Use two levels of config:

**Global** (`~/.engram/config.yml`) — sets `db` and `additional_stores`:

```yaml
db: ~/.engram/index.sqlite
additional_stores:
  - /nfs/team/engram/index.sqlite
```

**Repo-level** (`.engram/config.yml` in your repo root) — sets `tapes_dir` so tapes travel with the repo:

```yaml
tapes_dir: .engram/tapes
```

Do not set `db:` or `additional_stores:` in repo-level configs. Let those walk up to the global config.

### Field reference

- `db`: primary SQLite store this directory writes to and reads from.
- `tapes_dir`: where tapes are stored. Relative paths resolve from the config file's parent directory.
- `additional_stores`: extra read-only stores queried by `engram explain` (fan-out + dedupe).

### Watch config

Add a `watch:` section to the config where `engram watch` will be run (typically the global config):

```yaml
watch:
  debounce_secs: 5          # seconds to wait after a file event before ingesting (default: 5)
  ingest_timeout_secs: 120  # max seconds per ingest run (default: 120)
  log: ~/.engram/watch.log  # log file path (default: ~/.engram/watch.log)
  sources:
    - path: ~/shared/openclaw
      pattern: "*.jsonl"
    - path: ~/sessions
      pattern: "session-*.json"
```

Each source entry:
- `path`: directory to watch (recursive).
- `pattern`: glob pattern for files to ingest within that directory.

## 4. How you install it

Build from source:

```bash
git clone https://github.com/clickety-clacks/engram.git
cd engram
cargo build --release
```

Install for your user:

```bash
cargo install --path .
# or copy target/release/engram to a directory on PATH
```

Verify:

```bash
engram --help
```

`engram init` is optional: it creates `./.engram/config.yml` with `db: .engram/index.sqlite` and local store directories.

## 5. How you link multiple levels of agents together

Include the same marker in handoff content across sessions:

```text
<engram-src id="f47ac10b-58cc-4372-a567-0e02b2c3d479"/>
```

Human model:
1. Upstream session sends work with marker `X`.
2. Downstream session receives marker `X` and edits code.
3. A later session continues with marker `X`.
4. `engram explain` on touched code follows dispatch links upstream and returns the causal chain.

OpenClaw note (example only):
- An OpenClaw submitter can propagate the UUID in a header and mirror it in message content as `<engram-src .../>`.
- That submitter/header pattern is an integration example, not Engram core behavior.

## Specs

- Core event contract: `specs/core/event-contract.md`
- Dispatch marker: `specs/core/dispatch-marker.md`
- Adapter contracts: `specs/adapters/*.md`

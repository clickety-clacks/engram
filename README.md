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

Normal loop:

```bash
# from the folder you are working in
engram ingest

# ask why a span exists
engram explain src/auth.rs:40-78
```

How commands work:
- `engram ingest`: local-scoped. Walks the current directory tree for transcript files, converts recognized harness logs into tapes, and fingerprints those tapes into the resolved DB.
- `engram fingerprint`: local-scoped. Indexes existing `./.engram/tapes/*.jsonl.zst` into the resolved DB (no transcript parsing, no tape creation).
- `engram explain <file>:<start>-<end>`: computes anchors for the selected span, queries the resolved DB, follows lineage and dispatch-marker links, and returns evidence sessions/windows.

Dispatch markers are traversed during normal explain:

```text
<engram-src id="f47ac10b-58cc-4372-a567-0e02b2c3d479"/>
```

There is no separate `--dispatch` explain mode.

## 3. How you configure it

Config resolution is walk-up, first-found-wins:
1. `./.engram/config.yml`
2. parent directories’ `.engram/config.yml`
3. fallback `~/.engram/config.yml`

No merge between levels.

On first invocation, Engram auto-creates `~/.engram/config.yml` if missing.

Primary schema:

```yaml
db: ~/.engram/index.sqlite
additional_stores:
  - /nfs/team/engram/index.sqlite
```

Field meanings:
- `db`: primary SQLite store this directory writes to and reads from.
- `additional_stores`: extra read-only stores queried by `engram explain` (fan-out + dedupe).

Every command prints resolved config path and DB path before command output.

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

`engram init` is deprecated and not required.

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

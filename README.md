# Engram

Engram records agent work in immutable tape files and builds a local SQLite index from them. Use `explain` to find conversations linked to code or text, `grep` to search conversation text, and `peek` to read a session.

Each machine keeps its own SQLite database and tapes. Engram has no central index and starts no listening service. Search and read commands stay local unless you select peers with `--peers` or select one remote export with `--store`. For a remote request, Engram uses SSH to start one temporary `peer-serve` process on each selected owner.

Licensed under the Apache License, Version 2.0.

## 1. What it is

Engram is a deterministic provenance index for agent-driven work.

It stores immutable tapes and indexes their fingerprints in SQLite so a query on code or text can return the conversations that causally produced it.

Core model:
- Tapes are immutable files.
- Engram builds the SQLite index from tapes, so you can rebuild it.
- `ingest` writes local tapes and index rows. `fingerprint` indexes tapes that already exist locally.
- `explain` and `grep` use resolved local stores unless you pass `--peers`.
- `show` and `peek` read one remote export when you pass `--store <machine/export>`.

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

`engram watch` monitors directories listed under the `watch:` key in config.yml, runs ingest on each new or changed file that matches the configured pattern and optional glob filter, and logs activity to the configured log path. This is the recommended integration pattern.

### How commands work

- `engram ingest [PATH...]`: discovers transcript files, converts recognized logs into tapes, and fingerprints those tapes into the resolved DB.
- `engram watch`: long-running file watcher. Reads `watch.sources` from the resolved config.yml, watches those directories for new/changed files, debounces, and runs ingest on each file matching the source pattern and optional glob. Requires a `watch:` section in config.
- `engram fingerprint`: indexes existing `./.engram/tapes/*.jsonl.zst` into the resolved DB (no transcript parsing, no tape creation).
- `engram explain <file>:<start>-<end>`: computes anchors for the selected span, searches resolved local stores, follows lineage and dispatch-marker links, and returns evidence sessions/windows. Add `--peers <name[,name]>` to search selected owners too.
- `engram grep <pattern>`, `engram peek <session>`, and `engram show <tape-id>`: search conversation text, read a session, or read a tape. Pass `--peers <name[,name]>` to `grep` or `show` when you want to search selected peers. Pass `--store <machine/export>` to `peek` or `show` when you want one remote export.

Dispatch markers are traversed during normal explain:

```text
<engram-src id="f47ac10b-58cc-4372-a567-0e02b2c3d479"/>
```

`explain` follows dispatch markers automatically. It has no separate `--dispatch` mode.

## 3. How you configure it

### Config resolution

Engram starts in the current directory and reads `.engram/config.yml` files up to `~/.engram/config.yml`. For each key, the closest file that sets it wins. Other keys inherit from parent files. If the current directory is outside `HOME`, Engram reads only `~/.engram/config.yml`.

The first command that loads local configuration creates `~/.engram/config.yml` if it does not exist.

Commands that load the local store print the resolved config path and database path to stderr.

`config.yml` sets local database and tape paths. Peer settings live separately in the home-only `~/.engram/topology.yml`; Engram does not read peer settings from a repository. Search and read commands do not start configured peers unless you pass `--peers` or select one export with `--store`. Run `engram topology status` to check peer connections.

### Local and global config

Put shared database settings in `~/.engram/config.yml`:

```yaml
db: ~/.engram/index.sqlite
additional_stores:
  - /nfs/team/engram/index.sqlite
```

Put a repository's tape directory in its `.engram/config.yml`:

```yaml
tapes_dir: .engram/tapes
```

Keep `db` and `additional_stores` out of repository config files. Engram inherits those values from the global config.

### Field reference

- `db`: primary SQLite store this directory writes to and reads from.
- `tapes_dir`: where tapes are stored. Relative paths resolve from the config file's parent directory.
- `additional_stores`: extra local read-only databases included in local queries.

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
      glob: "codex/**/*.json"
```

Each source entry:
- `path`: directory to watch (recursive).
- `pattern`: glob pattern for files to ingest within that directory.
- `glob`: optional glob matched against each changed path relative to `path`.
  When omitted, existing `pattern`-only behavior is unchanged.

## 4. Query peers across machines

The owner keeps its SQLite database and tape files. The caller never opens those files. Instead, it uses SSH to start the owner's Engram binary and sends requests over standard input and output. The temporary `peer-serve --stdio` process reads only the exports that the owner declares in `~/.engram/topology.yml`. It exits when the caller closes the SSH stream or when the owner's idle or session limit expires. Engram does not copy an index or run a central service.

Put this file on an owner to export only its default store:

```yaml
# /home/alex/.engram/topology.yml on build-host
version: 1
self: build-host
exports:
  default:
    db: /home/alex/.engram/index.sqlite
    tape_dirs:
      - /home/alex/.engram/tapes
```

Put this peer entry in the caller's `/home/sam/.engram/topology.yml`:

```yaml
version: 1
self: laptop-a
peers:
  build-host:
    ssh: sam@build-host
    engram: /usr/local/bin/engram
    exports: [default]
```

Set `ssh` to a host or alias accepted by your SSH configuration. Set `engram` to the absolute path of the owner's binary. The owner exposes only stores listed under `exports`. This example exposes only `default`.

A peer name in topology does not select that peer for a query. Agents and scripts choose peers for each request:

```bash
engram explain src/auth.rs:40-55 --peers build-host
engram grep "token refresh" --peers build-host
engram show TAPE_ID --peers build-host
engram show TAPE_ID --store build-host/default
engram peek SESSION_ID --store build-host/default
```

Omit `--peers` and `--store` to query the local stores. The CLI does not add configured peers automatically. Pass only the peer names needed for the current task. Use `--store` on `show` or `peek` when you know which single remote export holds the tape.

Check the peer handshake with:

```bash
engram topology status --peers build-host
```

The command reports whether each selected peer answered and whether its protocol, schema, and query semantics match. Without `--peers`, it checks every configured peer. It does not tell you whether a watcher has indexed the newest transcript. Add `--check-exports` to count indexed tapes in this machine's declared exports that have no file in their tape directories; this also checks every configured peer unless you select peers explicitly.

When a selected peer fails, `explain`, `grep`, and `show --peers` keep results from sources that completed and report `federation.coverage: "partial"`. The matching entry in `federation.sources` names the machine and store, the failed phase, and a typed error code such as `timeout` or `incompatible`. For example, a connection timeout appears with phase `open`. The entry also carries the observed error message. Use the phase and typed code as the concise reason; the free-form message comes from the peer and may include local details. Add `--require-complete` to federated `explain`, `grep`, or `show --peers` when a failed source or incomplete conclusion should make the command exit nonzero. `show --store` and `peek --store` read one selected export and report a read failure as an error.

Here is a two-sided handoff. An agent on `laptop-a` sends a task to an agent on `build-host` and records this marker in the dispatch arguments:

```text
<engram-src id="8dfcc36a-75a0-4d18-9e50-8da567e0a44d"/>
```

The delivered prompt and receiving transcript on `build-host` carry the same marker. The receiving agent edits `src/auth.rs` on `build-host`. After the edit is available in the querying checkout and each machine has run `engram ingest`, query the edited span from `laptop-a`:

```bash
engram explain src/auth.rs:40-55 --peers build-host
```

The result can show the edit at `build-host/default` and follow the received marker back to the sender in `laptop-a`'s local store. Engram reports the hop only when it finds the receiver's `received` marker and one independent sender's `sent` marker in the selected stores. If it cannot find both occurrences, it does not invent a handoff. If a selected peer fails, the source coverage is partial.

`show` reads the selected tape's bytes from its owner and verifies the digest on the caller. `explain` and `grep` request derived facts. `peek` requests selected lines. These commands do not copy the remote database or tape collection.

## 5. How you install it

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

## 6. How you link multi-step work together

The sending integration must record a dispatch marker, and the receiving transcript must retain the same marker:

```text
<engram-src id="f47ac10b-58cc-4372-a567-0e02b2c3d479"/>
```

The handoff works like this:
1. One conversation sends work with marker `X`.
2. A later conversation receives marker `X` and edits code.
3. Another follow-up continues with marker `X`.
4. `engram explain` on touched code follows dispatch links upstream and returns the causal chain.

Engram indexes and follows markers that appear in transcripts. The sending integration must add the marker; Engram does not create the handoff.

## 7. Regression testing

Run the regression suite for explain anchors, performance, config lookup, and additional-store window resolution:

```bash
cargo test --test regression_suite
```

## 8. Project specifications

- Core event contract: `specs/core/event-contract.md`
- Dispatch marker: `specs/core/dispatch-marker.md`
- Adapter contracts: `specs/adapters/*.md`


## License

Apache License 2.0. See `LICENSE` for the full text.

# Multi-Machine Queries

Engram is local-first. Each machine keeps its SQLite index and tape files on
that machine. Without `--peers` or a remote `--store`, queries use local stores
and work offline. There is no central index or persistent listening service.
For a cross-machine query, SSH starts a temporary `peer-serve --stdio` process
on each selected owner. The owner computes the requested facts from its own
default store; the caller does not copy the database or tape collection.

## Configure an owner

On the owner, create `~/.engram/topology.yml` and export only the intended
default store and its tape directory:

```yaml
version: 1
self: build-host
exports:
  default:
    db: /home/alex/.engram/index.sqlite
    tape_dirs:
      - /home/alex/.engram/tapes
```

The export list scopes the peer protocol. It is not a shell access boundary:
anyone who can use the owner account interactively can read any files that
account can access. Use a dedicated key and a forced command when that key
should serve queries only. In the owner's `~/.ssh/authorized_keys`, add the
public key as one line:

```text
command="/usr/local/bin/engram peer-serve --stdio",restrict ssh-ed25519 AAAA... engram-query
```

Use the owner's absolute binary path. The OpenSSH `restrict` option disables
the forwarding and TTY features it controls; verify support on the owner's SSH
server before using it. Engram uses SSH's normal authentication and host-key
checks.

## Configure the caller

On the caller, add the owner and default export to its home-only
`~/.engram/topology.yml`:

```yaml
version: 1
self: laptop-a
peers:
  build-host:
    ssh: alex@build-host
    engram: /usr/local/bin/engram
    exports: [default]
```

`ssh` can name an SSH config alias. `engram` is the absolute path to the
owner's binary. A topology entry makes the peer available but does not include
it in each query; the calling agent selects peers explicitly.

## Query and inspect scope

```bash
engram topology status --peers build-host
engram explain src/auth.rs:40-55 --peers build-host
engram grep "token refresh" --peers build-host
engram show TAPE_ID --store build-host/default
engram peek SESSION_ID --store build-host/default
```

Use only the peer set needed to answer the question. `topology status` checks
connectivity and protocol, schema, and query-semantics compatibility. It does
not tell you whether the owner collector has ingested its newest transcript.
`--check-exports` reports indexed tapes that have no file in the declared tape
directories.

When a selected peer fails, `explain`, `grep`, and `show --peers` retain
completed results and mark source coverage `partial`. The source entry names
the machine and export, phase, typed error, and observed reason. Report only
what the connection exposed; a timeout does not establish whether a host is
asleep, a route is broken, or authentication failed. Use `--require-complete`
with `explain`, `grep`, or `show --peers` when a failed source or incomplete
conclusion should make the command exit nonzero.

## Follow a handoff across machines

The sending agent includes one fresh marker in the dispatch arguments. The
receiving transcript records the same marker as received text. Both owners
ingest their own transcripts. The caller then queries the edited span with the
owner selected:

```text
<engram-src id="8dfcc36a-75a0-4d18-9e50-8da567e0a44d"/>
```

```bash
engram explain src/auth.rs:40-55 --peers build-host
```

The querying checkout must contain the edited source span. The result can
locate the edit at `build-host/default` and link it to the sender only when the
receiver's earlier marker and one independent sender's marker are both in the
selected stores. A missing endpoint or ambiguous sender produces no invented
handoff. If a selected peer is unavailable, completed evidence remains visible
with partial coverage and the observed failure reason.

`show` reads a selected whole tape from its owner and verifies the digest on
the caller. `peek` returns the requested lines. Other peer operations ask the
owner to compute summaries, matches, or lineage; they do not send the database
or full tape set to the caller.

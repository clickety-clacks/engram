# T1772 R29 fresh proof runner

**Measurement correction in progress; not frozen or review-ready.** The recovered
historical implementation is `8820423184fe646e420c7a1613a6bf65bc13fd61`. The
correction retains that ancestry and the original proof/input/oracle bindings.
The frozen performance manifest pins baseline binary SHA-256
`13088f949fa7920615ff8873c1040d4b7ec9180976a7121a63e0de726e47571d` at source
`72821518037a9d896f0b4d784fee146800902e78`. That source has no statement-counter
output. This correction must not replace it with a rebuilt binary or report
replayed SQL counters as observations of the timed CLI. The PDO must resolve
that measurement dependency and supply exact frozen baseline database/binary
custody before a launch package can be completed. Missing counters fail closed.

This source is the fresh R29 proof implementation. It is descended from Engram
candidate `9f8afc65d0b365444446a473c04389adf80bd4b3`; it does not call, wrap, copy,
or root-rebind any R4-3 or R18-R27 proof executable.

The controller owns the trust boundary. Before it creates the proof root it
requires the exact reviewed input root and manifest, verifies the manifest's
SHA-256 and all eleven named files, checks the clean source revision and full
executable hashes, observes the immutable inputs and live paths read-only, and
checks capacity. The runner cannot start outside the controller: it enters a
post-exec stopped state and requires the controller to observe its process and
Darwin `csops(CS_OPS_STATUS)` result before sending `SIGCONT` number 19.

The runner writes only below the exact proof root. It builds two independent
schema-v4 indexes from the frozen ordered tape manifest; checks global and
per-tape cardinalities, the 14,369-row dispatch oracle, fixed direct-touch
queries, query plan, database bytes, and a 60-second/100-transaction concurrency
gate; and freezes its outputs. The controller then repeats the complete input
custody check, records any independently changing live index/WAL/SHM/cursor
state, freezes the full output manifest, and emits a local review-eligibility
record. Neither executable calls Tightbeam, publishes readiness, wakes Eezo,
installs Engram, or targets the live index, tapes, WAL/SHM, cursor, or raw source
for write.

All new JSON and JSONL evidence follows one byte contract:

> Recursively sort object keys by UTF-8 bytes; preserve array order and JSON
> scalar types; use compact UTF-8 JSON; append exactly one LF to every document
> or record; hash the exact emitted bytes.

## Reproducible build

Build from a clean checkout at the reviewed source revision:

```sh
PATH="$HOME/.cargo/bin:$PATH" scripts/build-t1772-r29.sh
```

The script pins `SOURCE_DATE_EPOCH`, locale, timezone, the embedded source
revision, locked Cargo dependencies, release mode, and the three exact binary
targets. It prints complete SHA-256 values plus compiler, Cargo, source, and
source-delta provenance. The frozen review package retains that output; prefixes
are invalid.

## Exact Eezo invocation shape

The review package replaces every `FULL_*_SHA256` token with the corresponding
64-hex build or retained input identity and replaces `SOURCE_REVISION` with the
reviewed 40-hex commit. No path or other argument is variable:

```sh
/Users/mike/.tightbeam/work/d009fb9f2357/engram-t1772-p0/target/release/t1772_p0_controller \
  --source-root /Users/mike/.tightbeam/work/d009fb9f2357/engram-t1772-p0 \
  --source-revision SOURCE_REVISION \
  --input-root /Users/mike/shared-workspace/engram/proofs/t1772/inputs \
  --proof-root /Users/mike/shared-workspace/engram/proofs/t1772/final-staging-asg-b260c05f-r27 \
  --tape-root /Users/mike/.engram/tapes \
  --live-index /Users/mike/.engram/index.sqlite \
  --cursor-root /Users/mike/.engram/cursors \
  --manifest /Users/mike/shared-workspace/engram/proofs/t1772/inputs/t1772-inputs-r27-9f8afc65d0b365444446a473c04389adf80bd4b3.sha256 \
  --runner /Users/mike/.tightbeam/work/d009fb9f2357/engram-t1772-p0/target/release/t1772_p0_runner \
  --runner-sha256 FULL_RUNNER_SHA256 \
  --controller-sha256 FULL_CONTROLLER_SHA256 \
  --candidate-binary /Users/mike/.tightbeam/work/d009fb9f2357/engram-t1772-p0/target/release/engram \
  --candidate-binary-sha256 FULL_CANDIDATE_SHA256
```

The corrected controller additionally requires `--baseline-binary`,
`--baseline-database`, and `--baseline-database-sha256`. Their approved paths and
snapshot SHA remain a PDO custody dependency; the invocation above is therefore
historical shape only, not a complete corrected launch invocation. The baseline
binary hash is fixed by the manifest, the snapshot must have exactly
120,001,798,144 bytes, and neither may change across execution. They are explicit
additional custody inputs, not changes to the eleven-entry manifest. No live
index can be used as the baseline input.

## Measurement correction

`proof/performance.rs` schedules all twelve frozen queries for both ordinary CLI
binaries. Each hot/filesystem-cold class has three fresh-process warmups and
thirty measured fresh-process iterations per binary/query. Order alternates by
query and iteration. Hot runs share a staged immutable database; filesystem-cold
runs copy the immutable database before every process, exclude copy time, hash it
after measurement, then remove only that iteration's copy. No OS cache purge is
claimed. Raw stdout/stderr, argv/environment, exit status, wall time, Darwin
`/usr/bin/time -l` maximum RSS, dedicated SQLite-temp directory samples and actual
per-statement SORT/AUTOINDEX/row/VM counters are retained. Counters are collected
inside the candidate's executed statements with an opt-in SQLite trace hook;
they are not a SQL replay. The hook is inactive in ordinary use. It does not
provide the absent historical-baseline instrumentation. Percentiles use nearest
rank with raw observations retained; candidate p95/p99, peak RSS and zero-temp/
SORT/AUTOINDEX gates are checked, not merely printed.

`proof/concurrency.rs` makes two independent disposable v4 copies. Before each
pass it removes the first hundred frozen tape IDs and their derived records from
the copy, then reingests those exact real tapes with the normal one-tape ingest
API and writer WAL/FULL settings. No synthetic probe table or synthetic tape ID
is used. A frozen-manifest query must exercise at least two populated posting
lookups and return multiple provenance rows. Reader A uses `open_reader` and
retains a transaction through writer completion and at least sixty seconds;
the second pass uses short read transactions in a loop. Writes have a 600ms
fixed schedule with actual start times retained; a third connection checkpoints
passively on a 1s fixed schedule during each pass, then once after reader exit.
The raw commit/reader/checkpoint observations, exact primary/extended errors,
application retry counts (zero, with SQLite busy timeout explicitly zero),
commit latency percentiles, maximum observed WAL bytes, stable retained results,
new-reader visibility and final checkpoint completion are recorded. Transaction
latency includes indexing and COMMIT but excludes reading/parsing source tapes.

`proof/measurement.rs` samples all staged non-directory file sizes and allocated
blocks from proof-root creation through operation and post-custody capture.
The requested interval is 100ms; every sample, maximum actual gap, vanished-path
count, and observed logical/allocated high-water mark is retained. Directory
metadata is excluded and hard links are counted once. This is explicitly a
sampled maximum, not a claim to see allocations that disappear between samples.
The controller joins the sampler before hashing outputs or writing eligibility;
errors forbid eligibility. Per-query SQLite temp sampling uses its own otherwise
empty subtree; harness output is outside it. It cannot observe unlinked temp
files, and that limitation must be assessed along with actual statement counters
before claiming the zero-temp gate is proven on the target host.

The new measurement test definitions are source only until executed on an
authorized non-Gibson host. A Gibson compilation proves type/link consistency,
not performance, concurrency correctness, macOS behavior, or proof completion.

## Static verification definitions

`tests/t1772_proof_contract.rs` pins the reviewed roots, candidate, manifest,
14,369 oracle, canonical bytes, path confinement, and Darwin signal number.
The runtime writes `runner/test-definitions.json` with the larger P0 checks and
records every observed result in hash-bound canonical evidence. Repository gates
remain `cargo fmt --check`, `cargo test --all-targets`, and the release build;
they run only on an authorized non-Gibson test host.

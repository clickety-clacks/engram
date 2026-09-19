# T1772 R29 fresh proof runner

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

## Static verification definitions

`tests/t1772_proof_contract.rs` pins the reviewed roots, candidate, manifest,
14,369 oracle, canonical bytes, path confinement, and Darwin signal number.
The runtime writes `runner/test-definitions.json` with the larger P0 checks and
records every observed result in hash-bound canonical evidence. Repository gates
remain `cargo fmt --check`, `cargo test --all-targets`, and the release build;
they run only on an authorized non-Gibson test host.

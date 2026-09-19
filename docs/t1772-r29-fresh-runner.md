# T1772 R29 fresh proof runner

**Measurement correction in progress; not frozen or review-ready.** The recovered
historical implementation is `8820423184fe646e420c7a1613a6bf65bc13fd61`. The
correction retains that ancestry and the original proof/input/oracle bindings.
The frozen performance manifest pins baseline binary SHA-256
`13088f949fa7920615ff8873c1040d4b7ec9180976a7121a63e0de726e47571d` at source
`72821518037a9d896f0b4d784fee146800902e78`. That source has no statement-counter
output. PDO has approved separate in-process SQL probes, explicitly excluded
from product wall timing; the frozen baseline binary remains unchanged.
The exact frozen baseline executable path/hash is now bound by receipt
art_ebfcc161, SHA-256 `4178f5a1d9d3cfdda476708a3548985bdf850aefcb053715dcb04963784aa329`.
The baseline snapshot and complete invocation remain pending PO asg_636cc02d;
no snapshot substitution or rebuild is authorized. Accepted PO amendment art_a24618f6, SHA-256
`3ce0623b7ebb575962be92bf1f9f3e94b2195682a4c8f8f5e96e88d24e95c85e`, now permits
the unchanged baseline to write `query_results` only in disposable staging
copies. Immutable masters and candidate read-only behavior remain protected.
Telemetry amendment art_73471b68, SHA-256
`376d581e12743643c5e5e4923355a530964e485d16437bd47298fb8ee23e9035`, applies with
that predecessor. Actual baseline CLI counters are explicitly unavailable /
non-comparable; separate baseline probes remain diagnostic. Candidate per-statement
direct-touch probes must cover each bound anchor with SORT=0 and AUTOINDEX=0,
and candidate temp observations must be present and zero, including warmups.
Acceptance is limited to observed named temporary files: unlinked/between-sample
allocations may be missed, and zero total allocation is never claimed.
The superseded telemetry hard failure is removed. Exact baseline input/executable
custody and bound invocation remain missing; this source build is not review-ready.

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

For §9.2 the controller creates fresh `manifests/live-clones-pre` and
`manifests/live-clones-post` staging directories at the two manifest boundaries.
Input and immutable-custody checks still precede proof-root creation; pre-clones
are captured after the root exists and before runner staging reads. Each existing
live index/WAL/SHM is opened read-only without following a final symlink, checked
to be on the same APFS filesystem as staging, and cloned with `fclonefileat`.
All three capture attempts precede any clone hashing. The source descriptor pins
its inode across pathname rotation. Hashes come only from retained read-only
staging clones; unsupported filesystems, failed clones and non-Darwin execution
fail without byte-copy or live-hash fallback. No SQLite connection/checkpoint is
opened on these paths.

Manifest rows retain source metadata before/after, source pathname state after
capture, per-file capture times, clone metadata/path and clone hash. Missing
files are recorded explicitly. These are per-file boundary observations, not a
multi-file atomic SQLite snapshot. Differences compare source metadata and
captured hashes, excluding clone destinations and observation clocks; complete
rows remain retained. Clones remain under the proof root through the final
manifest and are included by the existing staging sampler. APFS shared blocks
are not deduplicated by that sampler's named-file allocated-byte sum.

The API basis is Apple's [clonefile(2) manual](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/man/man2/clonefile.2);
the pinned libc 0.2.182 Darwin declarations were inspected. Gibson's Linux build
does not compile or execute the Darwin-only syscall branch; target validation
remains required on an authorized host.

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
  --candidate-binary-sha256 FULL_CANDIDATE_SHA256 \
  --baseline-binary /Users/mike/src/worktrees/engram-t1772-index-repair/scratch/t1772/engram-baseline-7282151
```

The baseline executable path above is the only verified pinned binary, with
5,120,032 bytes and SHA-256
`13088f949fa7920615ff8873c1040d4b7ec9180976a7121a63e0de726e47571d`.
Controller preflight (before proof-root creation) and measurement entry reject
any other path; full hash verification remains mandatory. Protocol evidence binds
the exact path, hash and receipt identity. This is receipt consumption, not a new
Eezo check or execution.

The controller still requires `--baseline-database` and
`--baseline-database-sha256`. Their exact custody and the complete invocation
remain pending PO asg_636cc02d and PDO disposition; the command above is therefore
incomplete and must not be launched. The snapshot must have exactly
120,001,798,144 bytes, and neither may change across execution. They are explicit
additional custody inputs, not changes to the eleven-entry manifest. No live
index, differently sized retained database or unapproved rebuilt snapshot can be
substituted. Source/build evidence does not make this package frozen or review-ready.

## Measurement correction

`proof/performance.rs` schedules all twelve frozen queries for both ordinary CLI
binaries. Each hot/filesystem-cold class has three fresh-process warmups and
thirty measured fresh-process iterations per binary/query. Order alternates by
query and iteration. Each hot query series has its own baseline working copy,
shared through that series' three warmups and thirty measured processes. Cold
slots receive fresh initially byte-identical copies. Only baseline copies are
writable. `baseline_custody.rs` checks existing schema-v3 objects/columns, records
schema and typed row digests for the seven fixed v3 tables, and rejects any
schema/content change outside `query_results`. Pre/post file, WAL/SHM and
directory custody is retained; unexpected entries fail. Whole-CLI timing includes
baseline initialization and feedback writes. Preparation and custody reads are
timed separately; they can warm the filesystem cache. No OS purge is claimed.
Candidate database and containing directory are read-only, with exact file and
directory custody after every product/probe slot. Completed disposable copies
created by this run are removed only after custody is saved; failed copies remain.
`protocol-r3.json` identifies the original manifest and both accepted amendment hashes
without rewriting either. Raw stdout/stderr, argv/environment, exit status, wall time, Darwin
`/usr/bin/time -l` maximum RSS and dedicated SQLite-temp directory samples are
retained. Parsed product output is also saved under the canonical JSON/LF
contract. Candidate literal `explain` with `T1772_DIRECT_TOUCH_PROJECTION=1`
adds `t1772_direct_touches` from that invocation's already-computed direct rows,
canonically ordered by timestamp/tape/event/kind/path, without discarding any
duplicates or intersecting with the oracle. This single evidence field is the
only new product observation seam; its serialization cost stays inside timing.
Every warmup and measured candidate response must match the complete frozen
projection exactly; discrepancy evidence retains actual and expected arrays.
The pinned baseline is unchanged and gets this environment value set to zero.
Its raw output/result metadata remains retained. Its merged/truncated session
output cannot be honestly classified as a complete direct-touch projection;
each slot reports that unavailable projection and retains the historical output
without claiming direct-touch equality. This limitation no longer independently
fails the amended gate under PDO's current direction.

After the product timer and temp sampler stop, `proof/statement_probe.rs` opens
the same clone read-only and executes new instrumented SQL probes for direct
evidence, candidate posting-seed expansion, and bounded provenance traversal.
Each statement records SQL, bound anchor, SORT, AUTOINDEX, full-scan/VM steps,
and rows returned before Rust filtering/deduplication. Candidate feature-join
rows are additionally identified as posting rows; these counts do not claim
unobservable SQLite internal page/index visits. Presentation, tape reads and
baseline feedback writes are excluded. Query/cache class/phase/iteration/order,
database path and source hash, manifest hash, target, flags, derived anchors,
product binary hash and probe binary hash form a shared binding on the product
and probe records. CLI output anchors must equal the bound probe anchors.
Every summary labels `counter_scope` as `instrumented SQL probe excluded from
product CLI wall timing`. A filesystem-cold slot's probe follows its product
read and makes no independent cold-latency claim. Probe records and canonical
CLI output have separate hashes. Percentiles use nearest rank with raw
observations retained; candidate p95/p99, peak RSS and zero observed-temp/direct
probe SORT/AUTOINDEX thresholds must pass the amended performance gate. Every
candidate probe slot checks raw statement coverage and zero counters before
returning a successful observation. Missing, failed or nonzero observations fail;
no missing value becomes zero. Actual CLI counters are explicit unavailable /
non-comparable objects with null numeric fields in observations and summaries.
No comparative CLI counter improvement is claimed. Temp observations/summaries
retain collector identity, dedicated paths, requested interval, actual gaps,
raw samples/errors and incomplete-coverage limitations. Probe counters are never
attributed to the frozen CLI. Product/probe bindings include both source revisions
and binary hashes. Threshold failure still returns an error; only the superseded
unavailability failure is removed.

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
files or allocations entirely between samples. The telemetry amendment permits
only a qualified zero-observed claim; even zero probe counters do not close those
blind spots. Collector errors remain missing evidence and fail the gate.

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

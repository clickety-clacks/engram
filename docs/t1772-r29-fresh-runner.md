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
The original snapshot is unavailable in inspected custody. Accepted reconstruction
amendment art_1ee9a1bc / att_6310f382, SHA-256
`8c7223747f618b9253b6dd956309a2e49caf2f2ec6ba0169ca17dda4a362a63b`, permits a
separately labeled reconstructed comparator as an alternative identity. Actual
comparator custody and target invocation are still absent; the amendment does not
authorize reconstruction or execution. Accepted PO amendment art_a24618f6, SHA-256
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
database custody and bound invocation remain missing; this source build is not review-ready.

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
  --baseline-binary /Users/mike/src/worktrees/engram-t1772-index-repair/scratch/t1772/engram-baseline-7282151 \
  --baseline-database /Users/mike/shared-workspace/engram/proofs/t1772/reconstructed-comparator-asg-931d2f4b-r1/master/index.sqlite \
  --baseline-database-sha256 FULL_COMPARATOR_SHA256 \
  --comparator-receipt /Users/mike/shared-workspace/engram/proofs/t1772/reconstructed-comparator-asg-931d2f4b-r1/receipt.json \
  --comparator-receipt-sha256 FULL_COMPARATOR_RECEIPT_SHA256
```

The baseline executable path above is the only verified pinned binary, with
5,120,032 bytes and SHA-256
`13088f949fa7920615ff8873c1040d4b7ec9180976a7121a63e0de726e47571d`.
Controller preflight (before proof-root creation) and measurement entry reject
any other path; full hash verification remains mandatory. Protocol evidence binds
the exact path, hash and receipt identity. This is receipt consumption, not a new
Eezo check or execution.

The comparator paths are prospective staging locations, not observed artifacts.
The command remains unbound until actual target executable hashes, comparator
hash and receipt hash replace the explicit placeholders. It must not be launched
under current holds. These are additional custody inputs; the eleven-entry input
manifest remains unchanged. The original snapshot has not been recovered, and
bounded searches do not establish universal loss.

### Reconstructed comparator receipt contract

`baseline_custody::verify_comparator` consumes evidence only; it never rebuilds a
comparator or executes the baseline. It runs before proof-root creation. The
master must have its own exact actual bytes/hash, be at the canonical path above,
have a read-only file and containing directory, and have no WAL/SHM/journal.
Existing schema-v3 inspection verifies schema and typed, rowid-ordered digests
for all seven tables. Registered tape IDs must exactly equal the frozen corpus.
The receipt and its supporting artifacts are retained under the comparator root
and covered by the controller's full immutable pre/post custody observations.

The receipt uses the common canonical JSON/LF bytes and these required fields:

* `identity`: `reconstructed-pinned-baseline-v1`; `amendment_sha256`: the accepted
  reconstruction hash; `binary_path`, `binary_sha256`, `source_revision`: the
  pinned baseline identities above; `tape_manifest_sha256`: unchanged
  `29800d70e6812b446581c1f505ddd1b78c25effcd76a8c467c892553913f5757`.
* `database_path`, `database_sha256`, `database_bytes`: actual master custody;
  `snapshot_state`: `checkpointed-closed-master-no-sidecars`; `accounting`:
  `schema`, `user_version`, `tables` in the existing `snapshot` representation.
* `historical_snapshot_status`: `unavailable in inspected custody`;
  `historical_tool_message_postings_replay_delta`: 87711;
  `unexplained_corpus_or_semantic_differences`: empty array;
  `size_shaping_performed`: false. A hash-bound nonempty `provenance` artifact
  (`path`, `sha256`) must attribute the reconstruction and explain actual
  differences, including the T1771 replay policy delta. Semantic adequacy of that
  explanation remains an independent inspection requirement, not a parsed fact.
* `build_transcript`: a `path`/`sha256` reference to canonical JSONL below the
  comparator root, exactly 45,758 rows in frozen manifest order. Each row binds
  zero-based `ordinal`, `tape_id`, `source_path`, `source_blob_sha256`,
  `visible_tape_ids: [tape_id]`, exact `argv: [BASELINE_BINARY_PATH, "fingerprint"]`,
  canonical staging `cwd`, `environment_cleared: true`, complete string-valued
  `environment`, hash-bound `config: {path, sha256}`, `exit_code: 0`, parsed
  `stdout` and string `stderr`. The parsed baseline stdout must report `ok`, one
  scanned/fingerprinted tape, zero skipped/failures and an empty failures array.

The pinned `fingerprint` CLI enumerates `cwd/.engram/tapes` without sorting, so
each recorded invocation must expose exactly one tape in that staging directory,
in manifest order, retaining one staging database across invocations. This is a
receipt contract for later authorized construction, not construction tooling.
Raw transcript capture/attribution and the effective config/environment must be
independently inspected before execution permission. The verifier checks every
compressed source blob hash and normalized content SHA against its tape ID,
without re-normalization. It hashes and reads artifacts; it does not replay logs
or infer that a declaration alone proves how a process ran.

The candidate still must be smaller than the **historical ceiling of
120,001,798,144 bytes**. No comparator equality with that number is required.
The summary reports actual comparator bytes minus candidate bytes (signed), and
permits a comparator savings claim only for a positive result. It never claims
historical layout/latency identity or live reclamation. Each performance record
binds receipt and amendment hashes and labels the comparison `reconstructed
pinned-baseline versus candidate`; baseline/candidate slot names remain stable.

## Measurement correction

`proof/performance.rs` schedules all twelve frozen queries for both ordinary CLI
binaries. Each hot/filesystem-cold class has three fresh-process warmups and
thirty measured fresh-process iterations per binary/query. Order alternates by
query and iteration. Each hot query series has its own baseline working copy,
shared through that series' three warmups and thirty measured processes. Cold
slots receive fresh initially identical copies of their identified master. Accepted
cold-baseline amendment art_76f62142 / att_2fd6729a, SHA-256
`afeac1430bdbbade5c46e27eff440972d61176fc920cd38ad84b5a5570d4cf9e`, changes only
cold baseline evidence. It is composed with the three prior amendments.

Hot baseline clones retain complete pre/post file hashes and post-series
schema/seven-table checks against the controller-verified master. Full initial
hash equality supports inheritance of that verified master's initial logical
state; intermediate mutable hot-slot hashes remain unobserved/null.

Each cold baseline clone is created immediately before its assigned warmup or
measured CLI. An actual APFS `fclonefileat` result is recorded from a read-only
`O_NOFOLLOW` master descriptor. Master file/directory must be protected, source
sidecars absent, source descriptor/path metadata stable across cloning, and both
filesystems APFS. The new destination must be a distinct regular-file inode on
that device, with one link and the master's size. There is no hard-link, stale-copy
or byte-copy fallback. Failure/missing receipt fails the gate. Raw clone results,
source/destination metadata, descriptor identity, elapsed time and the master
receipt/hash are bound to the precise query/cache/phase/iteration/order and
product/configuration identity. The clone receipt's configuration expectation
links to the actual config file/hash retained in the completed process record.

**Cold-copy initial identity is clone-derived; no independent per-copy full-file
hash. Complete post-mutation byte identity and unchanged provenance tables/schema
were not independently measured per copy.** Cold measured-copy hash and logical-
validation fields are null; a master hash is never substituted. Every relevant
protocol/binding/observation/summary and controller eligibility record carries the
amendment and/or explicit limitation. A hot-series check does not validate a cold
copy's post-state.

After each cold invocation, retain metadata-only database/directory and explicit
WAL/SHM/journal presence/absence, raw stdout/stderr/exit status, configuration,
canonical output and projection evidence. Unexpected entries, file replacement,
mode/link/device changes, unexplained truncation, unsupported sidecar identity,
missing process/config binding or failed CLI remain failures. Other size/time and
SQLite sidecar effects are explained by the pinned write contract, with the lack
of exhaustive observation explicit. Post metadata is attempted even on product
failure; failed copies remain. Successful owned copies are released after their
records are saved, using the existing disposal authority and entry safety checks.

The cited write contract is source
`72821518037a9d896f0b4d784fee146800902e78`:
`src/query/format.rs::emit_query_result` (file SHA-256
`b127b370531f2d94af2d21e5937267d2d157af1491e3680979140977ec98fc48`) calls
`src/index/mod.rs::record_query_result` (file SHA-256
`ad18827f34e4f6e79ec75ca54e513a7afa379e74db79e3b000786de9b5afb0b9`), which checks
existing feedback schema and performs `INSERT OR REPLACE` into `query_results`.
Master preflight requires the existing full baseline-compatible schema; metrics
are disabled in the isolated CLI config. This is source-backed allowed behavior,
not observed proof that every cold copy preserved all application tables.

Candidate copies still get full database/directory custody before and after every
complete CLI/direct-probe slot, exact oracle projection every warmup/measurement,
and read-only permissions. Master hash checks remain at their required boundaries.
Whole-CLI timing still includes baseline initialization and feedback writes.
All clone preparation, metadata/hash/logical checks and validation stay outside
CLI wall timing; staging storage remains covered by the existing sampler.

The declared preparation order is now per-slot cold clone/metadata, CLI, then
metadata/output validation; no content pre-read is added for cold baseline copies.
Fresh CoW pages may share the master's cache. Candidate copy/hashes/direct probes
and hot boundary scans still condition caches. “Filesystem-cold” remains only the
manifest's fresh-copy fallback class; no OS-cache eviction, independently cold
page cache or stronger cold-state claim is made by omitting baseline hashing.
This changed preparation order and its asymmetric residual cache effects remain
explicit choices for independent technical review.

`protocol-r6.json` identifies the original manifest and all four accepted amendment hashes
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
each slot reports that unavailable projection and retains the reconstructed baseline output
without claiming direct-touch equality. This limitation no longer independently
fails the amended gate under PDO's current direction.

After the product timer and temp sampler stop, `proof/statement_probe.rs` runs
only candidate exact-anchor and feature-posting direct lookups, binding the same
query, flags, anchors, database/custody paths, cache/phase/iteration/order, and
product/probe identities. It retains raw per-statement SQL, SORT, AUTOINDEX,
rows delivered and posting rows, with complete coverage/zero-counter validation.
Feature-posting rows are counted once; the duplicate seed lookup, lineage BFS,
baseline diagnostics, VM/fullscan counters and probe-only full-file hash are gone.
These removals follow the required direct-touch scope of art_73471b68 and the
named §9.4 rows/postings evidence. No retained gate uses the removed lineage
traversal. Candidate rows/postings have p50/p95/p99 summaries. Baseline probe
metrics/hashes are unavailable/null, including summaries; they are never empty
sums reported as zero. Counters remain separate SQL-probe observations excluded
from CLI timing, never observations inside the frozen CLI.

`runner/performance/io-plan.json` uses actual B (comparator bytes) and C
(candidate bytes) to report the amended plan: 24B hot baseline working-file hash
bytes before growth/sidecars; 1,584C candidate slot hash bytes; zero cold baseline
full-file hashes and zero cold logical scans; 12 hot post seven-table passes plus
one independently verified master pass; 408 baseline CoW clones; and 408 candidate
byte copies (408C read and written each). Candidate and all master/corpus/output
custody costs remain. These savings are observer overhead, not CLI speedups.

Before this amendment the 792 cold full hashes alone cost 792B logical reads,
illustratively 95.041424130048 TB at the historical reference size. Actual B is
not inferred from that reference. The cold clones no longer need batch retention:
peak working databases return to one baseline plus one candidate (B+C logical
bytes before growth), in addition to both rebuilds, two concurrency copies,
masters, live clones and raw evidence. CoW sharing means named allocated-block
sums are not unique physical storage; source arithmetic is not measured device I/O.
The amendment accepts weaker cold baseline evidence; it does not certify this
implementation, observed conformance, timing comparability or readiness.

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

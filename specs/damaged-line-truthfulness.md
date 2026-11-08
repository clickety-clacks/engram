# Engram: read past damaged transcript lines and report the gap

Status: amended after review (art_4d939fcd, N1 to N5); for delta review
Work item: wi_4bec0282-ec7c-4136-8c9d-7cf047ed154b
Written against: release line v0.2.12 (tag v0.2.12, commit cd04aef)
Extends: `specs/incremental-ingest.md` (Partial Record Handling, Error Model)

Keywords MUST, MUST NOT, SHALL, SHALL NOT, SHOULD and MAY are used as in
RFC 2119 as clarified by RFC 8174. Requirements are written in EARS form.

## 1. Spirit

Intent relied on, from the work item body as revised at row version
1794172793017 and from the dispatch brief for this spec:

- S1. "Engram exists so that what was said can be found; hiding a gap is the
  defect." (dispatch brief)
- S2. "Minimum outcome: Engram names the files it has not fully indexed, local
  and peer status stop saying ok for them, and a peer answer shows the gap."
  (PO ruling, Nov 8 2026, spec release)
- S3. "Whether it reads past a damaged line, and how the live ... conversation
  catches up, is the spec's call." (same ruling)
- S4. "Nobody searching would know those messages are missing." (work item
  finding)
- S5. Limits from the brief: one-operator private setup, so no security or
  forgery hardening; no new native code or dependencies; no generalization
  beyond the damaged-line case; release content comes from the 0.2.12 line.

No separate Engram spirit document exists. The shared current Engram spec
says Engram "stores raw facts and transcript windows"; this spec relies on that
and on S1 to S5.

### The problem, as observed

On copies of 10 real transcript files, published 0.2.12 and 0.2.10 both stop
indexing at the first damaged line and never advance. Ingest reports
`status: ok`, `failure_count: 0`, and counts the file as `skipped_unchanged` on
every later pass. A phrase from before the stop is found. A phrase from after
the stop returns a bare `no_results` from both local and peer grep, with no
coverage block and no failure, so the peer answer shows no gap. 2,313,855,680
bytes and 23,759 messages are unsearchable. One
file is a live conversation, still being written, stuck since a damaged line
from Oct 11. (Evidence: art_20d82a7e, art_8f3b017f.)

Cause in v0.2.12: `complete_jsonl_prefix_len` in `src/ingest/mod.rs` ends the
"complete prefix" at the first newline-terminated line that does not parse as
JSON. That treats a damaged complete line the same as an unfinished tail, so
the cursor stays at the damaged line forever, and a zero-length complete prefix
is counted as unchanged rather than as a problem.

## 2. Decisions

- D1. Engram reads past a damaged line. It skips that one line, records it as a
  gap, and indexes everything after it. Serves S1 and S3: stopping hides every
  later message, and it is the stop that produced the 2.3 GB loss.
- D2. A recorded gap stays reported. A file with a gap is not fully indexed,
  and every status and federated answer that covers it says so and names the
  file. Serves S2 and S4.
- D3. Stuck files catch up without new writes: when the watcher starts, it
  ingests each known file whose cursor is behind the file's length. The live
  conversation also catches up on its next append. Serves S3; an upgrade that
  needs a manual step to stop hiding the gap would leave the defect in place
  for anyone who skips the step (S1).

## 3. Terms

- **Transcript input**: a file ingest reads as JSONL, meaning every candidate
  except `.json` files (the same split `complete_ingest_prefix_len` makes
  today).
- **Line**: a byte run ending in LF (0x0A), LF included.
- **Pending tail**: bytes after the last LF in the file.
- **Damaged line**: a line whose bytes before the LF are not empty or
  whitespace-only and do not parse as one JSON value under the rule ingest uses
  today to call a line complete. Examples: a record cut off mid-write and then
  ended by LF; a cut-off record with another record following on the same line;
  a line that starts with NUL bytes. Invalid UTF-8 inside a line also makes it
  damaged.
- **Blank line**: a line whose bytes before the LF are empty or only
  whitespace. v0.2.12 skips empty lines but stops at whitespace-only ones.
  Under this spec both are skipped without a gap (DL-1).
- **Gap**: the damaged lines Engram skipped in one source file.
- **Gap entry**: the per-file report of a gap. It has these fields:
  - `path`: absolute source path on the machine that owns the store.
  - `damaged_lines`: number of distinct damaged lines recorded for the file.
  - `damaged_bytes`: total bytes of those lines, LF included.
  - `first_damaged_offset`: byte offset of the earliest one.

  A damaged line is identified by (`path`, byte offset); recording the same one
  again does not change the entry.
- **Fully indexed**: a transcript input with no recorded gap and whose cursor
  is at the start of its pending tail.
- **Stuck file**: a transcript input whose cursor sits at the start of a
  damaged line. Only builds before this fix create one.

## 4. Requirements

Each requirement cites what it serves. "Fixture F" is defined in section 5.

### Ingest

**DL-1 Read past.** When ingest reads a damaged line in a transcript input,
ingest SHALL skip that line, record it in the file's gap, and continue with the
next line. When ingest reads a blank line, it SHALL skip it without recording
a gap. (D1, S1, S3)
Acceptance: ingest fixture F into an empty store. Phrases P1 (before the first
damage) and P3 (after the last damage) are both found by `engram grep`. The
cursor equals the offset of the pending tail. Fixture F's whitespace-only line
appears in no gap entry.

**DL-2 Gap before cursor.** Ingest SHALL persist a damaged line's gap record
(its tape and index row, DL-21) before it saves a cursor past that line. The
cursor never passes a damaged line that is not recorded. This is the order
v0.2.12 already uses (tape write, then index, then cursor save); a replay after
a crash between steps is harmless because DL-3 identifies each damaged line by
(`path`, offset). (S4; the "Atomic state update" invariant of
`incremental-ingest.md`)
Acceptance: make the tape write fail after the damaged lines are read (for
example, the tape directory is read-only). The cursor stays where it was and
no gap entry is recorded. Restore write access; the next ingest records the
gap entry with `damaged_lines` 3 and moves the cursor to the pending tail.

**DL-3 Exactly once.** When ingest runs again over a file with no new bytes,
or with appended undamaged lines, ingest SHALL NOT change that file's gap entry
and SHALL NOT index any line twice. (`incremental-ingest.md` idempotency)
Acceptance: ingest fixture F twice. The gap entry is unchanged and the
second run imports no tape. Append one valid record; the next run imports only
that record, and the gap entry is unchanged.

**DL-4 Gap-only appends.** When the bytes a pass reads contain damaged lines
and no indexable record, ingest SHALL write a tape carrying the gap record,
then move the cursor past them. In v0.2.12 a chunk with no non-meta events
saves the cursor without writing a tape; that path MUST NOT be taken when the
chunk has a damaged line. (S4)
Acceptance: ingest the undamaged prefix of fixture F, then append only damage
case C1. The next ingest reports the gap entry with `damaged_lines` 1.

**DL-5 Pending tail unchanged.** Ingest SHALL keep treating a pending tail that
does not parse as unfinished: no gap, cursor stays before it, and it is indexed
once an LF completes it. (`incremental-ingest.md` Partial Record Handling)
Acceptance: fixture F's pending tail holding P4 is not found and is not in the
gap. Append the rest of the record and an LF; the next ingest finds P4, and the
gap entry is unchanged.

**DL-6 Undamaged input unchanged.** When a transcript input has no damaged
line and no whitespace-only line, ingest SHALL produce the same tapes, tape ids, cursors and output fields
as v0.2.12. (S5, no generalization; `incremental-ingest.md` determinism)
Acceptance: ingest the repository's existing adapter fixtures with v0.2.12 and
with the fixed build in separate homes. The tape id sets are equal.

**DL-7 Ingest output names the file.** When an ingest run reads, or checks as
unchanged, a file that has a recorded gap, its JSON output SHALL have
`status: "partial"` and SHALL list that file's gap entry in a top-level `gaps`
array. Existing fields keep their meaning: `failure_count` and `failures`
still count only per-file errors. (S2; `incremental-ingest.md` says to add
output only additively)
Acceptance: the first and second ingest of fixture F both print
`"status":"partial"` and one `gaps` entry naming the fixture path, with
`damaged_lines` 3. An ingest of an undamaged file prints `"status":"ok"` and
`"gaps":[]` or no `gaps` field.

**DL-8 Watch log names the file.** When a watch-triggered ingest covers a
file with a recorded gap, the watch log SHALL NOT write `ingest ok` for it. It
SHALL write `ingest partial path=<path> damaged_lines=<n>`. (S2; the 0.2.12
watcher logged `ingest ok` for stuck files)
Acceptance: watch a directory and copy fixture F into it. The log line for it
reads `ingest partial path=<fixture path> damaged_lines=3`.

### Status

**DL-9 Local status.** When `engram topology status --check-exports` reports
a local export whose store records any gap, the export row SHALL have
`status: "partial"` and a `gaps` array of gap entries, and the top-level
`status` SHALL be `"partial"`. (S2)
Acceptance: with fixture F ingested into the store behind export `default`,
the `local_exports` row for it is `partial` with one gap entry naming the
fixture path; the top-level status is `partial`. With an undamaged store the
row stays `ok`.

**DL-10 Peer status.** When `engram topology status` reports a selected peer
export whose store records any gap, that export row SHALL have
`status: "partial"` and the same `gaps` array, the peer's machine row SHALL be
`partial`, and the top-level `status` SHALL be `"partial"`. (S2)
The owner's open response SHALL keep `"status":"ok"` for that export and add
a `gaps` array of its gap entries; the fixed searcher derives `partial` from a
non-empty `gaps` array. A v0.2.12 searcher accepts only `ok`,
`incompatible`, `unavailable` and `not_exported` there and fails the whole
peer with `protocol_error` on any other value (`src/access/client.rs`), which
would break DL-12 and DL-16. (S2, S5)
Acceptance: an owner home serves the fixture F store as an export; a searcher
home peers with it. On the fixed searcher, `topology status --peers <owner>`
shows the export row `partial` with the gap entry naming the fixture path as
the owner sees it. On a v0.2.12 searcher the same command still opens the
peer with no `protocol_error`.

### Federated answers

**DL-11 Peer grep shows the gap.** When a federated `engram grep` selects a
store that records any gap, the answer SHALL:

- mark that store's source `status: "partial"`;
- include, in that source's `failures`, at least one entry per gap file with
  `error.code: "damaged_line"` and the gap entry's `path`;
- set `federation.coverage` to `"partial"`.

This holds for every query, matching or not, and for the caller's own local
stores as well as peer stores. When the answer has no hits, Engram SHALL still
print the payload, and `--require-complete` SHALL reject it with
`incomplete_coverage`. (S2, S4; reuses the v0.2.12 over-limit failure pattern
in peer grep, where a skipped record keeps the search partial and the tape's
other hits still count)
Acceptance, with the owner/searcher pair of DL-10:

- grep P1 and grep P3 each return a hit, `coverage: partial`, and a
  `damaged_line` failure naming the fixture path.
- grep P2 (text inside damage case C1) returns `no_results` and still prints
  `coverage: partial` with that failure.
- grep P1 with `--require-complete` fails with `incomplete_coverage`.

**DL-12 Older searchers see it in grep.** When a v0.2.12 searcher runs a
federated grep against a fixed owner whose store records a gap, its answer
SHALL show `coverage: "partial"` and carry the `damaged_line` failure naming
the path. (S2: "a peer answer shows the gap"; a searcher may still run
v0.2.12 when its owner upgrades)
Acceptance: repeat the DL-11 P1 grep with the published v0.2.12 binary as the
searcher. Coverage is `partial`, and the source's `failures` holds the
`damaged_line` entry with the path.

**DL-13 Peer explain shows the gap.** When a federated `engram explain` from a
fixed searcher selects a store that records any gap, the answer SHALL mark that
source `partial` with a `damaged_line` failure naming each gap file, and set
`federation.coverage` to `"partial"`. (S2: "a peer answer shows the gap")
Acceptance: run a federated explain for any file range against the DL-10
owner. Coverage is `partial`, and the owner source carries the failure.

**DL-14 The gap is not a hit.** Grep SHALL NOT return a gap tape event
(DL-21) as a match. (S1: results stay what was said)
Acceptance: grep for the fixture's file name returns no hit that comes only
from the gap record.

### Durability

**DL-15 Survives index rebuild.** When the index is rebuilt from tapes by the
lifecycle in `docs/index-lifecycle.md` (`rebuild` re-ingests all tapes into a
staging index), the rebuilt index SHALL report the same gap entries. The index
is a disposable cache, so the gap must be recoverable from the tapes. (S4)
Acceptance: rebuild a staging index from the fixture F tapes. `topology status
--check-exports` on the staging config reports the same gap entry.

**DL-16 Mixed fleet keeps working.** The fix SHALL NOT change the index schema
version or make a v0.2.12 peer and a fixed peer unable to serve or query each
other. A v0.2.12 reader requires the exact schema version, so changing it would
make every older peer report a fixed owner's store as incompatible. (S1, S5)
Acceptance: a v0.2.12 searcher greps a fixed owner, and a fixed searcher greps
a v0.2.12 owner. Both return hits with no `incompatible` source.

**DL-21 Where gap state lives.** Ingest SHALL record each damaged line as a
tape event of a new kind carrying the source `path` and the line's byte offset
and length. Index ingest SHALL also store those in an additive index table.
A writer SHALL create that table if it is missing when it opens an existing
v4 index (v0.2.12 `open_writer` creates tables only for a new file), and
rebuild SHALL fill it from the tapes. The schema version stays 4. v0.2.12
readers ignore the table; a fixed reader treats a missing table as no gaps.
Ingest output (DL-7), status (DL-9, DL-10), the owner's open response and
explain (DL-13) SHALL read gap entries from that table. Peer grep MAY emit its
`damaged_line` failures from the tape scan it already does, as it does for
over-limit records. (DL-15: the tapes are the source of truth; DL-16; S2:
status must answer without rescanning every tape of a multi-gigabyte store)
Acceptance:

- A fixed writer opens an index built by v0.2.12. The table now exists and
  `PRAGMA user_version` is still 4.
- A fixed `topology status --check-exports` on a v0.2.12 index with no table
  reports `ok` with no error.
- A v0.2.12 `topology status --check-exports` on a fixed index holding fixture
  F reports `ok`, not `incompatible`.

### Catch-up

**DL-17 Stuck files resume on any ingest.** When ingest runs over a stuck
file, it SHALL apply DL-1 from the cursor: record the damaged line, then index
the rest of the file. No cursor reset or full re-ingest is needed. (S3)
Acceptance: ingest fixture F with v0.2.12 and confirm the cursor is at C1's
offset. Run the fixed build's `engram ingest <fixture path>` with the same
home. P3 is found, the gap entry has `damaged_lines` 3, and no tape from the
v0.2.12 run is re-imported.

**DL-18 The live conversation catches up on its next write.** When a stuck
file under a watch source is appended to, the watcher's ingest SHALL catch it up
per DL-17. (S3: "how the live conversation catches up")
Acceptance: with the v0.2.12 stuck state from DL-17 under a watch source, start
the fixed watcher and append one valid record. P3 and the new record are both
found.

**DL-19 Stuck files catch up when the watcher starts.** When `engram watch`
starts, it SHALL ingest every file under its configured sources that has a
cursor and whose length is greater than its cursor, without waiting for a file
event. Files with no cursor are outside this requirement and keep today's
behavior. (D3, S3: "how the live ... conversation catches up")
Acceptance: leave the v0.2.12 stuck state from DL-17 under a watch source with
no further writes. Start the fixed watcher. Within one debounce interval plus
ingest time, P3 is found and the log shows the DL-8 line for the fixture.

**DL-20 Release notes name the catch-up.** The release notes for the fix SHALL
say that stuck files catch up when the watcher restarts on the new build, and
that a stuck file outside every watch source catches up on the next
`engram ingest` that covers it. (S3)
Acceptance: the release notes contain both statements.

## 5. Fixture F

A codex-cli transcript in the shape the repository's existing codex fixtures
use, valid except for three damaged lines, holding five unique phrases:

| Part | Content |
|------|---------|
| Records | Session start and several message records; P1 in a message |
| C1 | A message record holding P2, cut off mid-string, then LF |
| Records | Valid message records |
| C2 | A record cut off mid-write, then a complete valid record on the same line, then LF |
| Records | Valid message records, one blank line holding only two spaces |
| C3 | Three NUL bytes, then a complete valid record, then LF |
| Records | Valid message records; P3 in the last one |
| Pending tail | The first half of a message record holding P4, no LF |

These three damage cases are the ones found in the 10 real files: 4 records
cut off where the line then ends, 4 cut off where the next record starts on
the same line, and 2 NUL-prefixed lines.

Expected after one fixed ingest:

- the gap entry is `damaged_lines` 3 with `damaged_bytes` equal to the summed
  lengths of C1, C2 and C3 (LF included);
- `first_damaged_offset` is C1's offset;
- the cursor is at the pending tail;
- P1 and P3 are found; P2 and P4 are not.

Tests run in isolated homes. Peer checks use two homes on one host through the
repository's existing peer test route.

Real-file check (operator evidence, kept out of the repository): on copies of
the 10 real files, the fixed build's ingest gives each file a gap entry. Each
cursor ends at its pending tail. The after-stop phrase chosen in art_20d82a7e is
found for every file. A federated grep from another home reports `partial`
naming all 10 paths.

## 6. Assumptions

- A1. Writers end every record with LF and never write a raw LF inside one, so
  a newline-terminated line is finished from the writer's side. If a writer
  wrote one record in two LF-separated parts, DL-1 would skip the first part as
  damage.
- A2. Installing the release restarts the watcher on each machine, so DL-19
  runs at upgrade. If a machine has no watcher, DL-20's ingest step covers it.
- A3. Catch-up of the largest stuck file fits in the watcher's ingest timeout.
  On v0.2.12, ingesting 1.27 GB of these files' prefixes took about 12 seconds.
- A4. The owner of a store must run the fixed build for its status and answers
  to report gaps. A v0.2.12 owner still stops and still says ok.
- A6. On the searcher side, a v0.2.12 searcher's `topology status` still says
  ok for a fixed owner with gaps, because it ignores the open response's
  `gaps` array (DL-10). Its grep answers do show the gap (DL-12). The searcher
  must run the fixed build for status to say partial.
- A5. Federated grep in v0.2.12 treats any `failure` record from an owner as
  making that source partial and keeps the record in `failures`
  (`cmd_grep_with_peer` in `src/main.rs`). DL-12 relies on that.

## 7. Non-goals

- Recovering a complete record from inside a damaged line (cases C2 and C3).
  The whole line is skipped and reported.
- Reporting a pending tail that never completes.
- Any change to `.json` inputs.
- Showing gaps in non-federated local `engram grep` or `engram explain`
  output, and in `show` or `peek`. Local status (DL-7 to DL-9) covers the
  owner's machine.
- Clearing or acknowledging a gap.
- A startup pass over files Engram has never ingested.
- Detecting stuck files before a fixed ingest has run over them.
- Any hardening, signing or tamper detection of gap records (S5).
- New native code or dependencies (S5).

## 8. Open questions

- Q1 (NON-BLOCKING). Under DL-10 and DL-11, every federated search of a store
  with a damaged file reports `partial` forever, and `--require-complete`
  always fails against it. This is the truthful reading of S1 and matches the
  over-limit precedent. The PO may want an acknowledge mechanism later; this
  spec does not add one.
- Q2 (NON-BLOCKING). Salvaging the complete record inside C2 and C3 lines
  would make 6 of the 10 real damaged lines lose nothing, at the cost of a
  heuristic for C2. Left out of this spec.
- Q3 (NON-BLOCKING). If a damaged source file is later repaired and fully
  re-ingested, the old gap stays reported (tapes are immutable). Not seen in
  the evidence.

No BLOCKING questions.

## 9. Release line

This contract is written against v0.2.12 and uses only behavior present there:

- the ingest loop and cursor model in `src/ingest/mod.rs`;
- the over-limit failure records in peer grep (`src/access/peer.rs`);
- federation `coverage` and `failures`;
- `topology status --check-exports`;
- `engram watch` and the index rebuild lifecycle.

Nothing in it depends on master-only changes. Master also carries the
over-limit change, plus ingest changes (task-context events, an owner-writer
open) that do not alter anything here. When the fix lands on both lines, the
behavior must be the same on both.

## 10. What this extends or supersedes

- `specs/incremental-ingest.md`, Partial Record Handling: the rule "ingest MUST
  only commit complete records" stands for the pending tail. A
  newline-terminated line that does not parse is no longer treated as an
  incomplete trailing record; it is a damaged line under this spec. The
  delivered change should add a pointer from that section to this spec so
  only one rule is live.
- Answers reuse the existing per-source `failures` and `coverage` fields and
  the over-limit failure-record pattern. This spec adds no second reporting
  channel.
- The current Engram spec's tape model list gains the gap event kind (DL-21),
  and the index gains its additive table without a schema version change.

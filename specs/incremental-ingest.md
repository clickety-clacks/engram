# Incremental Ingest for Append-Only Inputs

Status: Proposed
Owner: Engram core
Scope: `engram ingest` behavior + ingest cursor state

## Goal

Make ingest cost proportional to newly appended bytes for stable append-only
transcript files, instead of re-reading and re-hashing entire files on each run.

Target steady-state complexity for known append-only files:
- Current: O(file_size) per changed file
- Required: O(new_bytes) per changed file

## Problem

Today ingest re-reads full files for known inputs when they change. For large,
append-only transcript logs, this creates repeated O(file_size) work and slows
normal ingest cadence.

## Non-Goals

- No LLM interpretation.
- No change to provenance scoring/lineage/tombstone semantics.
- No global ingest sweep; ingest remains local-scoped.
- No behavioral change to `engram record`, `fingerprint`, or `explain`.

## CLI Surface

`engram ingest` gains optional path arguments:

```bash
engram ingest [paths...]
```

Semantics:
- No paths provided: current behavior (scan cwd recursively).
- Paths provided: only process those paths (files and/or directories), using the
  same transcript discovery rules already used by ingest.
- Duplicate resolved files are processed once.
- Paths outside local ingest scope are rejected with explicit per-path errors.
  (Local scope invariant remains intact.)

Primary use case: file watchers (`fswatch`, `watchman`) pass changed paths so
Engram avoids full directory discovery on every run.

## State Model Changes

Cursor state moves from a monolithic ingest-state file to per-file cursor files.

Per transcript cursor file path:

```
.engram/cursors/<sha256(absolute_transcript_path)>.json
```

Each cursor file contains only that transcript's state:
- `byte_cursor` (u64): last byte offset fully processed for this file.
- `cursor_guard`:
  - `offset` (u64)
  - `len` (u32)
  - `hash` (string)
- `adapter` (string)
- `tape_id` (string)

No monolithic `ingest-state.json` is used in this model.

Ingest IO behavior:
- For each transcript being processed, ingest reads only that transcript's
  cursor file (if present).
- Ingest writes only that transcript's cursor file on successful state advance.
- Ingest does not load/write cursor state for unrelated transcripts.

`cursor_guard` definition:
- Guard bytes are the last fully processed complete record ending at or before
  `byte_cursor`.
- Guard bytes MUST be derivable deterministically from the input file.
- Guard hash algorithm MUST be deterministic and fixed by implementation.

Why guard exists:
- Validate prefix integrity in O(guard_size), not O(byte_cursor), before delta
  ingest starts.

## Core Algorithm

For each candidate input file:

0. Resolve `abs_path`.
1. Compute `cursor_key = sha256(abs_path)`.
2. Resolve cursor file path at `.engram/cursors/<cursor_key>.json`.
3. Load that file if present; if absent, treat as new file.

### 1) New file (no prior state)
- Full ingest as today.
- Write cursor file with:
  - `byte_cursor` at last fully processed byte
  - `cursor_guard` from boundary record at cursor
  - `adapter`
  - `tape_id`

### 2) Known file

Given prior state `(byte_cursor, cursor_guard, adapter, tape_id)`:

1. Read file metadata.
2. If `file_len < byte_cursor`: fundamental change (truncate/rewrite) -> full
   re-ingest.
3. Validate prefix integrity before reading delta:
   - Read bytes at `cursor_guard.offset..offset+len`.
   - Hash and compare to `cursor_guard.hash`.
   - Mismatch => fundamental change -> full re-ingest.
4. If `file_len == byte_cursor` and guard matches: no-op for this file.
5. If `file_len > byte_cursor` and guard matches: append-only fast path.
   - Seek to `byte_cursor`.
   - Read only appended region.
   - Process complete newly available records.
   - Advance `byte_cursor` only to the last fully processed byte boundary.
   - Recompute and store new `cursor_guard` at new boundary.
   - Update `adapter`/`tape_id` as required by current ingest output.
   - Persist only this transcript's cursor file.

### 3) Fundamental change fallback

Fallback path MUST run full-file ingest and fully refresh state. Trigger on:
- truncate (`file_len < cursor`)
- guard mismatch
- parse contract break that indicates rewrite/corruption before cursor

Fallback rewrites only the current transcript's cursor file.

## Partial Record Handling

Input files may be written concurrently and end with an incomplete trailing
record.

Rules:
- Ingest MUST only commit complete records.
- `byte_cursor` MUST never advance past incomplete trailing bytes.
- Next run re-reads from the same cursor, so once the record is completed, it is
  processed exactly once.

This preserves idempotency and prevents partial-write corruption.

## Determinism + Idempotency Invariants

Implementation MUST preserve:
- Deterministic ingest output for identical input bytes and state.
- Idempotent re-run behavior when no new complete bytes were appended.
- Atomic state update: cursor state and index/tape side effects must commit as a
  single ingest unit per run/failure domain (existing ingest atomicity invariant
  remains hard).
- Cursor isolation: updating one transcript MUST NOT rewrite unrelated cursor
  files.

## Backward Compatibility

Legacy `ingest-state.json` is no longer authoritative.
- Implementation may ignore it and rebuild per-file cursor state lazily via
  first-run full ingest per transcript.
- Optional one-time migration is allowed but not required.

No manual migration step required.

## Cursor Cleanup

Per-file cursor cleanup policy:
- If a transcript is no longer found, its cursor file MAY be deleted eagerly.
- Or cursor files MAY be retained and cleaned by GC later.

Either policy is acceptable, but behavior MUST be deterministic and documented
by implementation.

## Error Model

- Per-file failures remain isolated and reported; other files continue.
- Path-arg mode reports invalid/missing paths explicitly.
- Outside-scope path args are explicit errors (not silently ingested).

## Acceptance Checks

1. Append-only fast path:
- Given an ingested file, append N bytes with complete records.
- Next ingest reads/processes only appended bytes and advances cursor.

2. No-op idempotency:
- Re-run ingest with unchanged files -> zero new processing.

3. Rewrite detection:
- Modify bytes before cursor (or truncate file).
- Next ingest detects mismatch and performs full re-ingest.

4. Partial trailing record:
- Append incomplete trailing bytes.
- Cursor does not advance past incomplete record.
- After completion append, ingest processes exactly once.

5. Path args:
- `engram ingest pathA pathB` only touches those targets.
- No-arg mode still scans cwd recursively.

6. Scope enforcement:
- Path arg outside local scope is rejected with explicit error output.

7. Per-file cursor IO:
- Ingest run touching file A does not read/write cursor file for unrelated file B.
- No monolithic `ingest-state.json` read/write occurs.

8. Cleanup behavior:
- Missing transcript cursor file is either removed on ingest or retained for GC,
  per selected policy.

## Notes for Implementation

- Keep `engram ingest` command JSON output compatible; add new counters only if
  additive and deterministic.
- Keep full ingest path unchanged as correctness fallback.
- Keep provenance/index lifecycle atomic as a hard invariant.

# T114 Forced-Compaction Continuation Validation

Date: 2026-02-27  
Run directory: `/Users/mike/src/engram/scratch/t114-forced-compaction/20260227T161024Z`

## Goal
Generate deterministic ground truth across forced compaction boundaries and evaluate continuation reconstruction accuracy end-to-end through Engram ingest/explain.

## Command
From repo root:

```bash
./scripts/t114_forced_compaction_validation.sh
```

## Controlled setup
- 3 OpenClaw-format session logs were generated:
  - `oc-t114-1` (pre-compact)
  - `oc-t114-2` (after forced boundary `B1`)
  - `oc-t114-3` (after forced boundary `B2`)
- Boundaries were marked in transcript text:
  - `FORCED_COMPACT_BOUNDARY_B1`
  - `FORCED_COMPACT_BOUNDARY_B2`
- Each session emitted deterministic `code.edit` evidence for the same target anchor (`after_hash` = SHA-256 of target span text).

Ground truth continuation edges:
- `oc-t114-1 -> oc-t114-2`
- `oc-t114-2 -> oc-t114-3`

## Artifact outputs
- ingest stdout: `/Users/mike/src/engram/scratch/t114-forced-compaction/20260227T161024Z/results/ingest.json`
- ingest stderr: `/Users/mike/src/engram/scratch/t114-forced-compaction/20260227T161024Z/results/ingest.stderr`
- explain stdout: `/Users/mike/src/engram/scratch/t114-forced-compaction/20260227T161024Z/results/explain.json`
- explain stderr: `/Users/mike/src/engram/scratch/t114-forced-compaction/20260227T161024Z/results/explain.stderr`
- predicted edges: `/Users/mike/src/engram/scratch/t114-forced-compaction/20260227T161024Z/results/predicted_edges.txt`
- ground truth edges: `/Users/mike/src/engram/scratch/t114-forced-compaction/20260227T161024Z/results/ground_truth_edges.txt`
- metrics: `/Users/mike/src/engram/scratch/t114-forced-compaction/20260227T161024Z/results/metrics.json`
- summary: `/Users/mike/src/engram/scratch/t114-forced-compaction/20260227T161024Z/results/summary.txt`

## Key results
- `ingest`: `{"status":"ok","scanned_inputs":3,"imported_tapes":3,"failure_count":0,...}`
- `explain`: returned 3 sessions with expected `source.session_id` values and boundary marker windows.
- stderr files for `init`/`ingest`/`explain`: zero bytes.

Metrics:
- precision: `1.000`
- recall: `1.000`
- f1: `1.000`
- tp/fp/fn: `2 / 0 / 0`

## Pass/Fail conclusion
Pass for this forced-compaction dataset: continuation reconstruction matched known boundaries exactly (`2/2` edges recovered, no false edges).

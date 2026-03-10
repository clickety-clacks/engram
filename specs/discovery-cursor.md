# Cursor Discovery Plan

## Goal

Define how Engram should discover Cursor transcript data for a repo path being ingested.

This spec is discovery-only. It does not change any Rust implementation.

## Current Harness Behavior

Today, repo-path ingestion is broad local scanning, not Cursor-specific discovery.

- `discover_local_transcript_candidates(cwd)` in `src/main.rs` walks the repo tree under `cwd`
- It includes any file ending in `.json` or `.jsonl`
- It skips only `.engram/`
- `detect_adapter_for_input(...)` prefers `AdapterId::Cursor` when the candidate path contains `cursor`, then falls back to trial parsing

Implication: the current harness does not map a repo path to Cursor’s own storage. It only discovers Cursor transcripts if Cursor-exported JSON/JSONL already exists somewhere inside the repo tree.

## Target Storage Location

Cursor’s normal chat history is local SQLite-backed workspace storage, not repo-local transcript files.

Primary local storage roots by platform:

- macOS: `~/Library/Application Support/Cursor/User/workspaceStorage/`
- Linux: `~/.config/Cursor/User/workspaceStorage/`
- Windows: `%APPDATA%/Cursor/User/workspaceStorage/`

Within that root, Cursor creates hash-named workspace directories:

- `<workspaceStorage>/<workspace-hash>/workspace.json`
- `<workspaceStorage>/<workspace-hash>/state.vscdb`
- sometimes `<workspaceStorage>/<workspace-hash>/state.vscdb.backup`

Important constraint:

- Official Cursor docs say chat history is stored locally in SQLite on the machine
- Official Cursor docs also say background agent chats are stored remotely, so local repo ingest should treat them as out of scope

## Repo To Storage Mapping

Cursor does not appear to key chats by git repo identity. The practical mapping is workspace-path based.

Expected lookup flow:

1. Canonicalize the repo path being ingested.
2. Enumerate Cursor workspace directories under `workspaceStorage`.
3. Read each `workspace.json`.
4. Match the workspace manifest’s folder/path entry to the canonical repo path.
5. Use the sibling `state.vscdb` as the source for that repo’s Cursor chat history.

Observed/inferred properties:

- Workspace directory names are opaque hashes, not human-readable repo names
- `workspace.json` is the stable repo-to-storage join point
- Matching should use canonical absolute paths, not raw user input

## File Patterns

Discovery should pivot from repo scanning to Cursor workspace storage scanning.

Patterns to probe:

- `~/Library/Application Support/Cursor/User/workspaceStorage/*/workspace.json`
- `~/Library/Application Support/Cursor/User/workspaceStorage/*/state.vscdb`
- `~/.config/Cursor/User/workspaceStorage/*/workspace.json`
- `~/.config/Cursor/User/workspaceStorage/*/state.vscdb`
- `%APPDATA%/Cursor/User/workspaceStorage/*/workspace.json`
- `%APPDATA%/Cursor/User/workspaceStorage/*/state.vscdb`

Cursor transcript payloads are not expected as standalone `.jsonl` files in normal local storage.

Instead, discovery should treat these SQLite keys as the likely transcript-bearing records inside `state.vscdb`:

- `workbench.panel.aichat.view.aichat.chatdata`
- `aiService.prompts`

These keys should be treated as implementation targets to validate against a real Cursor install before parser hardening.

## Edge Cases

- Repo moved or renamed: Cursor history can become inaccessible in the UI because the workspace path no longer matches; discovery should treat old path entries as non-matches unless explicitly asked to search stale workspaces.
- Symlinks: repo ingest should canonicalize both repo path and manifest path before comparison.
- Multi-root workspaces: one Cursor workspace may represent a `.code-workspace` style container rather than a single repo path; spec should treat this as unresolved until a real sample is captured.
- Nested repos: matching should prefer exact canonical path equality, not prefix matching.
- Missing `workspace.json`: skip the workspace directory.
- Missing `state.vscdb`: skip the workspace directory.
- Locked/corrupt SQLite DB: fall back to read-only open, then optional copy-to-temp before querying.
- Backup-only state: `state.vscdb.backup` may exist, but should be fallback-only.
- Background agents: not locally discoverable from workspace storage per Cursor docs.
- No Cursor install on the machine: discovery should return no candidates, not error.

## Implementation Sketch

1. Add a Cursor-specific discovery function separate from repo-local JSON walking.
2. Resolve the platform-specific Cursor `workspaceStorage` root.
3. Enumerate `*/workspace.json` manifests.
4. Parse each manifest and compare its workspace path to the canonical repo path.
5. For each match, open sibling `state.vscdb`.
6. Query known transcript-bearing keys from `ItemTable`.
7. Materialize extracted chat payloads into a deterministic temporary/export format the existing Cursor adapter can consume, or add a direct SQLite-to-tape conversion path.
8. Keep repo-local JSON scanning as a fallback for manually exported Cursor captures already checked into the repo.

Recommended shape:

- Discovery layer returns Cursor DB-backed candidates for the repo
- Extraction layer converts SQLite values into normalized intermediate transcript blobs
- Existing adapter remains responsible for event normalization only after extraction produces Cursor-shaped input

## Non-Goals

- Reverse-engineering every Cursor SQLite key in this task
- Supporting remote/background-agent history
- Modifying `src/tape/adapters/cursor.rs`
- Modifying `src/tape/adapter.rs`

## Open Questions

- What exact field in `workspace.json` stores the canonical workspace path on the Cursor versions we care about?
- Are current local Agent transcripts fully represented by `workbench.panel.aichat.view.aichat.chatdata`, or is additional extraction needed for tool-call fidelity?
- Should Engram query `state.vscdb` directly, or first export deterministic JSONL captures into `.engram/captures/cursor/` and reuse the current adapter path?
- How should multi-root Cursor workspaces map back to a single ingested repo path?

## Sources

- `src/main.rs`
- `src/tape/adapter.rs`
- `src/tape/adapters/cursor.rs`
- Cursor docs: chat history is stored locally in SQLite; background agent chats are remote
- Cursor forum reports describing `workspaceStorage/<hash>/workspace.json` plus `state.vscdb` layout and workspace-path-based recovery behavior

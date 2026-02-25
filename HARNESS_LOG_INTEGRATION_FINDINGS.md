# Engram P11 Harness Log Integration Findings (Codex CLI + Claude Code)

Date: 2026-02-22  
Host: eezo (`/Users/mike`)

## 1) On-Disk Locations

### Codex CLI
- `~/.codex/sessions/YYYY/MM/DD/*.jsonl`: structured per-session event streams (primary source).
- `~/.codex/history.jsonl`: prompt history only (not sufficient for tool/read/edit lineage).
- `~/.codex/log/`: TUI/runtime logs (diagnostic, not primary lineage source).

### Claude Code
- `~/.claude/projects/<project>/<session>.jsonl`: structured session events (primary source).
- `~/.claude/projects/<project>/<session>/tool-results/*.txt`: large tool outputs referenced by session activity.
- `~/.claude/history.jsonl`: prompt history (not sufficient alone for lineage).

## 2) Concrete Field Mapping Candidates to Engram Tape Schema

## Codex CLI mapping
- `session_meta` row:
  - `timestamp` -> `t`
  - `payload.model` or `payload.model_provider` -> `meta.model`
  - `payload.git.commit_hash` -> `meta.repo_head`
  - Emits `{"k":"meta", ...}`
- `response_item` with `payload.type=="message"`:
  - `payload.role in {user,system,developer}` -> `k:"msg.in"`
  - `payload.role=="assistant"` -> `k:"msg.out"`
  - Content text extracted from `content[].{text,input_text,output_text}`
- `response_item` with `payload.type=="function_call"`:
  - `payload.name` -> `tool.call.tool`
  - `payload.arguments` -> `tool.call.args` (JSON string preserved)
- `response_item` with `payload.type=="function_call_output"`:
  - Join with prior `function_call` by `call_id`
  - Emit `tool.result` with paired tool name and captured output
  - Exit code can be deterministically parsed when output contains `Process exited with code N`
- `payload.name=="apply_patch"`:
  - Parse `*** Update/Add/Delete File:` headers from patch text
  - Emit one `code.edit` per touched file

## Claude Code mapping
- Top-level `type=="assistant"` and `message.content[]`:
  - `content.type=="text"` -> `msg.out`
  - `content.type=="tool_use"` -> `tool.call` (`name` + serialized `input`)
- Top-level `type=="user"` and `message.content[]`:
  - `content.type=="tool_result"` -> `tool.result` paired by `tool_use_id`
- `tool_use` deterministic `code.read`:
  - `name=="Read"` and `input.file_path` -> `code.read.file`
  - `input.offset` + `input.limit` -> `code.read.range` (line span)
- `tool_use` deterministic `code.edit`:
  - `name=="Edit"` + `input.file_path` -> `code.edit.file`
  - `input.old_string/new_string` -> `before_hash/after_hash` (deterministic SHA-256)
  - `name=="Write"` + `input.content` -> `after_hash`
  - `name=="MultiEdit"` + `input.edits[]` -> one `code.edit` per edit

## 3) Gaps / Ambiguities

- Codex read lineage:
  - Most reads happen via generic `exec_command` calls; command strings are free-form shell.
  - Deterministically mapping every shell command to precise `code.read` range is not reliable without additional harness-side structure.
- Codex edit lineage:
  - Deterministic when `apply_patch` is used.
  - Non-`apply_patch` edits via shell/editor (`nvim`, redirection, perl/sed in-place edits) are not reliably extractable to span-level `code.edit` from logs alone.
- Tool-result completeness:
  - Claude may store large outputs in `tool-results/*.txt`; adapters must optionally resolve referenced artifacts for full `stdout`.
- Range semantics:
  - Claude `Read.offset/limit` are treated as line-based in current behavior; if semantics vary by tool version, adapter must gate by schema version.

## 4) Feasibility Verdict + Recommended Integration Contract

Verdict: **Feasible with contract constraints.**

- `tool.call`/`tool.result`: deterministic for both Codex and Claude.
- `code.read`:
  - Claude: deterministic from structured `Read`.
  - Codex: deterministic only for structured read tools (or if Codex adds explicit read events); generic shell reads are incomplete.
- `code.edit`:
  - Claude: deterministic from `Edit/Write/MultiEdit`.
  - Codex: deterministic for `apply_patch`; incomplete for free-form shell/editor edits.

Recommended contract (P11 seam):
- Require adapters to emit normalized Engram JSONL with:
  - guaranteed `meta/msg/tool`
  - harness-native deterministic read/edit only
  - explicit `adapter_flags` in `meta` for partial extraction, e.g.:
    - `codex_read_partial=true`
    - `codex_edit_partial=true`
- Strong recommendation for Codex harness evolution:
  - emit first-class structured `code.read` and `code.edit` tool events (or sidecar event stream) instead of relying on shell command inference.

## 5) Short Adapter Implementation Plan

1. Keep `src/tape/harness.rs` as normalization layer (`codex_jsonl_to_tape_jsonl`, `claude_jsonl_to_tape_jsonl`).
2. Add ingestion CLI path:
   - `engram record --stdin --format codex-jsonl|claude-jsonl`
   - Normalize first, then run existing tape parser/indexer.
3. Add fixture suite:
   - Real-format minimal samples for Codex and Claude.
   - Golden expected normalized events (`meta/msg/tool/read/edit`).
4. Add compatibility guardrails:
   - Detect schema drift (missing required fields) and emit explicit adapter parse errors.
5. Optional hardening:
   - Claude `tool-results/*.txt` resolver support for large outputs.

## Parser Spike Added

- File: `src/tape/harness.rs`
- Exposes:
  - `codex_jsonl_to_tape_jsonl`
  - `claude_jsonl_to_tape_jsonl`
- Tests included in same module validate:
  - Codex tool pairing + `apply_patch` file extraction -> `code.edit`
  - Claude `Read/Edit` extraction -> `code.read/code.edit` plus tool events

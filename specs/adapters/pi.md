# Adapter Spec: Pi

Status: implemented; structured read/edit coverage is partial.

## Artifact location

- `~/.pi/agent/sessions/--<cwd-with-path-separators-replaced>--/*.jsonl`
- The default session directory can be overridden in Pi; custom directories are not auto-discovered.

## Schema

Pi session JSONL uses a `session` header and append-only entry rows. The adapter recognizes the header's `id`, `version`, and `cwd`, plus `message` entries containing user, assistant, tool-result, or direct bash-execution messages. It accepts session versions with this shared entry shape; the checked-in fixture is Pi v3.

## Deterministic mapping

- Session header and `model_change` / assistant model fields -> `meta` (`source.session_id`, `cwd`, `provider`, `model`)
- User string or text blocks -> `msg.in`
- Assistant text blocks -> `msg.out`; thinking and image blocks are omitted
- Assistant `content[type=toolCall]` -> `tool.call`
- `message.role=toolResult` -> `tool.result`, paired by `toolCallId`
- `message.role=bashExecution` -> paired `bash` call/result events
- Successful `read` results -> `code.read` when a path is present
- Successful `edit` / `write` results -> `code.edit` when file and change details are present
- Relative structured file paths are resolved against the session header's `cwd`

System prompts and non-message entries such as labels, usage, custom state, compaction, and branch summaries are not emitted. The adapter processes message entries in file order and does not reconstruct Pi's active branch from `parentId`, so alternate branches in one file may also be indexed.

## Incremental ingestion

The adapter retains session metadata and pending tool-call context across appends so a tool result in a later ingest chunk remains correlated with its call.

## Coverage expectation

- `coverage.tool=full`
- `coverage.read=partial`
- `coverage.edit=partial`

Unstructured shell commands and tools without file/change arguments remain tool events without guaranteed span evidence.

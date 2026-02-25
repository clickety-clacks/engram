# Adapter Spec: OpenCode

Status: P0 candidate (implemented, explicit partial read/edit coverage)

## Artifact locations
- `~/.local/share/opencode/storage/session/<project-id>/*.json`
- `~/.local/share/opencode/storage/message/<session-id>/*.json`
- `~/.local/share/opencode/storage/part/<message-id>/*.json`
- `XDG_DATA_HOME/opencode/storage/**` (same layout, non-default base dir)

## Schema sample set
- `opencode-session-export-json` (`{ info, messages[{ info, parts[] }] }`)
- `opencode-storage-part-json` (`part` records with `type=tool|text|...`)

## Deterministic mapping
- `messages[].parts[].type=text` -> `msg.in|msg.out` (from `messages[].info.role`)
- `messages[].parts[].type=tool` -> `tool.call` (`tool`, `callID`, serialized `state.input`)
- tool `state.status=completed|error` -> `tool.result` (paired by `callID`)
- `tool=read` + `state.input.filePath` -> `code.read` (`range` from `offset/limit`, 1-based line basis)
- `tool=edit|write|patch` -> `code.edit` (`filePath` or file extraction from `patchText`)

## Known gaps
- Shell-driven file reads/writes through generic command tools are not guaranteed as explicit `code.read`/`code.edit`.
- `patch` events without parseable file targets remain represented as `tool.call/tool.result` only.

## Coverage expectation
- `coverage.tool=full`
- `coverage.read=partial`
- `coverage.edit=partial`

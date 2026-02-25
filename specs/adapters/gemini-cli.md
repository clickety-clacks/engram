# Adapter Spec: Gemini CLI

Status: P0 candidate (partial read/edit coverage)

## Artifact locations
- `~/.gemini/tmp/*/chats/session-*.json`
- `~/.gemini/tmp/*/logs.json`

## Schema sample set
- `gemini-session-json`
- `gemini-logs-json`

## Deterministic mapping
- `messages[type=user].content` -> `msg.in`
- `messages[type=gemini].content` -> `msg.out`
- `messages[type=gemini].toolCalls[]` -> `tool.call` + `tool.result` (paired by `toolCalls.id`)
- `toolCalls[name=read_file].args.file_path` -> `code.read` (`range=[1,1]` line basis)
- `toolCalls[name=write_file].args.{file_path,content}` -> `code.edit` (`after_hash` from deterministic hash)
- `logs.json[]` -> `meta` + `msg.in|msg.out` (message-only fallback)

## Known gaps
- `read_file` artifacts do not include explicit span offsets in observed samples, so emitted `code.read` uses normalized sentinel range.
- Writes outside observed structured `write_file` calls are not guaranteed to emit `code.edit`.

## Coverage expectation
- session artifacts: `coverage.tool=full`, `coverage.read=partial`, `coverage.edit=partial`
- logs artifacts: `coverage.tool=none`, `coverage.read=none`, `coverage.edit=none`

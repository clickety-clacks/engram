# Adapter Spec: Claude Code

Status: P0 candidate (high coverage)

## Artifact locations
- `~/.claude/projects/<project>/<session>.jsonl`
- optional large outputs: `~/.claude/projects/<project>/<session>/tool-results/*.txt`

## Deterministic mapping
- `assistant/text` -> `msg.out`
- `assistant/tool_use` -> `tool.call`
- `user/tool_result` -> `tool.result` (pair by `tool_use_id`)
- `Read` tool -> `code.read`
- `Edit/Write/MultiEdit` tools -> `code.edit`

## Known gaps
- Shell-side mutations outside structured edit tools are not guaranteed as explicit `code.edit` unless present in artifacts.

## Coverage expectation
- `coverage.tool=full`
- `coverage.read=full` for structured `Read`
- `coverage.edit=full` for structured edit tools

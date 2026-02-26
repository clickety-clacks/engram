use std::path::{Path, PathBuf};

use serde_json::Value;

use super::adapters::{
    claude_jsonl_to_tape_jsonl, codex_jsonl_to_tape_jsonl, cursor_jsonl_to_tape_jsonl,
    gemini_json_to_tape_jsonl, openclaw_jsonl_to_tape_jsonl, opencode_json_to_tape_jsonl,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AdapterId {
    ClaudeCode,
    CodexCli,
    OpenCode,
    GeminiCli,
    Cursor,
    OpenClaw,
}

impl AdapterId {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::CodexCli => "codex-cli",
            Self::OpenCode => "opencode",
            Self::GeminiCli => "gemini-cli",
            Self::Cursor => "cursor",
            Self::OpenClaw => "openclaw",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdapterStatus {
    Implemented,
    DiscoveryRequired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoverageGrade {
    Full,
    Partial,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoverageGrades {
    pub read: CoverageGrade,
    pub edit: CoverageGrade,
    pub tool: CoverageGrade,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MappingRule {
    pub source: &'static str,
    pub target: &'static str,
    pub note: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdapterDescriptor {
    pub id: AdapterId,
    pub status: AdapterStatus,
    pub artifact_path_templates: &'static [&'static str],
    pub schema_sample_set: &'static [&'static str],
    pub mapping_table: &'static [MappingRule],
    pub coverage: CoverageGrades,
}

pub fn adapter_registry() -> &'static [AdapterDescriptor] {
    &[
        AdapterDescriptor {
            id: AdapterId::ClaudeCode,
            status: AdapterStatus::Implemented,
            artifact_path_templates: &[
                "~/.claude/projects/<project>/<session>.jsonl",
                "~/.claude/projects/<project>/<session>/tool-results/*.txt",
            ],
            schema_sample_set: &["claude-jsonl"],
            mapping_table: &[
                MappingRule {
                    source: "assistant/text",
                    target: "msg.out",
                    note: "text block",
                },
                MappingRule {
                    source: "assistant/tool_use",
                    target: "tool.call",
                    note: "paired by tool_use.id",
                },
                MappingRule {
                    source: "user/tool_result",
                    target: "tool.result",
                    note: "paired by tool_use_id",
                },
                MappingRule {
                    source: "Read tool",
                    target: "code.read",
                    note: "structured file and range",
                },
                MappingRule {
                    source: "Edit/Write/MultiEdit tool",
                    target: "code.edit",
                    note: "structured file mutation",
                },
            ],
            coverage: CoverageGrades {
                read: CoverageGrade::Full,
                edit: CoverageGrade::Full,
                tool: CoverageGrade::Full,
            },
        },
        AdapterDescriptor {
            id: AdapterId::CodexCli,
            status: AdapterStatus::Implemented,
            artifact_path_templates: &[
                "~/.codex/sessions/YYYY/MM/DD/*.jsonl",
                "~/.codex/history.jsonl",
            ],
            schema_sample_set: &["codex-jsonl"],
            mapping_table: &[
                MappingRule {
                    source: "session metadata",
                    target: "meta",
                    note: "model/repo metadata",
                },
                MappingRule {
                    source: "response_item/message",
                    target: "msg.in|msg.out",
                    note: "role-dependent",
                },
                MappingRule {
                    source: "response_item/function_call",
                    target: "tool.call",
                    note: "name and arguments",
                },
                MappingRule {
                    source: "response_item/function_call_output",
                    target: "tool.result",
                    note: "paired by call_id",
                },
                MappingRule {
                    source: "apply_patch payload",
                    target: "code.edit",
                    note: "file touch extraction",
                },
            ],
            coverage: CoverageGrades {
                read: CoverageGrade::Partial,
                edit: CoverageGrade::Partial,
                tool: CoverageGrade::Full,
            },
        },
        AdapterDescriptor {
            id: AdapterId::OpenCode,
            status: AdapterStatus::Implemented,
            artifact_path_templates: &[
                "~/.local/share/opencode/storage/session/<project-id>/*.json",
                "~/.local/share/opencode/storage/message/<session-id>/*.json",
                "~/.local/share/opencode/storage/part/<message-id>/*.json",
                "XDG_DATA_HOME/opencode/storage/**",
            ],
            schema_sample_set: &["opencode-session-export-json", "opencode-storage-part-json"],
            mapping_table: &[
                MappingRule {
                    source: "messages[].parts[].type=text",
                    target: "msg.in|msg.out",
                    note: "role from messages[].info.role",
                },
                MappingRule {
                    source: "messages[].parts[].type=tool",
                    target: "tool.call",
                    note: "tool + callID + serialized state.input",
                },
                MappingRule {
                    source: "tool state.status=completed|error",
                    target: "tool.result",
                    note: "completed=>exit=0/stdout, error=>exit=1/stderr",
                },
                MappingRule {
                    source: "tool=read with state.input.filePath",
                    target: "code.read",
                    note: "range from offset/limit when present",
                },
                MappingRule {
                    source: "tool=edit|write|patch",
                    target: "code.edit",
                    note: "structured filePath or patchText file extraction",
                },
            ],
            coverage: CoverageGrades {
                read: CoverageGrade::Partial,
                edit: CoverageGrade::Partial,
                tool: CoverageGrade::Full,
            },
        },
        AdapterDescriptor {
            id: AdapterId::GeminiCli,
            status: AdapterStatus::Implemented,
            artifact_path_templates: &[
                "~/.gemini/tmp/*/chats/session-*.json",
                "~/.gemini/tmp/*/logs.json",
            ],
            schema_sample_set: &["gemini-session-json", "gemini-logs-json"],
            mapping_table: &[
                MappingRule {
                    source: "messages[type=user].content",
                    target: "msg.in",
                    note: "user prompt text",
                },
                MappingRule {
                    source: "messages[type=gemini].content",
                    target: "msg.out",
                    note: "assistant response text",
                },
                MappingRule {
                    source: "messages[type=gemini].toolCalls[]",
                    target: "tool.call|tool.result",
                    note: "paired by toolCalls.id",
                },
                MappingRule {
                    source: "toolCalls[name=read_file].args.file_path",
                    target: "code.read",
                    note: "range normalized to [1,1]",
                },
                MappingRule {
                    source: "toolCalls[name=write_file].args.{file_path,content}",
                    target: "code.edit",
                    note: "after_hash from deterministic content hash",
                },
                MappingRule {
                    source: "logs.json[]",
                    target: "meta+msg.in|msg.out",
                    note: "message-only fallback with none coverage",
                },
            ],
            coverage: CoverageGrades {
                read: CoverageGrade::Partial,
                edit: CoverageGrade::Partial,
                tool: CoverageGrade::Full,
            },
        },
        AdapterDescriptor {
            id: AdapterId::OpenClaw,
            status: AdapterStatus::Implemented,
            artifact_path_templates: &[
                "~/.openclaw/sessions/**/*.jsonl",
                "~/.openclaw/logs/*.log",
            ],
            schema_sample_set: &["openclaw-session-jsonl", "openclaw-node-log"],
            mapping_table: &[
                MappingRule {
                    source: "role/content",
                    target: "msg.in|msg.out",
                    note: "user/assistant transcript rows",
                },
                MappingRule {
                    source: "tool.call/tool.result rows",
                    target: "tool.call|tool.result",
                    note: "deterministic serialization of args/stdout/stderr",
                },
                MappingRule {
                    source: "code.read|code.edit rows",
                    target: "code.read|code.edit",
                    note: "structured file/range/hash fields when present",
                },
            ],
            coverage: CoverageGrades {
                read: CoverageGrade::Partial,
                edit: CoverageGrade::Partial,
                tool: CoverageGrade::Partial,
            },
        },
        AdapterDescriptor {
            id: AdapterId::Cursor,
            status: AdapterStatus::Implemented,
            artifact_path_templates: &[
                "<capture>/cursor-stream-jsonl.ndjson",
                "<capture>/cursor-stream-jsonl-*.ndjson",
            ],
            schema_sample_set: &[
                "cursor-cli-stream-json-ndjson",
                "tests/fixtures/cursor/supported_paths.jsonl",
            ],
            mapping_table: &[
                MappingRule {
                    source: "system/init",
                    target: "meta",
                    note: "model + fixed coverage grades",
                },
                MappingRule {
                    source: "user.message.content[].text",
                    target: "msg.in",
                    note: "joined text blocks",
                },
                MappingRule {
                    source: "assistant.message.content[].text",
                    target: "msg.out",
                    note: "joined text blocks",
                },
                MappingRule {
                    source: "tool_call[subtype=started]",
                    target: "tool.call",
                    note: "readToolCall/writeToolCall/function + call_id",
                },
                MappingRule {
                    source: "tool_call[subtype=completed]",
                    target: "tool.result",
                    note: "deterministic exit/stdout/stderr extraction",
                },
                MappingRule {
                    source: "writeToolCall.result.success.path",
                    target: "code.edit",
                    note: "file path only; read ranges unavailable in schema",
                },
            ],
            coverage: CoverageGrades {
                read: CoverageGrade::Partial,
                edit: CoverageGrade::Partial,
                tool: CoverageGrade::Full,
            },
        },
    ]
}

pub fn descriptor_for(id: AdapterId) -> &'static AdapterDescriptor {
    adapter_registry()
        .iter()
        .find(|descriptor| descriptor.id == id)
        .expect("descriptor must exist")
}

pub fn discovery_scaffold(id: AdapterId, home_dir: &Path) -> Vec<PathBuf> {
    descriptor_for(id)
        .artifact_path_templates
        .iter()
        .map(|template| template.replace('~', &home_dir.to_string_lossy()))
        .map(PathBuf::from)
        .collect()
}

#[derive(Debug)]
pub enum AdapterError {
    Json(serde_json::Error),
}

impl std::fmt::Display for AdapterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Json(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for AdapterError {}

impl From<serde_json::Error> for AdapterError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

pub trait HarnessAdapter {
    fn adapter_id(&self) -> AdapterId;

    fn convert_to_tape_jsonl(&self, input: &str) -> Result<String, AdapterError>;

    fn descriptor(&self) -> &'static AdapterDescriptor {
        descriptor_for(self.adapter_id())
    }
}

#[derive(Debug, Default)]
pub struct ClaudeCodeAdapter;

impl HarnessAdapter for ClaudeCodeAdapter {
    fn adapter_id(&self) -> AdapterId {
        AdapterId::ClaudeCode
    }

    fn convert_to_tape_jsonl(&self, input: &str) -> Result<String, AdapterError> {
        Ok(claude_jsonl_to_tape_jsonl(input)?)
    }
}

#[derive(Debug, Default)]
pub struct CodexCliAdapter;

impl HarnessAdapter for CodexCliAdapter {
    fn adapter_id(&self) -> AdapterId {
        AdapterId::CodexCli
    }

    fn convert_to_tape_jsonl(&self, input: &str) -> Result<String, AdapterError> {
        Ok(codex_jsonl_to_tape_jsonl(input)?)
    }
}

#[derive(Debug, Default)]
pub struct OpenCodeAdapter;

impl HarnessAdapter for OpenCodeAdapter {
    fn adapter_id(&self) -> AdapterId {
        AdapterId::OpenCode
    }

    fn convert_to_tape_jsonl(&self, input: &str) -> Result<String, AdapterError> {
        Ok(opencode_json_to_tape_jsonl(input)?)
    }
}

#[derive(Debug, Default)]
pub struct CursorAdapter;

impl HarnessAdapter for CursorAdapter {
    fn adapter_id(&self) -> AdapterId {
        AdapterId::Cursor
    }

    fn convert_to_tape_jsonl(&self, input: &str) -> Result<String, AdapterError> {
        Ok(cursor_jsonl_to_tape_jsonl(input)?)
    }
}

#[derive(Debug, Default)]
pub struct GeminiCliAdapter;

impl HarnessAdapter for GeminiCliAdapter {
    fn adapter_id(&self) -> AdapterId {
        AdapterId::GeminiCli
    }

    fn convert_to_tape_jsonl(&self, input: &str) -> Result<String, AdapterError> {
        Ok(gemini_json_to_tape_jsonl(input)?)
    }
}

#[derive(Debug, Default)]
pub struct OpenClawAdapter;

impl HarnessAdapter for OpenClawAdapter {
    fn adapter_id(&self) -> AdapterId {
        AdapterId::OpenClaw
    }

    fn convert_to_tape_jsonl(&self, input: &str) -> Result<String, AdapterError> {
        Ok(openclaw_jsonl_to_tape_jsonl(input)?)
    }
}

pub fn convert_with_adapter(id: AdapterId, input: &str) -> Result<String, AdapterError> {
    match id {
        AdapterId::ClaudeCode => ClaudeCodeAdapter.convert_to_tape_jsonl(input),
        AdapterId::CodexCli => CodexCliAdapter.convert_to_tape_jsonl(input),
        AdapterId::OpenCode => OpenCodeAdapter.convert_to_tape_jsonl(input),
        AdapterId::Cursor => CursorAdapter.convert_to_tape_jsonl(input),
        AdapterId::GeminiCli => GeminiCliAdapter.convert_to_tape_jsonl(input),
        AdapterId::OpenClaw => OpenClawAdapter.convert_to_tape_jsonl(input),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConformanceIssue {
    pub line: usize,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConformanceReport {
    pub adapter: AdapterId,
    pub event_count: usize,
    pub coverage: CoverageGrades,
    pub issues: Vec<ConformanceIssue>,
}

pub fn run_conformance(id: AdapterId, input: &str) -> Result<ConformanceReport, AdapterError> {
    let normalized = convert_with_adapter(id, input)?;
    let mut issues = Vec::new();
    let mut event_count = 0usize;
    let mut actual_coverage: Option<CoverageGrades> = None;

    for (idx, line) in normalized.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }

        event_count += 1;
        let row: Value = serde_json::from_str(line)?;

        // Extract coverage from the first meta event the adapter actually emits.
        if actual_coverage.is_none() && row.get("k").and_then(Value::as_str) == Some("meta") {
            actual_coverage = parse_meta_coverage(&row);
        }

        validate_contract_row(idx + 1, &row, &mut issues);
    }

    Ok(ConformanceReport {
        adapter: id,
        event_count,
        // Use actual adapter output; fall back to registry if meta is absent or
        // its coverage fields cannot be parsed (which would also produce issues above).
        coverage: actual_coverage.unwrap_or_else(|| descriptor_for(id).coverage),
        issues,
    })
}

fn parse_meta_coverage(meta: &Value) -> Option<CoverageGrades> {
    let read = coverage_grade_from_str(meta.get("coverage.read").and_then(Value::as_str)?)?;
    let edit = coverage_grade_from_str(meta.get("coverage.edit").and_then(Value::as_str)?)?;
    let tool = coverage_grade_from_str(meta.get("coverage.tool").and_then(Value::as_str)?)?;
    Some(CoverageGrades { read, edit, tool })
}

fn coverage_grade_from_str(s: &str) -> Option<CoverageGrade> {
    match s {
        "full" => Some(CoverageGrade::Full),
        "partial" => Some(CoverageGrade::Partial),
        "none" => Some(CoverageGrade::None),
        _ => None,
    }
}

fn validate_contract_row(line: usize, row: &Value, issues: &mut Vec<ConformanceIssue>) {
    let Some(obj) = row.as_object() else {
        issues.push(ConformanceIssue {
            line,
            detail: "row is not an object".to_string(),
        });
        return;
    };

    if !obj.get("t").is_some_and(Value::is_string) {
        issues.push(ConformanceIssue {
            line,
            detail: "missing string field `t`".to_string(),
        });
    }
    match obj.get("source").and_then(Value::as_object) {
        Some(source) => {
            if !source.get("harness").is_some_and(Value::is_string) {
                issues.push(ConformanceIssue {
                    line,
                    detail: "missing string field `source.harness`".to_string(),
                });
            }
            if source.contains_key("session_id")
                && !source.get("session_id").is_some_and(Value::is_string)
            {
                issues.push(ConformanceIssue {
                    line,
                    detail: "field `source.session_id` must be a string when present".to_string(),
                });
            }
        }
        None => issues.push(ConformanceIssue {
            line,
            detail: "missing object field `source`".to_string(),
        }),
    }

    let kind = obj.get("k").and_then(Value::as_str).unwrap_or("");
    if kind.is_empty() {
        issues.push(ConformanceIssue {
            line,
            detail: "missing string field `k`".to_string(),
        });
        return;
    }

    match kind {
        "meta" => {
            for field in ["coverage.read", "coverage.edit", "coverage.tool"] {
                if !obj.get(field).is_some_and(Value::is_string) {
                    issues.push(ConformanceIssue {
                        line,
                        detail: format!("meta missing string field `{field}`"),
                    });
                } else if !obj
                    .get(field)
                    .and_then(Value::as_str)
                    .and_then(coverage_grade_from_str)
                    .is_some()
                {
                    issues.push(ConformanceIssue {
                        line,
                        detail: format!("meta field `{field}` must be one of `full|partial|none`"),
                    });
                }
            }
        }
        "msg.in" | "msg.out" => {}
        "span.link" => {
            if !obj.get("from_file").is_some_and(Value::is_string) {
                issues.push(ConformanceIssue {
                    line,
                    detail: "span.link missing string field `from_file`".to_string(),
                });
            }
            if !obj.get("from_range").is_some() {
                issues.push(ConformanceIssue {
                    line,
                    detail: "span.link missing field `from_range`".to_string(),
                });
            }
            if !obj.get("to_file").is_some_and(Value::is_string) {
                issues.push(ConformanceIssue {
                    line,
                    detail: "span.link missing string field `to_file`".to_string(),
                });
            }
            if !obj.get("to_range").is_some() {
                issues.push(ConformanceIssue {
                    line,
                    detail: "span.link missing field `to_range`".to_string(),
                });
            }
        }
        "tool.call" => {
            if !obj.get("tool").is_some_and(Value::is_string) {
                issues.push(ConformanceIssue {
                    line,
                    detail: "tool.call missing string field `tool`".to_string(),
                });
            }
            if !obj.get("args").is_some() {
                issues.push(ConformanceIssue {
                    line,
                    detail: "tool.call missing field `args`".to_string(),
                });
            }
        }
        "tool.result" => {
            if !obj.get("tool").is_some_and(Value::is_string) {
                issues.push(ConformanceIssue {
                    line,
                    detail: "tool.result missing string field `tool`".to_string(),
                });
            }
        }
        "code.read" => {
            if !obj.get("file").is_some_and(Value::is_string) {
                issues.push(ConformanceIssue {
                    line,
                    detail: "code.read missing string field `file`".to_string(),
                });
            }
            if !obj.get("range").is_some() {
                issues.push(ConformanceIssue {
                    line,
                    detail: "code.read missing field `range`".to_string(),
                });
            }
        }
        "code.edit" => {
            if !obj.get("file").is_some_and(Value::is_string) {
                issues.push(ConformanceIssue {
                    line,
                    detail: "code.edit missing string field `file`".to_string(),
                });
            }
        }
        _ => issues.push(ConformanceIssue {
            line,
            detail: format!("unknown event kind `{kind}`"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AdapterId, AdapterStatus, CoverageGrade, adapter_registry, descriptor_for,
        discovery_scaffold, run_conformance,
    };

    #[test]
    fn codex_conformance_harness_passes() {
        let input = r#"{"timestamp":"2026-02-22T00:00:00Z","type":"session_meta","payload":{"model_provider":"openai","git":{"commit_hash":"abc123"}}}
{"timestamp":"2026-02-22T00:00:01Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"call_1","arguments":"{\"cmd\":\"echo hi\"}"}}
{"timestamp":"2026-02-22T00:00:02Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call_1","output":"Process exited with code 7\nOutput:\nboom"}}"#;

        let report = run_conformance(AdapterId::CodexCli, input).expect("adapter should parse");
        assert_eq!(report.adapter, AdapterId::CodexCli);
        assert_eq!(
            report.event_count, 3,
            "expected meta + tool.call + tool.result"
        );
        assert!(report.issues.is_empty(), "issues={:?}", report.issues);
        assert_eq!(report.coverage.tool, CoverageGrade::Full);
        assert_eq!(report.coverage.read, CoverageGrade::Partial);
        assert_eq!(report.coverage.edit, CoverageGrade::Partial);
    }

    #[test]
    fn codex_conformance_keeps_partial_coverage_for_unstructured_shell_io() {
        let input = include_str!("../../tests/fixtures/codex/unsupported_paths.jsonl");
        let report = run_conformance(AdapterId::CodexCli, input).expect("adapter should parse");
        assert_eq!(report.adapter, AdapterId::CodexCli);
        assert_eq!(report.event_count, 5, "expected meta + 2 calls + 2 results");
        assert!(report.issues.is_empty(), "issues={:?}", report.issues);
        assert_eq!(report.coverage.tool, CoverageGrade::Full);
        assert_eq!(report.coverage.read, CoverageGrade::Partial);
        assert_eq!(report.coverage.edit, CoverageGrade::Partial);
    }

    #[test]
    fn claude_conformance_harness_passes() {
        let input = r#"{"type":"assistant","timestamp":"2026-02-22T00:00:00Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Read","input":{"file_path":"/repo/src/lib.rs","offset":10,"limit":5}}]}}
{"type":"user","timestamp":"2026-02-22T00:00:01Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"10->line"}]}}"#;

        let report = run_conformance(AdapterId::ClaudeCode, input).expect("adapter should parse");
        assert_eq!(report.adapter, AdapterId::ClaudeCode);
        assert!(report.event_count >= 2);
        assert!(report.issues.is_empty(), "issues={:?}", report.issues);
        assert_eq!(report.coverage.tool, CoverageGrade::Full);
        assert_eq!(report.coverage.read, CoverageGrade::Full);
        assert_eq!(report.coverage.edit, CoverageGrade::Full);
    }

    #[test]
    fn long_tail_registry_entries_have_discovery_and_mapping_scaffolding() {
        let open = descriptor_for(AdapterId::OpenCode);
        assert_eq!(open.status, AdapterStatus::Implemented);
        assert_eq!(open.coverage.tool, CoverageGrade::Full);
        assert_eq!(open.coverage.read, CoverageGrade::Partial);
        assert_eq!(open.coverage.edit, CoverageGrade::Partial);
        assert!(!open.artifact_path_templates.is_empty());
        assert!(!open.schema_sample_set.is_empty());
        assert!(!open.mapping_table.is_empty());

        let gemini = descriptor_for(AdapterId::GeminiCli);
        assert_eq!(gemini.status, AdapterStatus::Implemented);
        assert_eq!(gemini.coverage.tool, CoverageGrade::Full);
        assert_eq!(gemini.coverage.read, CoverageGrade::Partial);
        assert_eq!(gemini.coverage.edit, CoverageGrade::Partial);
        assert!(!gemini.artifact_path_templates.is_empty());
        assert!(!gemini.schema_sample_set.is_empty());
        assert!(!gemini.mapping_table.is_empty());

        let cursor = descriptor_for(AdapterId::Cursor);
        assert_eq!(cursor.status, AdapterStatus::Implemented);
        assert_eq!(cursor.coverage.tool, CoverageGrade::Full);
        assert_eq!(cursor.coverage.read, CoverageGrade::Partial);
        assert_eq!(cursor.coverage.edit, CoverageGrade::Partial);
        assert!(!cursor.artifact_path_templates.is_empty());
        assert!(!cursor.schema_sample_set.is_empty());
        assert!(!cursor.mapping_table.is_empty());

        let openclaw = descriptor_for(AdapterId::OpenClaw);
        assert_eq!(openclaw.status, AdapterStatus::Implemented);
        assert_eq!(openclaw.coverage.tool, CoverageGrade::Partial);
        assert_eq!(openclaw.coverage.read, CoverageGrade::Partial);
        assert_eq!(openclaw.coverage.edit, CoverageGrade::Partial);
        assert!(!openclaw.artifact_path_templates.is_empty());
        assert!(!openclaw.schema_sample_set.is_empty());
        assert!(!openclaw.mapping_table.is_empty());
    }

    #[test]
    fn gemini_logs_conformance_emits_none_coverage() {
        let input = include_str!("../../tests/fixtures/gemini/logs.json");
        let report = run_conformance(AdapterId::GeminiCli, input).expect("adapter should parse");
        assert_eq!(report.event_count, 3, "expected meta + 2 log messages");
        assert!(report.issues.is_empty(), "issues={:?}", report.issues);
        assert_eq!(report.coverage.tool, CoverageGrade::None);
        assert_eq!(report.coverage.read, CoverageGrade::None);
        assert_eq!(report.coverage.edit, CoverageGrade::None);
    }

    #[test]
    fn gemini_conformance_harness_passes() {
        let input = include_str!("../../tests/fixtures/gemini/session_with_tools.json");
        let report = run_conformance(AdapterId::GeminiCli, input).expect("adapter should parse");
        assert_eq!(report.adapter, AdapterId::GeminiCli);
        assert!(report.event_count >= 8);
        assert!(report.issues.is_empty(), "issues={:?}", report.issues);
        assert_eq!(report.coverage.tool, CoverageGrade::Full);
        assert_eq!(report.coverage.read, CoverageGrade::Full);
        assert_eq!(report.coverage.edit, CoverageGrade::Full);
    }

    #[test]
    fn opencode_conformance_harness_passes() {
        let input = r#"{
  "info": {"id": "ses_1", "time": {"created": 1735689600000}},
  "messages": [{
    "info": {"id": "msg_1", "role": "assistant", "time": {"created": 1735689601000}},
    "parts": [
      {"id":"part_1","type":"tool","callID":"call_1","tool":"read","state":{"status":"completed","input":{"filePath":"src/lib.rs","offset":0,"limit":2},"output":"ok"}}
    ]
  }]
}"#;
        let report = run_conformance(AdapterId::OpenCode, input).expect("adapter should parse");
        assert_eq!(report.adapter, AdapterId::OpenCode);
        assert!(report.event_count >= 3);
        assert!(report.issues.is_empty(), "issues={:?}", report.issues);
        assert_eq!(report.coverage.tool, CoverageGrade::Full);
        assert_eq!(report.coverage.read, CoverageGrade::Partial);
        assert_eq!(report.coverage.edit, CoverageGrade::Partial);
    }

    #[test]
    fn cursor_conformance_harness_passes() {
        let input = include_str!("../../tests/fixtures/cursor/supported_paths.jsonl");
        let report = run_conformance(AdapterId::Cursor, input).expect("adapter should parse");
        assert_eq!(report.adapter, AdapterId::Cursor);
        assert!(report.event_count >= 7);
        assert!(report.issues.is_empty(), "issues={:?}", report.issues);
        assert_eq!(report.coverage.tool, CoverageGrade::Full);
        assert_eq!(report.coverage.read, CoverageGrade::Partial);
        assert_eq!(report.coverage.edit, CoverageGrade::Partial);
    }

    #[test]
    fn conformance_flags_invalid_meta_coverage_values() {
        let input = r#"{"timestamp":"2026-02-22T00:00:00Z","type":"session_meta","payload":{"model_provider":"openai","git":{"commit_hash":"abc123"}}}"#;
        let mut normalized =
            super::convert_with_adapter(AdapterId::CodexCli, input).expect("adapter should parse");
        normalized = normalized.replace(
            "\"coverage.read\":\"partial\"",
            "\"coverage.read\":\"PARTIAL\"",
        );

        let mut issues = Vec::new();
        for (idx, line) in normalized.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let row: serde_json::Value =
                serde_json::from_str(line).expect("normalized event should parse");
            super::validate_contract_row(idx + 1, &row, &mut issues);
        }

        assert!(
            issues
                .iter()
                .any(|issue| issue.detail.contains("must be one of `full|partial|none`")),
            "issues={issues:?}"
        );
    }

    #[test]
    fn conformance_flags_non_string_source_session_id() {
        let row = serde_json::json!({
            "t": "2026-02-22T00:00:00Z",
            "k": "msg.out",
            "source": {
                "harness": "claude-code",
                "session_id": 7
            }
        });
        let mut issues = Vec::new();
        super::validate_contract_row(1, &row, &mut issues);
        assert!(
            issues
                .iter()
                .any(|issue| issue.detail.contains("source.session_id")),
            "issues={issues:?}"
        );
    }

    #[test]
    fn registry_covers_all_known_adapters() {
        assert_eq!(adapter_registry().len(), 6);
    }

    #[test]
    fn discovery_scaffold_expands_home() {
        let paths = discovery_scaffold(AdapterId::CodexCli, Path::new("/home/tester"));
        assert!(paths.iter().any(|path| {
            path.to_string_lossy()
                .contains("/home/tester/.codex/sessions")
        }));
    }

    use std::path::Path;
}

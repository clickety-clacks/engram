use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::harness::{claude_jsonl_to_tape_jsonl, codex_jsonl_to_tape_jsonl};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AdapterId {
    ClaudeCode,
    CodexCli,
    OpenCode,
    GeminiCli,
    Cursor,
}

impl AdapterId {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::CodexCli => "codex-cli",
            Self::OpenCode => "opencode",
            Self::GeminiCli => "gemini-cli",
            Self::Cursor => "cursor",
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
            status: AdapterStatus::DiscoveryRequired,
            artifact_path_templates: &["TODO: discovery required"],
            schema_sample_set: &["TODO: discovery required"],
            mapping_table: &[MappingRule {
                source: "TODO: discovery required",
                target: "TODO: event-contract mapping",
                note: "deterministic mapping table pending",
            }],
            coverage: CoverageGrades {
                read: CoverageGrade::None,
                edit: CoverageGrade::None,
                tool: CoverageGrade::None,
            },
        },
        AdapterDescriptor {
            id: AdapterId::GeminiCli,
            status: AdapterStatus::DiscoveryRequired,
            artifact_path_templates: &["TODO: discovery required"],
            schema_sample_set: &["TODO: discovery required"],
            mapping_table: &[MappingRule {
                source: "TODO: discovery required",
                target: "TODO: event-contract mapping",
                note: "deterministic mapping table pending",
            }],
            coverage: CoverageGrades {
                read: CoverageGrade::None,
                edit: CoverageGrade::None,
                tool: CoverageGrade::None,
            },
        },
        AdapterDescriptor {
            id: AdapterId::Cursor,
            status: AdapterStatus::DiscoveryRequired,
            artifact_path_templates: &["TODO: discovery required"],
            schema_sample_set: &["TODO: discovery required"],
            mapping_table: &[MappingRule {
                source: "TODO: discovery required",
                target: "TODO: event-contract mapping",
                note: "deterministic mapping table pending",
            }],
            coverage: CoverageGrades {
                read: CoverageGrade::None,
                edit: CoverageGrade::None,
                tool: CoverageGrade::None,
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

pub fn convert_with_adapter(id: AdapterId, input: &str) -> Result<String, AdapterError> {
    match id {
        AdapterId::ClaudeCode => ClaudeCodeAdapter.convert_to_tape_jsonl(input),
        AdapterId::CodexCli => CodexCliAdapter.convert_to_tape_jsonl(input),
        AdapterId::OpenCode | AdapterId::GeminiCli | AdapterId::Cursor => {
            Ok(discovery_adapter_jsonl(id, input)?)
        }
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

    for (idx, line) in normalized.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }

        event_count += 1;
        let row: Value = serde_json::from_str(line)?;
        validate_contract_row(idx + 1, &row, &mut issues);
    }

    Ok(ConformanceReport {
        adapter: id,
        event_count,
        coverage: descriptor_for(id).coverage,
        issues,
    })
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

fn discovery_adapter_jsonl(id: AdapterId, input: &str) -> Result<String, serde_json::Error> {
    let mut first_timestamp = "1970-01-01T00:00:00Z".to_string();
    for line in input.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let row: Value = serde_json::from_str(line)?;
        if let Some(ts) = row.get("timestamp").and_then(Value::as_str) {
            first_timestamp = ts.to_string();
            break;
        }
        if let Some(ts) = row.get("t").and_then(Value::as_str) {
            first_timestamp = ts.to_string();
            break;
        }
    }

    serde_json::to_string(&json!({
        "t": first_timestamp,
        "k": "meta",
        "source": {"harness": id.as_str()},
        "coverage.read": "none",
        "coverage.edit": "none",
        "coverage.tool": "none"
    }))
    .map(|line| format!("{line}\n"))
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
        assert!(report.event_count >= 3);
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
        for adapter in [AdapterId::OpenCode, AdapterId::GeminiCli, AdapterId::Cursor] {
            let descriptor = descriptor_for(adapter);
            assert_eq!(descriptor.status, AdapterStatus::DiscoveryRequired);
            assert!(!descriptor.artifact_path_templates.is_empty());
            assert!(!descriptor.schema_sample_set.is_empty());
            assert!(!descriptor.mapping_table.is_empty());
            assert_eq!(descriptor.coverage.tool, CoverageGrade::None);
            assert_eq!(descriptor.coverage.read, CoverageGrade::None);
            assert_eq!(descriptor.coverage.edit, CoverageGrade::None);
        }
    }

    #[test]
    fn discovery_required_adapters_emit_deterministic_meta_with_none_coverage() {
        for adapter in [AdapterId::OpenCode, AdapterId::GeminiCli, AdapterId::Cursor] {
            let report = run_conformance(adapter, "{}\n").expect("adapter should normalize");
            assert_eq!(report.event_count, 1);
            assert!(report.issues.is_empty(), "issues={:?}", report.issues);
            assert_eq!(report.coverage.tool, CoverageGrade::None);
            assert_eq!(report.coverage.read, CoverageGrade::None);
            assert_eq!(report.coverage.edit, CoverageGrade::None);
        }
    }

    #[test]
    fn registry_covers_all_known_adapters() {
        assert_eq!(adapter_registry().len(), 5);
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

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use serde_json::Value;
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

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
            artifact_path_templates: &["~/.openclaw/sessions/**/*.jsonl", "~/.openclaw/logs/*.log"],
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

fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = out.pop();
            }
            Component::RootDir | Component::Prefix(_) | Component::Normal(_) => {
                out.push(component.as_os_str())
            }
        }
    }
    out
}

fn canonicalize_or_normalize(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| normalize_path(path))
}

fn sorted_unique(mut paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths.sort();
    paths.dedup();
    paths
}

fn list_files_by_extension_recursive(root: &Path, extension: &str) -> Vec<PathBuf> {
    if !root.exists() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for entry in WalkDir::new(root).into_iter().filter_map(Result::ok) {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if path
            .extension()
            .and_then(|value| value.to_str())
            .is_some_and(|value| value.eq_ignore_ascii_case(extension))
        {
            out.push(path.to_path_buf());
        }
    }
    sorted_unique(out)
}

fn list_files_by_extension_shallow(root: &Path, extension: &str) -> Vec<PathBuf> {
    if !root.exists() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path
            .extension()
            .and_then(|value| value.to_str())
            .is_some_and(|value| value.eq_ignore_ascii_case(extension))
        {
            out.push(path);
        }
    }
    sorted_unique(out)
}

fn path_contains_component(path: &Path, component: &str) -> bool {
    path.components().any(|part| {
        part.as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case(component)
    })
}

fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

fn read_first_matching_codex_cwd(path: &Path) -> Option<PathBuf> {
    let file = fs::File::open(path).ok()?;
    let reader = BufReader::new(file);
    for line in reader.lines().map_while(Result::ok) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let row: Value = serde_json::from_str(trimmed).ok()?;
        if row.get("type").and_then(Value::as_str) != Some("session_meta") {
            continue;
        }
        let cwd = row
            .get("payload")
            .and_then(Value::as_object)
            .and_then(|payload| payload.get("cwd"))
            .and_then(Value::as_str)?;
        return Some(canonicalize_or_normalize(Path::new(cwd)));
    }
    None
}

fn path_matches_repo_scope(candidate: &Path, repo_path: &Path) -> bool {
    let candidate = canonicalize_or_normalize(candidate);
    let repo = canonicalize_or_normalize(repo_path);
    candidate == repo || candidate.starts_with(&repo) || repo.starts_with(&candidate)
}

fn discover_codex_sessions(repo_path: &Path, home_dir: &Path) -> Vec<PathBuf> {
    let sessions_root = home_dir.join(".codex").join("sessions");
    if !sessions_root.exists() {
        return Vec::new();
    }
    let repo = canonicalize_or_normalize(repo_path);
    let mut out = Vec::new();
    for candidate in list_files_by_extension_recursive(&sessions_root, "jsonl") {
        let Some(session_cwd) = read_first_matching_codex_cwd(&candidate) else {
            continue;
        };
        if path_matches_repo_scope(&session_cwd, &repo) {
            out.push(candidate);
        }
    }
    sorted_unique(out)
}

fn discover_claude_sessions(repo_path: &Path, home_dir: &Path) -> Vec<PathBuf> {
    let repo = canonicalize_or_normalize(repo_path);
    let key = repo.to_string_lossy().replace('/', "-");
    let project_root = home_dir.join(".claude").join("projects").join(key);
    if !project_root.exists() {
        return Vec::new();
    }
    let mut out = list_files_by_extension_shallow(&project_root, "jsonl");
    for candidate in list_files_by_extension_recursive(&project_root, "jsonl") {
        if path_contains_component(&candidate, "memory") {
            continue;
        }
        if path_contains_component(&candidate, "subagents") {
            out.push(candidate);
        }
    }
    sorted_unique(out)
}

fn discover_gemini_sessions(repo_path: &Path, home_dir: &Path) -> Vec<PathBuf> {
    let repo = canonicalize_or_normalize(repo_path);
    let project_hash = sha256_hex(&repo.to_string_lossy());
    let bucket = home_dir.join(".gemini").join("tmp").join(project_hash);
    if !bucket.exists() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let chats = bucket.join("chats");
    if chats.exists() {
        for candidate in list_files_by_extension_shallow(&chats, "json") {
            let name = candidate
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("");
            if name.starts_with("session-") {
                out.push(candidate);
            }
        }
    }
    let logs = bucket.join("logs.json");
    if logs.is_file() {
        out.push(logs);
    }
    sorted_unique(out)
}

fn read_matches_repo_hint(path: &Path, repo_path: &Path) -> bool {
    let Ok(file) = fs::File::open(path) else {
        return false;
    };
    let reader = BufReader::new(file);
    let repo = canonicalize_or_normalize(repo_path);
    let repo_text = repo.to_string_lossy();
    for line in reader.lines().map_while(Result::ok).take(80) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(row) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        if value_contains_repo_path_hint(&row, &repo_text) {
            return true;
        }
    }
    false
}

fn value_contains_repo_path_hint(value: &Value, repo_text: &str) -> bool {
    match value {
        Value::String(text) => {
            text == repo_text
                || text.starts_with(repo_text)
                || repo_text.starts_with(text)
                || text.contains(repo_text)
        }
        Value::Array(items) => items
            .iter()
            .any(|item| value_contains_repo_path_hint(item, repo_text)),
        Value::Object(map) => map
            .iter()
            .filter(|(key, _)| {
                let lower = key.to_ascii_lowercase();
                lower.contains("cwd")
                    || lower.contains("repo")
                    || lower.contains("workspace")
                    || lower.contains("path")
            })
            .any(|(_, nested)| value_contains_repo_path_hint(nested, repo_text)),
        _ => false,
    }
}

fn discover_openclaw_sessions(repo_path: &Path, home_dir: &Path) -> Vec<PathBuf> {
    let sessions_root = home_dir.join(".openclaw").join("sessions");
    if !sessions_root.exists() {
        return Vec::new();
    }
    let repo = canonicalize_or_normalize(repo_path);
    let repo_text = repo.to_string_lossy().to_string();
    let repo_dash = repo_text.replace('/', "-");
    let repo_hash = sha256_hex(&repo_text);
    let mut out = Vec::new();
    for candidate in list_files_by_extension_recursive(&sessions_root, "jsonl") {
        let path_text = candidate.to_string_lossy();
        if path_text.contains(&repo_dash) || path_text.contains(&repo_hash) {
            out.push(candidate);
            continue;
        }
        if read_matches_repo_hint(&candidate, &repo) {
            out.push(candidate);
        }
    }
    sorted_unique(out)
}

fn opencode_data_root(home_dir: &Path) -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(xdg).join("opencode");
    }
    home_dir.join(".local").join("share").join("opencode")
}

fn run_git(repo_path: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn resolve_opencode_project_id(repo_path: &Path) -> String {
    let Some(top_level) = run_git(repo_path, &["rev-parse", "--show-toplevel"]) else {
        return "global".to_string();
    };
    let repo_root = PathBuf::from(top_level);
    let common_dir = run_git(&repo_root, &["rev-parse", "--git-common-dir"])
        .map(PathBuf::from)
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                repo_root.join(path)
            }
        })
        .unwrap_or_else(|| repo_root.join(".git"));
    let cache_file = common_dir.join("opencode");
    if let Ok(content) = fs::read_to_string(&cache_file) {
        let trimmed = content.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }

    let Some(root_commits) = run_git(&repo_root, &["rev-list", "--max-parents=0", "--all"]) else {
        return "global".to_string();
    };
    let mut roots = root_commits
        .lines()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    roots.sort();
    roots
        .into_iter()
        .next()
        .unwrap_or_else(|| "global".to_string())
}

fn discover_opencode_sessions(repo_path: &Path, home_dir: &Path) -> Vec<PathBuf> {
    let data_root = opencode_data_root(home_dir);
    if !data_root.exists() {
        return Vec::new();
    }
    let project_id = resolve_opencode_project_id(repo_path);
    let mut out = Vec::new();

    let current_bucket = data_root
        .join("project")
        .join(&project_id)
        .join("storage")
        .join("session");
    out.extend(list_files_by_extension_recursive(
        &current_bucket.join("info"),
        "json",
    ));
    out.extend(list_files_by_extension_recursive(
        &current_bucket.join("message"),
        "json",
    ));
    out.extend(list_files_by_extension_recursive(
        &current_bucket.join("part"),
        "json",
    ));

    let legacy_bucket = data_root.join("storage");
    out.extend(list_files_by_extension_recursive(
        &legacy_bucket.join("session").join(&project_id),
        "json",
    ));

    sorted_unique(out)
}

fn cursor_workspace_storage_roots(home_dir: &Path) -> Vec<PathBuf> {
    let mut roots = vec![
        home_dir
            .join("Library")
            .join("Application Support")
            .join("Cursor")
            .join("User")
            .join("workspaceStorage"),
        home_dir
            .join(".config")
            .join("Cursor")
            .join("User")
            .join("workspaceStorage"),
    ];
    if let Some(app_data) = std::env::var_os("APPDATA") {
        roots.push(
            PathBuf::from(app_data)
                .join("Cursor")
                .join("User")
                .join("workspaceStorage"),
        );
    }
    sorted_unique(roots)
}

fn normalize_workspace_manifest_path(raw: &str) -> Option<PathBuf> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let without_scheme = if let Some(rest) = trimmed.strip_prefix("file://") {
        rest
    } else {
        trimmed
    };
    let candidate = PathBuf::from(without_scheme);
    if !candidate.is_absolute() {
        return None;
    }
    Some(canonicalize_or_normalize(&candidate))
}

fn collect_workspace_paths_from_manifest(value: &Value, out: &mut Vec<PathBuf>) {
    match value {
        Value::Object(map) => {
            for (key, nested) in map {
                let lower = key.to_ascii_lowercase();
                if matches!(nested, Value::String(_))
                    && (lower.contains("folder")
                        || lower.contains("path")
                        || lower.contains("workspace")
                        || lower.contains("uri"))
                {
                    if let Some(text) = nested.as_str()
                        && let Some(path) = normalize_workspace_manifest_path(text)
                    {
                        out.push(path);
                    }
                }
                collect_workspace_paths_from_manifest(nested, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_workspace_paths_from_manifest(item, out);
            }
        }
        _ => {}
    }
}

fn workspace_manifest_matches_repo(workspace_json: &Path, repo_path: &Path) -> bool {
    let Ok(content) = fs::read_to_string(workspace_json) else {
        return false;
    };
    let Ok(json) = serde_json::from_str::<Value>(&content) else {
        return false;
    };
    let repo = canonicalize_or_normalize(repo_path);
    let mut candidates = Vec::new();
    collect_workspace_paths_from_manifest(&json, &mut candidates);
    candidates.into_iter().any(|candidate| candidate == repo)
}

fn discover_cursor_sessions(repo_path: &Path, home_dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for root in cursor_workspace_storage_roots(home_dir) {
        if !root.exists() {
            continue;
        }
        let Ok(entries) = fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let workspace_dir = entry.path();
            if !workspace_dir.is_dir() {
                continue;
            }
            let workspace_json = workspace_dir.join("workspace.json");
            if !workspace_json.is_file() {
                continue;
            }
            if !workspace_manifest_matches_repo(&workspace_json, repo_path) {
                continue;
            }
            let primary = workspace_dir.join("state.vscdb");
            if primary.is_file() {
                out.push(primary);
                continue;
            }
            let backup = workspace_dir.join("state.vscdb.backup");
            if backup.is_file() {
                out.push(backup);
            }
        }
    }
    sorted_unique(out)
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

    fn discover_sessions_for_repo(&self, repo_path: &Path, home_dir: &Path) -> Vec<PathBuf> {
        let _ = (repo_path, home_dir);
        vec![]
    }

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

    fn discover_sessions_for_repo(&self, repo_path: &Path, home_dir: &Path) -> Vec<PathBuf> {
        discover_claude_sessions(repo_path, home_dir)
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

    fn discover_sessions_for_repo(&self, repo_path: &Path, home_dir: &Path) -> Vec<PathBuf> {
        discover_codex_sessions(repo_path, home_dir)
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

    fn discover_sessions_for_repo(&self, repo_path: &Path, home_dir: &Path) -> Vec<PathBuf> {
        discover_opencode_sessions(repo_path, home_dir)
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

    fn discover_sessions_for_repo(&self, repo_path: &Path, home_dir: &Path) -> Vec<PathBuf> {
        discover_cursor_sessions(repo_path, home_dir)
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

    fn discover_sessions_for_repo(&self, repo_path: &Path, home_dir: &Path) -> Vec<PathBuf> {
        discover_gemini_sessions(repo_path, home_dir)
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

    fn discover_sessions_for_repo(&self, repo_path: &Path, home_dir: &Path) -> Vec<PathBuf> {
        discover_openclaw_sessions(repo_path, home_dir)
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

pub fn discover_sessions_with_adapter(
    id: AdapterId,
    repo_path: &Path,
    home_dir: &Path,
) -> Vec<PathBuf> {
    // TODO: Replace this adapter-id dispatch once discovery is wired through a
    // native adapter registry with concrete discovery implementations.
    match id {
        AdapterId::ClaudeCode => ClaudeCodeAdapter.discover_sessions_for_repo(repo_path, home_dir),
        AdapterId::CodexCli => CodexCliAdapter.discover_sessions_for_repo(repo_path, home_dir),
        AdapterId::OpenCode => OpenCodeAdapter.discover_sessions_for_repo(repo_path, home_dir),
        AdapterId::Cursor => CursorAdapter.discover_sessions_for_repo(repo_path, home_dir),
        AdapterId::GeminiCli => GeminiCliAdapter.discover_sessions_for_repo(repo_path, home_dir),
        AdapterId::OpenClaw => OpenClawAdapter.discover_sessions_for_repo(repo_path, home_dir),
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
        discover_sessions_with_adapter, discovery_scaffold, run_conformance,
    };
    use crate::anchor::fingerprint_anchor_hashes;
    use crate::index::SqliteIndex;
    use crate::index::lineage::{EvidenceKind, LINK_THRESHOLD_DEFAULT};
    use crate::tape::event::{TapeEventData, parse_jsonl_events};
    use std::fs;
    use std::process::Command;

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
    fn claude_conformance_ingest_stores_multiple_windowed_evidence_rows_for_single_edit() {
        let content = (1..=72)
            .map(|line| format!("fn line_{line}() {{ value_{line}(); }}\n"))
            .collect::<String>();
        let content_json = serde_json::to_string(&content).expect("escaped content");
        let input = format!(
            concat!(
                "{{\"type\":\"assistant\",\"timestamp\":\"2026-02-22T00:00:00Z\",",
                "\"message\":{{\"role\":\"assistant\",\"content\":[{{",
                "\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"Write\",",
                "\"input\":{{\"file_path\":\"/repo/src/lib.rs\",\"content\":{0}}}",
                "}}]}}}}"
            ),
            content_json
        );

        let normalized =
            super::convert_with_adapter(AdapterId::ClaudeCode, &input).expect("adapter output");
        let events = parse_jsonl_events(&normalized).expect("normalized events");
        let edit = events
            .iter()
            .find_map(|item| match &item.event.data {
                TapeEventData::CodeEdit(edit) => Some((item.offset, edit)),
                _ => None,
            })
            .expect("code.edit event");

        let anchors = fingerprint_anchor_hashes(
            edit.1
                .after_text
                .as_deref()
                .expect("adapter should emit after_text"),
        );
        assert!(
            anchors.len() >= 3,
            "expected overlapping window anchors, got {anchors:?}"
        );

        let index = SqliteIndex::open_in_memory().expect("sqlite");
        index
            .ingest_tape_events("tape", &events, LINK_THRESHOLD_DEFAULT)
            .expect("ingest");

        for anchor in &anchors {
            let evidence = index
                .evidence_for_anchor(anchor)
                .expect("evidence query should succeed");
            assert_eq!(evidence.len(), 1, "anchor={anchor} evidence={evidence:?}");
            assert_eq!(evidence[0].event_offset, edit.0);
            assert_eq!(evidence[0].kind, EvidenceKind::Edit);
        }
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
    fn cursor_discovery_returns_state_db_for_matching_workspace_manifest() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = temp.path().join("home");
        let repo = temp.path().join("repo");
        let other_repo = temp.path().join("other-repo");
        fs::create_dir_all(&repo).expect("repo");
        fs::create_dir_all(&other_repo).expect("other repo");
        let canonical_repo = super::canonicalize_or_normalize(&repo);
        let workspace_root = home.join("Library/Application Support/Cursor/User/workspaceStorage");
        let matching_dir = workspace_root.join("aaa");
        let other_dir = workspace_root.join("bbb");
        fs::create_dir_all(&matching_dir).expect("matching dir");
        fs::create_dir_all(&other_dir).expect("other dir");
        fs::write(
            matching_dir.join("workspace.json"),
            format!("{{\"folder\":\"{}\"}}\n", canonical_repo.to_string_lossy()),
        )
        .expect("matching workspace");
        fs::write(
            other_dir.join("workspace.json"),
            "{\"folder\":\"/tmp/other\"}\n",
        )
        .expect("other workspace");
        let state = matching_dir.join("state.vscdb");
        fs::write(&state, "sqlite").expect("state db");
        fs::write(other_dir.join("state.vscdb"), "sqlite").expect("other state");

        let discovered = discover_sessions_with_adapter(AdapterId::Cursor, &repo, &home);
        assert_eq!(discovered, vec![state]);
        let wrong_repo = discover_sessions_with_adapter(AdapterId::Cursor, &other_repo, &home);
        assert!(wrong_repo.is_empty(), "wrong_repo={wrong_repo:?}");
    }

    #[test]
    fn cursor_discovery_supports_file_uri_manifest_path() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = temp.path().join("home");
        let repo = temp.path().join("repo");
        let other_repo = temp.path().join("other-repo");
        fs::create_dir_all(&repo).expect("repo");
        fs::create_dir_all(&other_repo).expect("other repo");
        let canonical_repo = super::canonicalize_or_normalize(&repo);
        let workspace_dir =
            home.join("Library/Application Support/Cursor/User/workspaceStorage/abc");
        fs::create_dir_all(&workspace_dir).expect("workspace dir");
        fs::write(
            workspace_dir.join("workspace.json"),
            format!(
                "{{\"workspaceUri\":\"file://{}\"}}\n",
                canonical_repo.to_string_lossy()
            ),
        )
        .expect("workspace json");
        let state = workspace_dir.join("state.vscdb");
        fs::write(&state, "sqlite").expect("state db");

        let discovered = discover_sessions_with_adapter(AdapterId::Cursor, &repo, &home);
        assert_eq!(discovered, vec![state]);
        let wrong_repo = discover_sessions_with_adapter(AdapterId::Cursor, &other_repo, &home);
        assert!(wrong_repo.is_empty(), "wrong_repo={wrong_repo:?}");
    }

    #[test]
    fn cursor_discovery_uses_backup_when_primary_missing() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = temp.path().join("home");
        let repo = temp.path().join("repo");
        let other_repo = temp.path().join("other-repo");
        fs::create_dir_all(&repo).expect("repo");
        fs::create_dir_all(&other_repo).expect("other repo");
        let canonical_repo = super::canonicalize_or_normalize(&repo);
        let workspace_dir =
            home.join("Library/Application Support/Cursor/User/workspaceStorage/backup");
        fs::create_dir_all(&workspace_dir).expect("workspace dir");
        fs::write(
            workspace_dir.join("workspace.json"),
            format!(
                "{{\"workspacePath\":\"{}\"}}\n",
                canonical_repo.to_string_lossy()
            ),
        )
        .expect("workspace json");
        let backup = workspace_dir.join("state.vscdb.backup");
        fs::write(&backup, "sqlite").expect("backup db");

        let discovered = discover_sessions_with_adapter(AdapterId::Cursor, &repo, &home);
        assert_eq!(discovered, vec![backup]);
        let wrong_repo = discover_sessions_with_adapter(AdapterId::Cursor, &other_repo, &home);
        assert!(wrong_repo.is_empty(), "wrong_repo={wrong_repo:?}");
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

    #[test]
    fn claude_discovery_finds_repo_bucket_sessions_and_subagents() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = temp.path().join("home");
        let repo = temp.path().join("repo");
        let other_repo = temp.path().join("other-repo");
        fs::create_dir_all(&repo).expect("repo");
        fs::create_dir_all(&other_repo).expect("other repo");
        let canonical_repo = super::canonicalize_or_normalize(&repo);
        let project_key = canonical_repo.to_string_lossy().replace('/', "-");
        let claude_root = home.join(".claude/projects").join(project_key);
        let session_root = claude_root.join("session-a");
        let root_jsonl = claude_root.join("main.jsonl");
        let subagent_jsonl = session_root.join("subagents/sub.jsonl");
        fs::create_dir_all(subagent_jsonl.parent().expect("parent")).expect("subagents");
        fs::create_dir_all(claude_root.join("memory")).expect("memory");
        fs::create_dir_all(session_root.join("tool-results")).expect("tool-results");
        fs::write(&root_jsonl, "{\"type\":\"assistant\"}\n").expect("root");
        fs::write(&subagent_jsonl, "{\"type\":\"assistant\"}\n").expect("sub");
        fs::write(
            claude_root.join("memory/ignore.jsonl"),
            "{\"type\":\"assistant\"}\n",
        )
        .expect("ignore");
        fs::write(session_root.join("tool-results/out.txt"), "stdout").expect("txt");

        let discovered = discover_sessions_with_adapter(AdapterId::ClaudeCode, &repo, &home);
        assert_eq!(discovered, vec![root_jsonl, subagent_jsonl]);
        let wrong_repo = discover_sessions_with_adapter(AdapterId::ClaudeCode, &other_repo, &home);
        assert!(wrong_repo.is_empty(), "wrong_repo={wrong_repo:?}");
    }

    #[test]
    fn codex_discovery_filters_by_session_meta_cwd() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = temp.path().join("home");
        let repo = temp.path().join("repo");
        let other_repo = temp.path().join("other-repo");
        fs::create_dir_all(&repo).expect("repo");
        fs::create_dir_all(&other_repo).expect("other repo");
        let codex_root = home.join(".codex/sessions/2026/03/10");
        fs::create_dir_all(&codex_root).expect("codex root");
        let matching = codex_root.join("matching.jsonl");
        let other = codex_root.join("other.jsonl");
        fs::write(
            &matching,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"cwd\":\"{}\"}}}}\n",
                repo.to_string_lossy()
            ),
        )
        .expect("matching");
        fs::write(
            &other,
            "{\"type\":\"session_meta\",\"payload\":{\"cwd\":\"/tmp/other\"}}\n",
        )
        .expect("other");

        let discovered = discover_sessions_with_adapter(AdapterId::CodexCli, &repo, &home);
        assert_eq!(discovered, vec![matching]);
        let wrong_repo = discover_sessions_with_adapter(AdapterId::CodexCli, &other_repo, &home);
        assert!(wrong_repo.is_empty(), "wrong_repo={wrong_repo:?}");
    }

    #[test]
    fn gemini_discovery_uses_repo_hash_bucket() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = temp.path().join("home");
        let repo = temp.path().join("repo");
        let other_repo = temp.path().join("other-repo");
        fs::create_dir_all(&repo).expect("repo");
        fs::create_dir_all(&other_repo).expect("other repo");
        let repo_hash =
            super::sha256_hex(&super::canonicalize_or_normalize(&repo).to_string_lossy());
        let bucket = home.join(".gemini/tmp").join(repo_hash);
        let chat = bucket.join("chats/session-2026.json");
        let logs = bucket.join("logs.json");
        fs::create_dir_all(chat.parent().expect("chat parent")).expect("chat dir");
        fs::write(&chat, "{\"sessionId\":\"a\"}\n").expect("chat");
        fs::write(&logs, "[]\n").expect("logs");
        let other_bucket = home.join(".gemini/tmp").join("other").join("chats");
        fs::create_dir_all(&other_bucket).expect("other");
        fs::write(
            other_bucket.join("session-ignore.json"),
            "{\"sessionId\":\"b\"}\n",
        )
        .expect("other session");

        let discovered = discover_sessions_with_adapter(AdapterId::GeminiCli, &repo, &home);
        assert_eq!(discovered, vec![chat, logs]);
        let wrong_repo = discover_sessions_with_adapter(AdapterId::GeminiCli, &other_repo, &home);
        assert!(wrong_repo.is_empty(), "wrong_repo={wrong_repo:?}");
    }

    #[test]
    fn openclaw_discovery_matches_repo_key_or_content_hint() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = temp.path().join("home");
        let repo = temp.path().join("repo");
        let other_repo = temp.path().join("other-repo");
        fs::create_dir_all(&repo).expect("repo");
        fs::create_dir_all(&other_repo).expect("other repo");
        let canonical_repo = super::canonicalize_or_normalize(&repo);
        let repo_dash = canonical_repo.to_string_lossy().replace('/', "-");
        let sessions = home.join(".openclaw/sessions");
        let by_path = sessions.join(&repo_dash).join("a.jsonl");
        let by_content = sessions.join("misc").join("b.jsonl");
        let unrelated = sessions.join("misc").join("c.jsonl");
        fs::create_dir_all(by_path.parent().expect("parent")).expect("by-path dir");
        fs::create_dir_all(by_content.parent().expect("parent")).expect("by-content dir");
        fs::write(&by_path, "{\"type\":\"message\"}\n").expect("by path");
        fs::write(
            &by_content,
            format!(
                "{{\"type\":\"meta\",\"cwd\":\"{}\"}}\n",
                canonical_repo.to_string_lossy()
            ),
        )
        .expect("by content");
        fs::write(&unrelated, "{\"type\":\"meta\",\"cwd\":\"/tmp/other\"}\n").expect("other");

        let discovered = discover_sessions_with_adapter(AdapterId::OpenClaw, &repo, &home);
        assert_eq!(discovered, vec![by_path, by_content]);
        let wrong_repo = discover_sessions_with_adapter(AdapterId::OpenClaw, &other_repo, &home);
        assert!(wrong_repo.is_empty(), "wrong_repo={wrong_repo:?}");
    }

    #[test]
    fn opencode_discovery_uses_project_id_bucket_and_legacy_session_bucket() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = temp.path().join("home");
        let repo = temp.path().join("repo");
        let other_repo = temp.path().join("other-repo");
        fs::create_dir_all(&repo).expect("repo");
        fs::create_dir_all(&other_repo).expect("other repo");
        let git_init_ok = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .arg("init")
            .output()
            .map(|output| output.status.success())
            .expect("git init command should run");
        assert!(git_init_ok, "git init should succeed");
        let project_id = "proj123";
        fs::write(repo.join(".git/opencode"), format!("{project_id}\n")).expect("opencode cache");

        let data_root = home.join(".local/share/opencode");
        let current_info = data_root.join(format!(
            "project/{project_id}/storage/session/info/info.json"
        ));
        let current_message = data_root.join(format!(
            "project/{project_id}/storage/session/message/sess-a/msg.json"
        ));
        let current_part = data_root.join(format!(
            "project/{project_id}/storage/session/part/sess-a/part.json"
        ));
        let legacy_session = data_root.join(format!("storage/session/{project_id}/legacy.json"));
        let other_project = data_root.join("project/other/storage/session/info/ignore.json");
        for path in [
            &current_info,
            &current_message,
            &current_part,
            &legacy_session,
            &other_project,
        ] {
            fs::create_dir_all(path.parent().expect("parent")).expect("mkdirs");
            fs::write(path, "{}\n").expect("write");
        }

        let discovered = discover_sessions_with_adapter(AdapterId::OpenCode, &repo, &home);
        assert_eq!(
            discovered,
            vec![current_info, current_message, current_part, legacy_session]
        );
        let wrong_repo = discover_sessions_with_adapter(AdapterId::OpenCode, &other_repo, &home);
        assert!(wrong_repo.is_empty(), "wrong_repo={wrong_repo:?}");
    }

    use std::path::Path;
}

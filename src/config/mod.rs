use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdapterChoice {
    Auto,
    Claude,
    Codex,
    Cursor,
    Gemini,
    OpenCode,
    OpenClaw,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceSpec {
    pub path: String,
    pub adapter: AdapterChoice,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveConfig {
    pub sources: Vec<SourceSpec>,
    pub exclude: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RawConfig {
    #[serde(default)]
    sources: Option<Vec<RawSourceSpec>>,
    #[serde(default)]
    exclude: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct RawSourceSpec {
    path: String,
    #[serde(default = "default_adapter")]
    adapter: String,
}

fn default_adapter() -> String {
    String::new()
}

impl RawSourceSpec {
    fn into_source(self) -> Result<SourceSpec, ConfigError> {
        let adapter = if self.adapter.is_empty() {
            AdapterChoice::Auto
        } else {
            parse_adapter_choice(&self.adapter)?
        };
        Ok(SourceSpec {
            path: self.path,
            adapter,
        })
    }
}

#[derive(Debug)]
pub enum ConfigError {
    Io(std::io::Error),
    Yaml(serde_yaml::Error),
    InvalidAdapter(String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(err) => write!(f, "{err}"),
            Self::Yaml(err) => write!(f, "{err}"),
            Self::InvalidAdapter(value) => write!(f, "unknown adapter `{value}`"),
        }
    }
}

impl std::error::Error for ConfigError {}

impl From<std::io::Error> for ConfigError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_yaml::Error> for ConfigError {
    fn from(value: serde_yaml::Error) -> Self {
        Self::Yaml(value)
    }
}

#[derive(Debug)]
struct ConfigLayer {
    sources: Vec<SourceSpec>,
    exclude: Option<Vec<String>>,
}

pub fn load_effective_config(
    cwd: &Path,
    repo_config: Option<&Path>,
    user_config: Option<&Path>,
) -> Result<EffectiveConfig, ConfigError> {
    let mut merged = EffectiveConfig {
        sources: Vec::new(),
        exclude: Vec::new(),
    };

    if let Some(path) = user_config.filter(|path| path.exists()) {
        let cfg = load_config_layer(path)?;
        merge_layer(&mut merged, cfg);
    }

    if let Some(path) = find_nearest_project_config(cwd) {
        let cfg = load_config_layer(&path)?;
        merge_layer(&mut merged, cfg);
    }

    if let Some(path) = repo_config.filter(|path| path.exists()) {
        let cfg = load_config_layer(path)?;
        merge_layer(&mut merged, cfg);
    }

    Ok(merged)
}

pub fn find_nearest_project_config(start: &Path) -> Option<PathBuf> {
    for dir in start.ancestors() {
        let candidate = dir.join(".engram.project.yml");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn merge_layer(merged: &mut EffectiveConfig, layer: ConfigLayer) {
    merge_sources_dedup(&mut merged.sources, layer.sources);
    if let Some(exclude) = layer.exclude {
        merged.exclude = exclude;
    }
}

fn merge_sources_dedup(existing: &mut Vec<SourceSpec>, incoming: Vec<SourceSpec>) {
    let mut indices = HashMap::new();
    for (idx, source) in existing.iter().enumerate() {
        indices.insert(source.path.clone(), idx);
    }

    for source in incoming {
        if let Some(idx) = indices.get(&source.path).copied() {
            existing[idx] = source;
        } else {
            let idx = existing.len();
            indices.insert(source.path.clone(), idx);
            existing.push(source);
        }
    }
}

fn load_config_layer(path: &Path) -> Result<ConfigLayer, ConfigError> {
    let content = fs::read_to_string(path)?;
    parse_config_layer(&content)
}

fn parse_config_layer(content: &str) -> Result<ConfigLayer, ConfigError> {
    let raw: RawConfig = serde_yaml::from_str(content)?;
    let raw_sources = raw.sources.unwrap_or_default();
    let mut sources = Vec::with_capacity(raw_sources.len());
    for source in raw_sources {
        sources.push(source.into_source()?);
    }
    Ok(ConfigLayer {
        sources,
        exclude: raw.exclude,
    })
}

pub fn load_config_file(path: &Path) -> Result<EffectiveConfig, ConfigError> {
    let content = fs::read_to_string(path)?;
    let layer = parse_config_layer(&content)?;
    Ok(EffectiveConfig {
        sources: layer.sources,
        exclude: layer.exclude.unwrap_or_default(),
    })
}

pub fn default_repo_config_yaml() -> String {
    r#"sources:
  - path: ~/.codex/sessions/**/*.jsonl
    adapter: codex
  - path: ~/.claude/projects/**/*.jsonl
    adapter: claude
exclude: []
"#
    .to_string()
}

pub fn default_global_config_yaml() -> String {
    r#"sources:
  - path: ~/.codex/sessions/**/*.jsonl
    adapter: codex
  - path: ~/.claude/projects/**/*.jsonl
    adapter: claude
  - path: ~/.openclaw/sessions/**/*.jsonl
    adapter: openclaw
exclude: []
"#
    .to_string()
}

pub fn expand_tilde(path: &str, home: &Path) -> PathBuf {
    if path == "~" {
        return home.to_path_buf();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return home.join(rest);
    }
    PathBuf::from(path)
}

fn parse_adapter_choice(raw: &str) -> Result<AdapterChoice, ConfigError> {
    let normalized = raw.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "auto" => Ok(AdapterChoice::Auto),
        "codex" => Ok(AdapterChoice::Codex),
        "claude" => Ok(AdapterChoice::Claude),
        "cursor" => Ok(AdapterChoice::Cursor),
        "gemini" => Ok(AdapterChoice::Gemini),
        "opencode" => Ok(AdapterChoice::OpenCode),
        "openclaw" => Ok(AdapterChoice::OpenClaw),
        _ => Err(ConfigError::InvalidAdapter(raw.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::{AdapterChoice, expand_tilde, load_config_file, load_effective_config};
    use std::path::Path;

    #[test]
    fn parses_source_schema_and_excludes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.yml");
        std::fs::write(
            &path,
            r#"sources:
  - path: ~/.codex/sessions/**/*.jsonl
    adapter: codex
  - path: ./logs/*.jsonl
    adapter: auto
exclude:
  - "**/private-*"
"#,
        )
        .expect("write config");

        let parsed = load_config_file(&path).expect("parse config");
        assert_eq!(parsed.sources.len(), 2);
        assert_eq!(parsed.sources[0].adapter, AdapterChoice::Codex);
        assert_eq!(parsed.sources[1].adapter, AdapterChoice::Auto);
        assert_eq!(parsed.exclude, vec!["**/private-*".to_string()]);
    }

    #[test]
    fn expands_tilde_paths() {
        let expanded = expand_tilde("~/sessions", Path::new("/home/tester"));
        assert_eq!(expanded, Path::new("/home/tester/sessions"));
    }

    #[test]
    fn merges_global_project_and_repo_with_deduped_sources() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let repo = root.join("workspace/repo");
        std::fs::create_dir_all(repo.join(".engram")).expect("repo config dir");
        std::fs::create_dir_all(root.join("home/.engram")).expect("home config dir");

        let user_cfg = root.join("home/.engram/config.yml");
        std::fs::write(
            &user_cfg,
            r#"sources:
  - path: /shared/global.jsonl
    adapter: codex
  - path: /shared/dup.jsonl
    adapter: openclaw
exclude:
  - "user-*"
"#,
        )
        .expect("write user config");

        let project_cfg = root.join("workspace/.engram.project.yml");
        std::fs::write(
            &project_cfg,
            r#"sources:
  - path: /shared/project.jsonl
    adapter: cursor
  - path: /shared/dup.jsonl
    adapter: claude
exclude:
  - "project-*"
"#,
        )
        .expect("write project config");

        let repo_cfg = repo.join(".engram/config.yml");
        std::fs::write(
            &repo_cfg,
            r#"sources:
  - path: /shared/repo.jsonl
    adapter: gemini
  - path: /shared/dup.jsonl
    adapter: codex
exclude:
  - "repo-*"
"#,
        )
        .expect("write repo config");

        let merged =
            load_effective_config(&repo, Some(&repo_cfg), Some(&user_cfg)).expect("merge config");
        assert_eq!(merged.sources.len(), 4);
        assert_eq!(merged.sources[0].path, "/shared/global.jsonl");
        assert_eq!(merged.sources[1].path, "/shared/dup.jsonl");
        assert_eq!(merged.sources[1].adapter, AdapterChoice::Codex);
        assert_eq!(merged.sources[2].path, "/shared/project.jsonl");
        assert_eq!(merged.sources[3].path, "/shared/repo.jsonl");
        assert_eq!(merged.exclude, vec!["repo-*".to_string()]);
    }

    #[test]
    fn uses_nearest_project_config_when_walking_parents() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let repo = root.join("workspace/repo");
        std::fs::create_dir_all(repo.join(".engram")).expect("repo config dir");
        std::fs::create_dir_all(root.join("home/.engram")).expect("home config dir");

        let user_cfg = root.join("home/.engram/config.yml");
        std::fs::write(
            &user_cfg,
            r#"sources:
  - path: /shared/user.jsonl
    adapter: codex
"#,
        )
        .expect("write user config");

        std::fs::write(
            root.join(".engram.project.yml"),
            r#"sources:
  - path: /shared/root-project.jsonl
    adapter: codex
"#,
        )
        .expect("write root project config");

        std::fs::write(
            root.join("workspace/.engram.project.yml"),
            r#"sources:
  - path: /shared/nearest-project.jsonl
    adapter: claude
"#,
        )
        .expect("write nearest project config");

        let merged =
            load_effective_config(&repo, None, Some(&user_cfg)).expect("merge with nearest");
        assert_eq!(merged.sources.len(), 2);
        assert_eq!(merged.sources[0].path, "/shared/user.jsonl");
        assert_eq!(merged.sources[1].path, "/shared/nearest-project.jsonl");
        assert_eq!(merged.sources[1].adapter, AdapterChoice::Claude);
    }
}

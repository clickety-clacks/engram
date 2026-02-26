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
    sources: Vec<RawSourceSpec>,
    #[serde(default)]
    exclude: Vec<String>,
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

pub fn load_effective_config(
    repo_config: Option<&Path>,
    user_config: Option<&Path>,
) -> Result<EffectiveConfig, ConfigError> {
    let mut merged = EffectiveConfig {
        sources: Vec::new(),
        exclude: Vec::new(),
    };

    if let Some(path) = user_config {
        if path.exists() {
            let cfg = load_config_file(path)?;
            merged.sources.extend(cfg.sources);
            merged.exclude.extend(cfg.exclude);
        }
    }

    if let Some(path) = repo_config {
        if path.exists() {
            let cfg = load_config_file(path)?;
            merged.sources.extend(cfg.sources);
            merged.exclude.extend(cfg.exclude);
        }
    }

    Ok(merged)
}

pub fn load_config_file(path: &Path) -> Result<EffectiveConfig, ConfigError> {
    let content = fs::read_to_string(path)?;
    let raw: RawConfig = serde_yaml::from_str(&content)?;
    let mut sources = Vec::with_capacity(raw.sources.len());
    for source in raw.sources {
        sources.push(source.into_source()?);
    }
    Ok(EffectiveConfig {
        sources,
        exclude: raw.exclude,
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
    use super::{AdapterChoice, expand_tilde, load_config_file};

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
        let expanded = expand_tilde("~/sessions", std::path::Path::new("/home/tester"));
        assert_eq!(expanded, std::path::Path::new("/home/tester/sessions"));
    }
}

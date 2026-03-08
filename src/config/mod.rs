use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::store::atomic::atomic_write;
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveConfig {
    pub path: PathBuf,
    pub db: PathBuf,
    pub additional_stores: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedConfig {
    pub db: Option<String>,
    pub additional_stores: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    db: Option<String>,
    #[serde(default)]
    additional_stores: Option<Vec<String>>,
}

#[derive(Debug)]
pub enum ConfigError {
    Io(std::io::Error),
    Yaml(serde_yaml::Error),
    InvalidPath(String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(err) => write!(f, "{err}"),
            Self::Yaml(err) => write!(f, "{err}"),
            Self::InvalidPath(value) => write!(f, "invalid path `{value}`"),
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

pub fn load_effective_config(cwd: &Path, home: &Path) -> Result<EffectiveConfig, ConfigError> {
    let user_config_path = ensure_user_config(home)?;
    let config_path = find_walkup_config(cwd, home).unwrap_or(user_config_path);
    let parsed = load_parsed_config_file(&config_path)?;
    let base_dir = config_base_dir(&config_path)?;
    let default_db = home.join(".engram").join("index.sqlite");
    let db = parsed
        .db
        .as_deref()
        .map(|raw| resolve_path(raw, &base_dir, home))
        .transpose()?
        .unwrap_or(default_db);
    let mut additional_stores = Vec::new();
    for raw in parsed.additional_stores {
        additional_stores.push(resolve_path(&raw, &base_dir, home)?);
    }

    Ok(EffectiveConfig {
        path: config_path,
        db,
        additional_stores,
    })
}

pub fn find_walkup_config(start: &Path, home: &Path) -> Option<PathBuf> {
    for dir in start.ancestors() {
        let candidate = dir.join(".engram").join("config.yml");
        if candidate.is_file() {
            return Some(candidate);
        }
        if dir == home {
            break;
        }
    }
    None
}

fn ensure_user_config(home: &Path) -> Result<PathBuf, ConfigError> {
    let user_root = home.join(".engram");
    let config_path = user_root.join("config.yml");
    if config_path.exists() {
        return Ok(config_path);
    }
    fs::create_dir_all(&user_root)?;
    atomic_write(&config_path, default_user_config_yaml().as_bytes())?;
    Ok(config_path)
}

fn config_base_dir(config_path: &Path) -> Result<PathBuf, ConfigError> {
    let config_dir = config_path.parent().ok_or_else(|| {
        ConfigError::InvalidPath(format!(
            "missing parent for config path {}",
            config_path.display()
        ))
    })?;
    let base = config_dir.parent().unwrap_or(config_dir);
    Ok(base.to_path_buf())
}

fn resolve_path(raw: &str, base_dir: &Path, home: &Path) -> Result<PathBuf, ConfigError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ConfigError::InvalidPath(raw.to_string()));
    }
    let expanded = expand_tilde(trimmed, home);
    let resolved = if expanded.is_absolute() {
        expanded
    } else {
        base_dir.join(expanded)
    };
    Ok(normalize_path(&resolved))
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

fn parse_config(content: &str) -> Result<ParsedConfig, ConfigError> {
    let raw: RawConfig = serde_yaml::from_str(content)?;
    Ok(ParsedConfig {
        db: raw.db,
        additional_stores: raw.additional_stores.unwrap_or_default(),
    })
}

pub fn load_parsed_config_file(path: &Path) -> Result<ParsedConfig, ConfigError> {
    let content = fs::read_to_string(path)?;
    parse_config(&content)
}

pub fn default_user_config_yaml() -> String {
    "db: ~/.engram/index.sqlite\n".to_string()
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

#[cfg(test)]
mod tests {
    use super::{expand_tilde, find_walkup_config, load_effective_config, load_parsed_config_file};
    use std::path::Path;

    #[test]
    fn expands_tilde_paths() {
        let expanded = expand_tilde("~/sessions", Path::new("/home/tester"));
        assert_eq!(expanded, Path::new("/home/tester/sessions"));
    }

    #[test]
    fn walk_up_uses_first_found_config_without_merging() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let home = root.join("home");
        let repo = root.join("workspace/repo");
        let child = repo.join("nested/deeper");
        std::fs::create_dir_all(child.clone()).expect("child");
        std::fs::create_dir_all(home.join(".engram")).expect("home");
        std::fs::create_dir_all(root.join("workspace/.engram")).expect("workspace cfg dir");
        std::fs::create_dir_all(repo.join(".engram")).expect("repo cfg dir");
        std::fs::write(
            home.join(".engram/config.yml"),
            "db: ~/.engram/index.sqlite\n",
        )
        .expect("home config");
        std::fs::write(
            root.join("workspace/.engram/config.yml"),
            "db: /tmp/workspace.sqlite\n",
        )
        .expect("workspace config");
        std::fs::write(
            repo.join(".engram/config.yml"),
            "db: .engram/index.sqlite\n",
        )
        .expect("repo config");

        let nearest = find_walkup_config(&child, &home).expect("nearest walkup config");
        assert_eq!(nearest, repo.join(".engram/config.yml"));

        let resolved = load_effective_config(&child, &home).expect("resolved");
        assert_eq!(resolved.path, repo.join(".engram/config.yml"));
        assert_eq!(resolved.db, repo.join(".engram/index.sqlite"));
    }

    #[test]
    fn auto_creates_user_config_on_first_use() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).expect("home");
        let cwd = dir.path().join("workspace");
        std::fs::create_dir_all(&cwd).expect("cwd");

        let resolved = load_effective_config(&cwd, &home).expect("resolve config");
        let user_config = home.join(".engram/config.yml");
        assert!(user_config.exists());
        assert_eq!(resolved.path, user_config);
        assert_eq!(resolved.db, home.join(".engram/index.sqlite"));
    }

    #[test]
    fn supports_additional_stores_resolution() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join("home");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(workspace.join(".engram")).expect("workspace cfg dir");
        std::fs::create_dir_all(home.join(".engram")).expect("home");
        std::fs::write(
            home.join(".engram/config.yml"),
            "db: ~/.engram/index.sqlite\n",
        )
        .expect("home config");
        std::fs::write(
            workspace.join(".engram/config.yml"),
            "db: .engram/index.sqlite\nadditional_stores:\n  - /nfs/team/index.sqlite\n  - ../shared/engram.sqlite\n",
        )
        .expect("workspace config");

        let cfg = load_effective_config(&workspace, &home).expect("config");
        assert_eq!(cfg.db, workspace.join(".engram/index.sqlite"));
        assert_eq!(cfg.additional_stores.len(), 2);
        assert_eq!(
            cfg.additional_stores[0],
            Path::new("/nfs/team/index.sqlite").to_path_buf()
        );
        assert_eq!(
            cfg.additional_stores[1],
            dir.path().join("shared/engram.sqlite")
        );
    }

    #[test]
    fn rejects_legacy_pre_rev2_source_schema() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join(".engram")).expect("home");
        std::fs::write(
            home.join(".engram/config.yml"),
            "db: ~/.engram/index.sqlite\nsources:\n  - path: /tmp/input.jsonl\n    adapter: codex\n",
        )
        .expect("config");

        let err = load_parsed_config_file(&home.join(".engram/config.yml"))
            .expect_err("legacy schema should be rejected");
        assert!(
            err.to_string().contains("unknown field"),
            "err={err}"
        );
    }
}

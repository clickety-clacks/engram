use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::store::atomic::atomic_write;
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveConfig {
    pub path: PathBuf,
    pub db: PathBuf,
    pub tapes_dir: PathBuf,
    pub additional_stores: Vec<PathBuf>,
    pub explain_default_limit: usize,
    pub peek: EffectivePeekConfig,
    pub metrics: EffectiveMetricsConfig,
    pub watch: Option<EffectiveWatchConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectivePeekConfig {
    pub default_lines: usize,
    pub default_before: usize,
    pub default_after: usize,
    pub grep_context: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveMetricsConfig {
    pub enabled: bool,
    pub log: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveWatchConfig {
    pub debounce_secs: u64,
    pub ingest_timeout_secs: u64,
    pub log: PathBuf,
    pub sources: Vec<EffectiveWatchSource>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveWatchSource {
    pub path: PathBuf,
    pub pattern: String,
    pub glob: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedConfig {
    pub db: Option<String>,
    pub tapes_dir: Option<String>,
    pub additional_stores: Vec<String>,
    pub explain: Option<ParsedExplainConfig>,
    pub peek: Option<ParsedPeekConfig>,
    pub metrics: Option<ParsedMetricsConfig>,
    pub watch: Option<ParsedWatchConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedExplainConfig {
    pub default_limit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedPeekConfig {
    pub default_lines: Option<usize>,
    pub default_before: Option<usize>,
    pub default_after: Option<usize>,
    pub grep_context: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedMetricsConfig {
    pub enabled: Option<bool>,
    pub log: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedWatchConfig {
    pub debounce_secs: Option<u64>,
    pub ingest_timeout_secs: Option<u64>,
    pub log: Option<String>,
    pub sources: Vec<ParsedWatchSource>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedWatchSource {
    pub path: String,
    pub pattern: String,
    pub glob: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    db: Option<String>,
    #[serde(default)]
    tapes_dir: Option<String>,
    #[serde(default)]
    additional_stores: Option<Vec<String>>,
    #[serde(default)]
    explain: Option<RawExplainConfig>,
    #[serde(default)]
    peek: Option<RawPeekConfig>,
    #[serde(default)]
    metrics: Option<RawMetricsConfig>,
    #[serde(default)]
    watch: Option<RawWatchConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawExplainConfig {
    #[serde(default)]
    default_limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPeekConfig {
    #[serde(default)]
    default_lines: Option<usize>,
    #[serde(default)]
    default_before: Option<usize>,
    #[serde(default)]
    default_after: Option<usize>,
    #[serde(default)]
    grep_context: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMetricsConfig {
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    log: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWatchConfig {
    #[serde(default)]
    debounce_secs: Option<u64>,
    #[serde(default)]
    ingest_timeout_secs: Option<u64>,
    #[serde(default)]
    log: Option<String>,
    #[serde(default)]
    sources: Option<Vec<RawWatchSource>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWatchSource {
    path: String,
    pattern: String,
    #[serde(default)]
    glob: Option<String>,
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
    load_effective_config_with_override(cwd, home, None)
}

pub fn load_effective_config_with_override(
    cwd: &Path,
    home: &Path,
    config_override: Option<&Path>,
) -> Result<EffectiveConfig, ConfigError> {
    let user_config_path = ensure_user_config(home)?;
    let config_chain = if let Some(path) = config_override {
        vec![normalize_path(path)]
    } else {
        config_chain(cwd, home, &user_config_path)
    };
    let config_path = config_chain
        .first()
        .cloned()
        .unwrap_or_else(|| user_config_path.clone());
    let default_db = home.join(".engram").join("index.sqlite");
    let default_tapes_dir = cwd.join(".engram").join("tapes");
    let default_watch_log = home.join(".engram").join("watch.log");
    let default_metrics_log = home.join(".engram").join("metrics.jsonl");
    let default_explain_limit = 10usize;
    let default_peek = EffectivePeekConfig {
        default_lines: 30,
        default_before: 30,
        default_after: 10,
        grep_context: 5,
    };
    let mut db = None;
    let mut tapes_dir = None;
    let mut additional_stores = None;
    let mut explain_default_limit = None;
    let mut peek = None;
    let mut metrics = None;
    let mut watch = None;

    for layer_path in &config_chain {
        let raw = load_raw_config_file(layer_path)?;
        let base_dir = config_base_dir(layer_path)?;
        if db.is_none()
            && let Some(raw_db) = raw.db.as_deref()
        {
            db = Some(resolve_path(raw_db, &base_dir, home)?);
        }
        if additional_stores.is_none()
            && let Some(raw_stores) = raw.additional_stores.as_ref()
        {
            let mut resolved = Vec::new();
            for raw_store in raw_stores {
                resolved.push(resolve_path(raw_store, &base_dir, home)?);
            }
            additional_stores = Some(resolved);
        }
        if tapes_dir.is_none()
            && let Some(raw_tapes_dir) = raw.tapes_dir.as_deref()
        {
            tapes_dir = Some(resolve_path(raw_tapes_dir, &base_dir, home)?);
        }
        if explain_default_limit.is_none()
            && let Some(raw_explain) = raw.explain.as_ref()
        {
            explain_default_limit =
                Some(raw_explain.default_limit.unwrap_or(default_explain_limit));
        }
        if peek.is_none()
            && let Some(raw_peek) = raw.peek.as_ref()
        {
            peek = Some(EffectivePeekConfig {
                default_lines: raw_peek.default_lines.unwrap_or(default_peek.default_lines),
                default_before: raw_peek
                    .default_before
                    .unwrap_or(default_peek.default_before),
                default_after: raw_peek.default_after.unwrap_or(default_peek.default_after),
                grep_context: raw_peek.grep_context.unwrap_or(default_peek.grep_context),
            });
        }
        if metrics.is_none()
            && let Some(raw_metrics) = raw.metrics.as_ref()
        {
            let log = if let Some(raw_log) = raw_metrics.log.as_deref() {
                resolve_path(raw_log, &base_dir, home)?
            } else {
                default_metrics_log.clone()
            };
            metrics = Some(EffectiveMetricsConfig {
                enabled: raw_metrics.enabled.unwrap_or(true),
                log,
            });
        }
        if watch.is_none()
            && let Some(raw_watch) = raw.watch.as_ref()
        {
            let debounce_secs = raw_watch.debounce_secs.unwrap_or(5);
            let ingest_timeout_secs = raw_watch.ingest_timeout_secs.unwrap_or(120);
            let log = if let Some(raw_log) = raw_watch.log.as_deref() {
                resolve_path(raw_log, &base_dir, home)?
            } else {
                default_watch_log.clone()
            };
            let mut sources = Vec::new();
            if let Some(raw_sources) = raw_watch.sources.as_ref() {
                for source in raw_sources {
                    sources.push(EffectiveWatchSource {
                        path: resolve_path(&source.path, &base_dir, home)?,
                        pattern: source.pattern.clone(),
                        glob: source.glob.clone(),
                    });
                }
            }
            watch = Some(EffectiveWatchConfig {
                debounce_secs,
                ingest_timeout_secs,
                log,
                sources,
            });
        }
    }

    Ok(EffectiveConfig {
        path: config_path,
        db: db.unwrap_or(default_db),
        tapes_dir: tapes_dir.unwrap_or(default_tapes_dir),
        additional_stores: additional_stores.unwrap_or_default(),
        explain_default_limit: explain_default_limit.unwrap_or(default_explain_limit),
        peek: peek.unwrap_or(default_peek),
        metrics: metrics.unwrap_or(EffectiveMetricsConfig {
            enabled: true,
            log: default_metrics_log,
        }),
        watch,
    })
}

pub fn find_walkup_config(start: &Path, home: &Path) -> Option<PathBuf> {
    walkup_config_paths(start, home).into_iter().next()
}

pub fn ensure_user_config(home: &Path) -> Result<PathBuf, ConfigError> {
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
    let watch = raw.watch.map(|watch| ParsedWatchConfig {
        debounce_secs: watch.debounce_secs,
        ingest_timeout_secs: watch.ingest_timeout_secs,
        log: watch.log,
        sources: watch
            .sources
            .unwrap_or_default()
            .into_iter()
            .map(|source| ParsedWatchSource {
                path: source.path,
                pattern: source.pattern,
                glob: source.glob,
            })
            .collect(),
    });
    Ok(ParsedConfig {
        db: raw.db,
        tapes_dir: raw.tapes_dir,
        additional_stores: raw.additional_stores.unwrap_or_default(),
        explain: raw.explain.map(|explain| ParsedExplainConfig {
            default_limit: explain.default_limit,
        }),
        peek: raw.peek.map(|peek| ParsedPeekConfig {
            default_lines: peek.default_lines,
            default_before: peek.default_before,
            default_after: peek.default_after,
            grep_context: peek.grep_context,
        }),
        metrics: raw.metrics.map(|metrics| ParsedMetricsConfig {
            enabled: metrics.enabled,
            log: metrics.log,
        }),
        watch,
    })
}

pub fn load_parsed_config_file(path: &Path) -> Result<ParsedConfig, ConfigError> {
    let content = fs::read_to_string(path)?;
    parse_config(&content)
}

fn load_raw_config_file(path: &Path) -> Result<RawConfig, ConfigError> {
    let content = fs::read_to_string(path)?;
    Ok(serde_yaml::from_str(&content)?)
}

fn config_chain(cwd: &Path, home: &Path, user_config_path: &Path) -> Vec<PathBuf> {
    if !is_within_home(cwd, home) {
        return vec![user_config_path.to_path_buf()];
    }
    let chain = walkup_config_paths(cwd, home);
    if chain.is_empty() {
        vec![user_config_path.to_path_buf()]
    } else {
        chain
    }
}

fn walkup_config_paths(start: &Path, home: &Path) -> Vec<PathBuf> {
    if !is_within_home(start, home) {
        return Vec::new();
    }

    let mut out = Vec::new();
    for dir in start.ancestors() {
        let candidate = dir.join(".engram").join("config.yml");
        if candidate.is_file() {
            out.push(candidate);
        }
        if dir == home {
            break;
        }
    }
    out
}

fn is_within_home(path: &Path, home: &Path) -> bool {
    let normalized_path = canonicalize_or_normalize(path);
    let normalized_home = canonicalize_or_normalize(home);
    normalized_path.starts_with(&normalized_home)
}

fn canonicalize_or_normalize(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| normalize_path(path))
}

pub fn default_user_config_yaml() -> String {
    "db: ~/.engram/index.sqlite\ntapes_dir: .engram/tapes\n".to_string()
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
    use super::{
        config_chain, expand_tilde, find_walkup_config, load_effective_config,
        load_effective_config_with_override, load_parsed_config_file, walkup_config_paths,
    };
    use std::path::Path;

    #[test]
    fn expands_tilde_paths() {
        let expanded = expand_tilde("~/sessions", Path::new("/home/tester"));
        assert_eq!(expanded, Path::new("/home/tester/sessions"));
    }

    #[test]
    fn walk_up_collects_all_configs_from_nearest_to_home() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let home = root.join("home");
        let workspace = home.join("workspace");
        let repo = workspace.join("repo");
        let child = repo.join("nested/deeper");
        std::fs::create_dir_all(child.clone()).expect("child");
        std::fs::create_dir_all(home.join(".engram")).expect("home");
        std::fs::create_dir_all(workspace.join(".engram")).expect("workspace cfg dir");
        std::fs::create_dir_all(repo.join(".engram")).expect("repo cfg dir");
        std::fs::write(
            home.join(".engram/config.yml"),
            "db: ~/.engram/index.sqlite\n",
        )
        .expect("home config");
        std::fs::write(
            workspace.join(".engram/config.yml"),
            "db: /tmp/workspace.sqlite\n",
        )
        .expect("workspace config");
        std::fs::write(
            repo.join(".engram/config.yml"),
            "db: .engram/index.sqlite\n",
        )
        .expect("repo config");

        let collected = walkup_config_paths(&child, &home);
        assert_eq!(collected.len(), 3);
        assert_eq!(collected[0], repo.join(".engram/config.yml"));
        assert_eq!(collected[1], workspace.join(".engram/config.yml"));
        assert_eq!(collected[2], home.join(".engram/config.yml"));
        let nearest = find_walkup_config(&child, &home).expect("nearest walkup config");
        assert_eq!(nearest, repo.join(".engram/config.yml"));
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
        assert_eq!(
            std::fs::read_to_string(&user_config).expect("user config content"),
            "db: ~/.engram/index.sqlite\ntapes_dir: .engram/tapes\n"
        );
        assert_eq!(resolved.path, user_config);
        assert_eq!(resolved.db, home.join(".engram/index.sqlite"));
        assert_eq!(resolved.tapes_dir, home.join(".engram/tapes"));
    }

    #[test]
    fn supports_additional_stores_resolution() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join("home");
        let workspace = home.join("workspace");
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
        assert_eq!(cfg.tapes_dir, workspace.join(".engram/tapes"));
        assert_eq!(cfg.additional_stores.len(), 2);
        assert_eq!(
            cfg.additional_stores[0],
            Path::new("/nfs/team/index.sqlite").to_path_buf()
        );
        assert_eq!(cfg.additional_stores[1], home.join("shared/engram.sqlite"));
    }

    #[test]
    fn cascade_merge_inherits_missing_keys_from_parent_configs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let home = root.join("home");
        let workspace = home.join("workspace");
        let repo = workspace.join("repo");
        let child = repo.join("nested");
        std::fs::create_dir_all(child.clone()).expect("child");
        std::fs::create_dir_all(home.join(".engram")).expect("home");
        std::fs::create_dir_all(workspace.join(".engram")).expect("workspace");
        std::fs::create_dir_all(repo.join(".engram")).expect("repo");

        std::fs::write(
            home.join(".engram/config.yml"),
            "db: ~/.engram/home.sqlite\nadditional_stores:\n  - /nfs/home.sqlite\n",
        )
        .expect("home config");
        std::fs::write(
            workspace.join(".engram/config.yml"),
            "tapes_dir: ../workspace-tapes\nadditional_stores:\n  - ../team/workspace.sqlite\n",
        )
        .expect("workspace config");
        std::fs::write(repo.join(".engram/config.yml"), "db: .engram/repo.sqlite\n")
            .expect("repo config");

        let cfg = load_effective_config(&child, &home).expect("resolved");
        assert_eq!(cfg.path, repo.join(".engram/config.yml"));
        assert_eq!(cfg.db, repo.join(".engram/repo.sqlite"));
        assert_eq!(cfg.tapes_dir, home.join("workspace-tapes"));
        assert_eq!(
            cfg.additional_stores,
            vec![home.join("team/workspace.sqlite")]
        );
    }

    #[test]
    fn nearest_config_wins_when_it_specifies_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let home = root.join("home");
        let workspace = home.join("workspace");
        let repo = workspace.join("repo");
        std::fs::create_dir_all(repo.join(".engram")).expect("repo");
        std::fs::create_dir_all(workspace.join(".engram")).expect("workspace");
        std::fs::create_dir_all(home.join(".engram")).expect("home");
        std::fs::write(
            home.join(".engram/config.yml"),
            "db: ~/.engram/home.sqlite\nadditional_stores:\n  - /nfs/home.sqlite\n",
        )
        .expect("home config");
        std::fs::write(
            workspace.join(".engram/config.yml"),
            "db: /tmp/workspace.sqlite\nadditional_stores:\n  - /tmp/workspace-store.sqlite\n",
        )
        .expect("workspace config");
        std::fs::write(repo.join(".engram/config.yml"), "additional_stores: []\n")
            .expect("repo config");

        let cfg = load_effective_config(&repo, &home).expect("resolved");
        assert_eq!(cfg.path, repo.join(".engram/config.yml"));
        assert_eq!(cfg.db, Path::new("/tmp/workspace.sqlite").to_path_buf());
        assert_eq!(cfg.tapes_dir, repo.join(".engram/tapes"));
        assert!(cfg.additional_stores.is_empty());
    }

    #[test]
    fn outside_home_uses_user_config_directly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let home = root.join("home");
        let outside = root.join("outside/workspace");
        std::fs::create_dir_all(outside.join(".engram")).expect("outside");
        std::fs::create_dir_all(home.join(".engram")).expect("home");
        std::fs::write(
            home.join(".engram/config.yml"),
            "db: ~/.engram/home.sqlite\n",
        )
        .expect("home config");
        std::fs::write(
            outside.join(".engram/config.yml"),
            "db: /tmp/outside.sqlite\n",
        )
        .expect("outside config");

        let cfg = load_effective_config(&outside, &home).expect("resolved");
        assert_eq!(cfg.path, home.join(".engram/config.yml"));
        assert_eq!(cfg.db, home.join(".engram/home.sqlite"));
        assert_eq!(cfg.tapes_dir, outside.join(".engram/tapes"));

        let chain = config_chain(&outside, &home, &home.join(".engram/config.yml"));
        assert_eq!(chain, vec![home.join(".engram/config.yml")]);
    }

    #[test]
    fn resolves_tapes_dir_from_relative_config_value() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join("home");
        let workspace = home.join("workspace");
        std::fs::create_dir_all(workspace.join(".engram")).expect("workspace cfg dir");
        std::fs::create_dir_all(home.join(".engram")).expect("home");
        std::fs::write(
            home.join(".engram/config.yml"),
            "db: ~/.engram/index.sqlite\n",
        )
        .expect("home config");
        std::fs::write(
            workspace.join(".engram/config.yml"),
            "tapes_dir: ../compiled/tapes\n",
        )
        .expect("workspace config");

        let cfg = load_effective_config(&workspace, &home).expect("config");
        assert_eq!(cfg.tapes_dir, home.join("compiled/tapes"));
    }

    #[test]
    fn resolves_tapes_dir_from_tilde_config_value() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join("home");
        let workspace = home.join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        std::fs::create_dir_all(home.join(".engram")).expect("home");
        std::fs::write(
            home.join(".engram/config.yml"),
            "db: ~/.engram/index.sqlite\ntapes_dir: ~/compiled/tapes\n",
        )
        .expect("home config");

        let cfg = load_effective_config(&workspace, &home).expect("config");
        assert_eq!(cfg.tapes_dir, home.join("compiled/tapes"));
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
        assert!(err.to_string().contains("unknown field"), "err={err}");
    }

    #[test]
    fn resolves_watch_config_paths_and_defaults() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join("home");
        let workspace = home.join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        std::fs::create_dir_all(home.join(".engram")).expect("home");
        std::fs::write(
            home.join(".engram/config.yml"),
            "db: ~/.engram/index.sqlite\nwatch:\n  sources:\n    - path: ~/shared/openclaw\n      pattern: \"*.jsonl\"\n",
        )
        .expect("home config");

        let cfg = load_effective_config(&workspace, &home).expect("config");
        let watch = cfg.watch.expect("watch config");
        assert_eq!(watch.debounce_secs, 5);
        assert_eq!(watch.ingest_timeout_secs, 120);
        assert_eq!(watch.log, home.join(".engram/watch.log"));
        assert_eq!(watch.sources.len(), 1);
        assert_eq!(watch.sources[0].path, home.join("shared/openclaw"));
        assert_eq!(watch.sources[0].pattern, "*.jsonl");
        assert_eq!(watch.sources[0].glob, None);
    }

    #[test]
    fn resolves_watch_source_optional_glob_filter() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join("home");
        let workspace = home.join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        std::fs::create_dir_all(home.join(".engram")).expect("home");
        std::fs::write(
            home.join(".engram/config.yml"),
            "db: ~/.engram/index.sqlite\nwatch:\n  sources:\n    - path: ~/shared/openclaw\n      pattern: \"*.jsonl\"\n      glob: \"sessions/**/*.jsonl\"\n",
        )
        .expect("home config");

        let cfg = load_effective_config(&workspace, &home).expect("config");
        let watch = cfg.watch.expect("watch config");
        assert_eq!(watch.sources.len(), 1);
        assert_eq!(watch.sources[0].path, home.join("shared/openclaw"));
        assert_eq!(watch.sources[0].pattern, "*.jsonl");
        assert_eq!(
            watch.sources[0].glob.as_deref(),
            Some("sessions/**/*.jsonl")
        );
    }

    #[test]
    fn explicit_config_override_is_loaded_directly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join("home");
        let workspace = home.join("workspace");
        let custom_root = dir.path().join("custom-root");
        let custom = custom_root.join(".engram/config.yml");
        std::fs::create_dir_all(&workspace).expect("workspace");
        std::fs::create_dir_all(custom.parent().expect("parent")).expect("custom parent");
        std::fs::create_dir_all(home.join(".engram")).expect("home");
        std::fs::write(
            custom.clone(),
            "db: ./db.sqlite\nwatch:\n  log: ./watch.log\n  debounce_secs: 3\n  ingest_timeout_secs: 30\n  sources:\n    - path: ./sessions\n      pattern: \"session-*.json\"\n",
        )
        .expect("custom config");

        let cfg = load_effective_config_with_override(&workspace, &home, Some(&custom))
            .expect("override config");
        assert_eq!(cfg.path, custom);
        assert_eq!(cfg.db, custom_root.join("db.sqlite"));
        let watch = cfg.watch.expect("watch config");
        assert_eq!(watch.log, custom_root.join("watch.log"));
        assert_eq!(watch.debounce_secs, 3);
        assert_eq!(watch.ingest_timeout_secs, 30);
        assert_eq!(watch.sources.len(), 1);
        assert_eq!(watch.sources[0].path, custom_root.join("sessions"));
        assert_eq!(watch.sources[0].pattern, "session-*.json");
        assert_eq!(watch.sources[0].glob, None);
    }
}

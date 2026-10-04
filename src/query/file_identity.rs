use std::collections::HashMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

const MAX_IDENTITY_PATHS_PER_QUERY: usize = 128;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitFileIdentity {
    common_dir: Vec<String>,
    relative_path: Vec<String>,
    head: Option<String>,
}

impl GitFileIdentity {
    pub fn head(&self) -> Option<&str> {
        self.head.as_deref()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileIdentityRelation {
    SamePhysicalPath,
    SameRepositoryFile,
    DifferentRepositorySameRelativePath,
    DifferentRepositoryDifferentPath,
    DifferentPathSameRepository,
    Unknown,
}

#[derive(Default)]
pub struct FileIdentityResolver {
    cache: HashMap<PathBuf, Option<GitFileIdentity>>,
}

impl FileIdentityResolver {
    pub fn relation(
        &mut self,
        cwd: &Path,
        target_file: &str,
        evidence_file: &str,
    ) -> FileIdentityRelation {
        let Some(target_path) = absolute_path(cwd, target_file) else {
            return FileIdentityRelation::Unknown;
        };
        let Some(evidence_path) = absolute_evidence_path(evidence_file) else {
            return FileIdentityRelation::Unknown;
        };
        self.relation_absolute(&target_path, &evidence_path)
    }

    pub fn relation_absolute(
        &mut self,
        target_path: &Path,
        evidence_path: &Path,
    ) -> FileIdentityRelation {
        let target_path = canonicalize_absolute_with_missing_tail(target_path)
            .unwrap_or_else(|| normalize_absolute(target_path));
        let evidence_path = canonicalize_absolute_with_missing_tail(evidence_path)
            .unwrap_or_else(|| normalize_absolute(evidence_path));
        if same_path(&target_path, &evidence_path) {
            return FileIdentityRelation::SamePhysicalPath;
        }

        let Some(target_identity) = self.identity_for(&target_path) else {
            return FileIdentityRelation::Unknown;
        };
        let Some(evidence_identity) = self.identity_for(&evidence_path) else {
            return FileIdentityRelation::Unknown;
        };

        let same_repository = target_identity.common_dir == evidence_identity.common_dir;
        let same_relative_path = target_identity.relative_path == evidence_identity.relative_path;
        match (same_repository, same_relative_path) {
            (true, true) => FileIdentityRelation::SameRepositoryFile,
            (false, true) => FileIdentityRelation::DifferentRepositorySameRelativePath,
            (false, false) => FileIdentityRelation::DifferentRepositoryDifferentPath,
            (true, false) => FileIdentityRelation::DifferentPathSameRepository,
        }
    }

    pub fn identity_for_query_path(&mut self, cwd: &Path, file: &str) -> Option<GitFileIdentity> {
        let path = absolute_path(cwd, file)?;
        self.identity_for(&path)
    }

    /// Return true when `directory_path` names a non-root directory in the
    /// same Git repository and its repository-relative path contains
    /// `file_path`. This is used for file-specific context only; an unknown or
    /// unavailable repository identity never becomes a match.
    pub fn is_specific_same_repository_directory_prefix(
        &mut self,
        file_path: &Path,
        directory_path: &Path,
    ) -> bool {
        let file_path = canonicalize_absolute_with_missing_tail(file_path)
            .unwrap_or_else(|| normalize_absolute(file_path));
        let directory_path = canonicalize_absolute_with_missing_tail(directory_path)
            .unwrap_or_else(|| normalize_absolute(directory_path));
        if !directory_path.is_dir() {
            return false;
        }
        let Some(file_identity) = self.identity_for(&file_path) else {
            return false;
        };
        let Some(directory_identity) = self.identity_for(&directory_path) else {
            return false;
        };
        let prefix = &directory_identity.relative_path;
        file_identity.common_dir == directory_identity.common_dir
            && !prefix.is_empty()
            && file_identity.relative_path.len() > prefix.len()
            && file_identity.relative_path[..prefix.len()] == prefix[..]
    }

    fn identity_for(&mut self, path: &Path) -> Option<GitFileIdentity> {
        let path = canonicalize_absolute_with_missing_tail(path)
            .unwrap_or_else(|| normalize_absolute(path));
        if let Some(identity) = self.cache.get(&path) {
            return identity.clone();
        }
        if self.cache.len() >= MAX_IDENTITY_PATHS_PER_QUERY {
            return None;
        }
        let identity = git_file_identity(&path);
        self.cache.insert(path, identity.clone());
        identity
    }
}

fn absolute_path(cwd: &Path, path: &str) -> Option<PathBuf> {
    let path = Path::new(path);
    if path.is_absolute() {
        Some(normalize_absolute(path))
    } else if cwd.is_absolute() {
        Some(normalize_absolute(&cwd.join(path)))
    } else {
        None
    }
}

fn absolute_evidence_path(path: &str) -> Option<PathBuf> {
    let path = Path::new(path);
    path.is_absolute().then(|| normalize_absolute(path))
}

fn normalize_absolute(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

/// Resolve symlink aliases in the existing prefix while preserving a missing
/// suffix. macOS commonly exposes temporary directories through both `/var`
/// and `/private/var`; Git reports the physical root, so leaving the source
/// path lexical makes a real file appear outside its own repository.
fn canonicalize_absolute_with_missing_tail(path: &Path) -> Option<PathBuf> {
    let normalized = normalize_absolute(path);
    if !normalized.is_absolute() {
        return None;
    }
    let mut existing = normalized.as_path();
    let mut missing = Vec::new();
    while !existing.exists() {
        missing.push(existing.file_name()?.to_os_string());
        existing = existing.parent()?;
    }
    let mut canonical = fs::canonicalize(existing).ok()?;
    for component in missing.iter().rev() {
        canonical.push(component);
    }
    Some(canonical)
}

fn path_components(path: &Path) -> Vec<String> {
    let comparable_path = if cfg!(windows) {
        let text = path.to_string_lossy();
        if let Some(unc_path) = text.strip_prefix(r"\\?\UNC\") {
            PathBuf::from(format!(r"\\{unc_path}"))
        } else if let Some(path) = text.strip_prefix(r"\\?\") {
            PathBuf::from(path)
        } else {
            path.to_path_buf()
        }
    } else {
        path.to_path_buf()
    };
    comparable_path
        .components()
        .map(|component| {
            let value = component.as_os_str().to_string_lossy();
            if cfg!(windows) {
                value.to_lowercase()
            } else {
                value.into_owned()
            }
        })
        .collect()
}

fn same_path(left: &Path, right: &Path) -> bool {
    path_components(left) == path_components(right)
}

fn relative_components(path: &Path, root: &Path) -> Option<Vec<String>> {
    let file_components = path_components(path);
    let root_components = path_components(root);
    if file_components.len() < root_components.len()
        || file_components[..root_components.len()] != root_components
    {
        return None;
    }
    Some(file_components[root_components.len()..].to_vec())
}

fn existing_directory(path: &Path) -> Option<PathBuf> {
    let mut candidate = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent()?.to_path_buf()
    };
    loop {
        if candidate.is_dir() {
            return Some(candidate);
        }
        candidate = candidate.parent()?.to_path_buf();
    }
}

fn git_file_identity(path: &Path) -> Option<GitFileIdentity> {
    if !path.is_absolute() {
        return None;
    }
    let probe = existing_directory(path)?;
    let output = Command::new("git")
        .args(["-C"])
        .arg(git_compatible_path(&probe))
        .args([
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--git-common-dir",
            "--verify",
            "HEAD",
        ])
        .env("GIT_OPTIONAL_LOCKS", "0")
        // Do not let a caller's repository overrides redefine either path's
        // repository identity.
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_CEILING_DIRECTORIES")
        .env_remove("GIT_DISCOVERY_ACROSS_FILESYSTEM")
        .env_remove("GIT_NAMESPACE")
        .output()
        .ok()?;
    if output.stdout.len() > 4096 {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    let mut lines = stdout.lines();
    let root = PathBuf::from(lines.next()?.trim());
    let common_dir = PathBuf::from(lines.next()?.trim());
    if !root.is_absolute() || !common_dir.is_absolute() {
        return None;
    }
    let root = fs::canonicalize(root).ok()?;
    let common_dir = fs::canonicalize(common_dir).ok()?;
    let path = normalize_absolute(path);
    let relative_path = relative_components(&path, &root)?;
    let head = lines
        .next()
        .map(str::trim)
        .filter(|head| !head.is_empty())
        .map(ToOwned::to_owned);

    Some(GitFileIdentity {
        common_dir: path_components(&common_dir),
        relative_path,
        head,
    })
}

fn git_compatible_path(path: &Path) -> PathBuf {
    if cfg!(windows) {
        let text = path.to_string_lossy();
        if let Some(unc_path) = text.strip_prefix(r"\\?\UNC\") {
            PathBuf::from(format!(r"\\{unc_path}"))
        } else if let Some(path) = text.strip_prefix(r"\\?\") {
            PathBuf::from(path)
        } else {
            path.to_path_buf()
        }
    } else {
        path.to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use super::{FileIdentityRelation, FileIdentityResolver};

    fn git(cwd: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .output()
            .expect("git is available for identity tests");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("git stdout is UTF-8")
            .trim()
            .to_string()
    }

    fn init_repo(root: &Path, content: &str) -> String {
        fs::create_dir_all(root.join("scripts")).expect("scripts directory");
        fs::write(root.join("scripts/verify_mix.sh"), content).expect("source file");
        let output = Command::new("git")
            .arg("init")
            .arg("--quiet")
            .arg(root)
            .output()
            .expect("git is available for identity tests");
        assert!(output.status.success(), "git init failed");
        let _ = git(root, &["config", "user.name", "Identity Test"]);
        let _ = git(
            root,
            &["config", "user.email", "identity-test@example.invalid"],
        );
        let _ = git(root, &["add", "scripts/verify_mix.sh"]);
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["-c", "commit.gpgsign=false", "commit", "-m", "initial"])
            .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
            .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
            .output()
            .expect("git is available for identity tests");
        assert!(
            output.status.success(),
            "git commit failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        git(root, &["rev-parse", "HEAD"])
    }

    fn add_worktree(root: &Path, worktree: &Path, branch: &str) {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["worktree", "add", "--quiet", "-b", branch])
            .arg(worktree)
            .arg("HEAD")
            .output()
            .expect("git is available for identity tests");
        assert!(
            output.status.success(),
            "git worktree add failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn temp_path(temp: &tempfile::TempDir, name: &str) -> PathBuf {
        temp.path().join(name)
    }

    #[test]
    fn linked_worktrees_share_identity_by_common_dir_and_relative_path() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp_path(&temp, "repo");
        let worktree = temp_path(&temp, "second-checkout");
        let _head = init_repo(&root, "line one\nline two\n");
        add_worktree(&root, &worktree, "second-checkout");

        let target = root.join("scripts/verify_mix.sh");
        let evidence = worktree.join("scripts/verify_mix.sh");
        let mut resolver = FileIdentityResolver::default();
        assert_eq!(
            resolver.relation_absolute(&target, &evidence),
            FileIdentityRelation::SameRepositoryFile
        );
    }

    #[test]
    fn independent_repositories_with_same_suffix_and_identical_commit_stay_distinct() {
        let temp = tempfile::tempdir().expect("tempdir");
        let first = temp_path(&temp, "first");
        let second = temp_path(&temp, "second");
        let first_head = init_repo(&first, "same content\n");
        let second_head = init_repo(&second, "same content\n");
        assert_eq!(
            first_head, second_head,
            "fixture must share the same commit"
        );

        let mut resolver = FileIdentityResolver::default();
        assert_eq!(
            resolver.relation_absolute(
                &first.join("scripts/verify_mix.sh"),
                &second.join("scripts/verify_mix.sh")
            ),
            FileIdentityRelation::DifferentRepositorySameRelativePath
        );
    }

    #[test]
    fn a_path_without_queryable_git_provenance_has_unknown_identity() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp_path(&temp, "repo");
        let _head = init_repo(&repo, "line\n");
        let missing_checkout = temp_path(&temp, "removed-worktree");
        let mut resolver = FileIdentityResolver::default();
        assert_eq!(
            resolver.relation_absolute(
                &repo.join("scripts/verify_mix.sh"),
                &missing_checkout.join("scripts/verify_mix.sh")
            ),
            FileIdentityRelation::Unknown
        );
    }

    #[test]
    fn only_specific_same_repository_directories_match_as_file_context() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp_path(&temp, "repo");
        let _head = init_repo(&repo, "line\n");
        let file = repo.join("scripts/verify_mix.sh");
        let mut resolver = FileIdentityResolver::default();

        assert!(
            resolver.is_specific_same_repository_directory_prefix(&file, &repo.join("scripts"))
        );
        assert!(
            !resolver.is_specific_same_repository_directory_prefix(&file, &repo),
            "the repository root is too broad to qualify as file context"
        );
        assert!(
            !resolver.is_specific_same_repository_directory_prefix(&file, temp.path()),
            "a filesystem ancestor outside the repository is not a task-file match"
        );
    }

    #[cfg(unix)]
    #[test]
    fn existing_paths_through_symlink_alias_share_physical_identity() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp_path(&temp, "repo");
        let _head = init_repo(&repo, "line\n");
        let alias = temp_path(&temp, "repo-alias");
        symlink(&repo, &alias).expect("symlink repository alias");

        let mut resolver = FileIdentityResolver::default();
        assert_eq!(
            resolver.relation_absolute(
                &repo.join("scripts/verify_mix.sh"),
                &alias.join("scripts/verify_mix.sh")
            ),
            FileIdentityRelation::SamePhysicalPath
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_verbatim_drive_and_unc_paths_match_normal_paths() {
        use super::same_path;

        assert!(same_path(
            Path::new(r"C:\repo\scripts\verify_mix.sh"),
            Path::new(r"\\?\C:\repo\scripts\verify_mix.sh")
        ));
        assert!(same_path(
            Path::new(r"\\server\share\repo\scripts\verify_mix.sh"),
            Path::new(r"\\?\UNC\server\share\repo\scripts\verify_mix.sh")
        ));
    }

    #[cfg(windows)]
    #[test]
    fn canonical_verbatim_paths_still_resolve_git_identity() {
        use super::{FileIdentityRelation, FileIdentityResolver};

        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        let _head = init_repo(&repo, "line\n");
        let worktree = temp.path().join("second-checkout");
        add_worktree(&repo, &worktree, "second-checkout");
        let normal = repo.join("scripts/verify_mix.sh");
        let canonical = fs::canonicalize(worktree.join("scripts/verify_mix.sh"))
            .expect("canonical worktree source path");

        let mut resolver = FileIdentityResolver::default();
        assert_eq!(
            resolver.relation_absolute(&normal, &canonical),
            FileIdentityRelation::SameRepositoryFile
        );
    }
}

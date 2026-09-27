use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, ExitStatus};

use crate::CliError;

const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
const MOVEFILE_REPLACE_EXISTING: u32 = 0x0000_0001;
const MOVEFILE_WRITE_THROUGH: u32 = 0x0000_0008;

#[link(name = "Kernel32")]
unsafe extern "system" {
    fn MoveFileExW(existing: *const u16, replacement: *const u16, flags: u32) -> i32;
}

pub(super) fn home_dir() -> Result<PathBuf, CliError> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .ok_or_else(|| {
            CliError::new(
                "home_error",
                "USERPROFILE or HOME environment variable is not set",
            )
        })
}

pub(super) fn normalize_path(path: &Path) -> PathBuf {
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

pub(super) fn canonicalize_or_normalize(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| normalize_path(path))
}

pub(super) fn repository_path_key(path: &Path) -> String {
    let text = canonicalize_or_normalize(path).to_string_lossy();
    let text = text.strip_prefix(r"\\?\").unwrap_or(&text);
    text.replace('\\', "-")
        .replace('/', "-")
        .replace(':', "-")
}

pub(super) fn file_uri_path(decoded: &str) -> Option<PathBuf> {
    let decoded = decoded.strip_prefix('/').unwrap_or(decoded);
    let bytes = decoded.as_bytes();
    if bytes.len() < 3
        || !bytes[0].is_ascii_alphabetic()
        || bytes[1] != b':'
        || !matches!(bytes[2], b'/' | b'\\')
    {
        return None;
    }
    Some(PathBuf::from(decoded.replace('/', "\\")))
}

pub(super) fn open_read_nofollow(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    if file.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "refusing to open a reparse point",
        ));
    }
    Ok(file)
}

pub(super) fn atomic_replace(from: &Path, to: &Path) -> io::Result<()> {
    let from = wide_path(from)?;
    let to = wide_path(to)?;
    // Keep replacement on the same volume atomic and ask Windows to flush it.
    let replaced = unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if replaced == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn wide_path(path: &Path) -> io::Result<Vec<u16>> {
    let mut wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
    if wide.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path contains a NUL character",
        ));
    }
    wide.push(0);
    Ok(wide)
}

pub(super) fn sync_parent_dir(_parent: &Path) -> io::Result<()> {
    // `atomic_replace` requests MOVEFILE_WRITE_THROUGH for the directory update.
    Ok(())
}

pub(super) fn spawn_peer(_peer: &crate::config::TopologyPeer) -> io::Result<Child> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "cross-machine peer transport and process-group signal control are unsupported on Windows",
    ))
}

pub(super) fn exit_status_description(status: ExitStatus) -> String {
    status
        .code()
        .map(|code| format!("exited with code {code}"))
        .unwrap_or_else(|| "terminated without an exit code".into())
}

pub(super) fn terminate_peer_process_group(child: &mut Child) {
    let _ = child.kill();
}

pub(super) fn ensure_peer_commands_supported() -> Result<(), CliError> {
    Err(CliError::new(
        "unsupported_platform",
        "cross-machine peer queries and topology status are unsupported on Windows because peer process-group signal control is not implemented; local commands remain available",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_uri_keeps_windows_drive_paths_and_decodes_escaped_spaces() {
        assert_eq!(
            crate::platform::file_uri_path("/C:/work/My%20Repo"),
            Some(PathBuf::from(r"C:\work\My Repo"))
        );
        assert_eq!(crate::platform::file_uri_path("/workspace/path"), None);
    }

    #[test]
    fn nofollow_read_refuses_reparse_point_tapes() {
        let temp = tempfile::tempdir().expect("tempdir");
        let target = temp.path().join("real.jsonl.zst");
        let link = temp.path().join("link.jsonl.zst");
        fs::write(&target, b"tape").expect("target");
        std::os::windows::fs::symlink_file(&target, &link).expect("symlink");

        let error = open_read_nofollow(&link).expect_err("refuse reparse point");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn atomic_replace_replaces_existing_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let destination = temp.path().join("index.sqlite");
        let replacement = temp.path().join("index.sqlite.tmp");
        fs::write(&destination, b"old").expect("destination");
        fs::write(&replacement, b"new").expect("replacement");

        atomic_replace(&replacement, &destination).expect("atomic replacement");
        sync_parent_dir(temp.path()).expect("parent sync");

        assert_eq!(fs::read(destination).expect("read replacement"), b"new");
        assert!(!replacement.exists());
    }

    #[test]
    fn peer_commands_fail_with_a_clear_unsupported_reason() {
        let error = ensure_peer_commands_supported().expect_err("Windows peer query");
        assert_eq!(error.code, "unsupported_platform");
        assert!(error.message.contains("cross-machine peer queries"));
        assert!(error.message.contains("local commands remain available"));

        let peer = crate::config::TopologyPeer {
            ssh: Some("eezo".into()),
            command: None,
            engram: "engram".into(),
            exports: vec!["default".into()],
        };
        let spawn_error = spawn_peer(&peer).expect_err("Windows peer process");
        assert_eq!(spawn_error.kind(), io::ErrorKind::Unsupported);
        assert!(spawn_error.to_string().contains("cross-machine peer transport"));
    }
}

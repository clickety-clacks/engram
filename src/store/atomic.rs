use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static ATOMIC_COUNTER: AtomicU64 = AtomicU64::new(0);
const TEMP_PREFIX: &str = ".engram.tmp.";

pub fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path `{}` has no parent directory", path.display()),
        )
    })?;
    fs::create_dir_all(parent)?;

    let tmp_path = temp_path_in_parent(parent, path)?;
    let mut tmp_file = create_temp_file(&tmp_path)?;

    let write_result = (|| -> io::Result<()> {
        tmp_file.write_all(bytes)?;
        tmp_file.flush()?;
        tmp_file.sync_all()?;
        drop(tmp_file);

        rename_overwrite(&tmp_path, path)?;
        sync_parent_dir(parent)?;
        Ok(())
    })();

    if write_result.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    write_result
}

fn create_temp_file(path: &Path) -> io::Result<File> {
    OpenOptions::new().create_new(true).write(true).open(path)
}

fn rename_overwrite(from: &Path, to: &Path) -> io::Result<()> {
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(err) => {
            if to.exists() {
                fs::remove_file(to)?;
                fs::rename(from, to)
            } else {
                Err(err)
            }
        }
    }
}

#[cfg(unix)]
fn sync_parent_dir(parent: &Path) -> io::Result<()> {
    File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent_dir(_parent: &Path) -> io::Result<()> {
    Ok(())
}

fn temp_path_in_parent(parent: &Path, final_path: &Path) -> io::Result<PathBuf> {
    let file_name = final_path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid target filename"))?;
    let epoch_nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|err| io::Error::other(err.to_string()))?
        .as_nanos();
    let counter = ATOMIC_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp_name = format!(
        "{TEMP_PREFIX}{file_name}.{epoch_nanos}.{}.{}",
        std::process::id(),
        counter
    );
    Ok(parent.join(tmp_name))
}

#[cfg(test)]
mod tests {
    use super::{TEMP_PREFIX, atomic_write};
    use std::fs;

    #[test]
    fn atomic_write_creates_file_and_content() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");

        atomic_write(&path, br#"{"ok":true}"#).expect("atomic write");
        let content = fs::read_to_string(&path).expect("read content");
        assert_eq!(content, r#"{"ok":true}"#);
    }

    #[test]
    fn atomic_write_overwrites_existing_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.yml");
        fs::write(&path, "old").expect("seed");

        atomic_write(&path, b"new-content").expect("atomic overwrite");
        let content = fs::read_to_string(&path).expect("read content");
        assert_eq!(content, "new-content");
    }

    #[test]
    fn atomic_write_cleans_up_temp_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cursor-state.json");

        atomic_write(&path, b"v1").expect("write1");
        atomic_write(&path, b"v2").expect("write2");

        let leftovers = fs::read_dir(dir.path())
            .expect("list dir")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| name.starts_with(TEMP_PREFIX))
            .collect::<Vec<_>>();
        assert!(
            leftovers.is_empty(),
            "expected no temp files, found {leftovers:?}"
        );
    }
}

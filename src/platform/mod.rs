#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
use unix as implementation;
#[cfg(windows)]
use windows as implementation;

#[cfg(not(any(unix, windows)))]
compile_error!("Engram platform support is limited to Unix and Windows targets");

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus};

use crate::CliError;

pub fn home_dir() -> Result<PathBuf, CliError> {
    implementation::home_dir()
}

pub fn normalize_path(path: &Path) -> PathBuf {
    implementation::normalize_path(path)
}

pub fn canonicalize_or_normalize(path: &Path) -> PathBuf {
    implementation::canonicalize_or_normalize(path)
}

pub fn repository_path_key(path: &Path) -> String {
    implementation::repository_path_key(path)
}

pub fn open_read_nofollow(path: &Path) -> io::Result<File> {
    implementation::open_read_nofollow(path)
}

pub fn atomic_replace(from: &Path, to: &Path) -> io::Result<()> {
    implementation::atomic_replace(from, to)
}

pub fn sync_parent_dir(parent: &Path) -> io::Result<()> {
    implementation::sync_parent_dir(parent)
}

pub fn spawn_peer(peer: &crate::config::TopologyPeer) -> io::Result<Child> {
    implementation::spawn_peer(peer)
}

pub fn exit_status_description(status: ExitStatus) -> String {
    implementation::exit_status_description(status)
}

pub fn terminate_peer_process_group(child: &mut Child) {
    implementation::terminate_peer_process_group(child)
}

pub fn ensure_peer_commands_supported() -> Result<(), CliError> {
    implementation::ensure_peer_commands_supported()
}

pub fn file_uri_path(rest: &str) -> Option<PathBuf> {
    implementation::file_uri_path(&percent_decode_file_uri_path(rest)?)
}

fn percent_decode_file_uri_path(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = *bytes.get(index + 1)?;
            let low = *bytes.get(index + 2)?;
            let digit = |byte: u8| match byte {
                b'0'..=b'9' => Some(byte - b'0'),
                b'a'..=b'f' => Some(byte - b'a' + 10),
                b'A'..=b'F' => Some(byte - b'A' + 10),
                _ => None,
            };
            decoded.push((digit(high)? << 4) | digit(low)?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

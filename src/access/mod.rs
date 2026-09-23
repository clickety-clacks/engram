//! Machine-aware query ownership seams and the bounded peer endpoint.
//!
//! The local owner opens typed, read-only stores in process. The peer endpoint
//! serves only named read operations over stdio; neither path exposes writes.

use std::path::{Path, PathBuf};

use crate::index::{ReaderMode, SqliteIndex};

pub mod client;
pub mod peer;
pub mod transport;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MachineRef(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StoreRef {
    pub machine: MachineRef,
    pub export: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileAddress {
    pub machine: MachineRef,
    pub path: PathBuf,
    pub kind: FileKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Tape,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreDescription {
    pub store: StoreRef,
    pub db: PathBuf,
    pub reader_mode: ReaderMode,
}

pub trait Owner {
    fn open(&self) -> rusqlite::Result<Vec<StoreDescription>>;
}

pub struct LocalOwner {
    machine: MachineRef,
    stores: Vec<(StoreRef, PathBuf, ReaderMode)>,
}

impl LocalOwner {
    pub fn new(machine: impl Into<String>, db: impl AsRef<Path>) -> Self {
        Self {
            machine: MachineRef(machine.into()),
            stores: vec![(
                StoreRef {
                    machine: MachineRef(String::new()),
                    export: "local:0".into(),
                },
                db.as_ref().to_path_buf(),
                ReaderMode::Live,
            )],
        }
    }

    pub fn add_frozen_store(mut self, label: impl Into<String>, db: impl AsRef<Path>) -> Self {
        let n = self.stores.len();
        self.stores.push((
            StoreRef {
                machine: MachineRef(String::new()),
                export: format!("local:{n}"),
            },
            db.as_ref().to_path_buf(),
            ReaderMode::Frozen,
        ));
        let _ = label;
        self
    }

    pub fn open_indexes(&self) -> rusqlite::Result<Vec<SqliteIndex>> {
        self.stores
            .iter()
            .map(|(_, path, mode)| SqliteIndex::open_reader_mode(&path.to_string_lossy(), *mode))
            .collect()
    }
}

impl Owner for LocalOwner {
    fn open(&self) -> rusqlite::Result<Vec<StoreDescription>> {
        Ok(self
            .stores
            .iter()
            .map(|(store, db, reader_mode)| StoreDescription {
                store: StoreRef {
                    machine: self.machine.clone(),
                    export: store.export.clone(),
                },
                db: db.clone(),
                reader_mode: *reader_mode,
            })
            .collect())
    }
}

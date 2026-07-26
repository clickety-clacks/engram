pub mod atomic;
pub mod tapes;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageLayout {
    ContentAddressed,
}

pub mod atomic;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageLayout {
    ContentAddressed,
}

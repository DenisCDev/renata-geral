//! Repository metadata management module.
//!
//! This module provides:
//! - `watcher`: Debounced file watching with hybrid native/polling support
//! - `model`: Repository metadata model for processing events
//! - `file_tree_store`: Bounded LRU cache for file tree state

pub mod file_tree_store;
pub mod model;
pub mod watcher;

pub use file_tree_store::{FileEntry, FileTreeEntryState, FileTreeStats};
pub use model::{FileTreeUpdate, RepositoryMetadataModel, RepositoryMetadataModelBuilder};
pub use watcher::{
    DebouncedFileWatcher, FileChangeEvent, FileChangeKind, HybridFileWatcher, WatcherError,
};

//! File tree state storage with bounded LRU cache.
//!
//! This module provides `FileTreeEntryState` which maintains the file tree
//! in memory with an LRU cache to prevent unbounded memory growth.

use lru::LruCache;
use std::collections::HashSet;
use std::fs;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Metadata for a single file or directory entry.
#[derive(Debug, Clone)]
pub struct FileEntry {
    /// The full path to the file/directory.
    pub path: PathBuf,
    /// Whether this is a directory.
    pub is_dir: bool,
    /// File size in bytes (0 for directories).
    pub size: u64,
    /// Last modification time.
    pub modified: Option<SystemTime>,
    /// Whether the entry is marked as dirty (needs refresh).
    pub dirty: bool,
}

impl FileEntry {
    /// Create a new FileEntry from a path by reading its metadata.
    pub fn from_path(path: &Path) -> Self {
        let metadata = fs::metadata(path).ok();

        Self {
            path: path.to_path_buf(),
            is_dir: metadata.as_ref().map(|m| m.is_dir()).unwrap_or(false),
            size: metadata.as_ref().map(|m| m.len()).unwrap_or(0),
            modified: metadata.and_then(|m| m.modified().ok()),
            dirty: false,
        }
    }

    /// Create a placeholder entry (for when we can't read metadata).
    pub fn placeholder(path: &Path, is_dir: bool) -> Self {
        Self {
            path: path.to_path_buf(),
            is_dir,
            size: 0,
            modified: None,
            dirty: true,
        }
    }

    /// Refresh the metadata from disk.
    pub fn refresh_metadata(&mut self) {
        if let Ok(metadata) = fs::metadata(&self.path) {
            self.is_dir = metadata.is_dir();
            self.size = metadata.len();
            self.modified = metadata.modified().ok();
            self.dirty = false;
        } else {
            self.dirty = true;
        }
    }
}

/// File tree state with bounded LRU cache.
///
/// This structure maintains the file tree in memory while:
/// - Preventing unbounded memory growth via LRU eviction
/// - Tracking expanded directories for UI state
/// - Supporting efficient in-place updates (no cloning)
pub struct FileTreeEntryState {
    /// LRU cache of file entries, bounded by max_entries.
    entries: LruCache<PathBuf, FileEntry>,
    /// Set of directories currently expanded in the UI.
    expanded_dirs: HashSet<PathBuf>,
    /// Maximum number of entries before LRU eviction.
    max_entries: usize,
    /// Statistics for debugging/monitoring.
    stats: FileTreeStats,
}

/// Statistics about the file tree state.
#[derive(Debug, Default, Clone)]
pub struct FileTreeStats {
    /// Total entries added since creation.
    pub total_adds: u64,
    /// Total entries removed since creation.
    pub total_removes: u64,
    /// Total entries evicted by LRU.
    pub total_evictions: u64,
    /// Total metadata refreshes.
    pub total_refreshes: u64,
}

impl FileTreeEntryState {
    /// Create a new file tree state with the given capacity.
    ///
    /// # Arguments
    /// * `max_entries` - Maximum number of entries before LRU eviction kicks in.
    ///                   Recommended: 50,000-100,000 for typical projects.
    pub fn new(max_entries: usize) -> Self {
        Self {
            entries: LruCache::new(NonZeroUsize::new(max_entries).unwrap_or(NonZeroUsize::MIN)),
            expanded_dirs: HashSet::new(),
            max_entries,
            stats: FileTreeStats::default(),
        }
    }

    /// Get the current number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Check if the tree is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Get the maximum capacity.
    pub fn capacity(&self) -> usize {
        self.max_entries
    }

    /// Get the current statistics.
    pub fn stats(&self) -> &FileTreeStats {
        &self.stats
    }

    /// Clear all entries.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.expanded_dirs.clear();
    }

    /// Add an entry to the tree.
    ///
    /// If the entry already exists, it will be updated.
    /// If the cache is full, the least recently used entry will be evicted.
    pub fn add_entry(&mut self, path: &Path) {
        let entry = FileEntry::from_path(path);

        // Track if we're evicting
        if self.entries.len() >= self.max_entries {
            self.stats.total_evictions += 1;
        }

        self.entries.put(path.to_path_buf(), entry);
        self.stats.total_adds += 1;
    }

    /// Update an existing entry's metadata.
    ///
    /// If the entry doesn't exist, it will be added.
    pub fn update_entry(&mut self, path: &Path) {
        if let Some(entry) = self.entries.get_mut(path) {
            entry.refresh_metadata();
            self.stats.total_refreshes += 1;
        } else {
            // Entry doesn't exist, add it
            self.add_entry(path);
        }
    }

    /// Remove an entry from the tree.
    pub fn remove_entry(&mut self, path: &Path) {
        if self.entries.pop(path).is_some() {
            self.stats.total_removes += 1;
        }

        // Also remove from expanded dirs if it was a directory
        self.expanded_dirs.remove(path);

        // Remove any child entries if this was a directory
        let path_prefix = path.to_path_buf();
        let children_to_remove: Vec<_> = self
            .entries
            .iter()
            .filter(|(p, _)| p.starts_with(&path_prefix) && *p != &path_prefix)
            .map(|(p, _)| p.clone())
            .collect();

        for child in children_to_remove {
            self.entries.pop(&child);
            self.expanded_dirs.remove(&child);
            self.stats.total_removes += 1;
        }
    }

    /// Get an entry by path.
    pub fn get(&mut self, path: &Path) -> Option<&FileEntry> {
        self.entries.get(path)
    }

    /// Get an entry by path without updating LRU order.
    pub fn peek(&self, path: &Path) -> Option<&FileEntry> {
        self.entries.peek(path)
    }

    /// Check if an entry exists.
    pub fn contains(&self, path: &Path) -> bool {
        self.entries.contains(path)
    }

    /// Mark a directory as expanded in the UI.
    pub fn expand_dir(&mut self, path: &Path) {
        self.expanded_dirs.insert(path.to_path_buf());
    }

    /// Mark a directory as collapsed in the UI.
    pub fn collapse_dir(&mut self, path: &Path) {
        self.expanded_dirs.remove(path);
    }

    /// Check if a directory is expanded.
    pub fn is_expanded(&self, path: &Path) -> bool {
        self.expanded_dirs.contains(path)
    }

    /// Get all expanded directories.
    pub fn expanded_dirs(&self) -> &HashSet<PathBuf> {
        &self.expanded_dirs
    }

    /// Get all entries as an iterator.
    pub fn iter(&self) -> impl Iterator<Item = (&PathBuf, &FileEntry)> {
        self.entries.iter()
    }

    /// Get children of a directory.
    pub fn children(&self, parent: &Path) -> Vec<&FileEntry> {
        self.entries
            .iter()
            .filter(|(path, _)| {
                path.parent() == Some(parent)
            })
            .map(|(_, entry)| entry)
            .collect()
    }

    /// Apply a notify event directly (in-place, no clone).
    ///
    /// This is the preferred method for handling file watcher events
    /// as it avoids cloning the entire tree state.
    pub fn apply_event(&mut self, event: &notify::Event) {
        use notify::EventKind;

        match event.kind {
            EventKind::Create(_) => {
                for path in &event.paths {
                    self.add_entry(path);
                }
            }
            EventKind::Remove(_) => {
                for path in &event.paths {
                    self.remove_entry(path);
                }
            }
            EventKind::Modify(_) => {
                for path in &event.paths {
                    self.update_entry(path);
                }
            }
            EventKind::Access(_) => {
                // Ignore access events
            }
            EventKind::Other | EventKind::Any => {
                // For unknown events, refresh if the path exists
                for path in &event.paths {
                    if path.exists() {
                        self.update_entry(path);
                    } else {
                        self.remove_entry(path);
                    }
                }
            }
        }

        // Ensure we stay within bounds
        self.enforce_capacity();
    }

    /// Enforce the maximum capacity by evicting LRU entries.
    fn enforce_capacity(&mut self) {
        while self.entries.len() > self.max_entries {
            if self.entries.pop_lru().is_some() {
                self.stats.total_evictions += 1;
            }
        }
    }

    /// Get a summary of the tree for debugging.
    pub fn debug_summary(&self) -> String {
        format!(
            "FileTreeEntryState: {} entries (max: {}), {} expanded dirs, stats: {:?}",
            self.entries.len(),
            self.max_entries,
            self.expanded_dirs.len(),
            self.stats
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_add_and_get_entry() {
        let mut tree = FileTreeEntryState::new(100);
        let temp = tempdir().unwrap();
        let file_path = temp.path().join("test.txt");
        fs::write(&file_path, "hello").unwrap();

        tree.add_entry(&file_path);

        assert!(tree.contains(&file_path));
        assert_eq!(tree.len(), 1);
    }

    #[test]
    fn test_remove_entry() {
        let mut tree = FileTreeEntryState::new(100);
        let temp = tempdir().unwrap();
        let file_path = temp.path().join("test.txt");
        fs::write(&file_path, "hello").unwrap();

        tree.add_entry(&file_path);
        assert!(tree.contains(&file_path));

        tree.remove_entry(&file_path);
        assert!(!tree.contains(&file_path));
    }

    #[test]
    fn test_lru_eviction() {
        let mut tree = FileTreeEntryState::new(3);

        // Add 4 entries to a tree with capacity 3
        for i in 0..4 {
            let path = PathBuf::from(format!("/fake/path{}", i));
            tree.entries.put(path, FileEntry::placeholder(&PathBuf::from(format!("/fake/path{}", i)), false));
        }

        // Should have evicted one entry
        assert_eq!(tree.len(), 3);
    }

    #[test]
    fn test_expand_collapse_dir() {
        let mut tree = FileTreeEntryState::new(100);
        let dir_path = PathBuf::from("/test/dir");

        assert!(!tree.is_expanded(&dir_path));

        tree.expand_dir(&dir_path);
        assert!(tree.is_expanded(&dir_path));

        tree.collapse_dir(&dir_path);
        assert!(!tree.is_expanded(&dir_path));
    }

    #[test]
    fn test_remove_dir_removes_children() {
        let mut tree = FileTreeEntryState::new(100);
        let parent = PathBuf::from("/test/parent");
        let child1 = PathBuf::from("/test/parent/child1.txt");
        let child2 = PathBuf::from("/test/parent/child2.txt");

        // Add entries using placeholder
        tree.entries.put(parent.clone(), FileEntry::placeholder(&parent, true));
        tree.entries.put(child1.clone(), FileEntry::placeholder(&child1, false));
        tree.entries.put(child2.clone(), FileEntry::placeholder(&child2, false));

        assert_eq!(tree.len(), 3);

        tree.remove_entry(&parent);

        // All entries should be removed
        assert_eq!(tree.len(), 0);
    }
}

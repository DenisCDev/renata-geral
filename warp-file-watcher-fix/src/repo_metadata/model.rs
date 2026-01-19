//! Repository metadata model for handling file watcher events.
//!
//! This module provides the `RepositoryMetadataModel` which processes file watcher
//! events in a background thread and notifies the UI thread via broadcast channel.

use crate::repo_metadata::file_tree_store::FileTreeEntryState;
use crate::repo_metadata::watcher::{FileChangeEvent, FileChangeKind};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};

/// Update notification sent to UI components.
#[derive(Debug, Clone)]
pub struct FileTreeUpdate {
    /// Paths that were affected by the change.
    pub paths: Vec<PathBuf>,
    /// The kind of change that occurred.
    pub kind: FileChangeKind,
    /// Whether this is a batch update (multiple files changed at once).
    pub is_batch: bool,
}

/// Model for managing repository metadata and file tree state.
///
/// This model:
/// - Maintains the file tree state in memory
/// - Processes file watcher events in background tasks
/// - Notifies UI components of changes via broadcast channel
pub struct RepositoryMetadataModel {
    /// The file tree state, protected by RwLock for concurrent access.
    file_tree: Arc<RwLock<FileTreeEntryState>>,
    /// Channel for notifying UI components of updates.
    ui_notifier: broadcast::Sender<FileTreeUpdate>,
    /// Root path being watched.
    root_path: PathBuf,
}

impl RepositoryMetadataModel {
    /// Creates a new repository metadata model.
    ///
    /// # Arguments
    /// * `root_path` - The root path of the repository/project
    /// * `max_entries` - Maximum entries in the file tree cache
    /// * `buffer_size` - Size of the UI notification buffer
    ///
    /// # Returns
    /// A tuple of (model, receiver) where receiver gets UI updates.
    pub fn new(
        root_path: PathBuf,
        max_entries: usize,
        buffer_size: usize,
    ) -> (Self, broadcast::Receiver<FileTreeUpdate>) {
        let (ui_notifier, ui_receiver) = broadcast::channel(buffer_size);

        (
            Self {
                file_tree: Arc::new(RwLock::new(FileTreeEntryState::new(max_entries))),
                ui_notifier,
                root_path,
            },
            ui_receiver,
        )
    }

    /// Get a read-only reference to the file tree state.
    pub fn file_tree(&self) -> Arc<RwLock<FileTreeEntryState>> {
        Arc::clone(&self.file_tree)
    }

    /// Get the root path being watched.
    pub fn root_path(&self) -> &PathBuf {
        &self.root_path
    }

    /// Subscribe to UI update notifications.
    pub fn subscribe(&self) -> broadcast::Receiver<FileTreeUpdate> {
        self.ui_notifier.subscribe()
    }

    /// Handle a batch of watcher events.
    ///
    /// This method spawns a background task to process the events,
    /// avoiding blocking the main/UI thread.
    pub fn handle_watcher_events(&self, events: Vec<FileChangeEvent>) {
        if events.is_empty() {
            return;
        }

        let file_tree = Arc::clone(&self.file_tree);
        let ui_notifier = self.ui_notifier.clone();
        let is_batch = events.len() > 1;

        // Process events in background task (NOT on main thread)
        tokio::spawn(async move {
            // Group events by kind for more efficient updates
            let mut creates: Vec<PathBuf> = Vec::new();
            let mut modifies: Vec<PathBuf> = Vec::new();
            let mut removes: Vec<PathBuf> = Vec::new();
            let mut renames: Vec<PathBuf> = Vec::new();

            for event in &events {
                match event.kind {
                    FileChangeKind::Create => creates.push(event.path.clone()),
                    FileChangeKind::Modify => modifies.push(event.path.clone()),
                    FileChangeKind::Remove => removes.push(event.path.clone()),
                    FileChangeKind::Rename => renames.push(event.path.clone()),
                }
            }

            // Update state in background (using write lock)
            {
                let mut tree = file_tree.write().await;

                // Apply creates
                for path in &creates {
                    tree.add_entry(path);
                }

                // Apply modifies
                for path in &modifies {
                    tree.update_entry(path);
                }

                // Apply removes
                for path in &removes {
                    tree.remove_entry(path);
                }

                // Apply renames (treat as remove + add for simplicity)
                for path in &renames {
                    tree.update_entry(path);
                }
            }

            // CRITICAL: Notify UI that changes occurred
            // Group by kind for efficient UI updates
            let updates = [
                (FileChangeKind::Create, creates),
                (FileChangeKind::Modify, modifies),
                (FileChangeKind::Remove, removes),
                (FileChangeKind::Rename, renames),
            ];

            for (kind, paths) in updates {
                if !paths.is_empty() {
                    let update = FileTreeUpdate {
                        paths,
                        kind,
                        is_batch,
                    };

                    if let Err(e) = ui_notifier.send(update) {
                        tracing::warn!("Failed to notify UI of file change: {}", e);
                    }
                }
            }
        });
    }

    /// Handle a single watcher event (convenience method).
    pub fn handle_watcher_event(&self, event: FileChangeEvent) {
        self.handle_watcher_events(vec![event]);
    }

    /// Force a full refresh of the file tree.
    ///
    /// This rescans the root directory and rebuilds the tree.
    /// Use sparingly as it can be expensive for large directories.
    pub async fn refresh(&self) -> std::io::Result<()> {
        let root = self.root_path.clone();
        let file_tree = Arc::clone(&self.file_tree);
        let ui_notifier = self.ui_notifier.clone();

        tokio::spawn(async move {
            // Scan directory in background
            match Self::scan_directory(&root).await {
                Ok(paths) => {
                    // Update tree with all found paths
                    {
                        let mut tree = file_tree.write().await;
                        tree.clear();
                        for path in &paths {
                            tree.add_entry(path);
                        }
                    }

                    // Notify UI of full refresh
                    let update = FileTreeUpdate {
                        paths,
                        kind: FileChangeKind::Create, // Treat as "all new" for UI
                        is_batch: true,
                    };

                    if let Err(e) = ui_notifier.send(update) {
                        tracing::warn!("Failed to notify UI of refresh: {}", e);
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to scan directory {}: {}", root.display(), e);
                }
            }
        });

        Ok(())
    }

    /// Scan a directory recursively, respecting gitignore patterns.
    async fn scan_directory(root: &PathBuf) -> std::io::Result<Vec<PathBuf>> {
        use std::fs;

        let mut paths = Vec::new();
        let mut stack = vec![root.clone()];

        while let Some(dir) = stack.pop() {
            match fs::read_dir(&dir) {
                Ok(entries) => {
                    for entry in entries.filter_map(|e| e.ok()) {
                        let path = entry.path();

                        // Skip hidden and common ignored directories
                        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                            if name.starts_with('.')
                                || name == "node_modules"
                                || name == "target"
                                || name == "__pycache__"
                                || name == ".git"
                            {
                                continue;
                            }
                        }

                        if path.is_dir() {
                            stack.push(path.clone());
                        }

                        paths.push(path);
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to read directory {}: {}", dir.display(), e);
                }
            }

            // Yield to prevent blocking too long
            tokio::task::yield_now().await;
        }

        Ok(paths)
    }
}

/// Builder for RepositoryMetadataModel with sensible defaults.
pub struct RepositoryMetadataModelBuilder {
    root_path: PathBuf,
    max_entries: usize,
    buffer_size: usize,
}

impl RepositoryMetadataModelBuilder {
    pub fn new(root_path: PathBuf) -> Self {
        Self {
            root_path,
            max_entries: 100_000, // Default: 100k entries
            buffer_size: 256,     // Default: 256 pending updates
        }
    }

    pub fn max_entries(mut self, max: usize) -> Self {
        self.max_entries = max;
        self
    }

    pub fn buffer_size(mut self, size: usize) -> Self {
        self.buffer_size = size;
        self
    }

    pub fn build(self) -> (RepositoryMetadataModel, broadcast::Receiver<FileTreeUpdate>) {
        RepositoryMetadataModel::new(self.root_path, self.max_entries, self.buffer_size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[tokio::test]
    async fn test_handle_create_event() {
        let (model, mut rx) = RepositoryMetadataModel::new(
            PathBuf::from("/test"),
            1000,
            16,
        );

        let event = FileChangeEvent {
            path: PathBuf::from("/test/new_file.txt"),
            kind: FileChangeKind::Create,
            timestamp: Instant::now(),
        };

        model.handle_watcher_event(event);

        // Wait for the background task to complete
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Check that UI was notified
        let update = rx.try_recv();
        assert!(update.is_ok());
        let update = update.unwrap();
        assert_eq!(update.kind, FileChangeKind::Create);
        assert_eq!(update.paths.len(), 1);
    }

    #[tokio::test]
    async fn test_handle_batch_events() {
        let (model, mut rx) = RepositoryMetadataModel::new(
            PathBuf::from("/test"),
            1000,
            16,
        );

        let events = vec![
            FileChangeEvent {
                path: PathBuf::from("/test/file1.txt"),
                kind: FileChangeKind::Create,
                timestamp: Instant::now(),
            },
            FileChangeEvent {
                path: PathBuf::from("/test/file2.txt"),
                kind: FileChangeKind::Create,
                timestamp: Instant::now(),
            },
        ];

        model.handle_watcher_events(events);

        // Wait for background task
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        let update = rx.try_recv();
        assert!(update.is_ok());
        let update = update.unwrap();
        assert!(update.is_batch);
        assert_eq!(update.paths.len(), 2);
    }
}

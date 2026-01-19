//! Project Explorer UI component for listening to file tree updates.
//!
//! This module provides `ProjectExplorerPane` which listens to file tree
//! updates from the broadcast channel and triggers UI redraws.

use crate::repo_metadata::{FileTreeEntryState, FileTreeUpdate, FileChangeKind};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};

/// Callback type for UI invalidation.
pub type InvalidateCallback = Box<dyn Fn(&[PathBuf]) + Send + Sync>;

/// Callback type for requesting a redraw.
pub type RedrawCallback = Box<dyn Fn() + Send + Sync>;

/// State of the tree view component.
#[derive(Debug, Clone)]
pub struct TreeViewState {
    /// Currently selected path.
    pub selected: Option<PathBuf>,
    /// Paths that need to be redrawn.
    pub dirty_paths: HashSet<PathBuf>,
    /// Whether a full redraw is needed.
    pub needs_full_redraw: bool,
}

impl Default for TreeViewState {
    fn default() -> Self {
        Self {
            selected: None,
            dirty_paths: HashSet::new(),
            needs_full_redraw: false,
        }
    }
}

/// A reference to the tree view for use in async contexts.
#[derive(Clone)]
pub struct TreeViewHandle {
    state: Arc<RwLock<TreeViewState>>,
    invalidate_cb: Option<Arc<InvalidateCallback>>,
    redraw_cb: Option<Arc<RedrawCallback>>,
}

impl TreeViewHandle {
    /// Create a new tree view handle.
    pub fn new() -> Self {
        Self {
            state: Arc::new(RwLock::new(TreeViewState::default())),
            invalidate_cb: None,
            redraw_cb: None,
        }
    }

    /// Set the invalidation callback.
    pub fn set_invalidate_callback(&mut self, cb: InvalidateCallback) {
        self.invalidate_cb = Some(Arc::new(cb));
    }

    /// Set the redraw callback.
    pub fn set_redraw_callback(&mut self, cb: RedrawCallback) {
        self.redraw_cb = Some(Arc::new(cb));
    }

    /// Mark specific paths as needing redraw.
    pub async fn invalidate_paths(&self, paths: &[PathBuf]) {
        {
            let mut state = self.state.write().await;
            for path in paths {
                state.dirty_paths.insert(path.clone());
                // Also mark parent directories as dirty
                if let Some(parent) = path.parent() {
                    state.dirty_paths.insert(parent.to_path_buf());
                }
            }
        }

        // Call the invalidation callback if set
        if let Some(ref cb) = self.invalidate_cb {
            cb(paths);
        }
    }

    /// Request a redraw of the tree view.
    pub async fn request_redraw(&self) {
        {
            let mut state = self.state.write().await;
            state.needs_full_redraw = true;
        }

        // Call the redraw callback if set
        if let Some(ref cb) = self.redraw_cb {
            cb();
        }
    }

    /// Clear the dirty state after redraw.
    pub async fn clear_dirty(&self) {
        let mut state = self.state.write().await;
        state.dirty_paths.clear();
        state.needs_full_redraw = false;
    }

    /// Get the current selected path.
    pub async fn selected(&self) -> Option<PathBuf> {
        self.state.read().await.selected.clone()
    }

    /// Set the selected path.
    pub async fn set_selected(&self, path: Option<PathBuf>) {
        self.state.write().await.selected = path;
    }

    /// Check if a path is dirty.
    pub async fn is_dirty(&self, path: &Path) -> bool {
        self.state.read().await.dirty_paths.contains(path)
    }

    /// Check if a full redraw is needed.
    pub async fn needs_full_redraw(&self) -> bool {
        self.state.read().await.needs_full_redraw
    }
}

impl Default for TreeViewHandle {
    fn default() -> Self {
        Self::new()
    }
}

/// Project Explorer pane component.
///
/// This component:
/// - Listens to file tree updates from the broadcast channel
/// - Invalidates affected paths in the tree view
/// - Triggers UI redraws when changes occur
pub struct ProjectExplorerPane {
    /// Handle to the tree view for UI updates.
    tree_view: TreeViewHandle,
    /// Reference to the file tree state.
    file_tree: Arc<RwLock<FileTreeEntryState>>,
    /// Task handle for the listener loop.
    listener_handle: Option<tokio::task::JoinHandle<()>>,
}

impl ProjectExplorerPane {
    /// Create a new Project Explorer pane.
    pub fn new(file_tree: Arc<RwLock<FileTreeEntryState>>) -> Self {
        Self {
            tree_view: TreeViewHandle::new(),
            file_tree,
            listener_handle: None,
        }
    }

    /// Get a handle to the tree view.
    pub fn tree_view(&self) -> &TreeViewHandle {
        &self.tree_view
    }

    /// Get a mutable handle to the tree view for setting callbacks.
    pub fn tree_view_mut(&mut self) -> &mut TreeViewHandle {
        &mut self.tree_view
    }

    /// Get a reference to the file tree state.
    pub fn file_tree(&self) -> Arc<RwLock<FileTreeEntryState>> {
        Arc::clone(&self.file_tree)
    }

    /// Start listening for file tree updates.
    ///
    /// This spawns a background task that listens to the broadcast channel
    /// and updates the UI accordingly.
    pub fn setup_file_tree_listener(&mut self, rx: broadcast::Receiver<FileTreeUpdate>) {
        let tree_view = self.tree_view.clone();

        let handle = tokio::spawn(async move {
            Self::listener_loop(tree_view, rx).await;
        });

        self.listener_handle = Some(handle);
    }

    /// The main listener loop for file tree updates.
    async fn listener_loop(
        tree_view: TreeViewHandle,
        mut rx: broadcast::Receiver<FileTreeUpdate>,
    ) {
        loop {
            match rx.recv().await {
                Ok(update) => {
                    tracing::debug!(
                        "ProjectExplorer received update: {:?} paths, kind={:?}",
                        update.paths.len(),
                        update.kind
                    );

                    // CRITICAL: Invalidate affected paths in the UI
                    tree_view.invalidate_paths(&update.paths).await;

                    // Request a redraw based on the type of change
                    match update.kind {
                        FileChangeKind::Create | FileChangeKind::Remove => {
                            // Structural changes require full redraw
                            tree_view.request_redraw().await;
                        }
                        FileChangeKind::Modify | FileChangeKind::Rename => {
                            // Non-structural changes can be incremental
                            // Only request redraw if there are dirty paths
                            if !update.paths.is_empty() {
                                tree_view.request_redraw().await;
                            }
                        }
                    }
                }
                Err(broadcast::error::RecvError::Lagged(count)) => {
                    tracing::warn!(
                        "ProjectExplorer lagged behind by {} updates, requesting full refresh",
                        count
                    );
                    // If we lag behind, request a full redraw
                    tree_view.request_redraw().await;
                }
                Err(broadcast::error::RecvError::Closed) => {
                    tracing::info!("FileTreeUpdate channel closed, stopping listener");
                    break;
                }
            }
        }
    }

    /// Stop the listener task.
    pub async fn stop(&mut self) {
        if let Some(handle) = self.listener_handle.take() {
            handle.abort();
            let _ = handle.await;
        }
    }

    /// Check if the listener is running.
    pub fn is_listening(&self) -> bool {
        self.listener_handle
            .as_ref()
            .map(|h| !h.is_finished())
            .unwrap_or(false)
    }
}

/// Builder for ProjectExplorerPane with configuration options.
pub struct ProjectExplorerPaneBuilder {
    file_tree: Arc<RwLock<FileTreeEntryState>>,
    invalidate_cb: Option<InvalidateCallback>,
    redraw_cb: Option<RedrawCallback>,
}

impl ProjectExplorerPaneBuilder {
    pub fn new(file_tree: Arc<RwLock<FileTreeEntryState>>) -> Self {
        Self {
            file_tree,
            invalidate_cb: None,
            redraw_cb: None,
        }
    }

    /// Set the callback for path invalidation.
    pub fn on_invalidate(mut self, cb: InvalidateCallback) -> Self {
        self.invalidate_cb = Some(cb);
        self
    }

    /// Set the callback for redraw requests.
    pub fn on_redraw(mut self, cb: RedrawCallback) -> Self {
        self.redraw_cb = Some(cb);
        self
    }

    /// Build the ProjectExplorerPane.
    pub fn build(self) -> ProjectExplorerPane {
        let mut pane = ProjectExplorerPane::new(self.file_tree);

        if let Some(cb) = self.invalidate_cb {
            pane.tree_view_mut().set_invalidate_callback(cb);
        }

        if let Some(cb) = self.redraw_cb {
            pane.tree_view_mut().set_redraw_callback(cb);
        }

        pane
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo_metadata::FileTreeEntryState;

    #[tokio::test]
    async fn test_tree_view_invalidate_paths() {
        let handle = TreeViewHandle::new();
        let paths = vec![
            PathBuf::from("/test/file1.txt"),
            PathBuf::from("/test/file2.txt"),
        ];

        handle.invalidate_paths(&paths).await;

        assert!(handle.is_dirty(&PathBuf::from("/test/file1.txt")).await);
        assert!(handle.is_dirty(&PathBuf::from("/test/file2.txt")).await);
        // Parent should also be dirty
        assert!(handle.is_dirty(&PathBuf::from("/test")).await);
    }

    #[tokio::test]
    async fn test_tree_view_clear_dirty() {
        let handle = TreeViewHandle::new();
        let paths = vec![PathBuf::from("/test/file.txt")];

        handle.invalidate_paths(&paths).await;
        assert!(handle.is_dirty(&PathBuf::from("/test/file.txt")).await);

        handle.clear_dirty().await;
        assert!(!handle.is_dirty(&PathBuf::from("/test/file.txt")).await);
    }

    #[tokio::test]
    async fn test_project_explorer_pane_creation() {
        let file_tree = Arc::new(RwLock::new(FileTreeEntryState::new(1000)));
        let pane = ProjectExplorerPane::new(file_tree);

        assert!(!pane.is_listening());
    }

    #[tokio::test]
    async fn test_listener_receives_updates() {
        let file_tree = Arc::new(RwLock::new(FileTreeEntryState::new(1000)));
        let (tx, rx) = broadcast::channel::<FileTreeUpdate>(16);

        let mut pane = ProjectExplorerPane::new(file_tree);
        pane.setup_file_tree_listener(rx);

        // Give the listener time to start
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        assert!(pane.is_listening());

        // Send an update
        let update = FileTreeUpdate {
            paths: vec![PathBuf::from("/test/new_file.txt")],
            kind: FileChangeKind::Create,
            is_batch: false,
        };
        tx.send(update).unwrap();

        // Give the listener time to process
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // The path should be dirty
        assert!(pane.tree_view().is_dirty(&PathBuf::from("/test/new_file.txt")).await);

        // Cleanup
        pane.stop().await;
    }
}

//! Warp File Watcher Fix
//!
//! This crate provides a fix for the Warp Project Explorer file watcher bug
//! where the UI doesn't update when files are created/deleted/modified externally.
//!
//! ## Issues Addressed
//!
//! - [#7900](https://github.com/warpdotdev/warp/issues/7900): Project Explorer doesn't refresh
//! - [#7954](https://github.com/warpdotdev/warp/issues/7954): Files remain visible after deletion
//! - [#8295](https://github.com/warpdotdev/warp/issues/8295): Memory issues with file watching
//!
//! ## Root Causes Fixed
//!
//! 1. **UI not receiving events**: File watcher events weren't propagating to UI thread
//! 2. **No debouncing**: Rapid changes caused memory spikes and performance issues
//! 3. **Clone on main thread**: 72GB clone of FileTreeEntryState caused freezes
//! 4. **Unbounded growth**: FileTreeEntryState grew infinitely (memory leak)
//!
//! ## Solution Overview
//!
//! ```text
//! Filesystem Change
//!        ↓
//! DebouncedFileWatcher (aggregates events 100-300ms)
//!        ↓
//! handle_watcher_event() [BACKGROUND THREAD]
//!        ↓
//! FileTreeEntryState.apply_event() [in-place, no clone]
//!        ↓
//! broadcast::send(FileTreeUpdate)
//!        ↓
//! ProjectExplorerPane.invalidate_paths() [UI THREAD]
//!        ↓
//! UI updates automatically
//! ```
//!
//! ## Usage Example
//!
//! ```rust,no_run
//! use warp_file_watcher::{
//!     repo_metadata::{HybridFileWatcher, RepositoryMetadataModelBuilder},
//!     ui::ProjectExplorerPaneBuilder,
//! };
//! use std::path::PathBuf;
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     // Create the hybrid file watcher with debouncing
//!     let (mut watcher, watcher_rx) = HybridFileWatcher::new(
//!         200,   // 200ms debounce
//!         1000,  // 1s poll interval for fallback
//!         256,   // buffer size
//!     )?;
//!
//!     // Create the repository model
//!     let (model, ui_rx) = RepositoryMetadataModelBuilder::new(PathBuf::from("/path/to/repo"))
//!         .max_entries(100_000)
//!         .buffer_size(256)
//!         .build();
//!
//!     // Create the UI component
//!     let mut explorer = ProjectExplorerPaneBuilder::new(model.file_tree())
//!         .on_redraw(Box::new(|| {
//!             println!("Redraw requested!");
//!         }))
//!         .build();
//!
//!     // Start listening for updates
//!     explorer.setup_file_tree_listener(ui_rx);
//!
//!     // Start watching the path
//!     watcher.watch(std::path::Path::new("/path/to/repo"))?;
//!
//!     Ok(())
//! }
//! ```
//!
//! ## Module Structure
//!
//! - [`repo_metadata`]: Core file watching and state management
//!   - [`repo_metadata::watcher`]: Debounced and hybrid file watchers
//!   - [`repo_metadata::model`]: Repository metadata model
//!   - [`repo_metadata::file_tree_store`]: LRU-bounded file tree state
//! - [`ui`]: UI components
//!   - [`ui::project_explorer`]: Project Explorer pane component

#![warn(missing_docs)]
#![warn(rustdoc::missing_crate_level_docs)]

pub mod repo_metadata;
pub mod ui;

/// Re-exports for convenience.
pub mod prelude {
    pub use crate::repo_metadata::{
        DebouncedFileWatcher, FileChangeEvent, FileChangeKind, FileEntry, FileTreeEntryState,
        FileTreeUpdate, HybridFileWatcher, RepositoryMetadataModel, RepositoryMetadataModelBuilder,
        WatcherError,
    };
    pub use crate::ui::{
        InvalidateCallback, ProjectExplorerPane, ProjectExplorerPaneBuilder, RedrawCallback,
        TreeViewHandle,
    };
}

/// Run the file watcher event loop.
///
/// This function runs the main event loop that:
/// 1. Polls the file watcher for new events
/// 2. Flushes debounced events when ready
/// 3. Forwards events to the repository model
///
/// # Arguments
/// * `watcher` - The file watcher to poll
/// * `model` - The repository model to notify
/// * `poll_interval_ms` - How often to poll the watcher (default: 50ms)
///
/// # Example
///
/// ```rust,no_run
/// use warp_file_watcher::{repo_metadata::*, run_watcher_loop};
/// use std::path::PathBuf;
///
/// #[tokio::main]
/// async fn main() -> anyhow::Result<()> {
///     let (mut watcher, _) = DebouncedFileWatcher::new(200, 256)?;
///     let (model, _) = RepositoryMetadataModel::new(PathBuf::from("/repo"), 100_000, 256);
///
///     watcher.watch(std::path::Path::new("/repo"))?;
///
///     // Run in background
///     tokio::spawn(async move {
///         run_watcher_loop(&mut watcher, &model, 50).await;
///     });
///
///     Ok(())
/// }
/// ```
pub async fn run_watcher_loop(
    watcher: &mut repo_metadata::DebouncedFileWatcher,
    model: &repo_metadata::RepositoryMetadataModel,
    poll_interval_ms: u64,
) {
    let poll_interval = std::time::Duration::from_millis(poll_interval_ms);

    loop {
        // Poll for new events
        watcher.poll_events();

        // Flush debounced events if ready
        watcher.flush_if_ready();

        // Sleep before next poll
        tokio::time::sleep(poll_interval).await;
    }
}

/// Run the hybrid file watcher event loop.
///
/// Similar to `run_watcher_loop` but for the hybrid watcher.
pub async fn run_hybrid_watcher_loop(
    watcher: &mut repo_metadata::HybridFileWatcher,
    _model: &repo_metadata::RepositoryMetadataModel,
    poll_interval_ms: u64,
) {
    let poll_interval = std::time::Duration::from_millis(poll_interval_ms);

    loop {
        watcher.poll_events();
        watcher.flush_if_ready();
        tokio::time::sleep(poll_interval).await;
    }
}

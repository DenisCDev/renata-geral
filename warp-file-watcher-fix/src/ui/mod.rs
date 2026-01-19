//! UI components module.
//!
//! This module provides UI components that integrate with the file watcher system.

pub mod project_explorer;

pub use project_explorer::{
    InvalidateCallback, ProjectExplorerPane, ProjectExplorerPaneBuilder, RedrawCallback,
    TreeViewHandle, TreeViewState,
};

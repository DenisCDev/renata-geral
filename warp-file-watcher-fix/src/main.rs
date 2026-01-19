//! Demo application for the Warp File Watcher Fix.
//!
//! This demonstrates the corrected file watcher flow where:
//! 1. File changes are debounced and coalesced
//! 2. Events are processed in background threads
//! 3. UI is notified via broadcast channel
//! 4. Memory is bounded by LRU cache

use anyhow::Result;
use std::path::PathBuf;
use tokio::sync::broadcast;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use warp_file_watcher::{
    prelude::*,
    repo_metadata::{FileTreeUpdate, HybridFileWatcher, RepositoryMetadataModelBuilder},
    ui::ProjectExplorerPaneBuilder,
};

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(tracing_subscriber::EnvFilter::from_default_env()
            .add_directive("warp_file_watcher=debug".parse()?))
        .init();

    println!("=== Warp File Watcher Fix Demo ===\n");
    println!("This demo shows the corrected file watcher implementation");
    println!("that fixes issues #7900, #7954, and #8295.\n");

    // Get the path to watch (current directory or argument)
    let watch_path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap());

    println!("Watching: {}\n", watch_path.display());

    // Create the hybrid file watcher with debouncing
    // - 200ms debounce: aggregate rapid events
    // - 1000ms poll interval: fallback for problematic filesystems
    // - 256 buffer size: reasonable queue size
    let (mut watcher, watcher_rx) = HybridFileWatcher::new(200, 1000, 256)?;

    // Create the repository model
    // - 100k max entries: bounded memory
    // - 256 buffer size: UI notification queue
    let (model, ui_rx) = RepositoryMetadataModelBuilder::new(watch_path.clone())
        .max_entries(100_000)
        .buffer_size(256)
        .build();

    // Create the UI component with callbacks
    let mut explorer = ProjectExplorerPaneBuilder::new(model.file_tree())
        .on_invalidate(Box::new(|paths| {
            println!("[UI] Invalidated {} paths", paths.len());
            for path in paths.iter().take(5) {
                println!("     - {}", path.display());
            }
            if paths.len() > 5 {
                println!("     ... and {} more", paths.len() - 5);
            }
        }))
        .on_redraw(Box::new(|| {
            println!("[UI] Redraw requested");
        }))
        .build();

    // Start listening for UI updates
    explorer.setup_file_tree_listener(ui_rx);

    // Start watching the path
    watcher.watch(&watch_path)?;

    if watcher.is_using_polling(&watch_path) {
        println!("Note: Using polling fallback for this path\n");
    } else {
        println!("Using native file watcher\n");
    }

    println!("Press Ctrl+C to exit\n");
    println!("Try creating, modifying, or deleting files in the watched directory.\n");
    println!("---\n");

    // Subscribe to watcher events for demo output
    let mut watcher_subscriber = watcher.subscribe();

    // Spawn task to forward watcher events to model
    let model_for_task = std::sync::Arc::new(model);
    let model_clone = model_for_task.clone();
    tokio::spawn(async move {
        let mut rx = watcher_subscriber;
        while let Ok(events) = rx.recv().await {
            println!("\n[Watcher] Received {} debounced events:", events.len());
            for event in &events {
                println!(
                    "  {:?}: {}",
                    event.kind,
                    event.path.display()
                );
            }
            model_clone.handle_watcher_events(events);
        }
    });

    // Run the watcher event loop
    let poll_interval = std::time::Duration::from_millis(50);

    // Handle Ctrl+C gracefully
    let (shutdown_tx, mut shutdown_rx) = broadcast::channel::<()>(1);

    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.expect("Failed to listen for Ctrl+C");
        println!("\nShutting down...");
        let _ = shutdown_tx.send(());
    });

    loop {
        tokio::select! {
            _ = shutdown_rx.recv() => {
                break;
            }
            _ = tokio::time::sleep(poll_interval) => {
                watcher.poll_events();
                watcher.flush_if_ready();
            }
        }
    }

    // Cleanup
    explorer.stop().await;
    println!("Done!");

    Ok(())
}

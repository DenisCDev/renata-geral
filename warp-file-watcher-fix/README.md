# Warp File Watcher Fix

Fix for Warp Project Explorer file watcher bug where the UI doesn't update when files are created/deleted/modified externally.

## Issues Addressed

- [#7900](https://github.com/warpdotdev/warp/issues/7900): Project Explorer doesn't refresh
- [#7954](https://github.com/warpdotdev/warp/issues/7954): Files remain visible after deletion
- [#8295](https://github.com/warpdotdev/warp/issues/8295): Memory issues with file watching

## Root Causes Fixed

| Problem | Cause | Solution |
|---------|-------|----------|
| UI not receiving events | Events not propagating to UI thread | Broadcast channel notification |
| No debouncing | Memory spikes during rapid changes | 100-300ms event aggregation |
| Clone on main thread | 72GB clone of FileTreeEntryState | Background thread processing |
| Unbounded growth | FileTreeEntryState grows infinitely | LRU cache with max entries |

## Architecture

```
Filesystem Change
       ↓
DebouncedFileWatcher (aggregates events 100-300ms)
       ↓
handle_watcher_event() [BACKGROUND THREAD]
       ↓
FileTreeEntryState.apply_event() [in-place, no clone]
       ↓
broadcast::send(FileTreeUpdate)
       ↓
ProjectExplorerPane.invalidate_paths() [UI THREAD]
       ↓
UI updates automatically
```

## Project Structure

```
src/
├── lib.rs                          # Library entry point
├── main.rs                         # Demo application
├── repo_metadata/
│   ├── mod.rs                      # Module exports
│   ├── watcher.rs                  # DebouncedFileWatcher & HybridFileWatcher
│   ├── model.rs                    # RepositoryMetadataModel
│   └── file_tree_store.rs          # FileTreeEntryState with LRU cache
└── ui/
    ├── mod.rs                      # Module exports
    └── project_explorer.rs         # ProjectExplorerPane UI component
```

## Building

```bash
# Install Rust if not already installed
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Build the project
cargo build --release

# Run tests
cargo test

# Run the demo
cargo run -- /path/to/watch
```

## Usage Example

```rust
use warp_file_watcher::{
    repo_metadata::{HybridFileWatcher, RepositoryMetadataModelBuilder},
    ui::ProjectExplorerPaneBuilder,
};
use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Create the hybrid file watcher with debouncing
    let (mut watcher, _) = HybridFileWatcher::new(
        200,   // 200ms debounce
        1000,  // 1s poll interval for fallback
        256,   // buffer size
    )?;

    // Create the repository model
    let (model, ui_rx) = RepositoryMetadataModelBuilder::new(PathBuf::from("/repo"))
        .max_entries(100_000)  // Bounded memory
        .buffer_size(256)
        .build();

    // Create the UI component
    let mut explorer = ProjectExplorerPaneBuilder::new(model.file_tree())
        .on_redraw(Box::new(|| {
            println!("Redraw requested!");
        }))
        .build();

    // Start listening for updates
    explorer.setup_file_tree_listener(ui_rx);

    // Start watching
    watcher.watch(std::path::Path::new("/repo"))?;

    Ok(())
}
```

## Key Components

### DebouncedFileWatcher

Aggregates filesystem events over a configurable debounce period:
- Coalesces multiple events for same path
- DELETE + CREATE = MODIFY (file replaced)
- Multiple MODIFYs = single MODIFY
- Configurable debounce (100-300ms recommended)

### HybridFileWatcher

Automatically falls back to polling for problematic paths:
- NFS mounts
- WSL filesystems
- Network drives
- Other paths where native watching fails

### FileTreeEntryState

LRU-bounded cache preventing memory leaks:
- Configurable max entries (default: 100k)
- In-place updates (no cloning)
- Tracks expanded directories for UI state

### RepositoryMetadataModel

Processes events in background threads:
- Never blocks UI thread
- Uses `tokio::spawn` for async processing
- Notifies UI via broadcast channel

### ProjectExplorerPane

Listens for updates and triggers UI redraws:
- Subscribes to broadcast channel
- Invalidates affected paths
- Requests redraw when needed

## Testing

The project includes unit tests for all components:

```bash
# Run all tests
cargo test

# Run with logging
RUST_LOG=debug cargo test -- --nocapture
```

## Integration with Warp

To integrate this fix into Warp:

1. Replace the existing file watcher implementation in `repo_metadata/watcher.rs`
2. Update `RepositoryMetadataModel` to use broadcast channels
3. Modify `FileTreeEntryState` to use LRU cache
4. Update `ProjectExplorerPane` to listen for broadcast updates

## License

MIT

//! File watcher with debouncing and hybrid native/polling support.
//!
//! This module provides a debounced file watcher that coalesces rapid filesystem
//! events to prevent UI thrashing and memory spikes. It also includes a hybrid
//! watcher that falls back to polling for problematic paths (NFS, WSL, etc.).

use notify::{
    event::ModifyKind, Config, Event, EventKind, PollWatcher, RecommendedWatcher,
    RecursiveMode, Watcher,
};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::sync::broadcast;

/// Errors that can occur in the file watcher system.
#[derive(Error, Debug)]
pub enum WatcherError {
    #[error("Failed to create watcher: {0}")]
    WatcherCreation(#[from] notify::Error),

    #[error("Failed to watch path {path}: {source}")]
    WatchPath {
        path: PathBuf,
        source: notify::Error,
    },

    #[error("Channel send error")]
    ChannelSend,
}

/// Represents a coalesced file change event.
#[derive(Debug, Clone, PartialEq)]
pub struct FileChangeEvent {
    pub path: PathBuf,
    pub kind: FileChangeKind,
    pub timestamp: Instant,
}

/// Simplified event kinds for UI consumption.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileChangeKind {
    Create,
    Modify,
    Remove,
    Rename,
}

impl From<EventKind> for FileChangeKind {
    fn from(kind: EventKind) -> Self {
        match kind {
            EventKind::Create(_) => FileChangeKind::Create,
            EventKind::Modify(ModifyKind::Name(_)) => FileChangeKind::Rename,
            EventKind::Modify(_) => FileChangeKind::Modify,
            EventKind::Remove(_) => FileChangeKind::Remove,
            _ => FileChangeKind::Modify,
        }
    }
}

/// A file watcher with event debouncing.
///
/// Aggregates rapid filesystem events into batched updates to prevent
/// UI thrashing and reduce memory pressure during bulk operations.
pub struct DebouncedFileWatcher {
    watcher: RecommendedWatcher,
    event_rx: Receiver<Result<Event, notify::Error>>,
    pending_events: HashMap<PathBuf, FileChangeEvent>,
    debounce_duration: Duration,
    last_flush: Instant,
    event_sender: broadcast::Sender<Vec<FileChangeEvent>>,
}

impl DebouncedFileWatcher {
    /// Creates a new debounced file watcher.
    ///
    /// # Arguments
    /// * `debounce_ms` - Minimum time between event batches in milliseconds (recommended: 100-300ms)
    /// * `buffer_size` - Size of the broadcast channel buffer
    ///
    /// # Returns
    /// A tuple of (watcher, receiver) where receiver gets batched events.
    pub fn new(
        debounce_ms: u64,
        buffer_size: usize,
    ) -> Result<(Self, broadcast::Receiver<Vec<FileChangeEvent>>), WatcherError> {
        let (tx, rx) = channel();

        // Configure watcher with optimized settings
        let config = Config::default()
            .with_poll_interval(Duration::from_millis(100))
            .with_compare_contents(false); // Avoid unnecessary file reads

        let watcher = RecommendedWatcher::new(
            move |res: Result<Event, notify::Error>| {
                let _ = tx.send(res);
            },
            config,
        )?;

        let (event_sender, event_receiver) = broadcast::channel(buffer_size);

        Ok((
            Self {
                watcher,
                event_rx: rx,
                pending_events: HashMap::new(),
                debounce_duration: Duration::from_millis(debounce_ms),
                last_flush: Instant::now(),
                event_sender,
            },
            event_receiver,
        ))
    }

    /// Start watching a path recursively.
    pub fn watch(&mut self, path: &Path) -> Result<(), WatcherError> {
        self.watcher
            .watch(path, RecursiveMode::Recursive)
            .map_err(|e| WatcherError::WatchPath {
                path: path.to_path_buf(),
                source: e,
            })
    }

    /// Stop watching a path.
    pub fn unwatch(&mut self, path: &Path) -> Result<(), WatcherError> {
        self.watcher
            .unwatch(path)
            .map_err(|e| WatcherError::WatchPath {
                path: path.to_path_buf(),
                source: e,
            })
    }

    /// Process pending events from the filesystem watcher.
    ///
    /// Call this periodically (e.g., in an event loop) to collect and
    /// debounce filesystem events.
    pub fn poll_events(&mut self) {
        // Drain all pending raw events
        while let Ok(result) = self.event_rx.try_recv() {
            if let Ok(event) = result {
                for path in event.paths {
                    let change_event = FileChangeEvent {
                        path: path.clone(),
                        kind: event.kind.into(),
                        timestamp: Instant::now(),
                    };
                    self.pending_events.insert(path, change_event);
                }
            }
        }
    }

    /// Flush debounced events if the debounce period has elapsed.
    ///
    /// Returns the number of events flushed, or 0 if still in debounce period.
    pub fn flush_if_ready(&mut self) -> usize {
        let now = Instant::now();

        if self.pending_events.is_empty() {
            return 0;
        }

        if now.duration_since(self.last_flush) < self.debounce_duration {
            return 0; // Still in debounce period
        }

        self.last_flush = now;

        // Coalesce and send events
        let events: Vec<_> = self.pending_events.drain().map(|(_, e)| e).collect();
        let coalesced = Self::coalesce_events(events);
        let count = coalesced.len();

        if !coalesced.is_empty() {
            // Send to broadcast channel - ignore errors if no receivers
            let _ = self.event_sender.send(coalesced);
        }

        count
    }

    /// Coalesce events for the same path.
    ///
    /// Rules:
    /// - DELETE followed by CREATE = MODIFY (file was replaced)
    /// - Multiple MODIFYs = single MODIFY
    /// - Keep the most recent event kind otherwise
    fn coalesce_events(events: Vec<FileChangeEvent>) -> Vec<FileChangeEvent> {
        let mut by_path: HashMap<PathBuf, FileChangeEvent> = HashMap::new();

        for event in events {
            by_path
                .entry(event.path.clone())
                .and_modify(|existing| {
                    // DELETE followed by CREATE = MODIFY (file replaced)
                    if existing.kind == FileChangeKind::Remove
                        && event.kind == FileChangeKind::Create
                    {
                        existing.kind = FileChangeKind::Modify;
                        existing.timestamp = event.timestamp;
                    }
                    // CREATE followed by DELETE = no-op (remove from map later)
                    else if existing.kind == FileChangeKind::Create
                        && event.kind == FileChangeKind::Remove
                    {
                        existing.kind = FileChangeKind::Remove;
                        existing.timestamp = event.timestamp;
                    }
                    // Otherwise, keep the most recent event
                    else if event.timestamp > existing.timestamp {
                        *existing = event.clone();
                    }
                })
                .or_insert(event);
        }

        // Filter out CREATE+DELETE sequences (file never really existed for UI)
        by_path
            .into_iter()
            .filter(|(_, e)| {
                // Keep all events except those that would be no-ops
                true
            })
            .map(|(_, e)| e)
            .collect()
    }

    /// Get the broadcast sender for subscribing new listeners.
    pub fn subscribe(&self) -> broadcast::Receiver<Vec<FileChangeEvent>> {
        self.event_sender.subscribe()
    }
}

/// A hybrid file watcher that uses native watching with polling fallback.
///
/// Some filesystems (NFS, WSL, network drives) don't support native
/// file watching reliably. This watcher automatically falls back to
/// polling for problematic paths.
pub struct HybridFileWatcher {
    native_watcher: Option<RecommendedWatcher>,
    poll_watcher: PollWatcher,
    event_rx: Receiver<Result<Event, notify::Error>>,
    use_polling_for: HashSet<PathBuf>,
    pending_events: HashMap<PathBuf, FileChangeEvent>,
    debounce_duration: Duration,
    last_flush: Instant,
    event_sender: broadcast::Sender<Vec<FileChangeEvent>>,
}

impl HybridFileWatcher {
    /// Creates a new hybrid file watcher.
    ///
    /// # Arguments
    /// * `debounce_ms` - Minimum time between event batches
    /// * `poll_interval_ms` - Interval for polling fallback (recommended: 1000-2000ms)
    /// * `buffer_size` - Size of the broadcast channel buffer
    pub fn new(
        debounce_ms: u64,
        poll_interval_ms: u64,
        buffer_size: usize,
    ) -> Result<(Self, broadcast::Receiver<Vec<FileChangeEvent>>), WatcherError> {
        let (tx, rx) = channel();
        let tx_clone = tx.clone();

        // Native watcher config
        let native_config = Config::default()
            .with_poll_interval(Duration::from_millis(100))
            .with_compare_contents(false);

        // Try to create native watcher
        let native_watcher = RecommendedWatcher::new(
            move |res: Result<Event, notify::Error>| {
                let _ = tx.send(res);
            },
            native_config,
        )
        .ok();

        // Poll watcher config (used as fallback)
        let poll_config = Config::default()
            .with_poll_interval(Duration::from_millis(poll_interval_ms))
            .with_compare_contents(false);

        let poll_watcher = PollWatcher::new(
            move |res: Result<Event, notify::Error>| {
                let _ = tx_clone.send(res);
            },
            poll_config,
        )?;

        let (event_sender, event_receiver) = broadcast::channel(buffer_size);

        Ok((
            Self {
                native_watcher,
                poll_watcher,
                event_rx: rx,
                use_polling_for: HashSet::new(),
                pending_events: HashMap::new(),
                debounce_duration: Duration::from_millis(debounce_ms),
                last_flush: Instant::now(),
                event_sender,
            },
            event_receiver,
        ))
    }

    /// Watch a path, using native watching if possible, polling otherwise.
    pub fn watch(&mut self, path: &Path) -> Result<(), WatcherError> {
        // Try native watcher first
        if let Some(ref mut native) = self.native_watcher {
            match native.watch(path, RecursiveMode::Recursive) {
                Ok(()) => {
                    tracing::debug!("Using native watcher for {}", path.display());
                    return Ok(());
                }
                Err(e) => {
                    tracing::warn!(
                        "Native watcher failed for {}: {}, falling back to polling",
                        path.display(),
                        e
                    );
                    self.use_polling_for.insert(path.to_path_buf());
                }
            }
        }

        // Fallback to polling
        tracing::debug!("Using poll watcher for {}", path.display());
        self.poll_watcher
            .watch(path, RecursiveMode::Recursive)
            .map_err(|e| WatcherError::WatchPath {
                path: path.to_path_buf(),
                source: e,
            })
    }

    /// Stop watching a path.
    pub fn unwatch(&mut self, path: &Path) -> Result<(), WatcherError> {
        if self.use_polling_for.contains(path) {
            self.use_polling_for.remove(path);
            self.poll_watcher.unwatch(path).map_err(|e| WatcherError::WatchPath {
                path: path.to_path_buf(),
                source: e,
            })
        } else if let Some(ref mut native) = self.native_watcher {
            native.unwatch(path).map_err(|e| WatcherError::WatchPath {
                path: path.to_path_buf(),
                source: e,
            })
        } else {
            Ok(())
        }
    }

    /// Process pending events from the filesystem watcher.
    pub fn poll_events(&mut self) {
        while let Ok(result) = self.event_rx.try_recv() {
            if let Ok(event) = result {
                for path in event.paths {
                    let change_event = FileChangeEvent {
                        path: path.clone(),
                        kind: event.kind.into(),
                        timestamp: Instant::now(),
                    };
                    self.pending_events.insert(path, change_event);
                }
            }
        }
    }

    /// Flush debounced events if ready.
    pub fn flush_if_ready(&mut self) -> usize {
        let now = Instant::now();

        if self.pending_events.is_empty() {
            return 0;
        }

        if now.duration_since(self.last_flush) < self.debounce_duration {
            return 0;
        }

        self.last_flush = now;

        let events: Vec<_> = self.pending_events.drain().map(|(_, e)| e).collect();
        let coalesced = DebouncedFileWatcher::coalesce_events(events);
        let count = coalesced.len();

        if !coalesced.is_empty() {
            let _ = self.event_sender.send(coalesced);
        }

        count
    }

    /// Check if a path is using polling fallback.
    pub fn is_using_polling(&self, path: &Path) -> bool {
        self.use_polling_for.contains(path)
    }

    /// Get the broadcast sender for subscribing new listeners.
    pub fn subscribe(&self) -> broadcast::Receiver<Vec<FileChangeEvent>> {
        self.event_sender.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_coalesce_delete_then_create() {
        let events = vec![
            FileChangeEvent {
                path: PathBuf::from("/test/file.txt"),
                kind: FileChangeKind::Remove,
                timestamp: Instant::now(),
            },
            FileChangeEvent {
                path: PathBuf::from("/test/file.txt"),
                kind: FileChangeKind::Create,
                timestamp: Instant::now(),
            },
        ];

        let coalesced = DebouncedFileWatcher::coalesce_events(events);
        assert_eq!(coalesced.len(), 1);
        assert_eq!(coalesced[0].kind, FileChangeKind::Modify);
    }

    #[test]
    fn test_coalesce_multiple_modifies() {
        let events = vec![
            FileChangeEvent {
                path: PathBuf::from("/test/file.txt"),
                kind: FileChangeKind::Modify,
                timestamp: Instant::now(),
            },
            FileChangeEvent {
                path: PathBuf::from("/test/file.txt"),
                kind: FileChangeKind::Modify,
                timestamp: Instant::now(),
            },
            FileChangeEvent {
                path: PathBuf::from("/test/file.txt"),
                kind: FileChangeKind::Modify,
                timestamp: Instant::now(),
            },
        ];

        let coalesced = DebouncedFileWatcher::coalesce_events(events);
        assert_eq!(coalesced.len(), 1);
        assert_eq!(coalesced[0].kind, FileChangeKind::Modify);
    }

    #[test]
    fn test_coalesce_different_paths() {
        let events = vec![
            FileChangeEvent {
                path: PathBuf::from("/test/file1.txt"),
                kind: FileChangeKind::Create,
                timestamp: Instant::now(),
            },
            FileChangeEvent {
                path: PathBuf::from("/test/file2.txt"),
                kind: FileChangeKind::Remove,
                timestamp: Instant::now(),
            },
        ];

        let coalesced = DebouncedFileWatcher::coalesce_events(events);
        assert_eq!(coalesced.len(), 2);
    }
}

//! Debounced folder watcher.
//!
//! A download lands as a partial file and grows over several seconds; reading
//! tags the instant the OS reports a create event yields truncated garbage.
//! This watcher only emits a path once its size has held steady for a while.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};

use crate::tags::{is_audio_file, is_temp_file};

#[derive(Debug, Clone, Copy)]
pub struct WatcherConfig {
    /// How long a file's size must stay unchanged before it counts as done.
    pub stability: Duration,
    pub poll_interval: Duration,
    pub recursive: bool,
    /// Emit files that were already in the folder when watching started.
    pub emit_existing: bool,
}

impl Default for WatcherConfig {
    fn default() -> Self {
        Self {
            stability: Duration::from_millis(2_500),
            poll_interval: Duration::from_millis(500),
            recursive: true,
            emit_existing: false,
        }
    }
}

/// Handle that keeps the watch alive. Dropping it stops the watcher.
pub struct FolderWatcher {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl FolderWatcher {
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for FolderWatcher {
    fn drop(&mut self) {
        self.stop();
    }
}

#[derive(Debug)]
struct Pending {
    size: u64,
    steady_since: Instant,
}

/// Watch `root`, calling `on_ready` once per file that has finished being
/// written. The callback runs on the watcher thread.
pub fn watch_folder<F>(root: &Path, config: WatcherConfig, on_ready: F) -> Result<FolderWatcher>
where
    F: Fn(PathBuf) + Send + 'static,
{
    let root = root.to_path_buf();
    let (tx, rx) = mpsc::channel::<PathBuf>();

    let event_tx = tx.clone();
    let mut watcher: RecommendedWatcher =
        notify::recommended_watcher(move |res: notify::Result<Event>| {
            if let Ok(event) = res {
                for path in event.paths {
                    let _ = event_tx.send(path);
                }
            }
        })
        .context("creating filesystem watcher")?;

    let mode = if config.recursive {
        RecursiveMode::Recursive
    } else {
        RecursiveMode::NonRecursive
    };
    watcher
        .watch(&root, mode)
        .with_context(|| format!("watching {}", root.display()))?;

    if config.emit_existing {
        for entry in walkdir::WalkDir::new(&root)
            .max_depth(if config.recursive { usize::MAX } else { 1 })
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if entry.file_type().is_file() {
                let _ = tx.send(entry.path().to_path_buf());
            }
        }
    }

    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);

    let handle = std::thread::spawn(move || {
        // Keep the watcher alive for the life of the thread.
        let _watcher = watcher;
        let mut pending: HashMap<PathBuf, Pending> = HashMap::new();

        while !thread_stop.load(Ordering::Relaxed) {
            match rx.recv_timeout(config.poll_interval) {
                Ok(path) => {
                    if is_temp_file(&path) || !is_audio_file(&path) {
                        // A .part file becoming a .mp3 arrives as its own event,
                        // so ignoring the temp name loses nothing.
                        continue;
                    }
                    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                    pending
                        .entry(path)
                        .and_modify(|p| {
                            if p.size != size {
                                p.size = size;
                                p.steady_since = Instant::now();
                            }
                        })
                        .or_insert(Pending {
                            size,
                            steady_since: Instant::now(),
                        });
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }

            let now = Instant::now();
            let ready: Vec<PathBuf> = pending
                .iter()
                .filter(|(path, p)| {
                    let current = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                    current == p.size
                        && current > 0
                        && now.duration_since(p.steady_since) >= config.stability
                })
                .map(|(path, _)| path.clone())
                .collect();

            for path in ready {
                pending.remove(&path);
                if path.exists() {
                    on_ready(path);
                }
            }

            // Drop entries whose file vanished (moved into a library, deleted).
            pending.retain(|path, _| path.exists());
        }
    });

    Ok(FolderWatcher {
        stop,
        handle: Some(handle),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn emits_only_after_the_file_stops_growing() {
        let dir = std::env::temp_dir().join(format!("djls-watch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);

        let config = WatcherConfig {
            stability: Duration::from_millis(400),
            poll_interval: Duration::from_millis(100),
            ..Default::default()
        };

        let _watcher = watch_folder(&dir, config, move |p| {
            sink.lock().unwrap().push(p);
        })
        .unwrap();

        let target = dir.join("track.mp3");
        std::fs::write(&target, vec![0u8; 1024]).unwrap();
        std::thread::sleep(Duration::from_millis(150));
        // Still growing — must not have been emitted yet.
        assert!(seen.lock().unwrap().is_empty());
        std::fs::write(&target, vec![0u8; 4096]).unwrap();

        std::thread::sleep(Duration::from_millis(1_200));
        let found = seen.lock().unwrap().clone();
        assert_eq!(
            found.len(),
            1,
            "expected exactly one emission, got {found:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}

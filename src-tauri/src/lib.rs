//! Tauri bridge — Phase 0.
//!
//! All the real logic lives in `djls-core` so it stays testable and reusable by
//! the headless CLI. This layer only exposes it to the window.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use djls_core::tags::{scan_folder, LocalTrack};
use djls_core::watcher::{watch_folder, FolderWatcher, WatcherConfig};
use serde::Serialize;
use tauri::{AppHandle, Emitter, State};

/// Event name the frontend listens on for newly-settled downloads.
const TRACK_DETECTED: &str = "track-detected";

#[derive(Default)]
struct WatchState {
    watcher: Mutex<Option<FolderWatcher>>,
    folder: Mutex<Option<PathBuf>>,
}

#[derive(Serialize, Clone)]
struct DetectedFile {
    path: String,
    track: Option<LocalTrack>,
    error: Option<String>,
}

#[derive(Serialize)]
struct FailedFile {
    path: String,
    error: String,
}

#[derive(Serialize)]
struct ScanResult {
    tracks: Vec<LocalTrack>,
    failures: Vec<FailedFile>,
}

fn ensure_folder(path: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(path);
    if !path.is_dir() {
        return Err(format!("{} is not a folder", path.display()));
    }
    Ok(path)
}

/// Read every audio file already sitting in the folder.
#[tauri::command]
fn scan(path: String, recursive: bool) -> Result<ScanResult, String> {
    let folder = ensure_folder(&path)?;
    let (tracks, failures) = scan_folder(&folder, recursive);

    Ok(ScanResult {
        tracks,
        failures: failures
            .into_iter()
            .map(|(path, error)| FailedFile {
                path: path.display().to_string(),
                error,
            })
            .collect(),
    })
}

/// Start watching a folder. Emits `track-detected` once per file that has
/// finished downloading — never while it is still being written.
#[tauri::command]
fn start_watching(app: AppHandle, state: State<WatchState>, path: String) -> Result<(), String> {
    let folder = ensure_folder(&path)?;

    // Replace any existing watch; dropping the old handle stops its thread.
    let mut guard = state.watcher.lock().map_err(|e| e.to_string())?;
    *guard = None;

    let emitter = app.clone();
    let watcher = watch_folder(&folder, WatcherConfig::default(), move |path: PathBuf| {
        let payload = build_payload(&path);
        if let Err(err) = emitter.emit(TRACK_DETECTED, payload) {
            eprintln!("failed to emit {TRACK_DETECTED}: {err}");
        }
    })
    .map_err(|e| format!("{e:#}"))?;

    *guard = Some(watcher);
    *state.folder.lock().map_err(|e| e.to_string())? = Some(folder);
    Ok(())
}

fn build_payload(path: &Path) -> DetectedFile {
    match LocalTrack::read(path) {
        Ok(track) => DetectedFile {
            path: path.display().to_string(),
            track: Some(track),
            error: None,
        },
        Err(err) => DetectedFile {
            path: path.display().to_string(),
            track: None,
            error: Some(format!("{err:#}")),
        },
    }
}

#[tauri::command]
fn stop_watching(state: State<WatchState>) -> Result<(), String> {
    *state.watcher.lock().map_err(|e| e.to_string())? = None;
    *state.folder.lock().map_err(|e| e.to_string())? = None;
    Ok(())
}

#[tauri::command]
fn watched_folder(state: State<WatchState>) -> Result<Option<String>, String> {
    Ok(state
        .folder
        .lock()
        .map_err(|e| e.to_string())?
        .as_ref()
        .map(|p| p.display().to_string()))
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(WatchState::default())
        .invoke_handler(tauri::generate_handler![
            scan,
            start_watching,
            stop_watching,
            watched_folder
        ])
        .run(tauri::generate_context!())
        .expect("error while running DJ Library Sync");
}

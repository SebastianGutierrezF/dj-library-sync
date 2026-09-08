//! Tauri bridge — Phase 0.
//!
//! All the real logic lives in `djls-core` so it stays testable and reusable by
//! the headless CLI. This layer only exposes it to the window.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use djls_core::auth::{self, AuthConfig, KeyringStore, TokenStore};
use djls_core::config::Config;
use djls_core::db::{Database, PLATFORM_SPOTIFY};
use djls_core::matcher::{evaluate, Candidate, MatchOutcome, ShorterVersionPolicy, Thresholds};
use djls_core::spotify::SpotifyClient;
use djls_core::tags::{scan_folder, LocalTrack};
use djls_core::watcher::{watch_folder, FolderWatcher, WatcherConfig};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};

/// Candidates per search query; Spotify caps development-mode apps at 10.
const SEARCH_LIMIT: u32 = 10;

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


// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

#[tauri::command]
fn load_config() -> Config {
    Config::load()
}

#[tauri::command]
fn save_config(config: Config) -> Result<(), String> {
    config.save().map_err(|e| format!("{e:#}"))
}

// ---------------------------------------------------------------------------
// Spotify account
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct AccountStatus {
    configured: bool,
    signed_in: bool,
    display_name: Option<String>,
    user_id: Option<String>,
    error: Option<String>,
}

fn auth_config() -> Result<AuthConfig, String> {
    Config::load()
        .client_id()
        .map(AuthConfig::new)
        .ok_or_else(|| "No Spotify client ID set yet".to_string())
}

fn user_client() -> Result<SpotifyClient, String> {
    SpotifyClient::for_user(auth_config()?, Box::new(KeyringStore::default()))
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
async fn account_status() -> AccountStatus {
    let Ok(config) = auth_config() else {
        return AccountStatus {
            configured: false,
            signed_in: false,
            display_name: None,
            user_id: None,
            error: None,
        };
    };
    let _ = config;

    match user_client() {
        Ok(client) => match client.current_user().await {
            Ok(me) => AccountStatus {
                configured: true,
                signed_in: true,
                display_name: me.display_name,
                user_id: Some(me.id),
                error: None,
            },
            Err(err) => AccountStatus {
                configured: true,
                signed_in: false,
                display_name: None,
                user_id: None,
                // "not signed in" is an expected state, not an error to show.
                error: {
                    let text = format!("{err:#}");
                    if text.contains("not signed in") { None } else { Some(text) }
                },
            },
        },
        Err(err) => AccountStatus {
            configured: true,
            signed_in: false,
            display_name: None,
            user_id: None,
            error: Some(err),
        },
    }
}

#[tauri::command]
async fn spotify_login(app: AppHandle) -> Result<(), String> {
    let config = auth_config()?;
    let store = KeyringStore::default();

    auth::login(&config, &store, |url| {
        // The consent screen belongs in the user's real browser, not a webview:
        // they may already be signed in there, and it keeps credentials out of
        // any surface this app controls.
        if !auth::open_in_browser(url) {
            let _ = app.emit("auth-url", url.to_string());
        }
    })
    .await
    .map(|_| ())
    .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
fn spotify_logout() -> Result<(), String> {
    KeyringStore::default().clear().map_err(|e| format!("{e:#}"))
}

/// What the connect screen needs to render each platform.
#[derive(Serialize)]
struct PlatformOption {
    id: String,
    display_name: String,
    /// "user_provided" — the user registers their own developer app.
    /// "hosted" — one-click sign-in against our developer account.
    credentials: String,
    metered: bool,
    available: bool,
    connected: bool,
}

#[tauri::command]
async fn available_platforms() -> Vec<PlatformOption> {
    use djls_core::platform::{CredentialModel, PlatformInfo};

    let spotify_connected = account_status().await.signed_in;

    PlatformInfo::ALL
        .iter()
        .map(|info| PlatformOption {
            id: info.id.to_string(),
            display_name: info.display_name.to_string(),
            credentials: match info.credentials {
                CredentialModel::UserProvided => "user_provided",
                CredentialModel::Hosted => "hosted",
            }
            .to_string(),
            metered: info.metered,
            available: info.available,
            connected: info.id == "spotify" && spotify_connected,
        })
        .collect()
}

#[tauri::command]
fn redirect_uri() -> Result<String, String> {
    Ok(auth_config()?.redirect_uri())
}

// ---------------------------------------------------------------------------
// Matching
// ---------------------------------------------------------------------------

#[derive(Serialize, Clone)]
struct MatchRow {
    track_id: i64,
    track: LocalTrack,
    verdict: String,
    method: String,
    confidence: f32,
    reason: String,
    candidates: Vec<Candidate>,
    /// True when this came from the local database rather than a fresh query.
    cached: bool,
}

#[derive(Serialize, Clone)]
struct MatchProgress {
    done: usize,
    total: usize,
}

#[tauri::command]
async fn match_folder(
    app: AppHandle,
    path: String,
    accept_shorter: bool,
    rescan: bool,
) -> Result<Vec<MatchRow>, String> {
    let folder = ensure_folder(&path)?;
    let client = user_client()?;
    let db = Database::open(&Database::default_path()).map_err(|e| format!("{e:#}"))?;

    let thresholds = Thresholds {
        shorter_version: if accept_shorter {
            ShorterVersionPolicy::Accept
        } else {
            ShorterVersionPolicy::Review
        },
        ..Thresholds::default()
    };

    let (tracks, _failures) = scan_folder(&folder, true);
    let total = tracks.len();
    let mut rows = Vec::with_capacity(total);

    for (index, track) in tracks.into_iter().enumerate() {
        let _ = app.emit("match-progress", MatchProgress { done: index, total });

        let record = db.upsert_track(&track).map_err(|e| format!("{e:#}"))?;

        if !rescan && record.can_reuse_match() {
            if let Ok(Some(stored)) = db.stored_match(record.id, PLATFORM_SPOTIFY) {
                rows.push(MatchRow {
                    track_id: record.id,
                    track,
                    verdict: stored.verdict.as_str().to_string(),
                    method: stored.method,
                    confidence: stored.confidence,
                    reason: stored.reason,
                    // Candidates are not persisted, so a cached row cannot
                    // offer alternatives; re-scan to get them back.
                    candidates: Vec::new(),
                    cached: true,
                });
                continue;
            }
        }

        let isrc_hits = match &track.isrc {
            Some(isrc) => client.search_isrc(isrc).await.unwrap_or_default(),
            None => Vec::new(),
        };
        let text_hits = if isrc_hits.is_empty() {
            client
                .search_for_track(&track, SEARCH_LIMIT)
                .await
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        let outcome: MatchOutcome = evaluate(&track, &isrc_hits, &text_hits, thresholds);
        let _ = db.record_match(record.id, PLATFORM_SPOTIFY, &outcome);

        rows.push(MatchRow {
            track_id: record.id,
            track,
            verdict: outcome.verdict.as_str().to_string(),
            method: outcome.method.as_str().to_string(),
            confidence: outcome.confidence(),
            reason: outcome.reason.clone(),
            candidates: outcome.candidates,
            cached: false,
        });
    }

    let _ = app.emit("match-progress", MatchProgress { done: total, total });
    Ok(rows)
}

// ---------------------------------------------------------------------------
// Playlists
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct PlaylistInfo {
    id: String,
    name: String,
    track_count: Option<u32>,
    owned: bool,
}

#[tauri::command]
async fn list_playlists() -> Result<Vec<PlaylistInfo>, String> {
    let client = user_client()?;
    let me = client.current_user().await.map_err(|e| format!("{e:#}"))?;
    let playlists = client.list_playlists().await.map_err(|e| format!("{e:#}"))?;

    Ok(playlists
        .into_iter()
        .filter(|p| p.is_owned_by(&me.id))
        .map(|p| PlaylistInfo {
            track_count: p.track_count(),
            owned: true,
            id: p.id,
            name: p.name,
        })
        .collect())
}

/// Carries the matched track's metadata, not just its URI: Spotify lists one
/// recording under several URIs, so the already-in-playlist check has to
/// compare recordings.
#[derive(Deserialize)]
struct PushItem {
    track_id: i64,
    uri: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    artists: String,
    #[serde(default)]
    duration_ms: u64,
}

#[derive(Serialize)]
struct PushResult {
    playlist_name: String,
    added: usize,
    skipped: usize,
}

#[tauri::command]
async fn push_tracks(playlist_name: String, items: Vec<PushItem>) -> Result<PushResult, String> {
    let client = user_client()?;
    let db = Database::open(&Database::default_path()).map_err(|e| format!("{e:#}"))?;
    let me = client.current_user().await.map_err(|e| format!("{e:#}"))?;

    let existing = client
        .list_playlists()
        .await
        .map_err(|e| format!("{e:#}"))?
        .into_iter()
        .find(|p| p.name.eq_ignore_ascii_case(&playlist_name) && p.is_owned_by(&me.id));

    let already = match &existing {
        Some(p) => client.playlist_entries(&p.id).await.unwrap_or_default(),
        None => Vec::new(),
    };

    let playlist = match existing {
        Some(p) => p,
        None => client
            .create_playlist(&playlist_name, false)
            .await
            .map_err(|e| format!("{e:#}"))?,
    };

    // Three guards, same as the CLI: what the playlist holds, what we logged
    // pushing before, and duplicates inside this batch.
    let mut to_add: Vec<(i64, String)> = Vec::new();
    let mut skipped = 0usize;

    for item in items {
        let logged = db
            .already_synced(item.track_id, &playlist.id, PLATFORM_SPOTIFY)
            .unwrap_or(false);
        let dupe = to_add.iter().any(|(_, uri)| uri == &item.uri);

        let in_playlist = already.iter().any(|e| {
            e.uri == item.uri
                || djls_core::matcher::same_recording_meta(
                    &e.artist_field(),
                    &e.name,
                    e.duration_ms,
                    &item.artists,
                    &item.name,
                    item.duration_ms,
                )
        });

        if in_playlist || logged || dupe {
            skipped += 1;
            continue;
        }
        to_add.push((item.track_id, item.uri));
    }

    let uris: Vec<String> = to_add.iter().map(|(_, uri)| uri.clone()).collect();
    let added = client
        .add_tracks_to_playlist(&playlist.id, &uris)
        .await
        .map_err(|e| format!("{e:#}"))?;

    // Logged only after the write lands, so a failure is retried not swallowed.
    for (track_id, uri) in &to_add {
        let _ = db.record_sync(*track_id, PLATFORM_SPOTIFY, uri, &playlist.id);
    }

    Ok(PushResult {
        playlist_name: playlist.name,
        added,
        skipped,
    })
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(WatchState::default())
        .invoke_handler(tauri::generate_handler![
            scan,
            start_watching,
            stop_watching,
            watched_folder,
            load_config,
            save_config,
            account_status,
            available_platforms,
            spotify_login,
            spotify_logout,
            redirect_uri,
            match_folder,
            list_playlists,
            push_tracks
        ])
        .run(tauri::generate_context!())
        .expect("error while running DJ Library Sync");
}

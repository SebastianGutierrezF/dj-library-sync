//! Tauri bridge — Phase 0.
//!
//! All the real logic lives in `djls-core` so it stays testable and reusable by
//! the headless CLI. This layer only exposes it to the window.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use djls_core::auth::{self, AuthConfig, KeyringStore, TokenStore};
use djls_core::config::Config;
use djls_core::apple::{AppleClient, DeveloperTokenSource};
use djls_core::db::{Database, PLATFORM_APPLE_MUSIC, PLATFORM_SPOTIFY};
use djls_core::licence;
use djls_core::matcher::{evaluate, Candidate, MatchOutcome, ShorterVersionPolicy, Thresholds};
use djls_core::platform::MusicPlatform;
use djls_core::spotify::SpotifyClient;
use djls_core::tags::{scan_folder, LocalTrack};
use djls_core::watcher::{watch_folder, FolderWatcher, WatcherConfig};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};

/// Give up once this many searches have been attempted and most have failed.
const MIN_ATTEMPTS_BEFORE_ABORT: usize = 5;
const ABORT_ERROR_RATE: f64 = 0.5;

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

/// Build a client for whichever platform the user is syncing to.
///
/// Everything above this — scanning, scoring, the database, the push guards —
/// is platform-agnostic, so the sync path takes a trait object and never asks
/// which service it is talking to.
async fn platform_client(platform: &str) -> Result<Box<dyn MusicPlatform>, String> {
    match platform {
        PLATFORM_SPOTIFY => Ok(Box::new(user_client()?)),
        PLATFORM_APPLE_MUSIC => {
            let stored = current_licence()
                .await?
                .ok_or_else(|| "Start a trial or enter a licence key first.".to_string())?;

            let user_token = licence::AppleTokenKeyring::default()
                .load()
                .map_err(|e| format!("{e:#}"))?
                .ok_or_else(|| {
                    "Apple Music is not connected on this machine. Connect it from                      the Services screen."
                        .to_string()
                })?;

            let client = AppleClient::new(
                DeveloperTokenSource::Hosted {
                    service_url: licence::service_url(),
                    activation_token: stored.token,
                },
                Some(user_token),
            )
            .map_err(|e| format!("{e:#}"))?;

            Ok(Box::new(client))
        }
        other => Err(format!("Unknown platform: {other}")),
    }
}

/// Tell the service how many tracks were pushed.
///
/// Best-effort and deliberately non-fatal: the tracks are already in the
/// playlist, and a billing failure must never be reported to the user as a
/// failed push. Spotify is not metered at all, so it never gets here.
async fn report_usage(platform: &str, tracks: usize) {
    if platform != PLATFORM_APPLE_MUSIC || tracks == 0 {
        return;
    }

    let Ok(Some(stored)) = current_licence().await else {
        return;
    };
    let Ok(client) = licence::ServiceClient::new(licence::service_url()) else {
        return;
    };

    if let Err(err) = client.report_usage(&stored.token, tracks as u32).await {
        eprintln!("[licence] usage not reported ({tracks} tracks): {err:#}");
    }
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
                    if text.contains("not signed in") {
                        None
                    } else {
                        Some(text)
                    }
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
    KeyringStore::default()
        .clear()
        .map_err(|e| format!("{e:#}"))
}

// ---------------------------------------------------------------------------
// Licence and Apple Music
// ---------------------------------------------------------------------------

/// Everything the licence panel renders.
#[derive(Serialize, Default)]
struct LicenceStatus {
    /// True once this machine holds an activation token of any kind.
    active: bool,
    /// "trial" | "pack" | "unlimited"
    plan: Option<String>,
    credits: Option<i64>,
    unlimited: bool,
    expires_at: Option<String>,
    /// Present only for a purchase — a trial has no key to show.
    has_key: bool,
    apple_connected: bool,
    /// Shown as a warning, not a failure: Spotify keeps working regardless.
    error: Option<String>,
}

fn device_id() -> Result<String, String> {
    let mut config = Config::load();
    config.device_id_or_create().map_err(|e| format!("{e:#}"))
}

fn service() -> Result<licence::ServiceClient, String> {
    licence::ServiceClient::new(licence::service_url()).map_err(|e| format!("{e:#}"))
}

/// Read the stored licence, refreshing its token if it is near expiry.
///
/// Refresh failures are swallowed on purpose: a stale-but-valid token still
/// works, and an outage must not lock someone out of a licence they paid for.
async fn current_licence() -> Result<Option<licence::StoredLicence>, String> {
    let store = licence::LicenceKeyring::default();
    let Some(stored) = store.load().map_err(|e| format!("{e:#}"))? else {
        return Ok(None);
    };

    if stored.needs_refresh() {
        if let (Ok(client), Ok(device)) = (service(), device_id()) {
            match client.refresh_if_needed(&stored, &device).await {
                Ok(Some(fresh)) => {
                    let _ = store.save(&fresh);
                    return Ok(Some(fresh));
                }
                Ok(None) => {}
                Err(err) => {
                    // Worth knowing about, not worth failing over.
                    eprintln!("[licence] refresh failed, using the stored token: {err:#}");
                }
            }
        }
    }

    Ok(Some(stored))
}

#[tauri::command]
async fn licence_status() -> LicenceStatus {
    let apple_connected = licence::AppleTokenKeyring::default()
        .load()
        .ok()
        .flatten()
        .is_some();

    let stored = match current_licence().await {
        Ok(Some(stored)) => stored,
        Ok(None) => {
            return LicenceStatus {
                apple_connected,
                ..Default::default()
            }
        }
        Err(err) => {
            return LicenceStatus {
                apple_connected,
                error: Some(err),
                ..Default::default()
            }
        }
    };

    let mut status = LicenceStatus {
        active: !stored.is_expired(),
        plan: Some(stored.plan.clone()),
        expires_at: Some(stored.expires_at.clone()),
        has_key: stored.key.is_some(),
        apple_connected,
        ..Default::default()
    };

    // The balance is worth showing but not worth blocking on.
    match service() {
        Ok(client) => match client.entitlement(&stored.token).await {
            Ok(ent) => {
                status.credits = Some(ent.credits);
                status.unlimited = ent.unlimited;
            }
            Err(err) => status.error = Some(format!("{err:#}")),
        },
        Err(err) => status.error = Some(err),
    }

    status
}

#[tauri::command]
async fn start_trial() -> Result<LicenceStatus, String> {
    let client = service()?;
    let device = device_id()?;

    let licence = client
        .start_trial(&device)
        .await
        .map_err(|e| format!("{e:#}"))?;

    licence::LicenceKeyring::default()
        .save(&licence)
        .map_err(|e| format!("{e:#}"))?;

    Ok(licence_status().await)
}

#[tauri::command]
async fn activate_licence(key: String) -> Result<LicenceStatus, String> {
    let key = key.trim().to_string();
    if key.is_empty() {
        return Err("Paste your licence key first.".to_string());
    }

    let client = service()?;
    let device = device_id()?;

    let licence = client
        .activate(&key, &device)
        .await
        .map_err(|e| format!("{e:#}"))?;

    licence::LicenceKeyring::default()
        .save(&licence)
        .map_err(|e| format!("{e:#}"))?;

    Ok(licence_status().await)
}

#[tauri::command]
fn clear_licence() -> Result<(), String> {
    // Deliberately leaves the Apple connection alone: they are separate
    // credentials, revoked independently.
    licence::LicenceKeyring::default()
        .clear()
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
async fn apple_login(app: AppHandle) -> Result<LicenceStatus, String> {
    let stored = current_licence()
        .await?
        .ok_or_else(|| "Start a trial or enter a licence key first.".to_string())?;

    let token = licence::connect_apple_music(
        &licence::service_url(),
        &stored.token,
        licence::APPLE_CALLBACK_PORT,
        |url| {
            // Apple's consent screen belongs in the user's real browser, where
            // they may already be signed in — the same reasoning as Spotify.
            if !auth::open_in_browser(url) {
                let _ = app.emit("auth-url", url.to_string());
            }
        },
    )
    .await
    .map_err(|e| format!("{e:#}"))?;

    licence::AppleTokenKeyring::default()
        .save(&token)
        .map_err(|e| format!("{e:#}"))?;

    Ok(licence_status().await)
}

#[tauri::command]
fn apple_logout() -> Result<(), String> {
    licence::AppleTokenKeyring::default()
        .clear()
        .map_err(|e| format!("{e:#}"))
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
    let apple = licence_status().await;
    let apple_ready = apple.active && apple.apple_connected;

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
            connected: match info.id {
                "spotify" => spotify_connected,
                // Apple needs both a licence and a Music User Token; either
                // alone cannot reach the API.
                "apple_music" => apple_ready,
                _ => false,
            },
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
    /// Set when the search itself failed, as opposed to finding nothing. The
    /// difference matters: one is worth retrying, the other is not.
    error: Option<String>,
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
    platform: String,
    accept_shorter: bool,
    rescan: bool,
) -> Result<Vec<MatchRow>, String> {
    let folder = ensure_folder(&path)?;
    let client = platform_client(&platform).await?;
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
    let mut failed = 0usize;

    for (index, track) in tracks.into_iter().enumerate() {
        let _ = app.emit("match-progress", MatchProgress { done: index, total });

        let record = db.upsert_track(&track).map_err(|e| format!("{e:#}"))?;

        if !rescan && record.can_reuse_match() {
            if let Ok(Some(stored)) = db.stored_match(record.id, &platform) {
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
                    error: None,
                    cached: true,
                });
                continue;
            }
        }

        // A failed search is not the same thing as a track that is not on the
        // platform. `unwrap_or_default()` conflated them, which turned an
        // outage into a confident 0% match rate — and then cached it.
        let mut search_error = None;

        let isrc_hits = match &track.isrc {
            Some(isrc) => match client.search_isrc(isrc).await {
                Ok(hits) => hits,
                Err(err) => {
                    search_error = Some(format!("{err:#}"));
                    Vec::new()
                }
            },
            None => Vec::new(),
        };

        let text_hits = if isrc_hits.is_empty() && search_error.is_none() {
            match client.search_for_track(&track, SEARCH_LIMIT).await {
                Ok(hits) => hits,
                Err(err) => {
                    search_error = Some(format!("{err:#}"));
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };

        if let Some(message) = &search_error {
            failed += 1;

            // Stop early rather than grinding through hundreds of tracks to
            // produce a match rate that means nothing.
            let attempts = index + 1;
            if attempts >= MIN_ATTEMPTS_BEFORE_ABORT
                && (failed as f64 / attempts as f64) >= ABORT_ERROR_RATE
            {
                return Err(format!(
                    "Stopped: {failed} of the first {attempts} searches failed, so any                      match rate would be meaningless. The last error was:\n{message}"
                ));
            }
        }

        let outcome: MatchOutcome = evaluate(&track, &isrc_hits, &text_hits, thresholds);

        // Only cache a real answer. Recording a failed search as "no match"
        // makes the failure permanent: the next run serves it from the cache
        // and never retries.
        if search_error.is_none() {
            if let Err(err) = db.record_match(record.id, &platform, &outcome) {
                // Not fatal — the match is still returned to the UI — but a
                // silent failure here means the next run has no memory of
                // this one, and nothing would ever say why.
                eprintln!("[db] could not record the {platform} match: {err:#}");
            }
        }

        rows.push(MatchRow {
            track_id: record.id,
            track,
            verdict: outcome.verdict.as_str().to_string(),
            method: outcome.method.as_str().to_string(),
            confidence: outcome.confidence(),
            reason: outcome.reason.clone(),
            candidates: outcome.candidates,
            error: search_error,
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
async fn list_playlists(platform: String) -> Result<Vec<PlaylistInfo>, String> {
    let client = platform_client(&platform).await?;
    let me = client.current_user().await.map_err(|e| format!("{e:#}"))?;
    let playlists = client
        .list_playlists()
        .await
        .map_err(|e| format!("{e:#}"))?;

    Ok(playlists
        .into_iter()
        // Offer what can actually be written to. Apple reports no owner, and
        // filtering on ownership would leave the list empty.
        .filter(|p| p.is_writable_by(&me.id))
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

/// Whether an identifier plausibly belongs to `platform`.
///
/// Spotify URIs are `spotify:track:<id>`; Apple's are bare numeric catalogue
/// ids. Sending one to the other is not a near miss — Apple answers a Spotify
/// URI with `500 Unable to update tracks`, which reads like an outage on their
/// side rather than a mistake on ours.
fn uri_belongs_to(platform: &str, uri: &str) -> bool {
    match platform {
        PLATFORM_SPOTIFY => uri.starts_with("spotify:track:") && uri.len() > "spotify:track:".len(),
        // `all` is vacuously true on an empty string, so the emptiness check
        // has to come first or a blank id sails through.
        PLATFORM_APPLE_MUSIC => !uri.is_empty() && uri.chars().all(|c| c.is_ascii_digit()),
        _ => true,
    }
}

#[tauri::command]
async fn push_tracks(
    playlist_name: String,
    platform: String,
    items: Vec<PushItem>,
) -> Result<PushResult, String> {
    // Catch a cross-platform push before it leaves the machine. Matches are
    // stored per platform and the UI clears them when the target changes, so
    // this should be unreachable — which is exactly why it is worth asserting
    // rather than trusting.
    if let Some(wrong) = items.iter().find(|i| !uri_belongs_to(&platform, &i.uri)) {
        return Err(format!(
            "These matches are not for {platform}: \"{}\" looks like it came from another              service. Re-match this folder with {platform} selected before pushing.",
            wrong.uri
        ));
    }

    let client = platform_client(&platform).await?;
    let db = Database::open(&Database::default_path()).map_err(|e| format!("{e:#}"))?;
    let me = client.current_user().await.map_err(|e| format!("{e:#}"))?;

    let existing = client
        .list_playlists()
        .await
        .map_err(|e| format!("{e:#}"))?
        .into_iter()
        // Writable, not owned: Apple's library playlists carry no owner, and
        // reading that as "not mine" would create a duplicate every run.
        .find(|p| p.name.eq_ignore_ascii_case(&playlist_name) && p.is_writable_by(&me.id));

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
            .already_synced(item.track_id, &playlist.id, &platform)
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
        .add_tracks(&playlist.id, &uris)
        .await
        .map_err(|e| format!("{e:#}"))?;

    // Logged only after the write lands, so a failure is retried not swallowed.
    for (track_id, uri) in &to_add {
        if let Err(err) = db.record_sync(*track_id, &platform, uri, &playlist.id) {
            // The track is in the playlist either way; losing the log only
            // risks offering it again next time, which the playlist contents
            // check would then catch.
            eprintln!("[db] could not log the {platform} push of {uri}: {err:#}");
        }
    }

    // After the log, for the same reason: billing must never be the thing that
    // makes a successful push look like a failure.
    report_usage(&platform, added).await;

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
            push_tracks,
            licence_status,
            start_trial,
            activate_licence,
            clear_licence,
            apple_login,
            apple_logout
        ])
        .run(tauri::generate_context!())
        .expect("error while running DJ Library Sync");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_spotify_uri_is_never_pushed_to_apple() {
        // Apple answers a Spotify URI with "500 Unable to update tracks",
        // which reads like an outage rather than a mistake on our side.
        assert!(!uri_belongs_to(
            PLATFORM_APPLE_MUSIC,
            "spotify:track:6I9VzXrHxO9rA9A5euc8Ak"
        ));
        assert!(uri_belongs_to(PLATFORM_APPLE_MUSIC, "1440857781"));
    }

    #[test]
    fn an_apple_id_is_never_pushed_to_spotify() {
        assert!(!uri_belongs_to(PLATFORM_SPOTIFY, "1440857781"));
        assert!(uri_belongs_to(
            PLATFORM_SPOTIFY,
            "spotify:track:6I9VzXrHxO9rA9A5euc8Ak"
        ));
    }

    #[test]
    fn an_unknown_platform_is_not_second_guessed() {
        // A new adapter should not be blocked by a guard that has never heard
        // of its identifier format.
        assert!(uri_belongs_to("tidal", "anything-at-all"));
    }

    #[test]
    fn apple_ids_are_digits_only() {
        // Library ids (i.xxx) and playlist ids (p.xxx) cannot be added as
        // catalogue songs, so they must not pass either.
        assert!(!uri_belongs_to(PLATFORM_APPLE_MUSIC, "i.abc123"));
        assert!(!uri_belongs_to(PLATFORM_APPLE_MUSIC, "p.xyz789"));
        assert!(!uri_belongs_to(PLATFORM_APPLE_MUSIC, ""));
    }
}

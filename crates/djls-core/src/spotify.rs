//! Minimal Spotify Web API client.
//!
//! The CLI uses the Client Credentials flow, which is enough for search and
//! needs no user account. Playlist writes in the desktop app will need
//! Authorization Code + PKCE with a `127.0.0.1` loopback redirect (Spotify does
//! not accept `localhost`), and must persist the rotated refresh token on every
//! refresh.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::auth::{self, AuthConfig, TokenStore};
use crate::tags::LocalTrack;

const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";
const SEARCH_URL: &str = "https://api.spotify.com/v1/search";
const API_BASE: &str = "https://api.spotify.com/v1";

/// Playlist additions are one of the few endpoints that *does* batch.
const MAX_URIS_PER_ADD: usize = 100;

/// Search is one track per request — there is no batch endpoint — so a large
/// download folder means one call per track. Pace them.
const MIN_REQUEST_GAP: Duration = Duration::from_millis(120);
const MAX_RETRIES: u32 = 4;

/// The February 2026 dev-mode changes cut search `limit` from a maximum of 50
/// to 10 (default 5). Anything above 10 fails the whole query with
/// `400 Invalid limit`, so clamp instead of trusting the older docs. Widen the
/// candidate pool with extra *queries*; `offset` paging is still available.
const MAX_SEARCH_LIMIT: u32 = 10;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SpotifyTrack {
    pub id: String,
    pub uri: String,
    pub name: String,
    pub artists: Vec<String>,
    pub album: String,
    pub duration_ms: u64,
    pub isrc: Option<String>,
    pub url: Option<String>,
    pub popularity: Option<u32>,
}

impl SpotifyTrack {
    /// All artists joined, for token-set comparison against the local tag.
    pub fn artist_field(&self) -> String {
        self.artists.join(", ")
    }

    pub fn duration_display(&self) -> String {
        let total = self.duration_ms / 1000;
        format!("{}:{:02}", total / 60, total % 60)
    }
}

#[derive(Debug, Default)]
pub struct RequestStats {
    pub requests: AtomicU64,
    pub rate_limited: AtomicU64,
    pub errors: AtomicU64,
}

/// Where the bearer token comes from.
///
/// Search works with either. Anything touching the user's account — their
/// playlists, their identity — requires `User`.
enum TokenSource {
    ClientCredentials {
        client_id: String,
        client_secret: String,
    },
    User {
        config: AuthConfig,
        store: Box<dyn TokenStore>,
    },
}

pub struct SpotifyClient {
    http: reqwest::Client,
    source: TokenSource,
    market: Option<String>,
    token: Mutex<Option<CachedToken>>,
    last_request: Mutex<Option<Instant>>,
    pub stats: RequestStats,
}

struct CachedToken {
    value: String,
    expires_at: Instant,
}

impl SpotifyClient {
    fn build(source: TokenSource) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .user_agent("dj-library-sync/0.1")
            .build()
            .context("building HTTP client")?;

        Ok(Self {
            http,
            source,
            market: None,
            token: Mutex::new(None),
            last_request: Mutex::new(None),
            stats: RequestStats::default(),
        })
    }

    /// Search-only client. No user account involved.
    pub fn new(client_id: impl Into<String>, client_secret: impl Into<String>) -> Result<Self> {
        Self::build(TokenSource::ClientCredentials {
            client_id: client_id.into(),
            client_secret: client_secret.into(),
        })
    }

    /// Client acting as the signed-in user, for playlist access.
    pub fn for_user(config: AuthConfig, store: Box<dyn TokenStore>) -> Result<Self> {
        Self::build(TokenSource::User { config, store })
    }

    /// Restrict results to a market, so the match rate reflects what is
    /// actually available to the user rather than every catalogue worldwide.
    pub fn with_market(mut self, market: Option<String>) -> Self {
        self.market = market;
        self
    }

    async fn access_token(&self) -> Result<String> {
        {
            let cached = self.token.lock().await;
            if let Some(t) = cached.as_ref() {
                if t.expires_at > Instant::now() + Duration::from_secs(30) {
                    return Ok(t.value.clone());
                }
            }
        }

        let (value, ttl) = match &self.source {
            TokenSource::ClientCredentials {
                client_id,
                client_secret,
            } => {
                self.client_credentials_token(client_id, client_secret)
                    .await?
            }

            TokenSource::User { config, store } => {
                // Reading the OS credential store can block indefinitely on a
                // GUI authorization prompt. Say so rather than appearing hung.
                let nudge = tokio::spawn(async {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    eprintln!(
                        "Waiting on the OS credential store — if a keychain \
                         permission dialog is showing, approve it to continue."
                    );
                });
                let loaded = store.load();
                nudge.abort();

                let stored = loaded?
                    .ok_or_else(|| anyhow!("not signed in to Spotify — run `djls login` first"))?;

                // Refreshing persists any rotated refresh token, which is the
                // whole reason this goes through the store rather than a
                // cached copy.
                let tokens = if stored.is_expired() {
                    auth::refresh(config, store.as_ref(), &stored).await?
                } else {
                    stored
                };

                let ttl = tokens
                    .expires_at
                    .saturating_sub(
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or(0),
                    )
                    .max(1);

                (tokens.access_token, ttl)
            }
        };

        let mut cached = self.token.lock().await;
        *cached = Some(CachedToken {
            value: value.clone(),
            expires_at: Instant::now() + Duration::from_secs(ttl),
        });

        Ok(value)
    }

    async fn client_credentials_token(
        &self,
        client_id: &str,
        client_secret: &str,
    ) -> Result<(String, u64)> {
        #[derive(Deserialize)]
        struct TokenResponse {
            access_token: String,
            expires_in: u64,
        }

        let basic = B64.encode(format!("{client_id}:{client_secret}"));
        let resp = self
            .http
            .post(TOKEN_URL)
            .header("Authorization", format!("Basic {basic}"))
            .form(&[("grant_type", "client_credentials")])
            .send()
            .await
            .context("requesting Spotify access token")?;

        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("Spotify token request failed ({status}): {body}");
        }

        let parsed: TokenResponse =
            serde_json::from_str(&body).context("parsing Spotify token response")?;

        Ok((parsed.access_token, parsed.expires_in))
    }

    async fn pace(&self) {
        let mut last = self.last_request.lock().await;
        if let Some(prev) = *last {
            let elapsed = prev.elapsed();
            if elapsed < MIN_REQUEST_GAP {
                tokio::time::sleep(MIN_REQUEST_GAP - elapsed).await;
            }
        }
        *last = Some(Instant::now());
    }

    /// GET with 429 handling. Spotify returns `Retry-After` in seconds and
    /// expects the client to actually wait.
    async fn get_search(&self, query: &str, limit: u32) -> Result<Vec<SpotifyTrack>> {
        let mut attempt = 0u32;

        loop {
            self.pace().await;
            let token = self.access_token().await?;

            let mut params: Vec<(&str, String)> = vec![
                ("q", query.to_string()),
                ("type", "track".to_string()),
                ("limit", limit.clamp(1, MAX_SEARCH_LIMIT).to_string()),
            ];
            if let Some(market) = &self.market {
                params.push(("market", market.clone()));
            }

            self.stats.requests.fetch_add(1, Ordering::Relaxed);
            let resp = self
                .http
                .get(SEARCH_URL)
                .bearer_auth(&token)
                .query(&params)
                .send()
                .await;

            let resp = match resp {
                Ok(r) => r,
                Err(err) if attempt < MAX_RETRIES => {
                    attempt += 1;
                    self.stats.errors.fetch_add(1, Ordering::Relaxed);
                    tokio::time::sleep(Duration::from_millis(400 * u64::from(attempt))).await;
                    let _ = err;
                    continue;
                }
                Err(err) => return Err(err).context("Spotify search request failed"),
            };

            let status = resp.status();

            if status.as_u16() == 429 {
                self.stats.rate_limited.fetch_add(1, Ordering::Relaxed);
                let wait = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(2);
                if attempt >= MAX_RETRIES {
                    bail!("Spotify rate limit persisted after {MAX_RETRIES} retries");
                }
                attempt += 1;
                tokio::time::sleep(Duration::from_secs(wait + 1)).await;
                continue;
            }

            if status.as_u16() == 401 {
                // Token expired mid-flight; drop it and retry once.
                *self.token.lock().await = None;
                if attempt >= MAX_RETRIES {
                    bail!("Spotify authorization failed");
                }
                attempt += 1;
                continue;
            }

            if status.as_u16() >= 500 && attempt < MAX_RETRIES {
                attempt += 1;
                tokio::time::sleep(Duration::from_millis(500 * u64::from(attempt))).await;
                continue;
            }

            let body = resp.text().await.context("reading Spotify response body")?;
            if !status.is_success() {
                self.stats.errors.fetch_add(1, Ordering::Relaxed);
                return Err(anyhow!("Spotify search failed ({status}): {body}"));
            }

            let parsed: SearchResponse =
                serde_json::from_str(&body).context("parsing Spotify search response")?;

            return Ok(parsed
                .tracks
                .map(|page| {
                    page.items
                        .into_iter()
                        .filter_map(ApiTrack::into_track)
                        .collect()
                })
                .unwrap_or_default());
        }
    }

    /// Exact lookup by recording identifier. Highest-confidence path, and the
    /// reason extended mixes resolve correctly when the tag carries an ISRC:
    /// each recording has its own.
    pub async fn search_isrc(&self, isrc: &str) -> Result<Vec<SpotifyTrack>> {
        let cleaned: String = isrc.chars().filter(|c| c.is_alphanumeric()).collect();
        if cleaned.is_empty() {
            return Ok(Vec::new());
        }
        self.get_search(&format!("isrc:{}", cleaned.to_uppercase()), 10)
            .await
    }

    /// Text search. Tries the field-filtered form first because it is far more
    /// precise, then falls back to a loose query, since filters miss when the
    /// tag spells the artist differently from the catalogue.
    pub async fn search_text(
        &self,
        artist: &str,
        title: &str,
        limit: u32,
    ) -> Result<Vec<SpotifyTrack>> {
        let artist = artist.trim();
        let title = title.trim();
        if title.is_empty() {
            return Ok(Vec::new());
        }

        let mut results = Vec::new();

        if !artist.is_empty() {
            let filtered = format!("track:\"{}\" artist:\"{}\"", escape(title), escape(artist));
            results = self.get_search(&filtered, limit).await?;
        }

        if results.len() < 3 {
            let loose = if artist.is_empty() {
                title.to_string()
            } else {
                format!("{artist} {title}")
            };
            let more = self.get_search(&loose, limit).await?;
            for track in more {
                if !results.iter().any(|t| t.id == track.id) {
                    results.push(track);
                }
            }
        }

        Ok(results)
    }

    /// Every query worth trying for one local track.
    ///
    /// A plain "artist title" search often ranks the radio edit or a bare
    /// re-release above the extended mix, so a long-form local file never
    /// even sees its own match in the top results. When the local file is
    /// extended/club-length, run a second query naming the mix explicitly and
    /// merge in whatever it turns up that the first query missed.
    pub async fn search_for_track(
        &self,
        local: &LocalTrack,
        limit: u32,
    ) -> Result<Vec<SpotifyTrack>> {
        let artist = local.primary_artist();
        let title = &local.parsed.base;

        let mut results = self.search_text(&artist, title, limit).await?;

        if local.parsed.kind.is_long_form() {
            let hinted = format!("{artist} {title} extended mix");
            let more = self.get_search(&hinted, limit).await.unwrap_or_default();
            for track in more {
                if !results.iter().any(|t| t.id == track.id) {
                    results.push(track);
                }
            }
        }

        Ok(results)
    }
}

/// The signed-in user.
#[derive(Debug, Clone, Deserialize)]
pub struct SpotifyUser {
    pub id: String,
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Playlist {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub public: Option<bool>,
    #[serde(default)]
    pub owner: Option<PlaylistOwner>,
    /// Renamed from `tracks` in the February 2026 Web API changes. The alias
    /// keeps extended-quota apps, which still send `tracks`, working.
    #[serde(default, alias = "tracks")]
    pub items: Option<PlaylistItemsRef>,
}

/// One track already sitting in a playlist.
#[derive(Debug, Clone)]
pub struct PlaylistEntry {
    pub uri: String,
    pub name: String,
    pub artists: Vec<String>,
    pub duration_ms: u64,
}

impl PlaylistEntry {
    pub fn artist_field(&self) -> String {
        self.artists.join(", ")
    }

    /// Whether this entry is the same recording as a candidate, even if
    /// Spotify indexes them under different URIs.
    pub fn is_same_recording_as(&self, track: &SpotifyTrack) -> bool {
        self.uri == track.uri
            || crate::matcher::same_recording_meta(
                &self.artist_field(),
                &self.name,
                self.duration_ms,
                &track.artist_field(),
                &track.name,
                track.duration_ms,
            )
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct PlaylistOwner {
    pub id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PlaylistItemsRef {
    pub total: u32,
}

impl Playlist {
    /// `None` when Spotify omits the count, which it does for playlists the
    /// user neither owns nor collaborates on. Reporting that as 0 misleads.
    pub fn track_count(&self) -> Option<u32> {
        self.items.as_ref().map(|t| t.total)
    }

    pub fn is_owned_by(&self, user_id: &str) -> bool {
        self.owner
            .as_ref()
            .map(|o| o.id == user_id)
            .unwrap_or(false)
    }

    /// Whether this playlist can be a sync target.
    ///
    /// Not the same question as ownership. Spotify's playlist list includes
    /// playlists the user merely follows, which cannot be written to, so there
    /// the owner has to match. Apple's library playlists are by definition the
    /// user's own and carry no owner field at all — reading that absence as
    /// "not mine" would mean never finding the playlist again and creating a
    /// fresh duplicate on every run.
    pub fn is_writable_by(&self, user_id: &str) -> bool {
        match &self.owner {
            Some(owner) => owner.id == user_id,
            None => true,
        }
    }
}

#[derive(Deserialize)]
struct Page<T> {
    items: Vec<T>,
    next: Option<String>,
}

/// User-scoped endpoints. Every one of these needs a `TokenSource::User`
/// client; with client-credentials the token has no user attached and Spotify
/// answers 401/403.
impl SpotifyClient {
    /// Authenticated request with the same 429/401/5xx handling as search.
    async fn api_request(
        &self,
        method: reqwest::Method,
        url: &str,
        body: Option<serde_json::Value>,
    ) -> Result<String> {
        let mut attempt = 0u32;

        loop {
            self.pace().await;
            let token = self.access_token().await?;

            let mut req = self.http.request(method.clone(), url).bearer_auth(&token);
            if let Some(body) = &body {
                req = req.json(body);
            }

            self.stats.requests.fetch_add(1, Ordering::Relaxed);
            let resp = match req.send().await {
                Ok(r) => r,
                Err(err) if attempt < MAX_RETRIES => {
                    attempt += 1;
                    self.stats.errors.fetch_add(1, Ordering::Relaxed);
                    tokio::time::sleep(Duration::from_millis(400 * u64::from(attempt))).await;
                    let _ = err;
                    continue;
                }
                Err(err) => return Err(err).context("Spotify API request failed"),
            };

            let status = resp.status();

            if status.as_u16() == 429 {
                self.stats.rate_limited.fetch_add(1, Ordering::Relaxed);
                let wait = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(2);
                if attempt >= MAX_RETRIES {
                    bail!("Spotify rate limit persisted after {MAX_RETRIES} retries");
                }
                attempt += 1;
                tokio::time::sleep(Duration::from_secs(wait + 1)).await;
                continue;
            }

            if status.as_u16() >= 500 && attempt < MAX_RETRIES {
                attempt += 1;
                tokio::time::sleep(Duration::from_millis(500 * u64::from(attempt))).await;
                continue;
            }

            let text = resp.text().await.context("reading Spotify response body")?;
            if !status.is_success() {
                self.stats.errors.fetch_add(1, Ordering::Relaxed);
                if status.as_u16() == 403 {
                    // Spotify distinguishes these two, and they need opposite
                    // fixes — conflating them sends you to the wrong place.
                    let hint = if text.contains("Insufficient client scope") {
                        "The token is missing a required scope. Run `djls logout` \
                         then `djls login` to re-consent."
                    } else {
                        "Scopes look fine, so this is an app-level permission. \
                         Check the app in the Spotify developer dashboard: your \
                         account must be listed under Settings -> User Management \
                         while the app is in development mode."
                    };
                    return Err(anyhow!(
                        "Spotify refused the request ({status}): {text}\n{hint}"
                    ));
                }
                return Err(anyhow!("Spotify API error ({status}): {text}"));
            }

            return Ok(text);
        }
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T> {
        let body = self.api_request(reqwest::Method::GET, url, None).await?;
        serde_json::from_str(&body).with_context(|| format!("parsing response from {url}"))
    }

    /// Walk a paginated collection to the end.
    async fn get_all<T: serde::de::DeserializeOwned>(&self, first_url: String) -> Result<Vec<T>> {
        let mut url = Some(first_url);
        let mut all = Vec::new();

        while let Some(next) = url {
            let page: Page<T> = self.get_json(&next).await?;
            all.extend(page.items);
            url = page.next;
        }

        Ok(all)
    }

    pub async fn current_user(&self) -> Result<SpotifyUser> {
        self.get_json(&format!("{API_BASE}/me")).await
    }

    /// Every playlist the user can see, newest first as Spotify returns them.
    pub async fn list_playlists(&self) -> Result<Vec<Playlist>> {
        self.get_all(format!("{API_BASE}/me/playlists?limit=50"))
            .await
    }

    /// `POST /users/{id}/playlists` was removed in February 2026 — creation is
    /// `POST /me/playlists`, which always targets the signed-in user.
    pub async fn create_playlist(&self, name: &str, public: bool) -> Result<Playlist> {
        let body = serde_json::json!({
            "name": name,
            "public": public,
            "description": "Created by DJ Library Sync",
        });

        let text = self
            .api_request(
                reqwest::Method::POST,
                &format!("{API_BASE}/me/playlists"),
                Some(body),
            )
            .await?;

        serde_json::from_str(&text).context("parsing created playlist")
    }

    /// What a playlist already holds.
    ///
    /// Returns full metadata, not just URIs: Spotify lists the same recording
    /// under several URIs (single, album cut, re-release), so a URI comparison
    /// alone cannot tell whether a track is already in the playlist.
    pub async fn playlist_entries(&self, playlist_id: &str) -> Result<Vec<PlaylistEntry>> {
        #[derive(Deserialize)]
        struct Row {
            /// Renamed from `track` in February 2026; alias keeps the old
            /// shape working for extended-quota apps.
            #[serde(alias = "track")]
            item: Option<TrackRef>,
        }
        #[derive(Deserialize)]
        struct TrackRef {
            uri: Option<String>,
            #[serde(default)]
            name: String,
            #[serde(default)]
            artists: Vec<ApiArtist>,
            #[serde(default)]
            duration_ms: u64,
        }

        let rows: Vec<Row> = self
            // No `fields` filter here on purpose: trimming the response has
            // silently dropped the very metadata the duplicate check needs.
            .get_all(format!("{API_BASE}/playlists/{playlist_id}/items?limit=50"))
            .await?;

        Ok(rows
            .into_iter()
            .filter_map(|r| r.item)
            .filter_map(|t| {
                t.uri.map(|uri| PlaylistEntry {
                    uri,
                    name: t.name,
                    artists: t.artists.into_iter().map(|a| a.name).collect(),
                    duration_ms: t.duration_ms,
                })
            })
            .collect())
    }

    /// Just the URIs, when metadata is not needed.
    pub async fn playlist_track_uris(&self, playlist_id: &str) -> Result<HashSet<String>> {
        Ok(self
            .playlist_entries(playlist_id)
            .await?
            .into_iter()
            .map(|e| e.uri)
            .collect())
    }

    /// Add tracks, 100 per request. Returns how many were sent.
    pub async fn add_tracks_to_playlist(
        &self,
        playlist_id: &str,
        uris: &[String],
    ) -> Result<usize> {
        if uris.is_empty() {
            return Ok(0);
        }

        for chunk in uris.chunks(MAX_URIS_PER_ADD) {
            let body = serde_json::json!({ "uris": chunk });
            self.api_request(
                reqwest::Method::POST,
                &format!("{API_BASE}/playlists/{playlist_id}/items"),
                Some(body),
            )
            .await?;
        }

        Ok(uris.len())
    }
}

fn escape(input: &str) -> String {
    input.replace(['"', '\\'], " ")
}

#[derive(Deserialize)]
struct SearchResponse {
    tracks: Option<TrackPage>,
}

#[derive(Deserialize)]
struct TrackPage {
    items: Vec<ApiTrack>,
}

#[derive(Deserialize)]
struct ApiTrack {
    id: Option<String>,
    uri: Option<String>,
    name: String,
    artists: Vec<ApiArtist>,
    album: Option<ApiAlbum>,
    duration_ms: u64,
    external_ids: Option<ExternalIds>,
    external_urls: Option<ExternalUrls>,
    popularity: Option<u32>,
}

#[derive(Deserialize)]
struct ApiArtist {
    name: String,
}

#[derive(Deserialize)]
struct ApiAlbum {
    name: String,
}

#[derive(Deserialize)]
struct ExternalIds {
    isrc: Option<String>,
}

#[derive(Deserialize)]
struct ExternalUrls {
    spotify: Option<String>,
}

impl ApiTrack {
    fn into_track(self) -> Option<SpotifyTrack> {
        let id = self.id?;
        Some(SpotifyTrack {
            uri: self.uri.unwrap_or_else(|| format!("spotify:track:{id}")),
            id,
            name: self.name,
            artists: self.artists.into_iter().map(|a| a.name).collect(),
            album: self.album.map(|a| a.name).unwrap_or_default(),
            duration_ms: self.duration_ms,
            isrc: self
                .external_ids
                .and_then(|e| e.isrc)
                .map(|s| s.to_uppercase()),
            url: self.external_urls.and_then(|e| e.spotify),
            popularity: self.popularity,
        })
    }
}

#[async_trait::async_trait]
impl crate::platform::MusicPlatform for SpotifyClient {
    fn info(&self) -> crate::platform::PlatformInfo {
        crate::platform::PlatformInfo::SPOTIFY
    }

    async fn current_user(&self) -> Result<SpotifyUser> {
        SpotifyClient::current_user(self).await
    }

    async fn search_isrc(&self, isrc: &str) -> Result<Vec<SpotifyTrack>> {
        SpotifyClient::search_isrc(self, isrc).await
    }

    async fn search_for_track(&self, local: &LocalTrack, limit: u32) -> Result<Vec<SpotifyTrack>> {
        SpotifyClient::search_for_track(self, local, limit).await
    }

    async fn list_playlists(&self) -> Result<Vec<Playlist>> {
        SpotifyClient::list_playlists(self).await
    }

    async fn create_playlist(&self, name: &str, public: bool) -> Result<Playlist> {
        SpotifyClient::create_playlist(self, name, public).await
    }

    async fn playlist_entries(&self, playlist_id: &str) -> Result<Vec<PlaylistEntry>> {
        SpotifyClient::playlist_entries(self, playlist_id).await
    }

    async fn add_tracks(&self, playlist_id: &str, uris: &[String]) -> Result<usize> {
        SpotifyClient::add_tracks_to_playlist(self, playlist_id, uris).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_playlist_with_no_owner_is_writable() {
        // Apple's library playlists carry no owner. Treating that absence as
        // "not mine" meant never finding the playlist again, so every push
        // would have created a fresh duplicate.
        let apple: Playlist =
            serde_json::from_str(r#"{"id":"p.1","name":"New Downloads"}"#).unwrap();
        assert!(apple.is_writable_by("anyone"));
        assert!(
            !apple.is_owned_by("anyone"),
            "ownership is genuinely unknown — only writability is being asserted"
        );
    }

    #[test]
    fn a_followed_playlist_is_not_writable() {
        // Spotify lists playlists the user merely follows; writing to one
        // fails, so there the owner still has to match.
        let followed: Playlist = serde_json::from_str(
            r#"{"id":"1","name":"Someone else's","owner":{"id":"them"}}"#,
        )
        .unwrap();
        assert!(!followed.is_writable_by("me"));

        let mine: Playlist =
            serde_json::from_str(r#"{"id":"2","name":"Mine","owner":{"id":"me"}}"#).unwrap();
        assert!(mine.is_writable_by("me"));
        assert!(mine.is_owned_by("me"));
    }
}

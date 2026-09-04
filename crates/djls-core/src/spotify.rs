//! Minimal Spotify Web API client.
//!
//! The CLI uses the Client Credentials flow, which is enough for search and
//! needs no user account. Playlist writes in the desktop app will need
//! Authorization Code + PKCE with a `127.0.0.1` loopback redirect (Spotify does
//! not accept `localhost`), and must persist the rotated refresh token on every
//! refresh.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";
const SEARCH_URL: &str = "https://api.spotify.com/v1/search";

/// Search is one track per request — there is no batch endpoint — so a large
/// download folder means one call per track. Pace them.
const MIN_REQUEST_GAP: Duration = Duration::from_millis(120);
const MAX_RETRIES: u32 = 4;

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

pub struct SpotifyClient {
    http: reqwest::Client,
    client_id: String,
    client_secret: String,
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
    pub fn new(client_id: impl Into<String>, client_secret: impl Into<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .user_agent("dj-library-sync/0.1")
            .build()
            .context("building HTTP client")?;

        Ok(Self {
            http,
            client_id: client_id.into(),
            client_secret: client_secret.into(),
            market: None,
            token: Mutex::new(None),
            last_request: Mutex::new(None),
            stats: RequestStats::default(),
        })
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

        #[derive(Deserialize)]
        struct TokenResponse {
            access_token: String,
            expires_in: u64,
        }

        let basic = B64.encode(format!("{}:{}", self.client_id, self.client_secret));
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

        let mut cached = self.token.lock().await;
        *cached = Some(CachedToken {
            value: parsed.access_token.clone(),
            expires_at: Instant::now() + Duration::from_secs(parsed.expires_in),
        });

        Ok(parsed.access_token)
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
                ("limit", limit.to_string()),
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
                .map(|page| page.items.into_iter().filter_map(ApiTrack::into_track).collect())
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
    pub async fn search_text(&self, artist: &str, title: &str, limit: u32) -> Result<Vec<SpotifyTrack>> {
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
}

fn escape(input: &str) -> String {
    input.replace('"', " ").replace('\\', " ")
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
            isrc: self.external_ids.and_then(|e| e.isrc).map(|s| s.to_uppercase()),
            url: self.external_urls.and_then(|e| e.spotify),
            popularity: self.popularity,
        })
    }
}

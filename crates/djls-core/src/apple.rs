//! Apple Music.
//!
//! The shape of this client is dictated by two Apple constraints, both settled
//! in `docs/apple-music-auth.md`:
//!
//! 1. **Every request needs a developer token**, an ES256 JWT signed with a
//!    private key that cannot ship inside an open-source app. So the token
//!    comes from our service, which holds the key. That is also what makes the
//!    paid tier enforceable: a fork can copy every line here and still not
//!    issue tokens.
//! 2. **Library requests additionally need a Music User Token**, and Apple
//!    publishes no REST endpoint that issues one. It arrives from the hosted
//!    MusicKit page via the loopback listener, and this client only ever reads
//!    it back out of the OS keychain.
//!
//! Catalogue reads are storefront-scoped, so the storefront is discovered once
//! from the signed-in user rather than guessed.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::platform::{
    MusicPlatform, PlatformEntry, PlatformInfo, PlatformPlaylist, PlatformTrack, PlatformUser,
};
use crate::spotify::RequestStats;
use crate::tags::LocalTrack;

const API_BASE: &str = "https://api.music.apple.com";
const MAX_RETRIES: u32 = 3;

/// Apple's own ceiling for catalogue search.
const MAX_SEARCH_LIMIT: u32 = 25;

/// Re-fetch a developer token slightly before it expires, so a request never
/// races the expiry it just checked.
const TOKEN_REFRESH_SKEW: Duration = Duration::from_secs(60);

/// Where the developer token comes from.
///
/// Only the hosted variant exists today. The `UserProvided` model in
/// [`crate::platform::CredentialModel`] additionally needs local ES256 signing
/// of a `.p8`, which means a new dependency; it is deliberately not stubbed
/// here, so the free Apple path is visibly absent rather than silently broken.
pub enum DeveloperTokenSource {
    /// Our token service signs with the key it holds. The activation token
    /// identifies the caller, and the service refuses without credits.
    Hosted {
        service_url: String,
        activation_token: String,
    },
}

/// A developer token and when to stop using it.
struct CachedDeveloperToken {
    value: String,
    expires_at: Instant,
}

pub struct AppleClient {
    http: reqwest::Client,
    source: DeveloperTokenSource,
    /// Obtained through the hosted MusicKit page; required for anything under
    /// `/me`. Absent for a catalogue-only client.
    music_user_token: Option<String>,
    developer_token: Mutex<Option<CachedDeveloperToken>>,
    /// Id and display name together: `current_user` wants the name, and
    /// caching only the id meant fetching the same document twice on every
    /// push.
    storefront: Mutex<Option<Storefront>>,
    last_request: Mutex<Option<Instant>>,
    pub stats: RequestStats,
}

/// Raised when the service will not issue a developer token because the
/// licence has no credits left. Distinct from every other failure: it is the
/// one the app should answer with "buy more", not "try again".
#[derive(Debug)]
pub struct OutOfCredits;

impl std::fmt::Display for OutOfCredits {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "No credits remaining on this licence. Apple Music pushes are paused \
             until the next billing period or a top-up; Spotify is unaffected."
        )
    }
}

impl std::error::Error for OutOfCredits {}

impl AppleClient {
    pub fn new(source: DeveloperTokenSource, music_user_token: Option<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .user_agent("dj-library-sync/0.1")
            .build()
            .context("building HTTP client")?;

        Ok(Self {
            http,
            source,
            music_user_token,
            developer_token: Mutex::new(None),
            storefront: Mutex::new(None),
            last_request: Mutex::new(None),
            stats: RequestStats::default(),
        })
    }

    /// Space requests out. Apple does not publish a rate limit, so this is
    /// politeness rather than a documented requirement — and it keeps a 200
    /// track backlog from looking like an attack.
    async fn pace(&self) {
        const MIN_INTERVAL: Duration = Duration::from_millis(120);
        let mut last = self.last_request.lock().await;
        if let Some(prev) = *last {
            let elapsed = prev.elapsed();
            if elapsed < MIN_INTERVAL {
                tokio::time::sleep(MIN_INTERVAL - elapsed).await;
            }
        }
        *last = Some(Instant::now());
    }

    async fn developer_token(&self) -> Result<String> {
        {
            let cached = self.developer_token.lock().await;
            if let Some(token) = cached.as_ref() {
                if Instant::now() < token.expires_at {
                    return Ok(token.value.clone());
                }
            }
        }

        let DeveloperTokenSource::Hosted {
            service_url,
            activation_token,
        } = &self.source;

        let url = format!("{}/api/developer-token", service_url.trim_end_matches('/'));
        let resp = self
            .http
            .post(&url)
            .bearer_auth(activation_token)
            .json(&serde_json::json!({}))
            .send()
            .await
            .with_context(|| format!("requesting a developer token from {url}"))?;

        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();

        if status.as_u16() == 402 {
            return Err(OutOfCredits.into());
        }
        if status.as_u16() == 401 {
            bail!(
                "The licence on this machine is not valid. Re-enter it in \
                 Settings -> Licence, or start a trial."
            );
        }
        if !status.is_success() {
            bail!("Could not get a developer token ({status}): {body}");
        }

        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct TokenResponse {
            token: String,
            /// RFC 3339. Only used to decide when to ask again.
            ///
            /// Optional, so a missing value falls back rather than failing —
            /// which is exactly why the camelCase mismatch here went unnoticed
            /// while the same bug in `licence` announced itself. Every token
            /// was being cached for the default hour regardless of what the
            /// service said.
            #[serde(default)]
            expires_at: Option<String>,
        }

        let parsed: TokenResponse =
            serde_json::from_str(&body).context("parsing the developer token response")?;

        // Trust our own service's lifetime only as a hint; cap it so a bug
        // there cannot pin a stale token in memory for hours.
        let lifetime = parsed
            .expires_at
            .as_deref()
            .and_then(parse_rfc3339_secs_from_now)
            .unwrap_or(Duration::from_secs(3600))
            .min(Duration::from_secs(3600));

        let expires_at = Instant::now() + lifetime.saturating_sub(TOKEN_REFRESH_SKEW);

        let mut cached = self.developer_token.lock().await;
        *cached = Some(CachedDeveloperToken {
            value: parsed.token.clone(),
            expires_at,
        });

        Ok(parsed.token)
    }

    fn user_token(&self) -> Result<&str> {
        self.music_user_token.as_deref().ok_or_else(|| {
            anyhow!(
                "Apple Music is not connected on this machine. Connect it from \
                 the app, which opens Apple's sign-in page in your browser."
            )
        })
    }

    async fn request(
        &self,
        method: reqwest::Method,
        url: &str,
        body: Option<serde_json::Value>,
        needs_user: bool,
    ) -> Result<String> {
        let mut attempt = 0u32;

        loop {
            self.pace().await;
            let developer = self.developer_token().await?;

            let mut req = self
                .http
                .request(method.clone(), url)
                .bearer_auth(&developer);

            if needs_user {
                req = req.header("Music-User-Token", self.user_token()?);
            }
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
                Err(err) => return Err(err).context("Apple Music request failed"),
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
                    bail!("Apple Music rate limit persisted after {MAX_RETRIES} retries");
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

            let text = resp.text().await.context("reading Apple Music response")?;

            if !status.is_success() {
                self.stats.errors.fetch_add(1, Ordering::Relaxed);
                return Err(anyhow!("{}", describe_failure(status.as_u16(), &text)));
            }

            return Ok(text);
        }
    }

    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        needs_user: bool,
    ) -> Result<T> {
        let body = self
            .request(reqwest::Method::GET, url, None, needs_user)
            .await?;
        serde_json::from_str(&body).with_context(|| format!("parsing response from {url}"))
    }

    /// The user's storefront, which every catalogue URL is scoped to.
    ///
    /// Looked up once: it cannot change mid-session, and guessing "us" would
    /// silently match tracks the user cannot actually play.
    ///
    /// Note that this reads `/v1/me/storefront`, which needs the Music User
    /// Token — so catalogue search transitively requires one too, even though
    /// the search request itself does not. A client built without a user token
    /// cannot search, and says so here rather than failing deeper in.
    async fn storefront_record(&self) -> Result<Storefront> {
        {
            let cached = self.storefront.lock().await;
            if let Some(found) = cached.as_ref() {
                return Ok(found.clone());
            }
        }

        let resp: StorefrontResponse = self
            .get_json(&format!("{API_BASE}/v1/me/storefront"), true)
            .await?;

        let found = resp
            .data
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("Apple Music returned no storefront for this account"))?;

        let mut cached = self.storefront.lock().await;
        *cached = Some(found.clone());
        Ok(found)
    }

    async fn storefront(&self) -> Result<String> {
        Ok(self.storefront_record().await?.id)
    }

    async fn search_text(&self, term: &str, limit: u32) -> Result<Vec<PlatformTrack>> {
        let storefront = self.storefront().await?;
        let limit = limit.clamp(1, MAX_SEARCH_LIMIT);
        let url = format!(
            "{API_BASE}/v1/catalog/{storefront}/search?term={}&types=songs&limit={limit}",
            urlencode(term)
        );

        let resp: SearchResponse = self.get_json(&url, false).await?;
        Ok(resp
            .results
            .songs
            .map(|s| s.data.into_iter().map(Song::into_track).collect())
            .unwrap_or_default())
    }

    /// Walk a paginated `/me` collection to the end.
    async fn get_all_library<T: serde::de::DeserializeOwned>(
        &self,
        first: String,
    ) -> Result<Vec<T>> {
        let mut url = Some(first);
        let mut all = Vec::new();

        while let Some(current) = url.take() {
            let page: LibraryPage<T> = self.get_json(&current, true).await?;
            all.extend(page.data);
            // `next` is a path, not an absolute URL.
            url = page.next.map(|n| format!("{API_BASE}{n}"));
        }

        Ok(all)
    }
}

/// How hard to look for a playlist Apple has accepted but not yet listed.
const CREATE_LOOKUP_ATTEMPTS: u32 = 4;
const CREATE_LOOKUP_DELAY: Duration = Duration::from_millis(700);

/// The created playlist, when Apple bothered to return one.
///
/// Separate so the empty-body case is testable without a network: an empty or
/// non-JSON body is a normal successful response here, not a failure, and
/// treating it as a parse error failed a push whose playlist had been created.
fn parse_created_playlist(body: &str) -> Option<PlatformPlaylist> {
    if body.trim().is_empty() {
        return None;
    }
    serde_json::from_str::<LibraryPage<LibraryPlaylist>>(body)
        .ok()?
        .data
        .into_iter()
        .next()
        .map(LibraryPlaylist::into_playlist)
}

/// Turn Apple's status codes into something that names the actual fix.
fn describe_failure(status: u16, body: &str) -> String {
    match status {
        401 => format!(
            "Apple Music rejected the developer token ({status}). It may have expired; \
             retrying usually re-issues one. Body: {body}"
        ),
        403 => format!(
            "Apple Music refused the request ({status}). The usual cause is an account \
             without an active Apple Music subscription — the API allows catalogue \
             search without one but refuses every library operation. Body: {body}"
        ),
        404 => format!(
            "Apple Music found nothing at that address ({status}). For a playlist this \
             normally means it was deleted on another device. Body: {body}"
        ),
        _ => format!("Apple Music error ({status}): {body}"),
    }
}

/// Seconds between now and an RFC 3339 timestamp, or `None` if unparseable or
/// already past. Deliberately tiny: the only consumer is a cache lifetime, and
/// a date-time dependency here would be out of proportion.
fn parse_rfc3339_secs_from_now(value: &str) -> Option<Duration> {
    let secs = rfc3339_to_unix(value)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    let remaining = secs - now;
    (remaining > 0).then(|| Duration::from_secs(remaining as u64))
}

/// Parse `YYYY-MM-DDTHH:MM:SS` (with optional fraction and `Z`) to a Unix
/// timestamp. Only UTC is accepted, which is all our service emits.
///
/// Shared with `licence`, which needs the same arithmetic for activation-token
/// expiry. Kept here rather than promoted to its own module: two callers is
/// not yet a reason to invent a `time` module, and a real date-time dependency
/// would be out of proportion to reading one field.
pub(crate) fn rfc3339_to_unix(value: &str) -> Option<i64> {
    let bytes = value.as_bytes();
    if bytes.len() < 19 {
        return None;
    }
    let num = |a: usize, b: usize| value.get(a..b)?.parse::<i64>().ok();

    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, s) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }

    // Days from civil, per Howard Hinnant's algorithm.
    let y_adj = if mo <= 2 { y - 1 } else { y };
    let era = if y_adj >= 0 { y_adj } else { y_adj - 399 } / 400;
    let yoe = y_adj - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;

    Some(days * 86_400 + h * 3_600 + mi * 60 + s)
}

fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/* ------------------------------------------------------------------ wire */

#[derive(Deserialize)]
struct StorefrontResponse {
    #[serde(default)]
    data: Vec<Storefront>,
}

#[derive(Deserialize, Clone)]
struct Storefront {
    id: String,
    #[serde(default)]
    attributes: Option<StorefrontAttributes>,
}

#[derive(Deserialize, Clone)]
struct StorefrontAttributes {
    #[serde(default)]
    name: Option<String>,
}

#[derive(Deserialize)]
struct SearchResponse {
    #[serde(default)]
    results: SearchResults,
}

#[derive(Deserialize, Default)]
struct SearchResults {
    #[serde(default)]
    songs: Option<SongList>,
}

#[derive(Deserialize)]
struct SongList {
    #[serde(default)]
    data: Vec<Song>,
}

#[derive(Deserialize)]
struct SongsResponse {
    #[serde(default)]
    data: Vec<Song>,
}

#[derive(Deserialize)]
struct Song {
    id: String,
    #[serde(default)]
    attributes: Option<SongAttributes>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct SongAttributes {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    artist_name: Option<String>,
    #[serde(default)]
    album_name: Option<String>,
    #[serde(default)]
    duration_in_millis: Option<u64>,
    #[serde(default)]
    isrc: Option<String>,
    #[serde(default)]
    url: Option<String>,
    /// Present on library songs; carries the catalogue id the library entry
    /// was created from.
    #[serde(default)]
    play_params: Option<PlayParams>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct PlayParams {
    #[serde(default)]
    catalog_id: Option<String>,
}

impl Song {
    fn into_track(self) -> PlatformTrack {
        let attrs = self.attributes.unwrap_or_default();
        // Apple's "uri" is just the catalogue id — that is what an add-to-
        // playlist request takes, so storing anything else would need
        // translating back at push time.
        let id = attrs
            .play_params
            .as_ref()
            .and_then(|p| p.catalog_id.clone())
            .unwrap_or_else(|| self.id.clone());

        PlatformTrack {
            uri: id.clone(),
            id,
            name: attrs.name.unwrap_or_default(),
            artists: split_artist_name(attrs.artist_name.as_deref().unwrap_or_default()),
            album: attrs.album_name.unwrap_or_default(),
            duration_ms: attrs.duration_in_millis.unwrap_or(0),
            isrc: attrs.isrc,
            url: attrs.url,
            // Apple publishes no popularity score. `None` rather than 0, which
            // the matcher would read as "known to be unpopular".
            popularity: None,
        }
    }

    fn into_entry(self) -> PlatformEntry {
        let track = self.into_track();
        PlatformEntry {
            uri: track.uri,
            name: track.name,
            artists: track.artists,
            duration_ms: track.duration_ms,
        }
    }
}

/// Apple returns one `artistName` string rather than a list. Splitting it lets
/// the matcher's token comparison work the same way it does for Spotify.
fn split_artist_name(value: &str) -> Vec<String> {
    let names = crate::normalize::split_artists(value);
    if names.is_empty() {
        vec![value.to_string()]
    } else {
        names
    }
}

#[derive(Deserialize)]
struct LibraryPage<T> {
    #[serde(default = "Vec::new")]
    data: Vec<T>,
    #[serde(default)]
    next: Option<String>,
}

#[derive(Deserialize)]
struct LibraryPlaylist {
    id: String,
    #[serde(default)]
    attributes: Option<LibraryPlaylistAttributes>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct LibraryPlaylistAttributes {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    can_edit: Option<bool>,
}

impl LibraryPlaylist {
    /// Whether tracks can be added. `canEdit` absent means unknown, which is
    /// treated as yes — see `list_playlists`.
    fn is_writable(&self) -> bool {
        self.attributes
            .as_ref()
            .and_then(|a| a.can_edit)
            .unwrap_or(true)
    }

    fn into_playlist(self) -> PlatformPlaylist {
        let attrs = self.attributes.unwrap_or_default();
        PlatformPlaylist {
            id: self.id,
            name: attrs.name.unwrap_or_default(),
            // Apple library playlists have no public/private flag in the API.
            // `None` says "unknown", which is true, rather than asserting one.
            public: None,
            owner: None,
            // Apple does not return a track count on the playlist object, and
            // counting would cost a request per playlist.
            items: None,
        }
    }
}

/* ------------------------------------------------------------------ trait */

#[async_trait]
impl MusicPlatform for AppleClient {
    fn info(&self) -> PlatformInfo {
        PlatformInfo::APPLE_MUSIC
    }

    async fn current_user(&self) -> Result<PlatformUser> {
        // Apple exposes no "me" profile — no name, no id, by design. The
        // storefront is the only identifying fact available, and fetching it
        // proves both tokens work, which is what callers actually want.
        let storefront = self.storefront_record().await?;

        Ok(PlatformUser {
            display_name: storefront.attributes.and_then(|a| a.name),
            id: storefront.id,
        })
    }

    async fn search_isrc(&self, isrc: &str) -> Result<Vec<PlatformTrack>> {
        let storefront = self.storefront().await?;
        let url = format!(
            "{API_BASE}/v1/catalog/{storefront}/songs?filter[isrc]={}",
            urlencode(isrc)
        );
        let resp: SongsResponse = self.get_json(&url, false).await?;
        Ok(resp.data.into_iter().map(Song::into_track).collect())
    }

    async fn search_for_track(&self, local: &LocalTrack, limit: u32) -> Result<Vec<PlatformTrack>> {
        let artist = local.primary_artist();
        let title = &local.parsed.base;

        let mut results = self
            .search_text(&format!("{artist} {title}"), limit)
            .await?;

        // Same problem Spotify has: a bare "artist title" search ranks the
        // radio edit above the extended mix, so a long-form local file never
        // sees its own match. Ask again, naming the mix.
        if local.parsed.kind.is_long_form() {
            let hinted = format!("{artist} {title} extended mix");
            let more = self.search_text(&hinted, limit).await.unwrap_or_default();
            for track in more {
                if !results.iter().any(|t| t.id == track.id) {
                    results.push(track);
                }
            }
        }

        Ok(results)
    }

    async fn list_playlists(&self) -> Result<Vec<PlatformPlaylist>> {
        let playlists: Vec<LibraryPlaylist> = self
            .get_all_library(format!("{API_BASE}/v1/me/library/playlists?limit=100"))
            .await?;

        // Playlists added from the Apple Music catalogue live in the library
        // but refuse writes. Listing them as sync targets would turn a clear
        // "no editable playlist by that name" into a 403 at push time, so
        // they are filtered here. Absent `canEdit` is treated as editable:
        // hiding everything because Apple omitted a field would be worse.
        Ok(playlists
            .into_iter()
            .filter(LibraryPlaylist::is_writable)
            .map(LibraryPlaylist::into_playlist)
            .collect())
    }

    async fn create_playlist(&self, name: &str, _public: bool) -> Result<PlatformPlaylist> {
        // Creating by name is idempotent here, deliberately.
        //
        // Apple's library is eventually consistent, so a playlist created a
        // moment ago may not be in the list the caller checked before deciding
        // to create one. That is how two playlists with the same name appeared:
        // one run created it and then failed on the empty response body, and
        // the next run could not yet see it.
        //
        // The cost is that two playlists genuinely meant to share a name cannot
        // be made from here. They are named by date, so that is not a case this
        // app produces.
        if let Some(existing) = self
            .list_playlists()
            .await?
            .into_iter()
            .find(|p| p.name.eq_ignore_ascii_case(name))
        {
            return Ok(existing);
        }

        // `public` is ignored on purpose: Apple's library playlist API has no
        // such attribute. Accepting it and doing nothing is better than
        // changing the trait for one platform's omission.
        let body = serde_json::json!({
            "attributes": { "name": name, "description": "Created by DJ Library Sync" }
        });

        let text = self
            .request(
                reqwest::Method::POST,
                &format!("{API_BASE}/v1/me/library/playlists"),
                Some(body),
                true,
            )
            .await?;

        // Apple accepts the write and frequently answers with an empty body —
        // library mutations are applied asynchronously, so there is nothing to
        // echo back yet. Parse the playlist when it is there.
        if let Some(created) = parse_created_playlist(&text) {
            return Ok(created);
        }

        // Otherwise find it by name. The library is eventually consistent, so
        // it may take a moment to appear; a few short waits beat failing a
        // push that actually succeeded.
        for attempt in 0..CREATE_LOOKUP_ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(CREATE_LOOKUP_DELAY).await;
            }
            if let Some(found) = self
                .list_playlists()
                .await?
                .into_iter()
                .find(|p| p.name.eq_ignore_ascii_case(name))
            {
                return Ok(found);
            }
        }

        Err(anyhow!(
            "Apple Music accepted the new playlist \"{name}\" but it has not appeared in              the library yet. It may show up shortly — try the push again."
        ))
    }

    async fn playlist_entries(&self, playlist_id: &str) -> Result<Vec<PlatformEntry>> {
        let songs: Vec<Song> = self
            .get_all_library(format!(
                "{API_BASE}/v1/me/library/playlists/{playlist_id}/tracks?limit=100"
            ))
            .await?;
        Ok(songs.into_iter().map(Song::into_entry).collect())
    }

    async fn add_tracks(&self, playlist_id: &str, uris: &[String]) -> Result<usize> {
        if uris.is_empty() {
            return Ok(0);
        }

        // Apple accepts a batch; keep it modest so one rejected id does not
        // cost a whole sync.
        const CHUNK: usize = 25;
        let mut added = 0usize;

        for chunk in uris.chunks(CHUNK) {
            let data: Vec<_> = chunk
                .iter()
                .map(|id| serde_json::json!({ "id": id, "type": "songs" }))
                .collect();

            self.request(
                reqwest::Method::POST,
                &format!("{API_BASE}/v1/me/library/playlists/{playlist_id}/tracks"),
                Some(serde_json::json!({ "data": data })),
                true,
            )
            .await?;

            added += chunk.len();
        }

        Ok(added)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn song_from(json: &str) -> Song {
        serde_json::from_str(json).expect("valid song json")
    }

    #[test]
    fn catalogue_song_maps_onto_the_shared_track_type() {
        let song = song_from(
            r#"{
              "id": "1440857781",
              "type": "songs",
              "attributes": {
                "name": "Keep Up",
                "artistName": "Adam Beyer, Argy, Alok",
                "albumName": "Keep Up - Single",
                "durationInMillis": 241000,
                "isrc": "SE5V42200001",
                "url": "https://music.apple.com/us/album/keep-up/1440857781"
              }
            }"#,
        );

        let track = song.into_track();
        assert_eq!(track.id, "1440857781");
        assert_eq!(
            track.uri, "1440857781",
            "the id is what an add request takes"
        );
        assert_eq!(track.name, "Keep Up");
        assert_eq!(track.duration_ms, 241_000);
        assert_eq!(track.isrc.as_deref(), Some("SE5V42200001"));
        assert_eq!(
            track.artists,
            vec!["Adam Beyer", "Argy", "Alok"],
            "one artistName string has to become a list for token comparison"
        );
        assert!(
            track.popularity.is_none(),
            "Apple publishes no popularity; 0 would read as 'known to be unpopular'"
        );
    }

    #[test]
    fn library_song_prefers_its_catalogue_id() {
        // A library song's own id (i.xxx) cannot be added to a playlist; the
        // catalogue id inside playParams can.
        let song = song_from(
            r#"{
              "id": "i.abc123",
              "type": "library-songs",
              "attributes": {
                "name": "Your Mind",
                "artistName": "Adam Beyer & Bart Skils",
                "durationInMillis": 502000,
                "playParams": { "id": "i.abc123", "catalogId": "1500000001" }
              }
            }"#,
        );

        let entry = song.into_entry();
        assert_eq!(entry.uri, "1500000001");
        assert_eq!(entry.duration_ms, 502_000);
        assert_eq!(entry.artists, vec!["Adam Beyer", "Bart Skils"]);
    }

    #[test]
    fn a_song_missing_every_attribute_still_parses() {
        // Apple omits fields freely. Failing to parse would lose a whole page
        // of results over one incomplete row.
        let song = song_from(r#"{ "id": "123", "type": "songs" }"#);
        let track = song.into_track();
        assert_eq!(track.id, "123");
        assert_eq!(track.duration_ms, 0);
        assert!(track.name.is_empty());
    }

    #[test]
    fn search_results_without_songs_are_empty_not_an_error() {
        // A search matching nothing omits the `songs` key entirely.
        let resp: SearchResponse = serde_json::from_str(r#"{ "results": {} }"#).unwrap();
        assert!(resp.results.songs.is_none());
    }

    #[test]
    fn playlists_that_refuse_writes_are_not_sync_targets() {
        let editable: LibraryPlaylist = serde_json::from_str(
            r#"{ "id": "p.1", "attributes": { "name": "Mine", "canEdit": true } }"#,
        )
        .unwrap();
        let catalogue: LibraryPlaylist = serde_json::from_str(
            r#"{ "id": "p.2", "attributes": { "name": "Apple's", "canEdit": false } }"#,
        )
        .unwrap();
        let unknown: LibraryPlaylist =
            serde_json::from_str(r#"{ "id": "p.3", "attributes": { "name": "No flag" } }"#)
                .unwrap();

        assert!(editable.is_writable());
        assert!(
            !catalogue.is_writable(),
            "adding here would 403 at push time"
        );
        assert!(
            unknown.is_writable(),
            "a missing flag must not hide every playlist"
        );
    }

    #[test]
    fn playlist_reports_unknown_rather_than_guessing() {
        let playlist: LibraryPlaylist =
            serde_json::from_str(r#"{ "id": "p.1", "attributes": { "name": "New Downloads" } }"#)
                .unwrap();
        let mapped = playlist.into_playlist();
        assert_eq!(mapped.name, "New Downloads");
        assert!(mapped.public.is_none(), "Apple has no public/private flag");
        assert!(mapped.track_count().is_none(), "no count means no count");
    }

    #[test]
    fn library_pages_chain_through_next() {
        let page: LibraryPage<Song> = serde_json::from_str(
            r#"{ "data": [{ "id": "1" }], "next": "/v1/me/library/playlists?offset=100" }"#,
        )
        .unwrap();
        assert_eq!(page.data.len(), 1);
        assert_eq!(
            page.next.as_deref(),
            Some("/v1/me/library/playlists?offset=100"),
            "next is a path, so the base URL has to be prepended"
        );

        let last: LibraryPage<Song> = serde_json::from_str(r#"{ "data": [] }"#).unwrap();
        assert!(last.next.is_none());
    }

    #[test]
    fn search_terms_are_encoded() {
        assert_eq!(urlencode("Above & Beyond"), "Above+%26+Beyond");
        assert_eq!(urlencode("Tomorrow's Dream"), "Tomorrow%27s+Dream");
        assert_eq!(urlencode("Caf\u{e9}"), "Caf%C3%A9");
        assert_eq!(urlencode("plain"), "plain");
    }

    #[test]
    fn rfc3339_parses_to_unix_seconds() {
        // Checked against known epoch values.
        assert_eq!(rfc3339_to_unix("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(rfc3339_to_unix("2000-01-01T00:00:00Z"), Some(946_684_800));
        assert_eq!(rfc3339_to_unix("2026-09-16T12:00:00Z"), Some(1_789_560_000));
        assert_eq!(
            rfc3339_to_unix("2026-09-16T12:00:00.123Z"),
            Some(1_789_560_000),
            "a fractional second must not break it"
        );
    }

    #[test]
    fn unparseable_or_past_expiry_falls_back() {
        assert!(parse_rfc3339_secs_from_now("not a date").is_none());
        assert!(parse_rfc3339_secs_from_now("").is_none());
        assert!(
            parse_rfc3339_secs_from_now("2000-01-01T00:00:00Z").is_none(),
            "an expiry in the past must not be treated as a lifetime"
        );
    }

    #[test]
    fn failures_name_the_actual_fix() {
        let forbidden = describe_failure(403, "{}");
        assert!(
            forbidden.contains("subscription"),
            "403 is nearly always a missing Apple Music subscription: {forbidden}"
        );
        // Catalogue search works without one, which is why this is worth saying.
        assert!(forbidden.contains("library"));

        assert!(describe_failure(401, "{}").contains("developer token"));
        assert!(describe_failure(404, "{}").contains("deleted"));
        assert!(describe_failure(500, "boom").contains("boom"));
    }

    #[test]
    fn out_of_credits_says_spotify_still_works() {
        // The whole point of the free tier is that running out of Apple credits
        // does not brick the app.
        let message = OutOfCredits.to_string();
        assert!(message.contains("Spotify is unaffected"), "{message}");
    }

    #[test]
    fn a_storefront_carries_its_display_name() {
        // current_user reads both the id and the name off one document. Before,
        // it fetched the same document twice on every push.
        let resp: StorefrontResponse = serde_json::from_str(
            r#"{"data":[{"id":"mx","type":"storefronts","attributes":{"name":"Mexico"}}]}"#,
        )
        .unwrap();
        let first = resp.data.first().unwrap();
        assert_eq!(first.id, "mx");
        assert_eq!(
            first.attributes.as_ref().and_then(|a| a.name.as_deref()),
            Some("Mexico")
        );
    }

    #[test]
    fn a_storefront_without_attributes_still_identifies_the_account() {
        let resp: StorefrontResponse =
            serde_json::from_str(r#"{"data":[{"id":"us","type":"storefronts"}]}"#).unwrap();
        assert_eq!(resp.data.first().unwrap().id, "us");
    }

    #[test]
    fn an_empty_create_response_is_not_a_failure() {
        // Apple applies library writes asynchronously and answers with an
        // empty body. Treating that as a parse error — "expected value at line
        // 1 column 1" — failed a push whose playlist had in fact been created.
        assert!(parse_created_playlist("").is_none());
        assert!(parse_created_playlist("   \n ").is_none());
        // Not JSON at all is the same situation, not a reason to give up.
        assert!(parse_created_playlist("Accepted").is_none());
    }

    #[test]
    fn a_create_response_with_the_playlist_is_used_directly() {
        let created = parse_created_playlist(
            r#"{"data":[{"id":"p.abc","type":"library-playlists",
                "attributes":{"name":"New Downloads 2026-09-17","canEdit":true}}]}"#,
        )
        .expect("a populated response should parse");
        assert_eq!(created.id, "p.abc");
        assert_eq!(created.name, "New Downloads 2026-09-17");
    }

    #[test]
    fn a_response_with_an_empty_data_array_falls_back_too() {
        assert!(parse_created_playlist(r#"{"data":[]}"#).is_none());
    }

    #[test]
    fn apple_is_hosted_and_metered() {
        let info = PlatformInfo::APPLE_MUSIC;
        assert!(matches!(
            info.credentials,
            crate::platform::CredentialModel::Hosted
        ));
        assert!(info.metered, "Apple is what the paid tier sells");
    }
}

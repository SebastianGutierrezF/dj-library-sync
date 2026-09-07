//! The music-platform abstraction.
//!
//! Everything above this line — tag reading, normalization, scoring, the
//! database, the watcher — is platform-agnostic already. Only the client is
//! not, so a second service means implementing this trait, not touching the
//! matcher.
//!
//! The distinction that matters commercially is [`CredentialModel`]. Spotify
//! forbids one developer serving many users (Development Mode caps at five,
//! and extended quota is organizations-only), so Spotify can only ever work
//! with credentials the user supplies themselves. Platforms that allow a
//! hosted developer account can be offered with one-click sign-in instead.

use anyhow::Result;
use async_trait::async_trait;

use crate::spotify::{Playlist, PlaylistEntry, SpotifyTrack, SpotifyUser};
use crate::tags::LocalTrack;

/// A track in a streaming catalogue. Named for the role, not the vendor —
/// Spotify was simply the first implementation.
pub type PlatformTrack = SpotifyTrack;
pub type PlatformUser = SpotifyUser;
pub type PlatformPlaylist = Playlist;
pub type PlatformEntry = PlaylistEntry;

/// Who owns the API credentials, which decides whether a platform can be
/// offered to an unlimited number of users.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialModel {
    /// Each user registers their own developer app and supplies the client ID.
    /// The only compliant way to offer Spotify beyond five people — and the
    /// reason Spotify has to be the free tier: it costs the vendor nothing and
    /// cannot be scaled by them anyway.
    UserProvided,
    /// One developer account serves every user, so sign-in is a single click.
    /// This is what can carry a paid tier.
    Hosted,
}

/// Static facts about a platform, for the connect screen and for billing.
#[derive(Debug, Clone, Copy)]
pub struct PlatformInfo {
    pub id: &'static str,
    pub display_name: &'static str,
    pub credentials: CredentialModel,
    /// Whether usage counts against a paid quota.
    pub metered: bool,
}

impl PlatformInfo {
    pub const SPOTIFY: PlatformInfo = PlatformInfo {
        id: "spotify",
        display_name: "Spotify",
        credentials: CredentialModel::UserProvided,
        metered: false,
    };
}

/// What a streaming service has to do to be a sync target.
///
/// Deliberately narrow: search, identify the user, and manage playlists.
/// Nothing here streams audio, which keeps every implementation inside the
/// "non-streaming" category that platforms treat most permissively.
#[async_trait]
pub trait MusicPlatform: Send + Sync {
    fn info(&self) -> PlatformInfo;

    /// Who is signed in. Also the cheapest way to check a token still works.
    async fn current_user(&self) -> Result<PlatformUser>;

    /// Exact lookup by recording identifier, when the local file has one.
    /// Return an empty vec if the platform has no ISRC search.
    async fn search_isrc(&self, isrc: &str) -> Result<Vec<PlatformTrack>>;

    /// Every candidate worth scoring for this file.
    async fn search_for_track(&self, local: &LocalTrack, limit: u32) -> Result<Vec<PlatformTrack>>;

    async fn list_playlists(&self) -> Result<Vec<PlatformPlaylist>>;
    async fn create_playlist(&self, name: &str, public: bool) -> Result<PlatformPlaylist>;

    /// Full metadata, not just ids: catalogues list one recording under
    /// several ids, so duplicate detection has to compare recordings.
    async fn playlist_entries(&self, playlist_id: &str) -> Result<Vec<PlatformEntry>>;

    async fn add_tracks(&self, playlist_id: &str, uris: &[String]) -> Result<usize>;
}

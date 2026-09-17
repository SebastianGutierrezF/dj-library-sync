//! Core library for DJ Library Sync.
//!
//! Deliberately headless: tag reading, title/artist normalization, candidate
//! scoring and the Spotify client all live here so the matcher can be measured
//! from the CLI long before any UI exists.

pub mod apple;
pub mod auth;
pub mod config;
pub mod db;
pub mod matcher;
pub mod normalize;
pub mod platform;
pub mod spotify;
pub mod tags;
pub mod watcher;

pub use auth::{AuthConfig, KeyringStore, TokenStore, Tokens};
pub use config::Config;
pub use db::{Database, TrackState};
pub use matcher::{Candidate, MatchMethod, Score, ShorterVersionPolicy, Verdict};
pub use normalize::{MixKind, ParsedTitle};
pub use platform::{CredentialModel, MusicPlatform, PlatformInfo, PlatformTrack};
pub use spotify::{Playlist, SpotifyClient, SpotifyTrack, SpotifyUser};
pub use tags::LocalTrack;

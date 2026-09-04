//! Core library for DJ Library Sync.
//!
//! Deliberately headless: tag reading, title/artist normalization, candidate
//! scoring and the Spotify client all live here so the matcher can be measured
//! from the CLI long before any UI exists.

pub mod matcher;
pub mod normalize;
pub mod spotify;
pub mod tags;
pub mod watcher;

pub use matcher::{Candidate, MatchMethod, Score, ShorterVersionPolicy, Verdict};
pub use normalize::{MixKind, ParsedTitle};
pub use spotify::{SpotifyClient, SpotifyTrack};
pub use tags::LocalTrack;

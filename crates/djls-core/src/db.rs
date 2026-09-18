//! Local state — Phase 2.
//!
//! Two jobs: don't re-match a file that has already been resolved, and don't
//! push the same track to the same playlist twice. Both are enforced in the
//! schema rather than in caller logic, so a bug upstream cannot cause a
//! duplicate.
//!
//! Identity is deliberately not the file path. Rekordbox and Serato rewrite
//! tags on import and DJs move files between folders; keying on path alone
//! would re-process the whole library after either. `LocalTrack::identity_key`
//! survives both.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

use crate::matcher::{MatchOutcome, Verdict};
use crate::tags::LocalTrack;

pub const PLATFORM_SPOTIFY: &str = "spotify";
/// Matches are keyed per platform, so the same local file can hold a Spotify
/// match and an Apple Music one without either overwriting the other.
pub const PLATFORM_APPLE_MUSIC: &str = "apple_music";

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// What the database knew about a file before this run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackState {
    /// Never seen before.
    New,
    /// Same path, same identity — safe to reuse a stored match.
    Unchanged,
    /// Same path, but the tags changed enough to alter its identity. Re-match.
    Retagged,
    /// Seen before at a different path. Reuse the match; just update the path.
    Moved,
}

#[derive(Debug, Clone)]
pub struct TrackRecord {
    pub id: i64,
    pub state: TrackState,
}

impl TrackRecord {
    /// Whether a stored match may be reused without hitting the API again.
    pub fn can_reuse_match(&self) -> bool {
        matches!(self.state, TrackState::Unchanged | TrackState::Moved)
    }
}

#[derive(Debug, Clone)]
pub struct StoredMatch {
    pub platform_uri: Option<String>,
    pub platform_name: Option<String>,
    /// Comma-joined, as Spotify presents them.
    pub platform_artists: Option<String>,
    pub platform_duration_ms: Option<u64>,
    pub confidence: f32,
    pub method: String,
    pub verdict: Verdict,
    pub reason: String,
}

/// A track nothing could be found for — the future AcoustID queue.
#[derive(Debug, Clone)]
pub struct MissedTrack {
    pub file_path: PathBuf,
    pub artist: String,
    pub title: String,
    pub reason: String,
    pub missed_at: i64,
}

pub struct Database {
    conn: Connection,
}

impl Database {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening database at {}", path.display()))?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        // Foreign keys are off by default in SQLite and must be enabled per
        // connection, or the ON DELETE CASCADE below silently does nothing.
        conn.execute_batch(
            r#"
            PRAGMA foreign_keys = ON;
            PRAGMA journal_mode = WAL;

            CREATE TABLE IF NOT EXISTS tracks (
                id            INTEGER PRIMARY KEY,
                file_path     TEXT    NOT NULL UNIQUE,
                identity_key  TEXT    NOT NULL,
                artist        TEXT    NOT NULL,
                title         TEXT    NOT NULL,
                base_title    TEXT    NOT NULL,
                version       TEXT,
                isrc          TEXT,
                duration_ms   INTEGER NOT NULL,
                file_size     INTEGER NOT NULL,
                first_seen_at INTEGER NOT NULL,
                last_seen_at  INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_tracks_identity ON tracks(identity_key);

            CREATE TABLE IF NOT EXISTS matches (
                id                INTEGER PRIMARY KEY,
                track_id          INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
                platform          TEXT    NOT NULL,
                platform_track_id TEXT,
                platform_uri      TEXT,
                confidence        REAL    NOT NULL,
                method            TEXT    NOT NULL,
                verdict           TEXT    NOT NULL,
                reason            TEXT,
                matched_at        INTEGER NOT NULL,
                UNIQUE(track_id, platform)
            );

            -- The unique constraint is the duplicate-push guard: pushing the
            -- same track to the same playlist twice is impossible by schema.
            CREATE TABLE IF NOT EXISTS sync_log (
                id           INTEGER PRIMARY KEY,
                track_id     INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
                platform     TEXT    NOT NULL,
                platform_uri TEXT    NOT NULL,
                playlist_id  TEXT    NOT NULL,
                synced_at    INTEGER NOT NULL,
                UNIQUE(track_id, playlist_id, platform)
            );
            "#,
        )
        .context("creating schema")?;

        // Additive migrations. A URI alone turned out to be too little to
        // reason about a cached match later — comparing recordings needs the
        // name, artists and duration too. `ALTER TABLE` has no IF NOT EXISTS,
        // so a duplicate-column error here just means the column already runs.
        for stmt in [
            "ALTER TABLE matches ADD COLUMN platform_name TEXT",
            "ALTER TABLE matches ADD COLUMN platform_artists TEXT",
            "ALTER TABLE matches ADD COLUMN platform_duration_ms INTEGER",
        ] {
            let _ = conn.execute(stmt, []);
        }

        Ok(Self { conn })
    }

    /// Default location: the platform's per-user data directory.
    pub fn default_path() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());

        let dir = if cfg!(target_os = "macos") {
            PathBuf::from(&home).join("Library/Application Support/dj-library-sync")
        } else if cfg!(target_os = "windows") {
            std::env::var("APPDATA")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from(&home))
                .join("dj-library-sync")
        } else {
            std::env::var("XDG_DATA_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from(&home).join(".local/share"))
                .join("dj-library-sync")
        };

        dir.join("library.db")
    }

    /// Record that a file was seen, and report what we already knew about it.
    pub fn upsert_track(&self, track: &LocalTrack) -> Result<TrackRecord> {
        let path = track.path.to_string_lossy().to_string();
        let identity = track.identity_key();
        let ts = now();

        // Seen at this exact path before?
        let existing: Option<(i64, String)> = self
            .conn
            .query_row(
                "SELECT id, identity_key FROM tracks WHERE file_path = ?1",
                params![path],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;

        if let Some((id, stored_identity)) = existing {
            let state = if stored_identity == identity {
                TrackState::Unchanged
            } else {
                TrackState::Retagged
            };
            self.conn.execute(
                "UPDATE tracks SET identity_key = ?1, artist = ?2, title = ?3, base_title = ?4,
                                   version = ?5, isrc = ?6, duration_ms = ?7, file_size = ?8,
                                   last_seen_at = ?9
                 WHERE id = ?10",
                params![
                    identity,
                    track.artist,
                    track.title,
                    track.parsed.base,
                    track.parsed.descriptor_label(),
                    track.isrc,
                    track.duration_ms as i64,
                    track.file_size as i64,
                    ts,
                    id
                ],
            )?;
            return Ok(TrackRecord { id, state });
        }

        // Same recording at a new path — the file moved, so keep its history.
        let moved: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM tracks WHERE identity_key = ?1 LIMIT 1",
                params![identity],
                |row| row.get(0),
            )
            .optional()?;

        if let Some(id) = moved {
            self.conn.execute(
                "UPDATE tracks SET file_path = ?1, last_seen_at = ?2 WHERE id = ?3",
                params![path, ts, id],
            )?;
            return Ok(TrackRecord {
                id,
                state: TrackState::Moved,
            });
        }

        self.conn.execute(
            "INSERT INTO tracks (file_path, identity_key, artist, title, base_title, version,
                                 isrc, duration_ms, file_size, first_seen_at, last_seen_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)",
            params![
                path,
                identity,
                track.artist,
                track.title,
                track.parsed.base,
                track.parsed.descriptor_label(),
                track.isrc,
                track.duration_ms as i64,
                track.file_size as i64,
                ts
            ],
        )?;

        Ok(TrackRecord {
            id: self.conn.last_insert_rowid(),
            state: TrackState::New,
        })
    }

    pub fn stored_match(&self, track_id: i64, platform: &str) -> Result<Option<StoredMatch>> {
        let row = self
            .conn
            .query_row(
                "SELECT platform_uri, confidence, method, verdict, reason,
                        platform_name, platform_artists, platform_duration_ms
                 FROM matches WHERE track_id = ?1 AND platform = ?2",
                params![track_id, platform],
                |row| {
                    Ok(StoredMatch {
                        platform_uri: row.get(0)?,
                        confidence: row.get::<_, f64>(1)? as f32,
                        method: row.get(2)?,
                        verdict: match row.get::<_, String>(3)?.as_str() {
                            "auto" => Verdict::Auto,
                            "review" => Verdict::Review,
                            _ => Verdict::NoMatch,
                        },
                        reason: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
                        platform_name: row.get(5)?,
                        platform_artists: row.get(6)?,
                        platform_duration_ms: row.get::<_, Option<i64>>(7)?.map(|d| d as u64),
                    })
                },
            )
            .optional()?;

        Ok(row)
    }

    pub fn record_match(
        &self,
        track_id: i64,
        platform: &str,
        outcome: &MatchOutcome,
    ) -> Result<()> {
        let best = outcome.best();
        self.conn.execute(
            "INSERT INTO matches (track_id, platform, platform_track_id, platform_uri,
                                  confidence, method, verdict, reason, matched_at,
                                  platform_name, platform_artists, platform_duration_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT(track_id, platform) DO UPDATE SET
                platform_track_id    = excluded.platform_track_id,
                platform_uri         = excluded.platform_uri,
                confidence           = excluded.confidence,
                method               = excluded.method,
                verdict              = excluded.verdict,
                reason               = excluded.reason,
                matched_at           = excluded.matched_at,
                platform_name        = excluded.platform_name,
                platform_artists     = excluded.platform_artists,
                platform_duration_ms = excluded.platform_duration_ms",
            params![
                track_id,
                platform,
                best.map(|c| c.track.id.clone()),
                best.map(|c| c.track.uri.clone()),
                outcome.confidence() as f64,
                outcome.method.as_str(),
                outcome.verdict.as_str(),
                outcome.reason,
                now(),
                best.map(|c| c.track.name.clone()),
                best.map(|c| c.track.artist_field()),
                best.map(|c| c.track.duration_ms as i64)
            ],
        )?;
        Ok(())
    }

    pub fn already_synced(&self, track_id: i64, playlist_id: &str, platform: &str) -> Result<bool> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM sync_log
             WHERE track_id = ?1 AND playlist_id = ?2 AND platform = ?3",
            params![track_id, playlist_id, platform],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Returns false if this track was already logged for this playlist.
    pub fn record_sync(
        &self,
        track_id: i64,
        platform: &str,
        platform_uri: &str,
        playlist_id: &str,
    ) -> Result<bool> {
        let changed = self.conn.execute(
            "INSERT OR IGNORE INTO sync_log (track_id, platform, platform_uri, playlist_id, synced_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![track_id, platform, platform_uri, playlist_id, now()],
        )?;
        Ok(changed > 0)
    }

    /// Everything that resolved to nothing — the AcoustID queue.
    pub fn missed_tracks(&self, platform: &str) -> Result<Vec<MissedTrack>> {
        let mut stmt = self.conn.prepare(
            "SELECT t.file_path, t.artist, t.title, m.reason, m.matched_at
             FROM matches m JOIN tracks t ON t.id = m.track_id
             WHERE m.platform = ?1 AND m.verdict = 'no_match'
             ORDER BY m.matched_at DESC",
        )?;

        let rows = stmt
            .query_map(params![platform], |row| {
                Ok(MissedTrack {
                    file_path: PathBuf::from(row.get::<_, String>(0)?),
                    artist: row.get(1)?,
                    title: row.get(2)?,
                    reason: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                    missed_at: row.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        Ok(rows)
    }

    pub fn counts(&self) -> Result<(i64, i64, i64)> {
        let tracks = self
            .conn
            .query_row("SELECT COUNT(*) FROM tracks", [], |r| r.get(0))?;
        let matches = self
            .conn
            .query_row("SELECT COUNT(*) FROM matches", [], |r| r.get(0))?;
        let synced = self
            .conn
            .query_row("SELECT COUNT(*) FROM sync_log", [], |r| r.get(0))?;
        Ok((tracks, matches, synced))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matcher::{Candidate, MatchMethod, Score};
    use crate::normalize::parse_title;
    use crate::spotify::SpotifyTrack;

    fn track(
        path: &str,
        artist: &str,
        title: &str,
        duration_ms: u64,
        isrc: Option<&str>,
    ) -> LocalTrack {
        LocalTrack {
            path: PathBuf::from(path),
            file_name: path.rsplit('/').next().unwrap().to_string(),
            artist: artist.into(),
            title: title.into(),
            album: None,
            isrc: isrc.map(|s| s.to_string()),
            bpm: None,
            musical_key: None,
            duration_ms,
            file_size: 1000,
            parsed: parse_title(title),
        }
    }

    fn outcome(verdict: Verdict, uri: &str) -> MatchOutcome {
        MatchOutcome {
            verdict,
            method: MatchMethod::Exact,
            candidates: vec![Candidate {
                track: SpotifyTrack {
                    id: "id1".into(),
                    uri: uri.into(),
                    name: "Song".into(),
                    artists: vec!["A".into()],
                    album: "Album".into(),
                    duration_ms: 400_000,
                    isrc: None,
                    url: None,
                    popularity: None,
                },
                score: Score {
                    total: 0.95,
                    artist: 1.0,
                    title: 1.0,
                    duration: 1.0,
                    mix: 1.0,
                    duration_delta_ms: 0,
                    notes: vec![],
                },
            }],
            reason: "exact match".into(),
        }
    }

    #[test]
    fn a_file_is_new_once_then_unchanged() {
        let db = Database::open_in_memory().unwrap();
        let t = track("/m/a.aiff", "Kolsch", "Grey (Extended Mix)", 400_000, None);

        assert_eq!(db.upsert_track(&t).unwrap().state, TrackState::New);
        let second = db.upsert_track(&t).unwrap();
        assert_eq!(second.state, TrackState::Unchanged);
        assert!(second.can_reuse_match());
    }

    #[test]
    fn a_moved_file_keeps_its_history() {
        // Same recording, new folder. Re-matching it would waste an API call
        // and lose its sync history.
        let db = Database::open_in_memory().unwrap();
        let before = track(
            "/m/a.aiff",
            "Kolsch",
            "Grey (Extended Mix)",
            400_000,
            Some("X1"),
        );
        let after = track(
            "/other/a.aiff",
            "Kolsch",
            "Grey (Extended Mix)",
            400_000,
            Some("X1"),
        );

        let first = db.upsert_track(&before).unwrap();
        let moved = db.upsert_track(&after).unwrap();

        assert_eq!(moved.state, TrackState::Moved);
        assert_eq!(moved.id, first.id, "must be the same row, not a new one");
        assert!(moved.can_reuse_match());
        assert_eq!(db.counts().unwrap().0, 1);
    }

    #[test]
    fn a_retagged_file_is_matched_again() {
        let db = Database::open_in_memory().unwrap();
        let before = track("/m/a.aiff", "Kolsch", "Grey (Extended Mix)", 400_000, None);
        // A tag editor rewrote the title to a different version.
        let after = track("/m/a.aiff", "Kolsch", "Grey (Radio Edit)", 400_000, None);

        db.upsert_track(&before).unwrap();
        let changed = db.upsert_track(&after).unwrap();

        assert_eq!(changed.state, TrackState::Retagged);
        assert!(
            !changed.can_reuse_match(),
            "changed tags must force a re-match"
        );
    }

    #[test]
    fn matches_round_trip_and_overwrite_in_place() {
        let db = Database::open_in_memory().unwrap();
        let t = track("/m/a.aiff", "Kolsch", "Grey", 400_000, None);
        let id = db.upsert_track(&t).unwrap().id;

        db.record_match(
            id,
            PLATFORM_SPOTIFY,
            &outcome(Verdict::Review, "spotify:track:1"),
        )
        .unwrap();
        db.record_match(
            id,
            PLATFORM_SPOTIFY,
            &outcome(Verdict::Auto, "spotify:track:2"),
        )
        .unwrap();

        let stored = db.stored_match(id, PLATFORM_SPOTIFY).unwrap().unwrap();
        assert_eq!(stored.verdict, Verdict::Auto);
        assert_eq!(stored.platform_uri.as_deref(), Some("spotify:track:2"));
        // Enough metadata to compare recordings later, not just an opaque URI.
        assert_eq!(stored.platform_name.as_deref(), Some("Song"));
        assert_eq!(stored.platform_artists.as_deref(), Some("A"));
        assert_eq!(stored.platform_duration_ms, Some(400_000));
        assert_eq!(
            db.counts().unwrap().1,
            1,
            "re-matching must update, not duplicate"
        );
    }

    #[test]
    fn the_same_track_cannot_be_pushed_to_one_playlist_twice() {
        let db = Database::open_in_memory().unwrap();
        let t = track("/m/a.aiff", "Kolsch", "Grey", 400_000, None);
        let id = db.upsert_track(&t).unwrap().id;

        assert!(!db.already_synced(id, "pl1", PLATFORM_SPOTIFY).unwrap());
        assert!(db
            .record_sync(id, PLATFORM_SPOTIFY, "spotify:track:1", "pl1")
            .unwrap());
        assert!(db.already_synced(id, "pl1", PLATFORM_SPOTIFY).unwrap());

        // Second attempt is refused by the unique constraint, not by caller logic.
        assert!(!db
            .record_sync(id, PLATFORM_SPOTIFY, "spotify:track:1", "pl1")
            .unwrap());
        assert_eq!(db.counts().unwrap().2, 1);

        // A different playlist is a legitimate second push.
        assert!(db
            .record_sync(id, PLATFORM_SPOTIFY, "spotify:track:1", "pl2")
            .unwrap());
        assert_eq!(db.counts().unwrap().2, 2);
    }

    #[test]
    fn no_match_tracks_collect_into_a_queue() {
        let db = Database::open_in_memory().unwrap();

        let found = track("/m/a.aiff", "Kolsch", "Grey", 400_000, None);
        let lost = track("/m/b.aiff", "White Label", "Untitled B2", 380_000, None);
        let found_id = db.upsert_track(&found).unwrap().id;
        let lost_id = db.upsert_track(&lost).unwrap().id;

        db.record_match(
            found_id,
            PLATFORM_SPOTIFY,
            &outcome(Verdict::Auto, "spotify:track:1"),
        )
        .unwrap();
        db.record_match(
            lost_id,
            PLATFORM_SPOTIFY,
            &outcome(Verdict::NoMatch, "spotify:track:2"),
        )
        .unwrap();

        let missed = db.missed_tracks(PLATFORM_SPOTIFY).unwrap();
        assert_eq!(missed.len(), 1);
        assert_eq!(missed[0].title, "Untitled B2");
    }

    #[test]
    fn deleting_a_track_takes_its_matches_with_it() {
        // Guards the foreign_keys pragma, which SQLite leaves off by default.
        let db = Database::open_in_memory().unwrap();
        let t = track("/m/a.aiff", "Kolsch", "Grey", 400_000, None);
        let id = db.upsert_track(&t).unwrap().id;
        db.record_match(
            id,
            PLATFORM_SPOTIFY,
            &outcome(Verdict::Auto, "spotify:track:1"),
        )
        .unwrap();

        db.conn
            .execute("DELETE FROM tracks WHERE id = ?1", params![id])
            .unwrap();
        assert_eq!(db.counts().unwrap().1, 0, "orphaned match rows left behind");
    }
}

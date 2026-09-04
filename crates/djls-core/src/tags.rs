//! Reading identity out of local audio files.
//!
//! Beatport ships MP3, AIFF, WAV and FLAC; AIFF in particular is common in DJ
//! libraries, which is why this is built on `lofty` rather than an MP3-only
//! parser.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use lofty::prelude::*;
use lofty::probe::Probe;
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::normalize::{normalize, parse_title, split_artists, ParsedTitle};

pub const AUDIO_EXTENSIONS: &[&str] = &[
    "mp3", "aiff", "aif", "aifc", "wav", "flac", "m4a", "aac", "ogg", "opus", "wv",
];

/// Files a downloader is still writing. Never read these.
pub const TEMP_EXTENSIONS: &[&str] = &["part", "crdownload", "download", "tmp", "partial"];

pub fn is_audio_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| AUDIO_EXTENSIONS.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false)
}

pub fn is_temp_file(path: &Path) -> bool {
    let hidden = path
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.starts_with('.') || n.starts_with("~"))
        .unwrap_or(false);

    let temp_ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| TEMP_EXTENSIONS.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false);

    hidden || temp_ext
}

/// A track as it exists on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalTrack {
    pub path: PathBuf,
    pub file_name: String,
    pub artist: String,
    pub title: String,
    pub album: Option<String>,
    pub isrc: Option<String>,
    pub bpm: Option<String>,
    pub musical_key: Option<String>,
    pub duration_ms: u64,
    pub file_size: u64,
    /// Title split into base name + version information.
    pub parsed: ParsedTitle,
}

impl LocalTrack {
    /// Read tags from a single file. Falls back to the filename when the file
    /// is untagged, since a white label with no tags still deserves an attempt.
    pub fn read(path: &Path) -> Result<Self> {
        let tagged = Probe::open(path)
            .with_context(|| format!("opening {}", path.display()))?
            .read()
            .with_context(|| format!("reading tags from {}", path.display()))?;

        let duration_ms = tagged.properties().duration().as_millis() as u64;
        let tag = tagged.primary_tag().or_else(|| tagged.first_tag());

        let artist = tag
            .and_then(|t| t.artist())
            .map(|c| c.to_string())
            .filter(|s| !s.trim().is_empty());
        let title = tag
            .and_then(|t| t.title())
            .map(|c| c.to_string())
            .filter(|s| !s.trim().is_empty());
        let album = tag
            .and_then(|t| t.album())
            .map(|c| c.to_string())
            .filter(|s| !s.trim().is_empty());

        let isrc = tag
            .and_then(|t| t.get_string(&ItemKey::Isrc))
            .map(|s| s.trim().to_uppercase())
            .filter(|s| !s.is_empty());
        let bpm = tag
            .and_then(|t| {
                t.get_string(&ItemKey::IntegerBpm)
                    .or_else(|| t.get_string(&ItemKey::Bpm))
            })
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let musical_key = tag
            .and_then(|t| t.get_string(&ItemKey::InitialKey))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();

        let (fallback_artist, fallback_title) = artist_title_from_filename(&file_name);
        let artist = artist.unwrap_or(fallback_artist);
        let title = title.unwrap_or(fallback_title);

        let file_size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);

        Ok(LocalTrack {
            parsed: parse_title(&title),
            path: path.to_path_buf(),
            file_name,
            artist,
            title,
            album,
            isrc,
            bpm,
            musical_key,
            duration_ms,
            file_size,
        })
    }

    pub fn primary_artist(&self) -> String {
        split_artists(&self.artist)
            .into_iter()
            .next()
            .unwrap_or_else(|| self.artist.clone())
    }

    /// All credited names, including guests lifted out of the title, as one
    /// string for token-set comparison.
    pub fn artist_field_for_matching(&self) -> String {
        if self.parsed.featured.is_empty() {
            self.artist.clone()
        } else {
            format!("{} {}", self.artist, self.parsed.featured.join(" "))
        }
    }

    /// Stable identity that survives a re-tag by Rekordbox/Serato or a move to
    /// a different folder. Prefer ISRC; otherwise artist + base title + a
    /// coarse duration bucket.
    pub fn identity_key(&self) -> String {
        if let Some(isrc) = &self.isrc {
            return format!("isrc:{isrc}");
        }
        let bucket = self.duration_ms / 5_000;
        format!(
            "meta:{}|{}|{}|{bucket}",
            normalize(&self.artist),
            self.parsed.base_norm,
            self.parsed.descriptor_label()
        )
    }

    pub fn duration_display(&self) -> String {
        let total = self.duration_ms / 1000;
        format!("{}:{:02}", total / 60, total % 60)
    }
}

/// Best-effort "Artist - Title.mp3" split for untagged files.
fn artist_title_from_filename(file_name: &str) -> (String, String) {
    let stem = file_name
        .rsplit_once('.')
        .map(|(stem, _)| stem)
        .unwrap_or(file_name)
        .trim();

    match stem.split_once(" - ") {
        Some((artist, title)) => (artist.trim().to_string(), title.trim().to_string()),
        None => (String::new(), stem.to_string()),
    }
}

/// Walk a folder and read every audio file found.
///
/// Returns successfully-read tracks plus the files that could not be parsed, so
/// the caller can report both rather than silently dropping failures.
pub fn scan_folder(root: &Path, recursive: bool) -> (Vec<LocalTrack>, Vec<(PathBuf, String)>) {
    let max_depth = if recursive { usize::MAX } else { 1 };
    let mut tracks = Vec::new();
    let mut failures = Vec::new();

    for entry in WalkDir::new(root)
        .max_depth(max_depth)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if !entry.file_type().is_file() || is_temp_file(path) || !is_audio_file(path) {
            continue;
        }
        match LocalTrack::read(path) {
            Ok(track) => tracks.push(track),
            Err(err) => failures.push((path.to_path_buf(), format!("{err:#}"))),
        }
    }

    tracks.sort_by(|a, b| a.file_name.cmp(&b.file_name));
    (tracks, failures)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_downloads_are_ignored() {
        assert!(is_temp_file(Path::new("/x/track.mp3.crdownload")));
        assert!(is_temp_file(Path::new("/x/.hidden.mp3")));
        assert!(!is_temp_file(Path::new("/x/track.mp3")));
    }

    #[test]
    fn filename_fallback_splits_on_dash() {
        let (a, t) = artist_title_from_filename("Kolsch - Grey (Extended Mix).aiff");
        assert_eq!(a, "Kolsch");
        assert_eq!(t, "Grey (Extended Mix)");
    }
}

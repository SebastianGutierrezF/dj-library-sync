//! Candidate scoring.
//!
//! Four signals, weighted: artist, base title, duration, and mix agreement.
//! Duration is what separates an extended mix from a radio edit when the
//! strings are identical, and mix agreement is what stops a remix from being
//! confidently matched to the original.

use serde::{Deserialize, Serialize};

use crate::normalize::{normalize, parse_title, text_similarity, token_set_similarity, MixKind, ParsedTitle};
use crate::spotify::SpotifyTrack;
use crate::tags::LocalTrack;

/// How a match was arrived at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MatchMethod {
    Isrc,
    Exact,
    Fuzzy,
    None,
}

impl MatchMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            MatchMethod::Isrc => "isrc",
            MatchMethod::Exact => "exact",
            MatchMethod::Fuzzy => "fuzzy",
            MatchMethod::None => "none",
        }
    }
}

/// What should happen to this track without further input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Verdict {
    /// Push it. High confidence, no human needed.
    Auto,
    /// Ambiguous — park it in the "needs attention" bucket. Never block the batch on it.
    Review,
    /// Nothing plausible on the platform. This is the future AcoustID queue.
    NoMatch,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Auto => "auto",
            Verdict::Review => "review",
            Verdict::NoMatch => "no_match",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Score {
    pub total: f32,
    pub artist: f32,
    pub title: f32,
    pub duration: f32,
    pub mix: f32,
    pub duration_delta_ms: i64,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candidate {
    pub track: SpotifyTrack,
    pub score: Score,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatchOutcome {
    pub verdict: Verdict,
    pub method: MatchMethod,
    /// Best first, at most three.
    pub candidates: Vec<Candidate>,
    pub reason: String,
}

impl MatchOutcome {
    pub fn best(&self) -> Option<&Candidate> {
        self.candidates.first()
    }

    pub fn confidence(&self) -> f32 {
        self.best().map(|c| c.score.total).unwrap_or(0.0)
    }
}

/// Thresholds, kept in one place so they can be tuned against a real library
/// rather than guessed at.
#[derive(Debug, Clone, Copy)]
pub struct Thresholds {
    pub auto_total: f32,
    pub auto_artist: f32,
    pub auto_title: f32,
    pub auto_duration: f32,
    pub review_total: f32,
    /// If the runner-up is this close, the top match is not safe to auto-push
    /// — unless the runner-up turns out to be [`same_recording`] as the top
    /// pick, in which case there is nothing to review: either one is correct.
    pub ambiguity_margin: f32,
    /// An ISRC hit whose duration differs by more than this is treated as a
    /// mistagged ISRC and sent to review instead of pushed.
    pub isrc_duration_sanity_ms: i64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            auto_total: 0.88,
            auto_artist: 0.80,
            auto_title: 0.85,
            auto_duration: 0.85,
            review_total: 0.62,
            ambiguity_margin: 0.05,
            isrc_duration_sanity_ms: 30_000,
        }
    }
}

fn duration_score(local_ms: u64, remote_ms: u64) -> (f32, i64) {
    let delta = local_ms as i64 - remote_ms as i64;
    let abs = delta.abs();
    let score = if local_ms == 0 || remote_ms == 0 {
        // Unknown duration is not evidence either way.
        0.6
    } else if abs <= 2_000 {
        1.0
    } else if abs <= 5_000 {
        0.88
    } else if abs <= 10_000 {
        0.65
    } else if abs <= 30_000 {
        0.35
    } else {
        0.05
    };
    (score, delta)
}

/// Compare version information. Returns `None` for a hard reject — a different
/// remixer means a different recording, and no amount of string similarity
/// should override that.
fn mix_score(local: &ParsedTitle, remote: &ParsedTitle, notes: &mut Vec<String>) -> Option<f32> {
    match (&local.remixer_norm, &remote.remixer_norm) {
        (Some(a), Some(b)) => {
            let sim = text_similarity(a, b);
            if sim < 0.80 {
                notes.push(format!("different remixer: '{a}' vs '{b}'"));
                return None;
            }
            return Some(1.0);
        }
        (Some(a), None) if remote.kind.is_derivative() => {
            notes.push(format!("candidate is a derivative but does not name '{a}'"));
            return Some(0.35);
        }
        (Some(a), None) => {
            notes.push(format!("local is a '{a}' version, candidate is not a remix"));
            return Some(0.10);
        }
        (None, Some(b)) => {
            notes.push(format!("candidate is a '{b}' remix, local is not"));
            return Some(0.10);
        }
        (None, None) => {}
    }

    if local.kind == remote.kind {
        return Some(1.0);
    }

    let score = match (local.kind, remote.kind) {
        (a, b) if a.is_long_form() && b.is_short_form() => {
            notes.push("local is long-form, candidate is a radio edit".to_string());
            0.10
        }
        (a, b) if a.is_short_form() && b.is_long_form() => {
            notes.push("local is a radio edit, candidate is long-form".to_string());
            0.10
        }
        (a, b) if a.is_long_form() && matches!(b, MixKind::Original | MixKind::None) => {
            notes.push("candidate is unlabelled — duration decides".to_string());
            0.50
        }
        (a, b) if matches!(a, MixKind::Original | MixKind::None) && b.is_long_form() => {
            notes.push("candidate is labelled extended, local is not".to_string());
            0.50
        }
        (MixKind::Original, MixKind::None) | (MixKind::None, MixKind::Original) => 0.95,
        (a, b) if a.is_derivative() != b.is_derivative() => {
            notes.push("one side is a derivative version".to_string());
            0.20
        }
        _ => 0.60,
    };

    Some(score)
}

/// True when two candidates are almost certainly the same underlying
/// recording — the same single indexed under two albums, a compilation
/// re-release, that kind of catalogue duplication — rather than two
/// different versions genuinely competing for the slot. Same near-enough
/// duration plus the same name is enough: a real version difference (radio
/// vs extended, a different remix) always shows up as one or the other.
fn same_recording(a: &Candidate, b: &Candidate) -> bool {
    let dur_delta = (a.track.duration_ms as i64 - b.track.duration_ms as i64).abs();
    dur_delta <= 3_000 && normalize(&a.track.name) == normalize(&b.track.name)
}

/// Score one candidate against one local file.
pub fn score_candidate(local: &LocalTrack, candidate: &SpotifyTrack) -> Score {
    let mut notes = Vec::new();

    let remote_parsed = parse_title(&candidate.name);

    let artist = token_set_similarity(
        &local.artist_field_for_matching(),
        &candidate.artist_field(),
    );
    let title = text_similarity(&local.parsed.base, &remote_parsed.base);
    let (duration, duration_delta_ms) = duration_score(local.duration_ms, candidate.duration_ms);

    let mix = mix_score(&local.parsed, &remote_parsed, &mut notes);

    let mut total = match mix {
        None => 0.0,
        Some(mix) => 0.32 * artist + 0.30 * title + 0.24 * duration + 0.14 * mix,
    };

    // Gates: a strong title cannot rescue the wrong artist, and vice versa.
    if artist < 0.55 {
        total *= 0.5;
        notes.push("artist mismatch".to_string());
    }
    if title < 0.62 {
        total *= 0.5;
        notes.push("title mismatch".to_string());
    }

    if duration_delta_ms.abs() > 60_000 {
        notes.push(format!(
            "duration differs by {}s",
            duration_delta_ms.abs() / 1000
        ));
    }

    Score {
        total: total.clamp(0.0, 1.0),
        artist,
        title,
        duration,
        mix: mix.unwrap_or(0.0),
        duration_delta_ms,
        notes,
    }
}

/// Decide what to do with a track given the candidates the API returned.
///
/// `isrc_hits` come from an `isrc:` search and are trusted; `text_hits` are
/// scored.
pub fn evaluate(
    local: &LocalTrack,
    isrc_hits: &[SpotifyTrack],
    text_hits: &[SpotifyTrack],
    thresholds: Thresholds,
) -> MatchOutcome {
    if let Some(local_isrc) = &local.isrc {
        let exact = isrc_hits
            .iter()
            .find(|t| t.isrc.as_deref() == Some(local_isrc.as_str()))
            .or_else(|| isrc_hits.first());

        if let Some(track) = exact {
            let mut score = score_candidate(local, track);
            let delta = score.duration_delta_ms.abs();
            score.total = 1.0;

            // Guard against labels reusing or mistagging an ISRC.
            if delta > thresholds.isrc_duration_sanity_ms {
                score.total = 0.75;
                score.notes.push(format!(
                    "ISRC matched but duration differs by {}s — verify",
                    delta / 1000
                ));
                return MatchOutcome {
                    verdict: Verdict::Review,
                    method: MatchMethod::Isrc,
                    candidates: vec![Candidate { track: track.clone(), score }],
                    reason: "isrc hit with implausible duration".to_string(),
                };
            }

            return MatchOutcome {
                verdict: Verdict::Auto,
                method: MatchMethod::Isrc,
                candidates: vec![Candidate { track: track.clone(), score }],
                reason: "exact isrc".to_string(),
            };
        }
    }

    let mut scored: Vec<Candidate> = text_hits
        .iter()
        .map(|track| Candidate {
            score: score_candidate(local, track),
            track: track.clone(),
        })
        .collect();

    scored.sort_by(|a, b| {
        b.score
            .total
            .partial_cmp(&a.score.total)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    scored.truncate(3);

    let Some(best) = scored.first().cloned() else {
        return MatchOutcome {
            verdict: Verdict::NoMatch,
            method: MatchMethod::None,
            candidates: Vec::new(),
            reason: "no candidates returned".to_string(),
        };
    };

    let method = if best.score.artist >= 0.95 && best.score.title >= 0.98 {
        MatchMethod::Exact
    } else {
        MatchMethod::Fuzzy
    };

    let ambiguous = scored
        .get(1)
        .map(|runner_up| {
            best.score.total - runner_up.score.total < thresholds.ambiguity_margin
                && !same_recording(&best, runner_up)
        })
        .unwrap_or(false);

    let clears_auto = best.score.total >= thresholds.auto_total
        && best.score.artist >= thresholds.auto_artist
        && best.score.title >= thresholds.auto_title
        && best.score.duration >= thresholds.auto_duration;

    let (verdict, reason) = if clears_auto && ambiguous {
        (
            Verdict::Review,
            "two candidates within the ambiguity margin".to_string(),
        )
    } else if clears_auto {
        (Verdict::Auto, format!("{} match", method.as_str()))
    } else if best.score.total >= thresholds.review_total {
        (
            Verdict::Review,
            best.score
                .notes
                .first()
                .cloned()
                .unwrap_or_else(|| "below auto threshold".to_string()),
        )
    } else {
        (
            Verdict::NoMatch,
            best.score
                .notes
                .first()
                .cloned()
                .unwrap_or_else(|| "no plausible candidate".to_string()),
        )
    };

    MatchOutcome {
        verdict,
        method: if verdict == Verdict::NoMatch { MatchMethod::None } else { method },
        candidates: scored,
        reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::parse_title;
    use std::path::PathBuf;

    fn local(artist: &str, title: &str, duration_ms: u64, isrc: Option<&str>) -> LocalTrack {
        LocalTrack {
            path: PathBuf::from("/tmp/x.aiff"),
            file_name: "x.aiff".into(),
            artist: artist.into(),
            title: title.into(),
            album: None,
            isrc: isrc.map(|s| s.to_string()),
            bpm: None,
            musical_key: None,
            duration_ms,
            file_size: 0,
            parsed: parse_title(title),
        }
    }

    fn remote(artist: &str, title: &str, duration_ms: u64) -> SpotifyTrack {
        SpotifyTrack {
            id: format!("id-{title}"),
            uri: "spotify:track:x".into(),
            name: title.into(),
            artists: vec![artist.into()],
            album: "Album".into(),
            duration_ms,
            isrc: None,
            url: None,
            popularity: None,
        }
    }

    #[test]
    fn radio_edit_does_not_win_against_an_extended_mix() {
        let track = local("Kolsch", "Grey (Extended Mix)", 6 * 60 * 1000, None);
        let radio = remote("Kolsch", "Grey (Radio Edit)", 3 * 60 * 1000);
        let extended = remote("Kolsch", "Grey (Extended Mix)", 6 * 60 * 1000 + 1500);

        let out = evaluate(&track, &[], &[radio, extended], Thresholds::default());
        assert_eq!(out.verdict, Verdict::Auto);
        assert_eq!(out.best().unwrap().track.name, "Grey (Extended Mix)");
    }

    #[test]
    fn a_different_remixer_is_rejected_outright() {
        let track = local("Kolsch", "Grey (Adam Beyer Remix)", 400_000, None);
        let wrong = remote("Kolsch", "Grey (Charlotte de Witte Remix)", 400_000);

        let out = evaluate(&track, &[], &[wrong], Thresholds::default());
        assert_eq!(out.verdict, Verdict::NoMatch);
    }

    #[test]
    fn unlabelled_candidate_with_matching_duration_is_the_same_recording() {
        // Spotify often lists the extended cut with no descriptor at all. The
        // missing label costs some mix score, but a duration inside 2s settles it.
        let track = local("Kolsch", "Grey (Extended Mix)", 400_000, None);
        let bare = remote("Kolsch", "Grey", 400_500);

        let out = evaluate(&track, &[], &[bare], Thresholds::default());
        assert_eq!(out.verdict, Verdict::Auto);
    }

    #[test]
    fn unlabelled_candidate_at_radio_length_goes_to_review() {
        // Same title and artist, but only the short cut is on the platform.
        // This is the case duration scoring exists to catch.
        let track = local("Kolsch", "Grey (Extended Mix)", 400_000, None);
        let short = remote("Kolsch", "Grey", 200_000);

        let out = evaluate(&track, &[], &[short], Thresholds::default());
        assert_eq!(out.verdict, Verdict::Review);
    }

    #[test]
    fn isrc_wins_immediately() {
        let track = local("Kolsch", "Grey (Extended Mix)", 400_000, Some("DEUM71900123"));
        let mut hit = remote("Kolsch", "Grey - Extended Mix", 401_000);
        hit.isrc = Some("DEUM71900123".into());

        let out = evaluate(&track, &[hit], &[], Thresholds::default());
        assert_eq!(out.verdict, Verdict::Auto);
        assert_eq!(out.method, MatchMethod::Isrc);
        assert_eq!(out.confidence(), 1.0);
    }

    #[test]
    fn mistagged_isrc_with_wild_duration_goes_to_review() {
        let track = local("Kolsch", "Grey (Extended Mix)", 400_000, Some("DEUM71900123"));
        let mut hit = remote("Someone Else", "Different Song", 120_000);
        hit.isrc = Some("DEUM71900123".into());

        let out = evaluate(&track, &[hit], &[], Thresholds::default());
        assert_eq!(out.verdict, Verdict::Review);
    }

    #[test]
    fn near_duplicate_listings_auto_push_instead_of_review() {
        // The same single, indexed once on its own and once on a compilation.
        // Neither is wrong, so there is nothing to send to review.
        let track = local("Dennis Cruz", "Get Freaky", 360_000, None);
        let single = remote("Dennis Cruz", "Get Freaky", 360_000);
        let mut compilation = remote("Dennis Cruz", "Get Freaky", 360_200);
        compilation.album = "House Compilation Vol. 4".into();

        let out = evaluate(&track, &[], &[single, compilation], Thresholds::default());
        assert_eq!(out.verdict, Verdict::Auto);
    }

    #[test]
    fn genuinely_different_length_candidates_still_go_to_review_when_close() {
        // Same title, same artist, but the durations disagree by enough that
        // these are plausibly two different cuts — the ambiguity is real.
        let track = local("Dennis Cruz", "Get Freaky", 360_000, None);
        let a = remote("Dennis Cruz", "Get Freaky", 360_000);
        let b = remote("Dennis Cruz", "Get Freaky", 365_000);

        let out = evaluate(&track, &[], &[a, b], Thresholds::default());
        assert_eq!(out.verdict, Verdict::Review);
    }

    #[test]
    fn unrelated_track_is_no_match() {
        let track = local("Kolsch", "Grey (Extended Mix)", 400_000, None);
        let junk = remote("Taylor Swift", "Blank Space", 231_000);

        let out = evaluate(&track, &[], &[junk], Thresholds::default());
        assert_eq!(out.verdict, Verdict::NoMatch);
    }
}

//! String normalization and mix-descriptor parsing.
//!
//! The rule that drives this module: a mix descriptor is *parsed into a field*,
//! never stripped and thrown away. "Track (Extended Mix)" and "Track (Radio
//! Edit)" are different recordings, and a matcher that discards the descriptor
//! reports 100% confidence on the wrong one.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

/// What kind of version a mix descriptor names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MixKind {
    /// No descriptor present at all.
    None,
    Original,
    Extended,
    Radio,
    Club,
    Dub,
    Instrumental,
    Acapella,
    /// Edits, bootlegs, mashups, flips — someone else's cut of the record.
    Edit,
    Vip,
    Remix,
    Rework,
}

impl MixKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MixKind::None => "",
            MixKind::Original => "original",
            MixKind::Extended => "extended",
            MixKind::Radio => "radio",
            MixKind::Club => "club",
            MixKind::Dub => "dub",
            MixKind::Instrumental => "instrumental",
            MixKind::Acapella => "acapella",
            MixKind::Edit => "edit",
            MixKind::Vip => "vip",
            MixKind::Remix => "remix",
            MixKind::Rework => "rework",
        }
    }

    /// True when the descriptor implies a different performer/producer cut the
    /// record, not just a different length of the same recording.
    pub fn is_derivative(self) -> bool {
        matches!(
            self,
            MixKind::Remix | MixKind::Rework | MixKind::Edit | MixKind::Vip
        )
    }

    /// Descriptors that mean "the full-length club version".
    pub fn is_long_form(self) -> bool {
        matches!(self, MixKind::Extended | MixKind::Club)
    }

    /// Descriptors that mean "the shortened broadcast version".
    pub fn is_short_form(self) -> bool {
        matches!(self, MixKind::Radio)
    }
}

/// A title split into the part that identifies the song and the part that
/// identifies *which version of it* this is.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedTitle {
    pub raw: String,
    /// Title with mix descriptor and featured-artist clauses removed.
    pub base: String,
    pub base_norm: String,
    pub kind: MixKind,
    /// Who made the derivative version, when the descriptor names them.
    pub remixer: Option<String>,
    pub remixer_norm: Option<String>,
    /// Featured artists lifted out of the title, e.g. "(feat. Someone)".
    pub featured: Vec<String>,
    pub mix_raw: Option<String>,
}

impl ParsedTitle {
    pub fn descriptor_label(&self) -> String {
        match (&self.remixer, self.kind) {
            (Some(r), k) if k.is_derivative() => format!("{r} {}", k.as_str()),
            (_, MixKind::None) => "-".to_string(),
            (_, k) => k.as_str().to_string(),
        }
    }
}

/// Lowercase, strip diacritics and punctuation, collapse whitespace.
pub fn normalize(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut last_space = true;

    for ch in input.nfkd() {
        // Drop combining marks left behind by NFKD, so "Sébastien" == "Sebastien".
        if matches!(ch, '\u{0300}'..='\u{036f}') {
            continue;
        }
        let ch = match ch {
            '\u{2018}' | '\u{2019}' | '\u{02bc}' => '\'',
            '\u{201c}' | '\u{201d}' => '"',
            '\u{2013}' | '\u{2014}' | '\u{2212}' => '-',
            other => other,
        };

        if ch == '&' {
            if !last_space {
                out.push(' ');
            }
            out.push_str("and ");
            last_space = true;
            continue;
        }

        if ch.is_alphanumeric() {
            for lower in ch.to_lowercase() {
                out.push(lower);
            }
            last_space = false;
        } else if !last_space {
            out.push(' ');
            last_space = true;
        }
    }

    out.trim().to_string()
}

/// Tokens of a normalized string, minus connective words that carry no
/// identifying signal.
pub fn tokens(input: &str) -> BTreeSet<String> {
    const STOP: &[&str] = &[
        "feat",
        "featuring",
        "ft",
        "with",
        "and",
        "vs",
        "versus",
        "pres",
        "presents",
        "the",
        "a",
    ];
    normalize(input)
        .split_whitespace()
        .filter(|t| !STOP.contains(t))
        .map(|t| t.to_string())
        .collect()
}

/// Dice coefficient over token sets, with a containment boost so that
/// "Artist feat. Guest" still scores high against "Artist".
pub fn token_set_similarity(a: &str, b: &str) -> f32 {
    let ta = tokens(a);
    let tb = tokens(b);
    if ta.is_empty() || tb.is_empty() {
        return 0.0;
    }

    let shared = ta.intersection(&tb).count() as f32;
    let dice = (2.0 * shared) / (ta.len() + tb.len()) as f32;

    let contained = shared == ta.len().min(tb.len()) as f32;
    let containment = if contained { 0.92 } else { 0.0 };

    // Fuzzy floor catches spelling drift that token equality misses.
    let joined_a = ta.iter().cloned().collect::<Vec<_>>().join(" ");
    let joined_b = tb.iter().cloned().collect::<Vec<_>>().join(" ");
    let fuzzy = strsim::jaro_winkler(&joined_a, &joined_b) as f32;

    dice.max(containment).max(fuzzy * 0.95).clamp(0.0, 1.0)
}

/// Similarity of two already-normalized-ish strings, blending edit distance
/// with token overlap so word order does not dominate.
pub fn text_similarity(a: &str, b: &str) -> f32 {
    let na = normalize(a);
    let nb = normalize(b);
    if na.is_empty() || nb.is_empty() {
        return 0.0;
    }
    if na == nb {
        return 1.0;
    }
    let edit = strsim::jaro_winkler(&na, &nb) as f32;
    let set = token_set_similarity(a, b);
    (edit * 0.6 + set * 0.4).clamp(0.0, 1.0)
}

/// Split a tag's artist field into individual credited names. Used for search
/// queries and display; scoring uses token sets instead, because band names
/// like "Above & Beyond" must not be split apart during comparison.
pub fn split_artists(input: &str) -> Vec<String> {
    const SEPS: &[&str] = &[
        " feat. ",
        " feat ",
        " ft. ",
        " ft ",
        " featuring ",
        " with ",
        " vs. ",
        " vs ",
        " x ",
        " pres. ",
        " presents ",
        ",",
        ";",
        " & ",
        " and ",
        "/",
    ];

    let mut parts = vec![input.to_string()];
    for sep in SEPS {
        let mut next = Vec::new();
        for part in parts {
            let lower = part.to_lowercase();
            let mut start = 0usize;
            let mut pieces = Vec::new();
            while let Some(idx) = lower[start..].find(sep) {
                let abs = start + idx;
                pieces.push(part[start..abs].to_string());
                start = abs + sep.len();
            }
            pieces.push(part[start..].to_string());
            next.extend(pieces);
        }
        parts = next;
    }

    parts
        .into_iter()
        .map(|p| p.trim().trim_matches('-').trim().to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

/// Classify a bracketed segment. Returns `None` when the segment is part of the
/// song's actual name (e.g. "(Reprise)") rather than a version marker.
fn classify_segment(segment: &str) -> Option<(MixKind, Option<String>)> {
    let n = normalize(segment);
    if n.is_empty() {
        return None;
    }

    // Order matters: "radio edit" is Radio, not Edit; "extended remix" is Remix.
    let derivative: &[(&str, MixKind)] = &[
        ("remix", MixKind::Remix),
        ("rmx", MixKind::Remix),
        ("remake", MixKind::Rework),
        ("rework", MixKind::Rework),
        ("refix", MixKind::Rework),
        ("bootleg", MixKind::Edit),
        ("mashup", MixKind::Edit),
        ("flip", MixKind::Edit),
    ];

    for (needle, kind) in derivative {
        if let Some(idx) = n.find(needle) {
            let prefix = n[..idx].trim();
            let remixer = clean_remixer(prefix);
            return Some((*kind, remixer));
        }
    }

    if n.contains("radio") || n.contains("short edit") {
        return Some((MixKind::Radio, None));
    }
    if n.contains("vip") {
        let remixer = clean_remixer(n.split("vip").next().unwrap_or("").trim());
        return Some((MixKind::Vip, remixer));
    }
    if n.contains("extended") {
        return Some((MixKind::Extended, None));
    }
    if n.contains("instrumental") {
        return Some((MixKind::Instrumental, None));
    }
    if n.contains("acapella") || n.contains("a cappella") {
        return Some((MixKind::Acapella, None));
    }
    if n.contains("dub") {
        return Some((MixKind::Dub, None));
    }
    if n.contains("club") {
        return Some((MixKind::Club, None));
    }
    if n.contains("original") {
        return Some((MixKind::Original, None));
    }
    if n.contains("edit") {
        let remixer = clean_remixer(n.split("edit").next().unwrap_or("").trim());
        return Some((MixKind::Edit, remixer));
    }
    // A bare "(Mix)" / "(Version)" carries no information but is not part of
    // the song name either.
    if n == "mix" || n == "version" {
        return Some((MixKind::Original, None));
    }

    None
}

/// Turn "someone's" / "someone" into a comparable remixer name, dropping
/// leftover qualifiers like "extended".
fn clean_remixer(prefix: &str) -> Option<String> {
    const NOISE: &[&str] = &[
        "extended",
        "club",
        "dub",
        "official",
        "the",
        "vocal",
        "instrumental",
    ];
    let mut cleaned: Vec<&str> = prefix
        .split_whitespace()
        .filter(|w| !NOISE.contains(w))
        .collect();

    // `prefix` has already been normalized, so the possessive in "Beyer's" is
    // now a dangling "s" token rather than an apostrophe.
    if cleaned.len() > 1 && cleaned.last() == Some(&"s") {
        cleaned.pop();
    }

    let joined = cleaned.join(" ").trim().to_string();
    if joined.is_empty() {
        None
    } else {
        Some(joined)
    }
}

/// Remove a leading "feat." / "ft." / "featuring" / "with" marker, returning
/// just the guest names. Returns `None` when the segment is not a guest clause.
fn strip_feature_marker(segment: &str) -> Option<&str> {
    const MARKERS: &[&str] = &["feat.", "feat", "featuring", "ft.", "ft", "with"];
    let trimmed = segment.trim();
    for marker in MARKERS {
        if trimmed.len() > marker.len() {
            let (head, tail) = trimmed.split_at(marker.len());
            if head.eq_ignore_ascii_case(marker) && tail.starts_with(|c: char| c.is_whitespace()) {
                return Some(tail.trim());
            }
        }
    }
    None
}

/// Pull out `(...)` and `[...]` segments along with the text between them.
fn bracket_segments(title: &str) -> (Vec<String>, String) {
    let chars: Vec<char> = title.chars().collect();
    let mut segments = Vec::new();
    let mut remainder = String::new();
    let mut depth = 0usize;
    let mut current = String::new();

    for ch in chars {
        match ch {
            '(' | '[' => {
                if depth == 0 {
                    current.clear();
                } else {
                    current.push(ch);
                }
                depth += 1;
            }
            ')' | ']' => {
                if depth > 0 {
                    depth -= 1;
                    if depth == 0 {
                        segments.push(current.trim().to_string());
                        current.clear();
                    } else {
                        current.push(ch);
                    }
                } else {
                    remainder.push(ch);
                }
            }
            _ => {
                if depth == 0 {
                    remainder.push(ch);
                } else {
                    current.push(ch);
                }
            }
        }
    }

    if depth > 0 && !current.trim().is_empty() {
        // Unbalanced bracket — treat the tail as a segment anyway.
        segments.push(current.trim().to_string());
    }

    (segments, remainder)
}

/// Parse a track title into base name + version information.
pub fn parse_title(raw: &str) -> ParsedTitle {
    let (segments, remainder) = bracket_segments(raw);

    let mut kind = MixKind::None;
    let mut remixer = None;
    let mut mix_raw = None;
    let mut featured = Vec::new();
    let mut kept_segments = Vec::new();

    for segment in &segments {
        if let Some(guests) = strip_feature_marker(segment) {
            featured.extend(split_artists(guests));
            continue;
        }
        match classify_segment(segment) {
            Some((k, r)) => {
                kind = k;
                if r.is_some() {
                    remixer = r;
                }
                mix_raw = Some(segment.clone());
            }
            None => kept_segments.push(segment.clone()),
        }
    }

    let mut base = remainder.trim().to_string();

    // Beatport and some stores use "Title - Extended Mix" with no brackets.
    if kind == MixKind::None {
        if let Some(idx) = base.rfind(" - ") {
            let (head, tail) = base.split_at(idx);
            let tail = tail.trim_start_matches(" - ").trim();
            if let Some((k, r)) = classify_segment(tail) {
                kind = k;
                remixer = r;
                mix_raw = Some(tail.to_string());
                base = head.trim().to_string();
            }
        }
    }

    // Inline "feat." in the main title, e.g. "Title feat. Someone".
    for marker in [" feat. ", " feat ", " ft. ", " ft ", " featuring "] {
        let lower = base.to_lowercase();
        if let Some(idx) = lower.find(marker) {
            let (head, tail) = base.split_at(idx);
            featured.extend(split_artists(&tail[marker.len()..]));
            base = head.trim().to_string();
            break;
        }
    }

    for segment in kept_segments {
        base.push_str(&format!(" ({segment})"));
    }

    let base = base.trim().trim_end_matches('-').trim().to_string();
    let base = if base.is_empty() {
        raw.trim().to_string()
    } else {
        base
    };

    ParsedTitle {
        raw: raw.to_string(),
        base_norm: normalize(&base),
        base,
        kind,
        remixer_norm: remixer.as_deref().map(normalize),
        remixer,
        featured,
        mix_raw,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extended_mix_is_parsed_not_stripped() {
        let p = parse_title("Sun Rising (Extended Mix)");
        assert_eq!(p.base, "Sun Rising");
        assert_eq!(p.kind, MixKind::Extended);
        assert!(p.remixer.is_none());
    }

    #[test]
    fn remixer_is_captured() {
        let p = parse_title("Sun Rising (Adam Beyer Remix)");
        assert_eq!(p.kind, MixKind::Remix);
        assert_eq!(p.remixer_norm.as_deref(), Some("adam beyer"));
        assert_eq!(p.base, "Sun Rising");
    }

    #[test]
    fn possessive_extended_remix_keeps_remixer() {
        let p = parse_title("Sun Rising (Adam Beyer's Extended Remix)");
        assert_eq!(p.kind, MixKind::Remix);
        assert_eq!(p.remixer_norm.as_deref(), Some("adam beyer"));
    }

    #[test]
    fn dash_form_is_handled() {
        let p = parse_title("Sun Rising - Extended Mix");
        assert_eq!(p.base, "Sun Rising");
        assert_eq!(p.kind, MixKind::Extended);
    }

    #[test]
    fn radio_beats_edit() {
        assert_eq!(parse_title("Sun Rising (Radio Edit)").kind, MixKind::Radio);
    }

    #[test]
    fn featured_artists_lift_out_of_title() {
        let p = parse_title("Sun Rising (feat. Nia) (Original Mix)");
        assert_eq!(p.base, "Sun Rising");
        assert_eq!(p.kind, MixKind::Original);
        assert_eq!(p.featured, vec!["Nia".to_string()]);
    }

    #[test]
    fn non_version_brackets_stay_in_the_base() {
        let p = parse_title("Sun Rising (Reprise)");
        assert_eq!(p.kind, MixKind::None);
        assert_eq!(p.base, "Sun Rising (Reprise)");
    }

    #[test]
    fn ampersand_bands_survive_token_comparison() {
        // Local tag "Above & Beyond" vs Spotify artist array joined as "Above and Beyond".
        assert!(token_set_similarity("Above & Beyond", "Above and Beyond") > 0.95);
    }

    #[test]
    fn featured_guest_does_not_tank_artist_score() {
        assert!(token_set_similarity("Kolsch feat. Nia", "Kolsch") > 0.85);
    }

    #[test]
    fn diacritics_normalize() {
        assert_eq!(normalize("Sébastien Léger"), "sebastien leger");
    }
}

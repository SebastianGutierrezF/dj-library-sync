//! Telling the user a newer build exists.
//!
//! Not an updater: nothing is downloaded or installed here. The app asks the
//! service what the newest published release is, compares it to its own
//! version, and — if it is behind — says so and points at the download page.
//!
//! The bar for showing this is deliberately high. A banner that appears when
//! it should not is worse than one that occasionally fails to appear, because
//! the first teaches people to ignore it. So anything unclear — an
//! unparseable version, a pre-release, a service that cannot be reached —
//! results in silence rather than a guess.

use std::cmp::Ordering;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// What `GET /api/latest-release` answers.
///
/// Only the fields this needs. `detected` and the per-platform download
/// resolution are the landing page's business.
#[derive(Debug, Deserialize)]
struct LatestRelease {
    /// A git tag, so `v0.1.4` rather than `0.1.4`.
    version: Option<String>,
    #[serde(default)]
    prerelease: bool,
    /// False when there is no published release the public can reach.
    #[serde(default)]
    available: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UpdateStatus {
    /// This build's version, without a leading `v`.
    pub current: String,
    /// The newest published release, when there is one worth naming.
    pub latest: Option<String>,
    pub update_available: bool,
    /// Where to send someone who wants it. The service redirects this to the
    /// right asset for their platform, so it never goes stale.
    pub download_url: String,
}

/// A version as three numbers, which is all that is needed to order two
/// releases of this app.
///
/// Anything that is not that shape — a tag someone typed by hand, a build
/// suffix, a release named after a branch — parses to `None` and is treated as
/// "cannot tell", never as "older" or "newer".
fn parse_version(raw: &str) -> Option<(u64, u64, u64)> {
    let trimmed = raw.trim();
    let without_v = trimmed
        .strip_prefix('v')
        .or_else(|| trimmed.strip_prefix('V'))
        .unwrap_or(trimmed);

    // Drop a pre-release or build suffix before splitting: `0.1.5-rc.1` orders
    // as 0.1.5 for this purpose, and pre-releases are filtered out anyway.
    let core = without_v
        .split(['-', '+'])
        .next()
        .unwrap_or(without_v);

    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;

    // A fourth component means this is not the versioning scheme assumed here,
    // and guessing which way it orders is exactly the wrong move.
    if parts.next().is_some() {
        return None;
    }

    Some((major, minor, patch))
}

/// `None` when either side cannot be understood.
pub fn compare_versions(current: &str, latest: &str) -> Option<Ordering> {
    Some(parse_version(current)?.cmp(&parse_version(latest)?))
}

/// Ask the service what the newest release is.
///
/// Errors are the caller's to swallow: failing to reach the service is not
/// something to interrupt someone's work over, and there is nothing they could
/// do about it.
pub async fn check(service_url: &str, current_version: &str) -> Result<UpdateStatus> {
    let base = service_url.trim_end_matches('/');

    let http = reqwest::Client::builder()
        // Short: this runs at startup and nothing waits on it. A slow answer
        // is the same as no answer.
        .timeout(Duration::from_secs(8))
        .user_agent(concat!("dj-library-sync/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building HTTP client")?;

    let release: LatestRelease = http
        .get(format!("{base}/api/latest-release"))
        .send()
        .await
        .context("asking the service for the latest release")?
        .error_for_status()
        .context("the service refused the release query")?
        .json()
        .await
        .context("parsing the latest release")?;

    let current = parse_version(current_version)
        .map(|(a, b, c)| format!("{a}.{b}.{c}"))
        .unwrap_or_else(|| current_version.trim_start_matches('v').to_string());

    let download_url = format!("{base}/download");

    // A pre-release is something you go looking for, not something you are
    // told about. `available: false` means no published release at all.
    let latest_tag = match (&release.version, release.available, release.prerelease) {
        (Some(tag), true, false) => tag.clone(),
        _ => {
            return Ok(UpdateStatus {
                current,
                latest: None,
                update_available: false,
                download_url,
            })
        }
    };

    let update_available = matches!(
        compare_versions(current_version, &latest_tag),
        Some(Ordering::Less)
    );

    Ok(UpdateStatus {
        current,
        latest: Some(latest_tag.trim_start_matches('v').to_string()),
        update_available,
        download_url,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_newer_tag_is_newer() {
        assert_eq!(compare_versions("0.1.4", "v0.1.5"), Some(Ordering::Less));
        assert_eq!(compare_versions("v0.1.5", "0.1.4"), Some(Ordering::Greater));
        assert_eq!(compare_versions("0.1.4", "v0.1.4"), Some(Ordering::Equal));
    }

    #[test]
    fn ten_is_after_nine() {
        // The reason this is not a string comparison. Lexically "0.1.10" sorts
        // before "0.1.9", which would leave everyone stuck on .9 forever with
        // no error anywhere to explain it.
        assert_eq!(compare_versions("0.1.9", "0.1.10"), Some(Ordering::Less));
        assert_eq!(compare_versions("0.9.0", "0.10.0"), Some(Ordering::Less));
        assert_eq!(compare_versions("1.0.0", "10.0.0"), Some(Ordering::Less));
    }

    #[test]
    fn missing_components_are_zero() {
        assert_eq!(compare_versions("0.1", "0.1.0"), Some(Ordering::Equal));
        assert_eq!(compare_versions("1", "1.0.0"), Some(Ordering::Equal));
        assert_eq!(compare_versions("0.2", "0.1.9"), Some(Ordering::Greater));
    }

    #[test]
    fn a_suffix_orders_by_its_numbers() {
        assert_eq!(compare_versions("0.1.4", "0.1.5-rc.1"), Some(Ordering::Less));
        assert_eq!(compare_versions("0.1.5+build7", "0.1.5"), Some(Ordering::Equal));
    }

    #[test]
    fn nonsense_is_unknown_rather_than_older() {
        // Every one of these would otherwise have to be sorted against a real
        // version, and any answer would be invented. `None` reaches the caller
        // as "no banner".
        for tag in ["", "latest", "v", "nightly", "0.1.x", "1.2.3.4", "0..1"] {
            assert_eq!(
                compare_versions("0.1.4", tag),
                None,
                "{tag:?} should not compare"
            );
            assert_eq!(compare_versions(tag, "0.1.4"), None, "{tag:?} should not compare");
        }
    }

    #[test]
    fn whitespace_does_not_change_the_answer() {
        assert_eq!(compare_versions(" 0.1.4 ", "\tv0.1.4\n"), Some(Ordering::Equal));
    }
}

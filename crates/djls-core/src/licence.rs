//! The licence this machine holds, and the service that issues it.
//!
//! Two secrets with different jobs, mirroring the service side:
//!
//! - the **licence key** (`DJLS-…`) is what a human handles, once;
//! - the **activation token** is a short-lived JWT the machine uses for every
//!   request afterwards.
//!
//! Both live in the OS credential store alongside the Spotify tokens. The
//! device id does not: it is an identifier rather than a secret, and someone
//! editing it to claim a second trial is a threat we deliberately do not
//! defend against — the marginal cost of a trial is zero.
//!
//! Nothing here is required for Spotify. A user who never touches Apple Music
//! never has a licence, never has a device id on the server, and never reaches
//! this module.

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::auth::KeyringStore;

/// Default home of the token service. Overridable so a developer can point at
/// a local instance without rebuilding.
pub const DEFAULT_SERVICE_URL: &str = "https://dj-library-sync-service.onrender.com";

pub fn service_url() -> String {
    std::env::var("DJLS_SERVICE_URL").unwrap_or_else(|_| DEFAULT_SERVICE_URL.to_string())
}

/// Refresh once the token is inside this window of expiring, so a long sync
/// cannot start on a token that dies halfway through.
const REFRESH_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);

/// What this machine holds. `key` is absent for a trial, which has no licence.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoredLicence {
    #[serde(default)]
    pub key: Option<String>,
    pub token: String,
    pub expires_at: String,
    pub plan: String,
}

impl StoredLicence {
    pub fn is_trial(&self) -> bool {
        self.plan == "trial"
    }

    /// Seconds until the activation token expires, or `None` once it has.
    pub fn seconds_remaining(&self) -> Option<u64> {
        let expiry = crate::apple::rfc3339_to_unix(&self.expires_at)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs() as i64;
        (expiry > now).then(|| (expiry - now) as u64)
    }

    pub fn is_expired(&self) -> bool {
        self.seconds_remaining().is_none()
    }

    /// Whether it is worth asking the service for a fresh token. True once
    /// expired, and true while it is about to be.
    pub fn needs_refresh(&self) -> bool {
        match self.seconds_remaining() {
            None => true,
            Some(left) => left < REFRESH_WINDOW.as_secs(),
        }
    }
}

/// Licence storage in the OS credential store.
pub struct LicenceKeyring {
    inner: KeyringStore,
}

impl Default for LicenceKeyring {
    fn default() -> Self {
        Self {
            inner: KeyringStore::new("dj-library-sync", "licence"),
        }
    }
}

impl LicenceKeyring {
    pub fn load(&self) -> Result<Option<StoredLicence>> {
        self.inner.load_raw()
    }

    pub fn save(&self, licence: &StoredLicence) -> Result<()> {
        self.inner.save_raw(licence)
    }

    pub fn clear(&self) -> Result<()> {
        self.inner.clear_raw()
    }
}

/// The Music User Token, obtained through the hosted MusicKit page. Kept apart
/// from the licence because the two are revoked independently: disconnecting
/// Apple Music must not throw away a paid licence.
pub struct AppleTokenKeyring {
    inner: KeyringStore,
}

impl Default for AppleTokenKeyring {
    fn default() -> Self {
        Self {
            inner: KeyringStore::new("dj-library-sync", "apple-music-user-token"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredUserToken {
    token: String,
}

impl AppleTokenKeyring {
    pub fn load(&self) -> Result<Option<String>> {
        Ok(self.inner.load_raw::<StoredUserToken>()?.map(|t| t.token))
    }

    pub fn save(&self, token: &str) -> Result<()> {
        self.inner.save_raw(&StoredUserToken {
            token: token.to_string(),
        })
    }

    pub fn clear(&self) -> Result<()> {
        self.inner.clear_raw()
    }
}

/// What the service says this licence can do right now.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entitlement {
    #[serde(default)]
    pub credits: i64,
    #[serde(default)]
    pub unlimited: bool,
    #[serde(default)]
    pub plan: Option<String>,
}

#[derive(Deserialize)]
struct ActivationResponse {
    token: String,
    expires_at: String,
    plan: String,
}

/// Talks to the token service.
pub struct ServiceClient {
    base_url: String,
    http: reqwest::Client,
}

impl ServiceClient {
    pub fn new(base_url: impl Into<String>) -> Result<Self> {
        Ok(Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .user_agent("dj-library-sync/0.1")
                .build()
                .context("building HTTP client")?,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{}", self.base_url, path.trim_start_matches('/'))
    }

    /// Claim this device's free trial, or refresh the token for one it already
    /// claimed. Calling it repeatedly is how a trial token stays fresh; the
    /// service grants credits only the first time.
    pub async fn start_trial(&self, device_id: &str) -> Result<StoredLicence> {
        let resp = self
            .http
            .post(self.url("/api/trial"))
            .json(&serde_json::json!({ "deviceId": device_id }))
            .send()
            .await
            .context("asking the service for a trial")?;

        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("{}", describe_service_error(status.as_u16(), &body));
        }

        let parsed: ActivationResponse =
            serde_json::from_str(&body).context("parsing the trial response")?;

        Ok(StoredLicence {
            key: None,
            token: parsed.token,
            expires_at: parsed.expires_at,
            plan: parsed.plan,
        })
    }

    /// Exchange a licence key for an activation token. Also how a paid token
    /// is refreshed — re-activating a known device does not consume a seat.
    pub async fn activate(&self, licence_key: &str, device_id: &str) -> Result<StoredLicence> {
        let resp = self
            .http
            .post(self.url("/api/activate"))
            .json(&serde_json::json!({ "licence": licence_key, "deviceId": device_id }))
            .send()
            .await
            .context("activating the licence")?;

        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("{}", describe_service_error(status.as_u16(), &body));
        }

        let parsed: ActivationResponse =
            serde_json::from_str(&body).context("parsing the activation response")?;

        Ok(StoredLicence {
            key: Some(licence_key.to_string()),
            token: parsed.token,
            expires_at: parsed.expires_at,
            plan: parsed.plan,
        })
    }

    pub async fn entitlement(&self, activation_token: &str) -> Result<Entitlement> {
        let resp = self
            .http
            .get(self.url("/api/entitlement"))
            .bearer_auth(activation_token)
            .send()
            .await
            .context("reading the entitlement")?;

        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("{}", describe_service_error(status.as_u16(), &body));
        }

        serde_json::from_str(&body).context("parsing the entitlement")
    }

    /// Report tracks pushed. Best-effort by design: the push already happened,
    /// and failing to bill must never look to the user like the push failed.
    pub async fn report_usage(&self, activation_token: &str, tracks: u32) -> Result<Entitlement> {
        let resp = self
            .http
            .post(self.url("/api/usage"))
            .bearer_auth(activation_token)
            .json(&serde_json::json!({ "tracks": tracks }))
            .send()
            .await
            .context("reporting usage")?;

        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("{}", describe_service_error(status.as_u16(), &body));
        }

        serde_json::from_str(&body).context("parsing the usage response")
    }

    /// Give a stored licence a fresh token when it is near expiry. Returns
    /// `None` when nothing needed doing.
    pub async fn refresh_if_needed(
        &self,
        licence: &StoredLicence,
        device_id: &str,
    ) -> Result<Option<StoredLicence>> {
        if !licence.needs_refresh() {
            return Ok(None);
        }

        let refreshed = match &licence.key {
            Some(key) => self.activate(key, device_id).await?,
            None => self.start_trial(device_id).await?,
        };
        Ok(Some(refreshed))
    }
}

/// Turn a service status into something that names the fix. These are read by
/// people who have just paid, so vagueness is expensive.
fn describe_service_error(status: u16, body: &str) -> String {
    let detail = extract_error(body).unwrap_or_else(|| body.trim().to_string());
    match status {
        400 => format!("The service rejected the request: {detail}"),
        401 => "This licence is no longer valid on this machine. Re-enter it in Settings."
            .to_string(),
        402 => "This licence has not been paid for yet. If you have just checked out, \
                give it a few seconds and try again."
            .to_string(),
        403 => "That licence has been cancelled. Resubscribe and the same key will work again."
            .to_string(),
        404 => "That licence key is not recognised. Check for typos, or paste it again.".to_string(),
        409 => format!(
            "That licence is already active on all of its devices. {detail} \
             Deactivate one before adding this machine."
        ),
        503 => "The service is temporarily unavailable. Spotify sync is unaffected.".to_string(),
        _ => format!("The service returned {status}: {detail}"),
    }
}

/// Pull `error` out of a JSON body, falling back to the raw text.
fn extract_error(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    value
        .get("error")
        .and_then(|e| e.as_str())
        .map(|s| s.to_string())
}

/// Loopback port for the Apple Music sign-in redirect. Distinct from
/// Spotify's, so connecting one while the other is mid-flight cannot collide.
pub const APPLE_CALLBACK_PORT: u16 = 8889;

/// Connect Apple Music.
///
/// Apple issues a Music User Token only through MusicKit, and MusicKit only
/// runs in a web context — so the app cannot do this itself. The hosted page
/// runs `music.authorize()` in the user's real browser and redirects the token
/// back here. See `docs/apple-music-auth.md` for why the alternatives were
/// rejected.
///
/// `on_url` receives the page to open; the caller decides whether to launch a
/// browser, show the link, or both.
pub async fn connect_apple_music<F>(
    service_url: &str,
    activation_token: &str,
    port: u16,
    on_url: F,
) -> Result<String>
where
    F: FnOnce(&str),
{
    let state = crate::auth::random_b64url(16)?;

    // Bind before sending the user anywhere: a busy port should fail now, not
    // after they have already approved Apple's consent screen.
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .with_context(|| {
            format!("binding 127.0.0.1:{port} — is another instance of the app running?")
        })?;

    let url = format!(
        "{}/auth/apple?token={}&state={}&port={port}",
        service_url.trim_end_matches('/'),
        urlencode(activation_token),
        urlencode(&state),
    );
    on_url(&url);

    let token = tokio::time::timeout(
        crate::auth::CALLBACK_TIMEOUT,
        crate::auth::serve_loopback_callback(listener, |path| parse_apple_callback(path, &state)),
    )
    .await
    .map_err(|_| anyhow!("timed out waiting for Apple Music sign-in"))??;

    Ok(token)
}

fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Parse the Music User Token out of the loopback redirect the hosted MusicKit
/// page sends back, rejecting a mismatched `state`.
pub fn parse_apple_callback(path: &str, expected_state: &str) -> Result<String> {
    let url =
        reqwest::Url::parse(&format!("http://127.0.0.1{path}")).context("parsing redirect URL")?;

    let mut token = None;
    let mut state = None;
    let mut error = None;

    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "music_user_token" => token = Some(value.into_owned()),
            "state" => state = Some(value.into_owned()),
            "error" => error = Some(value.into_owned()),
            _ => {}
        }
    }

    if let Some(error) = error {
        bail!("Apple Music sign-in failed: {error}");
    }

    // Without this, anything able to reach the loopback port could hand the
    // app a token of its choosing.
    if state.as_deref() != Some(expected_state) {
        bail!("state mismatch on the Apple Music redirect — ignoring this response");
    }

    token.ok_or_else(|| anyhow!("the redirect carried no Music User Token"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn licence_expiring_in(secs: i64) -> StoredLicence {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        StoredLicence {
            key: Some("DJLS-AAAA-BBBB-CCCC-DDDD".into()),
            token: "jwt".into(),
            expires_at: unix_to_rfc3339(now + secs),
            plan: "pack".into(),
        }
    }

    /// Minimal inverse of `rfc3339_to_unix`, for building fixtures.
    fn unix_to_rfc3339(ts: i64) -> String {
        let days = ts.div_euclid(86_400);
        let rem = ts.rem_euclid(86_400);
        let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);

        let z = days + 719_468;
        let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = if m <= 2 { y + 1 } else { y };
        format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
    }

    #[test]
    fn a_fresh_token_is_left_alone() {
        let licence = licence_expiring_in(6 * 24 * 3600);
        assert!(!licence.is_expired());
        assert!(!licence.needs_refresh());
    }

    #[test]
    fn a_token_about_to_expire_is_refreshed_early() {
        // Refreshing only once it has died would strand a sync mid-run.
        let licence = licence_expiring_in(2 * 3600);
        assert!(!licence.is_expired());
        assert!(licence.needs_refresh());
    }

    #[test]
    fn an_expired_token_needs_refresh() {
        let licence = licence_expiring_in(-60);
        assert!(licence.is_expired());
        assert!(licence.needs_refresh());
        assert_eq!(licence.seconds_remaining(), None);
    }

    #[test]
    fn an_unparseable_expiry_is_treated_as_expired() {
        // Better to ask for a fresh token than to trust a value we cannot read.
        let licence = StoredLicence {
            key: None,
            token: "jwt".into(),
            expires_at: "who knows".into(),
            plan: "trial".into(),
        };
        assert!(licence.is_expired());
        assert!(licence.needs_refresh());
    }

    #[test]
    fn a_trial_is_distinguishable_from_a_purchase() {
        let trial = StoredLicence {
            key: None,
            token: "jwt".into(),
            expires_at: "2030-01-01T00:00:00Z".into(),
            plan: "trial".into(),
        };
        assert!(trial.is_trial());
        assert!(!licence_expiring_in(3600).is_trial());
    }

    #[test]
    fn service_errors_name_the_fix() {
        assert!(describe_service_error(402, "{}").contains("checked out"));
        assert!(describe_service_error(403, "{}").contains("Resubscribe"));
        assert!(describe_service_error(404, "{}").contains("typos"));
        assert!(describe_service_error(409, "{}").contains("Deactivate"));
        // An outage must not read as the whole app being broken.
        assert!(describe_service_error(503, "{}").contains("Spotify sync is unaffected"));
    }

    #[test]
    fn the_service_error_message_is_preferred_over_the_raw_body() {
        let described = describe_service_error(400, r#"{"error":"deviceId is required"}"#);
        assert!(described.contains("deviceId is required"), "{described}");
        assert!(!described.contains('{'), "the JSON should not leak through");
    }

    #[test]
    fn the_apple_redirect_must_carry_the_state_we_sent() {
        let token = parse_apple_callback("/callback?music_user_token=abc123&state=xyz", "xyz");
        assert_eq!(token.unwrap(), "abc123");

        // Anything able to reach the loopback port could otherwise hand us a
        // token of its choosing.
        assert!(parse_apple_callback("/callback?music_user_token=evil&state=other", "xyz").is_err());
        assert!(parse_apple_callback("/callback?music_user_token=abc", "xyz").is_err());
    }

    #[test]
    fn a_redirect_without_a_token_is_an_error_not_an_empty_string() {
        assert!(parse_apple_callback("/callback?state=xyz", "xyz").is_err());
        assert!(parse_apple_callback("/callback?error=denied&state=xyz", "xyz").is_err());
    }

    #[test]
    fn the_fixture_helper_round_trips() {
        // Guards the tests above: a broken fixture would make them meaningless.
        for ts in [0i64, 946_684_800, 1_789_560_000] {
            assert_eq!(crate::apple::rfc3339_to_unix(&unix_to_rfc3339(ts)), Some(ts));
        }
    }
}

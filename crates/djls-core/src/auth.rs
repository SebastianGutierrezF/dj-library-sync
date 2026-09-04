//! Spotify user authorization — Authorization Code flow with PKCE.
//!
//! Desktop apps cannot keep a client secret, so PKCE is the only correct
//! choice here. Two details that bite people:
//!
//! 1. Spotify rejects `localhost` as a redirect host. The loopback address
//!    must be spelled `127.0.0.1`.
//! 2. A refresh can return a *new* refresh token. It must be persisted every
//!    time or auth dies silently days later, when the old one stops working.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64URL, Engine as _};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";
const AUTHORIZE_URL: &str = "https://accounts.spotify.com/authorize";

/// Only what pushing to a playlist actually needs. Asking for more than this
/// is a reason for a user to decline the consent screen.
pub const REQUIRED_SCOPES: &[&str] = &[
    "playlist-read-private",
    "playlist-modify-public",
    "playlist-modify-private",
];

/// How long to wait for the user to finish the consent screen.
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    /// Unix seconds.
    pub expires_at: u64,
}

impl Tokens {
    pub fn is_expired(&self) -> bool {
        // Refresh a little early rather than racing the expiry.
        now_unix() + 30 >= self.expires_at
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Where tokens live between runs.
pub trait TokenStore: Send + Sync {
    fn load(&self) -> Result<Option<Tokens>>;
    fn save(&self, tokens: &Tokens) -> Result<()>;
    fn clear(&self) -> Result<()>;
}

/// The OS credential store — Keychain on macOS, Credential Manager on Windows,
/// Secret Service on Linux.
pub struct KeyringStore {
    service: String,
    account: String,
}

impl KeyringStore {
    pub fn new(service: impl Into<String>, account: impl Into<String>) -> Self {
        Self {
            service: service.into(),
            account: account.into(),
        }
    }

    fn entry(&self) -> Result<keyring::Entry> {
        keyring::Entry::new(&self.service, &self.account).context("opening OS credential store")
    }
}

impl Default for KeyringStore {
    fn default() -> Self {
        Self::new("dj-library-sync", "spotify")
    }
}

impl TokenStore for KeyringStore {
    fn load(&self) -> Result<Option<Tokens>> {
        match self.entry()?.get_password() {
            Ok(raw) => Ok(Some(
                serde_json::from_str(&raw).context("parsing stored tokens")?,
            )),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(err) => Err(anyhow!("reading stored tokens: {err}")),
        }
    }

    fn save(&self, tokens: &Tokens) -> Result<()> {
        let raw = serde_json::to_string(tokens)?;
        self.entry()?
            .set_password(&raw)
            .map_err(|err| anyhow!("saving tokens: {err}"))
    }

    fn clear(&self) -> Result<()> {
        match self.entry()?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(err) => Err(anyhow!("clearing tokens: {err}")),
        }
    }
}

/// In-memory store, for tests.
#[derive(Default)]
pub struct MemoryStore {
    tokens: std::sync::Mutex<Option<Tokens>>,
}

impl TokenStore for MemoryStore {
    fn load(&self) -> Result<Option<Tokens>> {
        Ok(self.tokens.lock().unwrap().clone())
    }
    fn save(&self, tokens: &Tokens) -> Result<()> {
        *self.tokens.lock().unwrap() = Some(tokens.clone());
        Ok(())
    }
    fn clear(&self) -> Result<()> {
        *self.tokens.lock().unwrap() = None;
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct AuthConfig {
    pub client_id: String,
    pub redirect_port: u16,
}

impl AuthConfig {
    pub fn new(client_id: impl Into<String>) -> Self {
        Self {
            client_id: client_id.into(),
            redirect_port: 8888,
        }
    }

    /// Must match a redirect URI registered on the Spotify app *exactly*.
    pub fn redirect_uri(&self) -> String {
        format!("http://127.0.0.1:{}/callback", self.redirect_port)
    }
}

/// A PKCE verifier and its derived challenge.
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    pub fn generate() -> Result<Self> {
        let verifier = random_b64url(64)?;
        let digest = Sha256::digest(verifier.as_bytes());
        Ok(Self {
            challenge: B64URL.encode(digest),
            verifier,
        })
    }
}

fn random_b64url(bytes: usize) -> Result<String> {
    let mut buf = vec![0u8; bytes];
    getrandom::getrandom(&mut buf).map_err(|err| anyhow!("generating random bytes: {err}"))?;
    Ok(B64URL.encode(buf))
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: u64,
    refresh_token: Option<String>,
}

impl TokenResponse {
    /// Spotify may or may not return a new refresh token. Keep the old one
    /// when it doesn't, and persist the new one when it does — dropping a
    /// rotated token is what makes auth fail days later.
    fn into_tokens(self, previous_refresh: Option<&str>) -> Result<Tokens> {
        let refresh_token = self
            .refresh_token
            .or_else(|| previous_refresh.map(|s| s.to_string()))
            .ok_or_else(|| anyhow!("Spotify returned no refresh token and none was stored"))?;

        Ok(Tokens {
            access_token: self.access_token,
            refresh_token,
            expires_at: now_unix() + self.expires_in,
        })
    }
}

/// Build the URL the user has to visit to grant access.
pub fn authorize_url(config: &AuthConfig, challenge: &str, state: &str) -> Result<String> {
    let url = reqwest::Url::parse_with_params(
        AUTHORIZE_URL,
        &[
            ("client_id", config.client_id.as_str()),
            ("response_type", "code"),
            ("redirect_uri", &config.redirect_uri()),
            ("code_challenge_method", "S256"),
            ("code_challenge", challenge),
            ("state", state),
            ("scope", &REQUIRED_SCOPES.join(" ")),
        ],
    )?;
    Ok(url.to_string())
}

/// Run the full interactive login and persist the result.
///
/// `on_url` is handed the authorization URL — the caller decides whether to
/// open a browser, print it, or both.
pub async fn login<F>(config: &AuthConfig, store: &dyn TokenStore, on_url: F) -> Result<Tokens>
where
    F: FnOnce(&str),
{
    let pkce = Pkce::generate()?;
    let state = random_b64url(16)?;

    // Bind before sending the user anywhere, so a busy port fails immediately
    // rather than after they have already approved the consent screen.
    let listener = TcpListener::bind(("127.0.0.1", config.redirect_port))
        .await
        .with_context(|| {
            format!(
                "binding {} — is another instance running, or the port in use?",
                config.redirect_uri()
            )
        })?;

    on_url(&authorize_url(config, &pkce.challenge, &state)?);

    let code = tokio::time::timeout(CALLBACK_TIMEOUT, wait_for_callback(listener, &state))
        .await
        .map_err(|_| anyhow!("timed out waiting for the Spotify redirect"))??;

    let tokens = exchange_code(config, &code, &pkce.verifier).await?;
    store.save(&tokens)?;
    Ok(tokens)
}

async fn exchange_code(config: &AuthConfig, code: &str, verifier: &str) -> Result<Tokens> {
    let http = reqwest::Client::new();
    let resp = http
        .post(TOKEN_URL)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", &config.redirect_uri()),
            ("client_id", &config.client_id),
            ("code_verifier", verifier),
        ])
        .send()
        .await
        .context("exchanging authorization code")?;

    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("token exchange failed ({status}): {body}");
    }

    serde_json::from_str::<TokenResponse>(&body)
        .context("parsing token response")?
        .into_tokens(None)
}

/// Exchange a refresh token for a fresh access token, persisting any rotation.
pub async fn refresh(config: &AuthConfig, store: &dyn TokenStore, current: &Tokens) -> Result<Tokens> {
    let http = reqwest::Client::new();
    let resp = http
        .post(TOKEN_URL)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", &current.refresh_token),
            ("client_id", &config.client_id),
        ])
        .send()
        .await
        .context("refreshing access token")?;

    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!(
            "token refresh failed ({status}): {body}\n\
             If this persists, run `djls login` again."
        );
    }

    let tokens = serde_json::from_str::<TokenResponse>(&body)
        .context("parsing refresh response")?
        .into_tokens(Some(&current.refresh_token))?;

    store.save(&tokens)?;
    Ok(tokens)
}

/// Parse `code` out of the OAuth redirect, rejecting a mismatched `state`.
fn parse_callback(path: &str, expected_state: &str) -> Result<String> {
    let url = reqwest::Url::parse(&format!("http://127.0.0.1{path}"))
        .context("parsing redirect URL")?;

    let mut code = None;
    let mut state = None;
    let mut error = None;

    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "code" => code = Some(value.into_owned()),
            "state" => state = Some(value.into_owned()),
            "error" => error = Some(value.into_owned()),
            _ => {}
        }
    }

    if let Some(error) = error {
        bail!("Spotify denied the request: {error}");
    }

    // Guards against a forged redirect landing on the loopback listener.
    if state.as_deref() != Some(expected_state) {
        bail!("state mismatch on the redirect — ignoring this response");
    }

    code.ok_or_else(|| anyhow!("redirect carried no authorization code"))
}

async fn wait_for_callback(listener: TcpListener, expected_state: &str) -> Result<String> {
    loop {
        let (mut stream, _) = listener.accept().await.context("accepting redirect")?;

        let mut buf = [0u8; 8192];
        let read = stream.read(&mut buf).await.unwrap_or(0);
        if read == 0 {
            continue;
        }

        let request = String::from_utf8_lossy(&buf[..read]);
        let path = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("/");

        // Browsers ask for /favicon.ico on the same origin; ignore anything
        // that is not the redirect itself.
        if !path.starts_with("/callback") {
            let _ = stream
                .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                .await;
            continue;
        }

        let result = parse_callback(path, expected_state);
        let page = match &result {
            Ok(_) => "<h2>Connected.</h2><p>You can close this tab and go back to the terminal.</p>",
            Err(_) => "<h2>Something went wrong.</h2><p>Check the terminal for details.</p>",
        };
        let body = format!(
            "<!doctype html><meta charset=utf-8><title>DJ Library Sync</title>\
             <body style=\"font:16px system-ui;padding:3rem;text-align:center\">{page}</body>"
        );
        let _ = stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await;
        let _ = stream.flush().await;

        return result;
    }
}

/// Best-effort "open this in the user's browser".
pub fn open_in_browser(url: &str) -> bool {
    let (program, args): (&str, Vec<&str>) = if cfg!(target_os = "macos") {
        ("open", vec![url])
    } else if cfg!(target_os = "windows") {
        ("cmd", vec!["/C", "start", "", url])
    } else {
        ("xdg-open", vec![url])
    };

    std::process::Command::new(program)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenge_is_the_base64url_sha256_of_the_verifier() {
        // The RFC 7636 appendix B test vector.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let digest = Sha256::digest(verifier.as_bytes());
        assert_eq!(
            B64URL.encode(digest),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn verifier_and_challenge_differ_each_time() {
        let a = Pkce::generate().unwrap();
        let b = Pkce::generate().unwrap();
        assert_ne!(a.verifier, b.verifier);
        assert_ne!(a.challenge, b.challenge);
        // Base64url must not contain characters needing escaping in a query.
        assert!(!a.challenge.contains('+') && !a.challenge.contains('/') && !a.challenge.contains('='));
    }

    #[test]
    fn redirect_uri_uses_the_loopback_ip_not_localhost() {
        // Spotify rejects `localhost`; this must never regress.
        let config = AuthConfig::new("abc");
        assert_eq!(config.redirect_uri(), "http://127.0.0.1:8888/callback");
    }

    #[test]
    fn authorize_url_carries_pkce_and_scopes() {
        let config = AuthConfig::new("client123");
        let url = authorize_url(&config, "chal", "st4te").unwrap();
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("code_challenge=chal"));
        assert!(url.contains("state=st4te"));
        assert!(url.contains("client_id=client123"));
        assert!(url.contains("playlist-modify-private"));
        // PKCE means no secret is ever sent.
        assert!(!url.contains("client_secret"));
    }

    #[test]
    fn callback_returns_the_code() {
        let code = parse_callback("/callback?code=abc123&state=xyz", "xyz").unwrap();
        assert_eq!(code, "abc123");
    }

    #[test]
    fn callback_rejects_a_mismatched_state() {
        let err = parse_callback("/callback?code=abc&state=attacker", "xyz").unwrap_err();
        assert!(err.to_string().contains("state mismatch"));
    }

    #[test]
    fn callback_surfaces_a_denied_consent() {
        let err = parse_callback("/callback?error=access_denied&state=xyz", "xyz").unwrap_err();
        assert!(err.to_string().contains("access_denied"));
    }

    #[test]
    fn a_rotated_refresh_token_replaces_the_old_one() {
        let resp = TokenResponse {
            access_token: "new-access".into(),
            expires_in: 3600,
            refresh_token: Some("rotated".into()),
        };
        let tokens = resp.into_tokens(Some("original")).unwrap();
        assert_eq!(tokens.refresh_token, "rotated");
    }

    #[test]
    fn an_absent_refresh_token_keeps_the_stored_one() {
        // Spotify often omits it; dropping it here is what silently breaks auth.
        let resp = TokenResponse {
            access_token: "new-access".into(),
            expires_in: 3600,
            refresh_token: None,
        };
        let tokens = resp.into_tokens(Some("original")).unwrap();
        assert_eq!(tokens.refresh_token, "original");
    }

    #[test]
    fn expiry_is_checked_with_a_margin() {
        let tokens = Tokens {
            access_token: "a".into(),
            refresh_token: "r".into(),
            expires_at: now_unix() + 10,
        };
        assert!(tokens.is_expired(), "a token expiring in 10s must refresh early");
    }

    #[test]
    fn memory_store_roundtrips() {
        let store = MemoryStore::default();
        assert!(store.load().unwrap().is_none());

        let tokens = Tokens {
            access_token: "a".into(),
            refresh_token: "r".into(),
            expires_at: now_unix() + 3600,
        };
        store.save(&tokens).unwrap();
        assert_eq!(store.load().unwrap().unwrap().access_token, "a");

        store.clear().unwrap();
        assert!(store.load().unwrap().is_none());
    }
}

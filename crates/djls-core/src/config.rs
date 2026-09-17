//! Persisted app settings.
//!
//! The CLI takes everything as flags, but the desktop app has to remember the
//! watched folder and which Spotify app it belongs to across launches — a
//! bundled app has no working directory to find a `.env` in.
//!
//! Only non-secret values live here. Tokens stay in the OS keychain; the
//! client ID is not a secret under PKCE, which is the point of PKCE.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Spotify app client ID. Not a secret in the PKCE flow.
    pub spotify_client_id: Option<String>,
    /// The folder new downloads land in.
    pub watch_folder: Option<PathBuf>,
    /// Push the shorter cut when the extended mix isn't on the platform.
    pub accept_shorter: bool,
    /// Playlist the last push went to, offered as the default next time.
    pub last_playlist: Option<String>,
    /// Identifies this machine to the token service, for trial metering and
    /// seat counting. Not a secret — it is an identifier, and someone editing
    /// it to claim a second trial is a threat we deliberately do not defend
    /// against, because a trial costs nothing to serve.
    #[serde(default)]
    pub device_id: Option<String>,
}

impl Config {
    /// This machine's id, generating and persisting one on first use.
    pub fn device_id_or_create(&mut self) -> Result<String> {
        if let Some(existing) = self.device_id.as_ref().filter(|id| id.len() >= 8) {
            return Ok(existing.clone());
        }

        let mut bytes = [0u8; 16];
        getrandom::getrandom(&mut bytes).map_err(|e| anyhow::anyhow!("generating a device id: {e}"))?;
        let id = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();

        self.device_id = Some(id.clone());
        self.save()?;
        Ok(id)
    }

    pub fn path() -> PathBuf {
        crate::db::Database::default_path()
            .parent()
            .map(|dir| dir.join("config.json"))
            .unwrap_or_else(|| PathBuf::from("config.json"))
    }

    /// Load from disk, falling back to defaults. A corrupt file is replaced
    /// rather than fatal — losing a remembered folder beats refusing to start.
    pub fn load() -> Self {
        Self::load_from(&Self::path()).unwrap_or_default()
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)?;
        Ok(serde_json::from_str(&raw)?)
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&Self::path())
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let raw = serde_json::to_string_pretty(self)?;
        std::fs::write(path, raw).with_context(|| format!("writing {}", path.display()))
    }

    /// The environment wins over the stored value, so a developer running from
    /// a shell with `.env` loaded does not have to re-enter anything.
    pub fn client_id(&self) -> Option<String> {
        std::env::var("SPOTIFY_CLIENT_ID")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| self.spotify_client_id.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_disk() {
        let dir = std::env::temp_dir().join(format!("djls-cfg-{}", std::process::id()));
        let path = dir.join("config.json");
        let _ = std::fs::remove_dir_all(&dir);

        let cfg = Config {
            spotify_client_id: Some("abc123".into()),
            watch_folder: Some(PathBuf::from("/music/new")),
            accept_shorter: true,
            last_playlist: Some("Gym".into()),
            device_id: Some("0123456789abcdef".into()),
        };
        cfg.save_to(&path).unwrap();

        let loaded = Config::load_from(&path).unwrap();
        assert_eq!(loaded.spotify_client_id.as_deref(), Some("abc123"));
        assert_eq!(loaded.watch_folder, Some(PathBuf::from("/music/new")));
        assert!(loaded.accept_shorter);
        assert_eq!(loaded.device_id.as_deref(), Some("0123456789abcdef"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_config_written_before_device_ids_existed_still_loads() {
        let dir = std::env::temp_dir().join(format!("djls-cfg-old-{}", std::process::id()));
        let path = dir.join("config.json");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, r#"{"spotify_client_id":"abc","accept_shorter":false}"#).unwrap();

        let loaded = Config::load_from(&path).unwrap();
        assert_eq!(loaded.spotify_client_id.as_deref(), Some("abc"));
        assert!(loaded.device_id.is_none(), "an absent id is generated on first use");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_file_yields_defaults_not_an_error() {
        let cfg = Config::load_from(Path::new("/nonexistent/nope.json"));
        assert!(cfg.is_err());
        // load() swallows it, which is the behaviour callers depend on.
        assert!(Config::default().watch_folder.is_none());
    }

    #[test]
    fn new_fields_do_not_break_an_old_config_file() {
        // serde(default) on the struct: a file written by an earlier version
        // must still load.
        let cfg: Config = serde_json::from_str(r#"{"watch_folder":"/music"}"#).unwrap();
        assert_eq!(cfg.watch_folder, Some(PathBuf::from("/music")));
        assert!(!cfg.accept_shorter);
    }
}

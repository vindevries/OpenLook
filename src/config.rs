//! Settings and on-disk locations.

use std::fs;
use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// Microsoft's public "Microsoft Graph Command Line Tools" application.
/// It allows device-code sign-in with delegated mail scopes, so OpenLook
/// works without anyone registering an app first.
pub const DEFAULT_CLIENT_ID: &str = "14d82eec-204b-4c2f-b7e8-296a70dab67e";
pub const DEFAULT_TENANT: &str = "organizations";

pub const APP_ID: &str = "com.opslogix.Openlook";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub tenant: String,
    /// Set once the user has chosen the demo mailbox (or signed out), so the
    /// account prompt does not reappear at every launch.
    #[serde(default)]
    pub demo_ack: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { client_id: DEFAULT_CLIENT_ID.into(), tenant: DEFAULT_TENANT.into(), demo_ack: false }
    }
}

impl Settings {
    pub fn load() -> Settings {
        let mut s: Settings = fs::read_to_string(settings_path())
            .ok()
            .and_then(|raw| serde_json::from_str::<Settings>(&raw).ok())
            .unwrap_or_default();
        // Empty values fall back to the defaults, so clearing a field in the
        // UI restores the built-in sign-in app rather than breaking it.
        if s.client_id.trim().is_empty() {
            s.client_id = DEFAULT_CLIENT_ID.into();
        }
        if s.tenant.trim().is_empty() {
            s.tenant = DEFAULT_TENANT.into();
        }
        s
    }

    pub fn save(&self) -> Result<()> {
        let path = settings_path();
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(path, serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }
}

pub fn config_dir() -> PathBuf {
    glib::user_config_dir().join("openlook")
}

pub fn data_dir() -> PathBuf {
    glib::user_data_dir().join("openlook")
}

pub fn settings_path() -> PathBuf {
    config_dir().join("settings.json")
}

pub fn tokens_path() -> PathBuf {
    config_dir().join("tokens.json")
}

/// One database per mailbox, plus a separate one for the demo account, so
/// signing in and out never mixes cached mail between accounts.
pub fn db_path(account_key: &str) -> PathBuf {
    data_dir().join(format!("{account_key}.db"))
}

/// Filesystem-safe key for an account name.
pub fn account_key(username: &str) -> String {
    if username.is_empty() {
        return "demo".into();
    }
    let cleaned: String = username
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '.' { c } else { '_' })
        .collect();
    cleaned.to_lowercase()
}

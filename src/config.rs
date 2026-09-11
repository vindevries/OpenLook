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
pub struct PipelineRef {
    pub id: String,
    pub label: String,
}

/// One connector the user has set up: which plugin, which part of it, and
/// what to call it in the folder pane.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectorRef {
    pub plugin: String,
    pub scope: String,
    pub label: String,
}

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
    /// Group the message list by conversation. Defaults to on.
    #[serde(default)]
    pub thread_view: Option<bool>,
    /// Announce new mail on the desktop. Defaults to on.
    #[serde(default)]
    pub notify_mail: Option<bool>,
    /// Widths of the folder pane and the message list, in pixels. Zero
    /// means "not set yet", so the defaults apply.
    #[serde(default)]
    pub pane_folders: i32,
    #[serde(default)]
    pub pane_list: i32,
    /// Connectors to show in the items pane. Each becomes its own section
    /// of the folder pane, with the connector's own sections as folders.
    #[serde(default)]
    pub connectors: Vec<ConnectorRef>,
    /// What HubSpot pipelines used to be listed under, before connectors
    /// were a thing. Read once, to carry an existing setup over.
    #[serde(default)]
    pub hubspot_pipelines: Vec<PipelineRef>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            client_id: DEFAULT_CLIENT_ID.into(),
            tenant: DEFAULT_TENANT.into(),
            demo_ack: false,
            thread_view: None,
            notify_mail: None,
            pane_folders: 0,
            pane_list: 0,
            connectors: Vec::new(),
            hubspot_pipelines: Vec::new(),
        }
    }
}

impl Settings {
    /// Conversations are grouped unless the user has turned it off.
    pub fn threaded(&self) -> bool {
        self.thread_view.unwrap_or(true)
    }

    /// New mail is announced unless the user has turned it off.
    pub fn notify_new_mail(&self) -> bool {
        self.notify_mail.unwrap_or(true)
    }

    /// Carry a HubSpot setup made before plugins existed over to the
    /// connector list, credential and all, so nothing has to be set up
    /// again.
    fn adopt_old_hubspot(&mut self) {
        if self.hubspot_pipelines.is_empty() {
            return;
        }
        if self.connectors.is_empty() {
            self.connectors = self
                .hubspot_pipelines
                .iter()
                .map(|pipeline| ConnectorRef {
                    plugin: "hubspot".into(),
                    scope: pipeline.id.clone(),
                    label: pipeline.label.clone(),
                })
                .collect();
        }
        let moved = credential_path("hubspot");
        if !moved.exists() {
            if let Ok(token) = fs::read_to_string(hubspot_token_path()) {
                let _ = save_credential("hubspot", token.trim());
            }
        }
        self.hubspot_pipelines.clear();
        let _ = self.save();
    }

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
        s.adopt_old_hubspot();
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

/// What a plugin was given to connect with, kept out of settings.json so
/// it is not caught up in anything the settings file gets used for.
pub fn credential_path(plugin: &str) -> PathBuf {
    config_dir().join("plugins").join(account_key(plugin)).join("credential")
}

/// The credential a plugin was set up with, if any.
pub fn credential(plugin: &str) -> Option<String> {
    fs::read_to_string(credential_path(plugin))
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

/// Write a plugin's credential, readable only by its owner.
pub fn save_credential(plugin: &str, value: &str) -> Result<()> {
    let path = credential_path(plugin);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(&path, value.trim())?;
    fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    Ok(())
}

/// Where the HubSpot key lived before plugins; read once, to carry it over.
pub fn hubspot_token_path() -> PathBuf {
    config_dir().join("hubspot-token")
}

/// The token, if one has been placed there.
pub fn hubspot_token() -> Option<String> {
    fs::read_to_string(hubspot_token_path())
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

/// Write the HubSpot key, readable only by its owner.
pub fn save_hubspot_token(token: &str) -> Result<()> {
    let path = hubspot_token_path();
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(&path, token.trim())?;
    fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    Ok(())
}

pub fn tokens_path() -> PathBuf {
    config_dir().join("tokens.json")
}

/// One database per mailbox, plus a separate one for the demo account, so
/// signing in and out never mixes cached mail between accounts.
pub fn db_path(account_key: &str) -> PathBuf {
    data_dir().join(format!("{account_key}.db"))
}

/// Downloaded attachments, one directory per mailbox. They are kept
/// rather than put in /tmp so an attachment opened once is still there
/// when the same mail is opened on a train with no signal.
pub fn attachments_dir(account_key: &str) -> PathBuf {
    data_dir().join("attachments").join(account_key)
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

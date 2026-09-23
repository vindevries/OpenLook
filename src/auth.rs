//! Microsoft identity platform device-code sign-in, for one or more mailboxes.
//!
//! The browser handles the password and any two-factor prompt; OpenLook only
//! ever sees the resulting tokens. They are cached in
//! `~/.config/openlook/tokens.json` (0600) and refreshed transparently.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};

use crate::config::{tokens_path, Settings};
use crate::model::AccountInfo;
use crate::util::now_unix;

/// Answering an invitation writes to the calendar — creating the
/// appointment, or letting Exchange move the one it already holds — so
/// reading it is not enough.
const SCOPES: &str = "openid profile email offline_access User.Read \
                      Mail.ReadWrite Mail.Send Calendars.ReadWrite";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Tokens {
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    /// Seconds since the epoch. Stored as a float so token files written by
    /// the earlier Python build still load.
    expires_at: f64,
    #[serde(default)]
    username: String,
    #[serde(default)]
    name: String,
}

/// On-disk shape. The older builds stored a single account at the top level;
/// [`Store::parse`] accepts both.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Store {
    accounts: Vec<Tokens>,
}

impl Store {
    fn parse(raw: &str) -> Store {
        if let Ok(store) = serde_json::from_str::<Store>(raw) {
            if !store.accounts.is_empty() {
                return store;
            }
        }
        // Single-account file from an earlier version.
        match serde_json::from_str::<Tokens>(raw) {
            Ok(single) if !single.refresh_token.is_empty() => Store { accounts: vec![single] },
            _ => Store::default(),
        }
    }
}

/// A device-code flow in progress; the user code is shown to the user.
#[derive(Debug, Clone)]
pub struct DeviceFlow {
    pub user_code: String,
    pub verification_uri: String,
    pub device_code: String,
    pub interval: u64,
    pub expires_in: i64,
    client_id: String,
    tenant: String,
}

#[derive(Deserialize)]
struct DeviceCodeResponse {
    user_code: String,
    device_code: String,
    verification_uri: String,
    #[serde(default = "default_interval")]
    interval: u64,
    #[serde(default = "default_expires")]
    expires_in: i64,
}

fn default_interval() -> u64 {
    5
}
fn default_expires() -> i64 {
    900
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default = "default_token_lifetime")]
    expires_in: i64,
}

fn default_token_lifetime() -> i64 {
    3600
}

#[derive(Deserialize)]
struct ErrorResponse {
    #[serde(default)]
    error: String,
    #[serde(default)]
    error_description: String,
}

pub struct Auth {
    http: reqwest::Client,
    store: Store,
}

impl Auth {
    pub fn load(http: reqwest::Client) -> Self {
        let mut store = fs::read_to_string(tokens_path())
            .map(|raw| Store::parse(&raw))
            .unwrap_or_default();
        store.accounts.retain(|t| !t.refresh_token.is_empty());
        Self { http, store }
    }

    /// Every signed-in mailbox, in the order they were added.
    pub fn accounts(&self) -> Vec<AccountInfo> {
        self.store
            .accounts
            .iter()
            .map(|t| AccountInfo { username: t.username.clone(), name: t.name.clone() })
            .collect()
    }

    fn save(&self) -> Result<()> {
        let path = tokens_path();
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(&path, serde_json::to_vec(&self.store)?)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        Ok(())
    }

    /// Forget one mailbox. Returns true when it was signed in.
    pub fn remove(&mut self, username: &str) -> bool {
        let before = self.store.accounts.len();
        self.store.accounts.retain(|t| t.username != username);
        let removed = self.store.accounts.len() != before;
        if removed {
            if self.store.accounts.is_empty() {
                let _ = fs::remove_file(tokens_path());
            } else {
                let _ = self.save();
            }
        }
        removed
    }

    pub fn sign_out_all(&mut self) {
        self.store.accounts.clear();
        let _ = fs::remove_file(tokens_path());
    }

    fn login_base(tenant: &str) -> String {
        format!("https://login.microsoftonline.com/{tenant}/oauth2/v2.0")
    }

    /// Ask Microsoft for a user code to display.
    pub async fn start_device_flow(&self) -> Result<DeviceFlow> {
        let settings = Settings::load();
        let resp = self
            .http
            .post(format!("{}/devicecode", Self::login_base(&settings.tenant)))
            .form(&[("client_id", settings.client_id.as_str()), ("scope", SCOPES)])
            .send()
            .await?;
        if !resp.status().is_success() {
            let err: ErrorResponse = resp.json().await.unwrap_or(ErrorResponse {
                error: "unknown".into(),
                error_description: "Could not start sign-in.".into(),
            });
            bail!(friendly_auth_error(&err));
        }
        let d: DeviceCodeResponse = resp.json().await?;
        Ok(DeviceFlow {
            user_code: d.user_code,
            verification_uri: d.verification_uri,
            device_code: d.device_code,
            interval: d.interval,
            expires_in: d.expires_in,
            client_id: settings.client_id,
            tenant: settings.tenant,
        })
    }

    /// Poll until the user finishes signing in (or the code expires).
    /// `cancel` lets the dialog abandon a flow the user walked away from.
    pub async fn poll_device_flow(
        &mut self,
        flow: &DeviceFlow,
        cancel: Arc<AtomicBool>,
    ) -> Result<AccountInfo> {
        let deadline = now_unix() + flow.expires_in;
        let mut interval = flow.interval;
        while now_unix() < deadline {
            tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
            if cancel.load(Ordering::Relaxed) {
                bail!("Sign-in cancelled.");
            }
            let resp = self
                .http
                .post(format!("{}/token", Self::login_base(&flow.tenant)))
                .form(&[
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                    ("client_id", flow.client_id.as_str()),
                    ("device_code", flow.device_code.as_str()),
                ])
                .send()
                .await?;
            if resp.status().is_success() {
                let t: TokenResponse = resp.json().await?;
                return self.finish_sign_in(t).await;
            }
            let err: ErrorResponse = resp.json().await.unwrap_or(ErrorResponse {
                error: "unknown".into(),
                error_description: String::new(),
            });
            match err.error.as_str() {
                "authorization_pending" => continue,
                "slow_down" => {
                    interval += 5;
                    continue;
                }
                _ => bail!(friendly_auth_error(&err)),
            }
        }
        bail!("The sign-in code expired. Try again.")
    }

    async fn finish_sign_in(&mut self, t: TokenResponse) -> Result<AccountInfo> {
        let mut tokens = Tokens {
            access_token: t.access_token,
            refresh_token: t.refresh_token,
            expires_at: (now_unix() + t.expires_in - 60) as f64,
            ..Default::default()
        };
        // Identify the mailbox for the folder pane and the cache filename.
        if let Ok(resp) = self
            .http
            .get("https://graph.microsoft.com/v1.0/me")
            .bearer_auth(&tokens.access_token)
            .send()
            .await
        {
            if let Ok(me) = resp.json::<serde_json::Value>().await {
                tokens.username = me["userPrincipalName"]
                    .as_str()
                    .or_else(|| me["mail"].as_str())
                    .unwrap_or_default()
                    .to_string();
                tokens.name = me["displayName"].as_str().unwrap_or_default().to_string();
            }
        }
        if tokens.username.is_empty() {
            bail!("Signed in, but Microsoft did not report which mailbox this is.");
        }
        let account = AccountInfo { username: tokens.username.clone(), name: tokens.name.clone() };
        // Re-adding an existing mailbox refreshes it rather than duplicating.
        self.store.accounts.retain(|t| t.username != tokens.username);
        self.store.accounts.push(tokens);
        self.save()?;
        Ok(account)
    }

    /// A valid access token for one mailbox, refreshed if needed.
    pub async fn token_for(&mut self, username: &str) -> Result<String> {
        let index = self
            .store
            .accounts
            .iter()
            .position(|t| t.username == username)
            .ok_or_else(|| anyhow!("{username} is not signed in."))?;
        let tokens = self.store.accounts[index].clone();
        if (now_unix() as f64) < tokens.expires_at {
            return Ok(tokens.access_token);
        }
        let settings = Settings::load();
        let resp = self
            .http
            .post(format!("{}/token", Self::login_base(&settings.tenant)))
            .form(&[
                ("grant_type", "refresh_token"),
                ("client_id", settings.client_id.as_str()),
                ("refresh_token", tokens.refresh_token.as_str()),
                ("scope", SCOPES),
            ])
            .send()
            .await?;
        if !resp.status().is_success() {
            let err: ErrorResponse = resp.json().await.unwrap_or(ErrorResponse {
                error: "invalid_grant".into(),
                error_description: String::new(),
            });
            bail!("Sign in again for {username}. {}", err.error_description);
        }
        let t: TokenResponse = resp.json().await?;
        let refreshed = Tokens {
            access_token: t.access_token.clone(),
            refresh_token: if t.refresh_token.is_empty() {
                tokens.refresh_token.clone()
            } else {
                t.refresh_token
            },
            expires_at: (now_unix() + t.expires_in - 60) as f64,
            username: tokens.username.clone(),
            name: tokens.name.clone(),
        };
        self.store.accounts[index] = refreshed;
        self.save()?;
        Ok(t.access_token)
    }
}

/// Turn Entra's raw error codes into something a user can act on.
fn friendly_auth_error(err: &ErrorResponse) -> String {
    let desc = err.error_description.split("Trace ID").next().unwrap_or("").trim().to_string();
    match err.error.as_str() {
        "authorization_declined" => "Sign-in was declined in the browser.".into(),
        "expired_token" => "The sign-in code expired. Try again.".into(),
        "unauthorized_client" | "invalid_client" => format!(
            "Your organization does not allow this sign-in app. Open Settings and \
             enter your own app registration's client ID. ({desc})"
        ),
        _ if !desc.is_empty() => desc,
        other => format!("Sign-in failed ({other})."),
    }
}

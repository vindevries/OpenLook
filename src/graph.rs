//! Microsoft Graph mail client.
//!
//! Errors are classified so the sync engine can tell "we are offline, keep
//! the change queued" apart from "the server rejected this permanently".

use std::fmt;
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::auth::Auth;
use crate::model::{Address, Body, Folder, MessageSummary, Outgoing, Pending, SendMode};
use crate::util::html_to_text;

const GRAPH: &str = "https://graph.microsoft.com/v1.0";
const SUMMARY_FIELDS: &str =
    "id,subject,from,receivedDateTime,bodyPreview,isRead,hasAttachments,parentFolderId";

/// Well-known folders first, in the order Outlook shows them.
const FOLDER_ORDER: [&str; 7] =
    ["Inbox", "Drafts", "Sent Items", "Outbox", "Deleted Items", "Junk Email", "Archive"];

pub fn folder_rank(display_name: &str) -> i64 {
    FOLDER_ORDER.iter().position(|n| *n == display_name).map(|i| i as i64).unwrap_or(100)
}

#[derive(Debug)]
pub enum GraphError {
    /// No usable network. The caller should keep pending work queued.
    Offline(String),
    /// Sign-in required or expired.
    Auth(String),
    /// The item no longer exists server-side.
    NotFound,
    /// Retry later (throttling or a server fault).
    Transient(String),
    /// Permanent rejection.
    Permanent(String),
}

impl fmt::Display for GraphError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GraphError::Offline(_) => write!(f, "You are offline — showing cached mail."),
            GraphError::Auth(m) => write!(f, "{m}"),
            GraphError::NotFound => write!(f, "That message no longer exists."),
            GraphError::Transient(m) => write!(f, "{m}"),
            GraphError::Permanent(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for GraphError {}

impl GraphError {
    pub fn is_offline(&self) -> bool {
        matches!(self, GraphError::Offline(_))
    }

    /// Underlying cause, shown as a tooltip on the status line.
    pub fn detail(&self) -> Option<&str> {
        match self {
            GraphError::Offline(m) | GraphError::Transient(m) | GraphError::Permanent(m) => {
                Some(m.as_str())
            }
            _ => None,
        }
    }
    /// Whether the queued operation should stay queued for a later attempt.
    pub fn should_retry(&self) -> bool {
        matches!(self, GraphError::Offline(_) | GraphError::Transient(_) | GraphError::Auth(_))
    }
}

impl From<reqwest::Error> for GraphError {
    fn from(e: reqwest::Error) -> Self {
        if e.is_connect() || e.is_timeout() || e.is_request() {
            GraphError::Offline(e.to_string())
        } else {
            GraphError::Transient(e.to_string())
        }
    }
}

pub type GraphResult<T> = Result<T, GraphError>;

/// One incremental change from a delta query.
pub enum Change {
    Upsert(MessageSummary),
    Removed(String),
}

pub struct DeltaPage {
    pub changes: Vec<Change>,
    /// Token to use for the next incremental sync, when the run completed.
    pub delta_link: Option<String>,
}

/// A Graph client bound to one signed-in mailbox.
#[derive(Clone)]
pub struct Graph {
    http: reqwest::Client,
    auth: Arc<Mutex<Auth>>,
    username: String,
}

impl Graph {
    pub fn new(http: reqwest::Client, auth: Arc<Mutex<Auth>>, username: String) -> Self {
        Self { http, auth, username }
    }

    async fn token(&self) -> GraphResult<String> {
        let mut auth = self.auth.lock().await;
        auth.token_for(&self.username).await.map_err(|e| GraphError::Auth(e.to_string()))
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> GraphResult<Option<Value>> {
        let token = self.token().await?;
        let resp = req.bearer_auth(token).send().await?;
        let status = resp.status();
        if status.is_success() {
            if status == reqwest::StatusCode::NO_CONTENT {
                return Ok(None);
            }
            let text = resp.text().await?;
            if text.is_empty() {
                return Ok(None);
            }
            return serde_json::from_str(&text)
                .map(Some)
                .map_err(|e| GraphError::Permanent(format!("Unreadable response: {e}")));
        }
        let code = status.as_u16();
        let body = resp.text().await.unwrap_or_default();
        let message = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
            .unwrap_or_else(|| body.chars().take(200).collect());
        Err(match code {
            401 | 403 => GraphError::Auth(format!("Sign in again — access was refused. {message}")),
            404 => GraphError::NotFound,
            408 | 429 | 500..=599 => GraphError::Transient(message),
            _ => GraphError::Permanent(message),
        })
    }

    async fn get(&self, url: &str) -> GraphResult<Value> {
        let url = if url.starts_with("http") { url.to_string() } else { format!("{GRAPH}{url}") };
        self.send(self.http.get(url)).await?.ok_or_else(|| GraphError::Transient("Empty response".into()))
    }

    pub async fn folders(&self) -> GraphResult<Vec<Folder>> {
        let data = self
            .get("/me/mailFolders?$top=100&$select=id,displayName,unreadItemCount,totalItemCount")
            .await?;
        let mut folders: Vec<Folder> = data["value"]
            .as_array()
            .map(|v| v.as_slice())
            .unwrap_or_default()
            .iter()
            .map(|f| Folder {
                id: f["id"].as_str().unwrap_or_default().to_string(),
                display_name: f["displayName"].as_str().unwrap_or("(folder)").to_string(),
                unread_count: f["unreadItemCount"].as_i64().unwrap_or(0),
                total_count: f["totalItemCount"].as_i64().unwrap_or(0),
            })
            .collect();
        folders.sort_by(|a, b| {
            folder_rank(&a.display_name)
                .cmp(&folder_rank(&b.display_name))
                .then_with(|| a.display_name.to_lowercase().cmp(&b.display_name.to_lowercase()))
        });
        Ok(folders)
    }

    /// Newest `top` messages in a folder — used for the first sync and as a
    /// fallback whenever a delta token is rejected.
    pub async fn messages_window(&self, folder_id: &str, top: u32) -> GraphResult<Vec<MessageSummary>> {
        let url = format!(
            "/me/mailFolders/{}/messages?$top={top}&$orderby=receivedDateTime desc&$select={SUMMARY_FIELDS}",
            urlencoding::encode(folder_id)
        );
        let data = self.get(&url).await?;
        Ok(data["value"]
            .as_array()
            .map(|v| v.as_slice())
            .unwrap_or_default()
            .iter()
            .map(|m| parse_summary(m, folder_id))
            .collect())
    }

    /// Incremental sync. Pass the stored delta link to get only what changed.
    pub async fn delta(
        &self,
        folder_id: &str,
        delta_link: Option<&str>,
        max_pages: usize,
    ) -> GraphResult<DeltaPage> {
        let mut url = match delta_link {
            Some(link) => link.to_string(),
            None => format!(
                "{GRAPH}/me/mailFolders/{}/messages/delta?$select={SUMMARY_FIELDS}",
                urlencoding::encode(folder_id)
            ),
        };
        let mut changes = Vec::new();
        for _ in 0..max_pages {
            let data = self.get(&url).await?;
            for item in data["value"].as_array().map(|v| v.as_slice()).unwrap_or_default() {
                let id = item["id"].as_str().unwrap_or_default().to_string();
                if id.is_empty() {
                    continue;
                }
                if item.get("@removed").is_some() {
                    changes.push(Change::Removed(id));
                } else {
                    changes.push(Change::Upsert(parse_summary(item, folder_id)));
                }
            }
            if let Some(next) = data["@odata.nextLink"].as_str() {
                url = next.to_string();
                continue;
            }
            return Ok(DeltaPage {
                changes,
                delta_link: data["@odata.deltaLink"].as_str().map(str::to_string),
            });
        }
        // Ran out of pages: keep what we have and resync fully next time.
        Ok(DeltaPage { changes, delta_link: None })
    }

    /// A delta token representing "now", so the first sync can seed the
    /// cache with a recent window instead of downloading the whole mailbox.
    pub async fn delta_token_latest(&self, folder_id: &str) -> GraphResult<Option<String>> {
        let url = format!(
            "/me/mailFolders/{}/messages/delta?$deltatoken=latest&$select={SUMMARY_FIELDS}",
            urlencoding::encode(folder_id)
        );
        let mut data = self.get(&url).await?;
        // Follow nextLinks (should be none for 'latest') until the token appears.
        for _ in 0..5 {
            if let Some(link) = data["@odata.deltaLink"].as_str() {
                return Ok(Some(link.to_string()));
            }
            match data["@odata.nextLink"].as_str() {
                Some(next) => data = self.get(next).await?,
                None => break,
            }
        }
        Ok(None)
    }

    /// Full message: recipients plus the body.
    pub async fn detail(&self, id: &str) -> GraphResult<(Vec<Address>, Vec<Address>, Body)> {
        let url = format!(
            "/me/messages/{}?$select=toRecipients,ccRecipients,body",
            urlencoding::encode(id)
        );
        let m = self.get(&url).await?;
        let to = parse_addresses(&m["toRecipients"]);
        let cc = parse_addresses(&m["ccRecipients"]);
        let body = Body {
            is_html: m["body"]["contentType"].as_str().unwrap_or("html") != "text",
            content: m["body"]["content"].as_str().unwrap_or_default().to_string(),
        };
        Ok((to, cc, body))
    }

    pub async fn set_read(&self, id: &str, is_read: bool) -> GraphResult<()> {
        let url = format!("{GRAPH}/me/messages/{}", urlencoding::encode(id));
        self.send(self.http.patch(url).json(&json!({ "isRead": is_read }))).await?;
        Ok(())
    }

    /// Move to Deleted Items, the way Outlook's Delete works — or delete for
    /// good when the message was already in the bin.
    pub async fn delete(&self, id: &str, purge: bool) -> GraphResult<()> {
        if purge {
            let url = format!("{GRAPH}/me/messages/{}", urlencoding::encode(id));
            self.send(self.http.delete(url)).await?;
            return Ok(());
        }
        let url = format!("{GRAPH}/me/messages/{}/move", urlencoding::encode(id));
        self.send(self.http.post(url).json(&json!({ "destinationId": "deleteditems" }))).await?;
        Ok(())
    }

    pub async fn send_mail(&self, msg: &Outgoing) -> GraphResult<()> {
        if let Some(original) = &msg.in_reply_to {
            let id = urlencoding::encode(original);
            let comment = msg.body.replace('\n', "<br>");
            match msg.mode {
                SendMode::Reply => {
                    let url = format!("{GRAPH}/me/messages/{id}/reply");
                    self.send(self.http.post(url).json(&json!({ "comment": comment }))).await?;
                    return Ok(());
                }
                SendMode::ReplyAll => {
                    let url = format!("{GRAPH}/me/messages/{id}/replyAll");
                    self.send(self.http.post(url).json(&json!({ "comment": comment }))).await?;
                    return Ok(());
                }
                SendMode::Forward => {
                    let url = format!("{GRAPH}/me/messages/{id}/forward");
                    let recipients: Vec<_> = msg
                        .to
                        .iter()
                        .map(|a| json!({ "emailAddress": { "address": a } }))
                        .collect();
                    self.send(
                        self.http
                            .post(url)
                            .json(&json!({ "comment": comment, "toRecipients": recipients })),
                    )
                    .await?;
                    return Ok(());
                }
                SendMode::New => {}
            }
        }
        let url = format!("{GRAPH}/me/sendMail");
        let payload = json!({
            "saveToSentItems": true,
            "message": {
                "subject": msg.subject,
                "body": { "contentType": "Text", "content": msg.body },
                "toRecipients": msg.to.iter().map(|a| json!({"emailAddress": {"address": a}})).collect::<Vec<_>>(),
                "ccRecipients": msg.cc.iter().map(|a| json!({"emailAddress": {"address": a}})).collect::<Vec<_>>(),
            }
        });
        self.send(self.http.post(url).json(&payload)).await?;
        Ok(())
    }
}

fn parse_address(v: &Value) -> Address {
    let email = &v["emailAddress"];
    Address {
        name: email["name"].as_str().unwrap_or_default().to_string(),
        address: email["address"].as_str().unwrap_or_default().to_string(),
    }
}

fn parse_addresses(v: &Value) -> Vec<Address> {
    v.as_array().map(|v| v.as_slice()).unwrap_or_default().iter().map(parse_address).collect()
}

fn parse_summary(m: &Value, folder_id: &str) -> MessageSummary {
    let preview = m["bodyPreview"].as_str().unwrap_or_default();
    MessageSummary {
        id: m["id"].as_str().unwrap_or_default().to_string(),
        folder_id: m["parentFolderId"].as_str().unwrap_or(folder_id).to_string(),
        subject: {
            let s = m["subject"].as_str().unwrap_or_default();
            if s.is_empty() { "(no subject)".into() } else { s.to_string() }
        },
        from: parse_address(&m["from"]),
        received: m["receivedDateTime"].as_str().unwrap_or_default().to_string(),
        preview: html_to_text(preview),
        is_read: m["isRead"].as_bool().unwrap_or(true),
        has_attachments: m["hasAttachments"].as_bool().unwrap_or(false),
        pending: Pending::None,
    }
}

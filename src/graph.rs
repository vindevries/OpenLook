//! Microsoft Graph mail client.
//!
//! Errors are classified so the sync engine can tell "we are offline, keep
//! the change queued" apart from "the server rejected this permanently".

use std::fmt;
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::auth::Auth;
use crate::model::{
    Address, Body, CalendarEvent, Folder, MessagePatch, MessageSummary, Outgoing, Pending, SendMode,
};
use crate::util::html_to_text;

const GRAPH: &str = "https://graph.microsoft.com/v1.0";
/// Messages per delta page. Graph's default is small; asking for more
/// cuts the number of round trips during the first enumeration.
const DELTA_PAGE_SIZE: u32 = 100;
/// How far back the first delta enumeration reaches. Without a bound Graph
/// walks the entire folder before it hands out a token — tens of thousands
/// of messages on a real mailbox — and until then every sync starts over.
/// Bounded, a token arrives in one request and syncs are incremental from
/// then on.
const DELTA_WINDOW_DAYS: i64 = 30;
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
    /// A message Graph has reported before: the entry carries only the
    /// properties that changed, so it patches the cached row instead of
    /// replacing it.
    Patch(MessagePatch),
    Removed(String),
}

pub struct DeltaPage {
    pub changes: Vec<Change>,
    /// Where to resume next time. Once the folder has been enumerated this
    /// is a deltaLink (only changes since last time); until then it is the
    /// nextLink partway through that enumeration, so a large folder picks up
    /// where it left off instead of starting over.
    pub cursor: Option<String>,
    /// Whether `cursor` is a deltaLink, i.e. the folder is caught up.
    pub complete: bool,
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
        self.read(resp).await
    }

    /// Turn a response into JSON, classifying failures for the sync engine.
    async fn read(&self, resp: reqwest::Response) -> GraphResult<Option<Value>> {
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

    async fn get_paged(&self, url: &str, page_size: u32) -> GraphResult<Value> {
        let url = if url.starts_with("http") { url.to_string() } else { format!("{GRAPH}{url}") };
        self.send(self.http.get(url).header("Prefer", format!("odata.maxpagesize={page_size}")))
            .await?
            .ok_or_else(|| GraphError::Transient("Empty response".into()))
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

    /// Fetch changes for a folder. Pass the stored cursor to resume: either
    /// a deltaLink (just what changed) or a nextLink partway through the
    /// initial enumeration. `max_pages` bounds the work done in one call.
    pub async fn delta(
        &self,
        folder_id: &str,
        cursor: Option<&str>,
        max_pages: usize,
    ) -> GraphResult<DeltaPage> {
        let mut url = match cursor {
            Some(link) => link.to_string(),
            None => {
                let since = (chrono::Utc::now() - chrono::Duration::days(DELTA_WINDOW_DAYS))
                    .format("%Y-%m-%dT%H:%M:%SZ")
                    .to_string();
                format!(
                    "{GRAPH}/me/mailFolders/{}/messages/delta?$select={SUMMARY_FIELDS}\
                     &$filter=receivedDateTime%20ge%20{since}",
                    urlencoding::encode(folder_id)
                )
            }
        };
        let mut changes = Vec::new();
        for _ in 0..max_pages {
            let data = self.get_paged(&url, DELTA_PAGE_SIZE).await?;
            for item in data["value"].as_array().map(|v| v.as_slice()).unwrap_or_default() {
                let id = item["id"].as_str().unwrap_or_default().to_string();
                if id.is_empty() {
                    continue;
                }
                if item.get("@removed").is_some() {
                    changes.push(Change::Removed(id));
                } else if item.get("receivedDateTime").is_some() {
                    changes.push(Change::Upsert(parse_summary(item, folder_id)));
                } else {
                    // Only the changed properties came back. Read as a whole
                    // message this would blank the date, sender and subject —
                    // which drops the message to the bottom of every list.
                    changes.push(Change::Patch(parse_patch(id, item)));
                }
            }
            if let Some(delta) = data["@odata.deltaLink"].as_str() {
                // Caught up: from here on, syncs are a single cheap request.
                return Ok(DeltaPage {
                    changes,
                    cursor: Some(delta.to_string()),
                    complete: true,
                });
            }
            match data["@odata.nextLink"].as_str() {
                Some(next) => url = next.to_string(),
                // Neither link: nothing more to read and no token to keep.
                None => return Ok(DeltaPage { changes, cursor: None, complete: false }),
            }
        }
        // Out of budget for this run; resume from this page next time.
        Ok(DeltaPage { changes, cursor: Some(url), complete: false })
    }

    /// Events overlapping a window. calendarView is the right endpoint here
    /// rather than /events: it expands recurring series into occurrences,
    /// which is what a month grid needs to show.
    pub async fn calendar_view(&self, start: &str, end: &str) -> GraphResult<Vec<CalendarEvent>> {
        let mut url = format!(
            "{GRAPH}/me/calendarView?startDateTime={start}&endDateTime={end}\
             &$select=id,subject,organizer,location,start,end,isAllDay,isCancelled,bodyPreview\
             &$orderby=start/dateTime&$top=200"
        );
        let mut events = Vec::new();
        for _ in 0..10 {
            let token = self.token().await?;
            // Ask for UTC so the times need no timezone guessing here.
            let resp = self
                .http
                .get(&url)
                .bearer_auth(token)
                .header("Prefer", "outlook.timezone=\"UTC\"")
                .send()
                .await?;
            let data = match self.read(resp).await? {
                Some(data) => data,
                None => break,
            };
            for item in data["value"].as_array().map(|v| v.as_slice()).unwrap_or_default() {
                let id = item["id"].as_str().unwrap_or_default().to_string();
                if id.is_empty() {
                    continue;
                }
                events.push(CalendarEvent {
                    id,
                    subject: {
                        let s = item["subject"].as_str().unwrap_or_default();
                        if s.is_empty() { "(no subject)".into() } else { s.to_string() }
                    },
                    organizer: item["organizer"]["emailAddress"]["name"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                    location: item["location"]["displayName"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                    start: graph_time(&item["start"]),
                    end: graph_time(&item["end"]),
                    all_day: item["isAllDay"].as_bool().unwrap_or(false),
                    cancelled: item["isCancelled"].as_bool().unwrap_or(false),
                    preview: html_to_text(item["bodyPreview"].as_str().unwrap_or_default()),
                    mailbox: self.username.clone(),
                });
            }
            match data["@odata.nextLink"].as_str() {
                Some(next) => url = next.to_string(),
                None => break,
            }
        }
        Ok(events)
    }

    /// The full appointment: its body and who was invited.
    pub async fn event_detail(&self, id: &str) -> GraphResult<(String, Vec<String>)> {
        let url = format!(
            "/me/events/{}?$select=body,attendees,onlineMeeting",
            urlencoding::encode(id)
        );
        let event = self.get(&url).await?;
        let body = event["body"]["content"].as_str().unwrap_or_default().to_string();
        let attendees = event["attendees"]
            .as_array()
            .map(|v| v.as_slice())
            .unwrap_or_default()
            .iter()
            .map(|a| {
                let email = &a["emailAddress"];
                email["name"]
                    .as_str()
                    .filter(|n| !n.is_empty())
                    .or_else(|| email["address"].as_str())
                    .unwrap_or_default()
                    .to_string()
            })
            .filter(|name| !name.is_empty())
            .collect();
        Ok((body, attendees))
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

    /// Move a message to another folder. Graph creates a new item in the
    /// destination, so the id changes; the new one comes back in the reply.
    pub async fn move_message(&self, id: &str, destination: &str) -> GraphResult<Option<String>> {
        let url = format!("{GRAPH}/me/messages/{}/move", urlencoding::encode(id));
        let moved =
            self.send(self.http.post(url).json(&json!({ "destinationId": destination }))).await?;
        Ok(moved.and_then(|v| v["id"].as_str().map(str::to_string)))
    }

    /// Move to Deleted Items, the way Outlook's Delete works — or delete for
    /// good when the message was already in the bin.
    pub async fn delete(&self, id: &str, purge: bool) -> GraphResult<Option<String>> {
        if purge {
            let url = format!("{GRAPH}/me/messages/{}", urlencoding::encode(id));
            self.send(self.http.delete(url)).await?;
            return Ok(None);
        }
        self.move_message(id, "deleteditems").await
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

/// Graph hands back `2026-09-09T10:00:00.0000000` with a separate timeZone
/// field; we ask for UTC, so trim the fraction and mark it as such.
fn graph_time(v: &Value) -> String {
    let raw = v["dateTime"].as_str().unwrap_or_default();
    let trimmed = raw.split('.').next().unwrap_or(raw);
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("{trimmed}Z")
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

/// Read a delta entry that carried only part of a message. Every field is
/// optional here precisely because a missing one means "unchanged", not
/// "empty".
fn parse_patch(id: String, m: &Value) -> MessagePatch {
    MessagePatch {
        id,
        folder_id: m["parentFolderId"].as_str().map(str::to_string),
        subject: m["subject"].as_str().map(|s| {
            if s.is_empty() { "(no subject)".to_string() } else { s.to_string() }
        }),
        from: m.get("from").filter(|v| !v.is_null()).map(parse_address),
        received: m["receivedDateTime"].as_str().map(str::to_string),
        preview: m["bodyPreview"].as_str().map(html_to_text),
        is_read: m["isRead"].as_bool(),
        has_attachments: m["hasAttachments"].as_bool(),
    }
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

//! HubSpot client: tickets, their conversation threads, and replying.
//!
//! Tickets reach a portal two ways, and OpenLook has to handle both:
//!
//!   * from the connected inbox — the customer's mail is a conversation
//!     thread, and a reply is another message on that thread;
//!   * from a web form — the text sits in the ticket's own `content`
//!     property with no thread at all, so answering means starting one.
//!
//! Shapes marked "unverified" are written from the published API and want
//! confirming against a real portal (see `src/bin/hubspot_probe.rs`).

use std::fmt;

use serde_json::Value;

const API: &str = "https://api.hubapi.com";

#[derive(Debug)]
pub enum Error {
    /// No usable network; the caller should keep queued work queued.
    Offline(String),
    /// Token missing, rejected, or lacking a scope.
    Auth(String),
    NotFound,
    /// Worth retrying — throttling (HubSpot returns 429) or a server fault.
    Transient(String),
    Permanent(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Offline(_) => write!(f, "You are offline — showing cached tickets."),
            Error::Auth(m) => write!(f, "HubSpot refused the token: {m}"),
            Error::NotFound => write!(f, "That ticket no longer exists."),
            Error::Transient(m) | Error::Permanent(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for Error {}

impl Error {
    pub fn is_offline(&self) -> bool {
        matches!(self, Error::Offline(_))
    }
    pub fn should_retry(&self) -> bool {
        matches!(self, Error::Offline(_) | Error::Transient(_) | Error::Auth(_))
    }
}

impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Self {
        if e.is_connect() || e.is_timeout() || e.is_request() {
            Error::Offline(e.to_string())
        } else {
            Error::Transient(e.to_string())
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// A stage in the ticket pipeline. These become folders in the pane.
#[derive(Debug, Clone)]
pub struct Stage {
    pub id: String,
    pub label: String,
    pub display_order: i64,
    pub closed: bool,
}

/// A ticket, as shown in the message list.
#[derive(Debug, Clone)]
pub struct Ticket {
    pub id: String,
    pub subject: String,
    /// Body of a web-form ticket; empty when the ticket came from mail.
    pub content: String,
    pub stage_id: String,
    pub created: String,
    pub updated: String,
    pub contact_ids: Vec<String>,
    pub thread_ids: Vec<String>,
}

/// One message in a ticket's conversation thread.
#[derive(Debug, Clone)]
pub struct ThreadMessage {
    pub id: String,
    pub sender: String,
    pub sender_email: String,
    pub sent_at: String,
    /// HTML when the channel provides it, otherwise plain text.
    pub body: String,
    pub is_html: bool,
    /// True when it came from the portal rather than the customer.
    pub outgoing: bool,
    pub attachments: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Contact {
    pub id: String,
    pub name: String,
    pub email: String,
}

#[derive(Clone)]
pub struct HubSpot {
    http: reqwest::Client,
    token: String,
}

impl HubSpot {
    pub fn new(http: reqwest::Client, token: String) -> Self {
        Self { http, token }
    }

    async fn get(&self, path: &str) -> Result<Value> {
        let url = if path.starts_with("http") { path.to_string() } else { format!("{API}{path}") };
        let resp = self.http.get(url).bearer_auth(&self.token).send().await?;
        self.read(resp).await
    }

    async fn post(&self, path: &str, body: &Value) -> Result<Value> {
        let resp = self
            .http
            .post(format!("{API}{path}"))
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .await?;
        self.read(resp).await
    }

    async fn read(&self, resp: reqwest::Response) -> Result<Value> {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if status.is_success() {
            if text.is_empty() {
                return Ok(Value::Null);
            }
            return serde_json::from_str(&text)
                .map_err(|e| Error::Permanent(format!("Unreadable response: {e}")));
        }
        let message = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v["message"].as_str().map(str::to_string))
            .unwrap_or_else(|| text.chars().take(200).collect());
        Err(match status.as_u16() {
            401 | 403 => Error::Auth(message),
            404 => Error::NotFound,
            429 | 500..=599 => Error::Transient(message),
            _ => Error::Permanent(message),
        })
    }

    /// Ticket pipeline stages, which become the folder list.
    pub async fn stages(&self) -> Result<Vec<Stage>> {
        let data = self.get("/crm/v3/pipelines/tickets").await?;
        let mut stages = Vec::new();
        for pipeline in data["results"].as_array().map(|v| v.as_slice()).unwrap_or_default() {
            for stage in pipeline["stages"].as_array().map(|v| v.as_slice()).unwrap_or_default() {
                stages.push(Stage {
                    id: stage["id"].as_str().unwrap_or_default().to_string(),
                    label: stage["label"].as_str().unwrap_or("(stage)").to_string(),
                    display_order: stage["displayOrder"].as_i64().unwrap_or(0),
                    closed: stage["metadata"]["isClosed"].as_str() == Some("true"),
                });
            }
        }
        stages.sort_by_key(|s| s.display_order);
        Ok(stages)
    }

    /// Tickets, newest activity first, with the associations needed to find
    /// the customer and the thread.
    pub async fn tickets(&self, limit: u32) -> Result<Vec<Ticket>> {
        let path = format!(
            "/crm/v3/objects/tickets?limit={limit}\
             &properties=subject,content,hs_pipeline_stage,createdate,hs_lastmodifieddate\
             &associations=contacts"
        );
        let data = self.get(&path).await?;
        let mut tickets = Vec::new();
        for item in data["results"].as_array().map(|v| v.as_slice()).unwrap_or_default() {
            let props = &item["properties"];
            tickets.push(Ticket {
                id: item["id"].as_str().unwrap_or_default().to_string(),
                subject: props["subject"].as_str().unwrap_or("(no subject)").to_string(),
                content: props["content"].as_str().unwrap_or_default().to_string(),
                stage_id: props["hs_pipeline_stage"].as_str().unwrap_or_default().to_string(),
                created: props["createdate"].as_str().unwrap_or_default().to_string(),
                updated: props["hs_lastmodifieddate"].as_str().unwrap_or_default().to_string(),
                contact_ids: ids_from_associations(&item["associations"]["contacts"]),
                // Threads are not returned inline; fetched separately.
                thread_ids: Vec::new(),
            });
        }
        Ok(tickets)
    }

    /// Conversation threads associated with a ticket. A web-form ticket has
    /// none until someone replies. (unverified: association type name)
    pub async fn ticket_threads(&self, ticket_id: &str) -> Result<Vec<String>> {
        let path = format!("/crm/v4/objects/tickets/{ticket_id}/associations/conversations");
        let data = self.get(&path).await?;
        Ok(data["results"]
            .as_array()
            .map(|v| v.as_slice())
            .unwrap_or_default()
            .iter()
            .filter_map(|r| r["toObjectId"].as_str().map(str::to_string).or_else(|| {
                r["toObjectId"].as_i64().map(|n| n.to_string())
            }))
            .collect())
    }

    /// Every message on a thread, oldest first — the mail thread itself.
    pub async fn thread_messages(&self, thread_id: &str) -> Result<Vec<ThreadMessage>> {
        let path = format!("/conversations/v3/conversations/threads/{thread_id}/messages");
        let data = self.get(&path).await?;
        let mut messages = Vec::new();
        for item in data["results"].as_array().map(|v| v.as_slice()).unwrap_or_default() {
            // Skip system entries; only actual messages carry text.
            if item["type"].as_str() == Some("SYSTEM") {
                continue;
            }
            let sender = item["senders"].as_array().and_then(|s| s.first()).cloned().unwrap_or(Value::Null);
            messages.push(ThreadMessage {
                id: item["id"].as_str().unwrap_or_default().to_string(),
                sender: sender["name"].as_str().unwrap_or_default().to_string(),
                sender_email: sender["deliveryIdentifier"]["value"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                sent_at: item["createdAt"].as_str().unwrap_or_default().to_string(),
                body: item["richText"]
                    .as_str()
                    .or_else(|| item["text"].as_str())
                    .unwrap_or_default()
                    .to_string(),
                is_html: item["richText"].is_string(),
                outgoing: item["direction"].as_str() == Some("OUTGOING"),
                attachments: item["attachments"]
                    .as_array()
                    .map(|v| v.as_slice())
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|a| a["name"].as_str().map(str::to_string))
                    .collect(),
            });
        }
        messages.sort_by(|a, b| a.sent_at.cmp(&b.sent_at));
        Ok(messages)
    }

    pub async fn contact(&self, id: &str) -> Result<Contact> {
        let path = format!("/crm/v3/objects/contacts/{id}?properties=email,firstname,lastname");
        let data = self.get(&path).await?;
        let props = &data["properties"];
        let name = [props["firstname"].as_str().unwrap_or(""), props["lastname"].as_str().unwrap_or("")]
            .join(" ")
            .trim()
            .to_string();
        Ok(Contact {
            id: data["id"].as_str().unwrap_or_default().to_string(),
            email: props["email"].as_str().unwrap_or_default().to_string(),
            name,
        })
    }

    /// Reply on an existing thread. HubSpot sends it through the connected
    /// inbox, so it is logged against the ticket automatically.
    /// (unverified: exact payload for the connected email channel)
    pub async fn reply(&self, thread_id: &str, html: &str) -> Result<()> {
        let path = format!("/conversations/v3/conversations/threads/{thread_id}/messages");
        let payload = serde_json::json!({
            "type": "MESSAGE",
            "text": crate::util::html_to_text(html),
            "richText": html,
        });
        self.post(&path, &payload).await?;
        Ok(())
    }
}

fn ids_from_associations(node: &Value) -> Vec<String> {
    node["results"]
        .as_array()
        .map(|v| v.as_slice())
        .unwrap_or_default()
        .iter()
        .filter_map(|r| {
            r["id"].as_str().map(str::to_string).or_else(|| r["id"].as_i64().map(|n| n.to_string()))
        })
        .collect()
}

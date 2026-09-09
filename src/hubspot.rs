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
/// Ticket→conversation association that actually yields a conversation
/// thread. A ticket is also associated with type 278, whose ids the
/// conversations API does not serve — following those produced a 404 per
/// ticket and inflated the apparent number of threads.
const THREAD_ASSOCIATION: i64 = 32;

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
            Error::NotFound => write!(f, "HubSpot no longer has that item."),
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
    pub pipeline_id: String,
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
    /// How the ticket reached HubSpot, e.g. EMAIL or FORM.
    pub source: String,
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

    /// Pipelines, for the settings list.
    pub async fn pipelines(&self) -> Result<Vec<(String, String)>> {
        let data = self.get("/crm/v3/pipelines/tickets").await?;
        Ok(data["results"]
            .as_array()
            .map(|v| v.as_slice())
            .unwrap_or_default()
            .iter()
            .map(|p| {
                (
                    p["id"].as_str().unwrap_or_default().to_string(),
                    p["label"].as_str().unwrap_or("(pipeline)").to_string(),
                )
            })
            .collect())
    }

    /// Stages of one pipeline. Labels repeat across pipelines, so a stage
    /// list is only meaningful scoped to its own.
    pub async fn stages_of(&self, pipeline_id: &str) -> Result<Vec<Stage>> {
        Ok(self.stages().await?.into_iter().filter(|s| s.pipeline_id == pipeline_id).collect())
    }

    /// Ticket pipeline stages, which become the folder list.
    pub async fn stages(&self) -> Result<Vec<Stage>> {
        let data = self.get("/crm/v3/pipelines/tickets").await?;
        let mut stages = Vec::new();
        for pipeline in data["results"].as_array().map(|v| v.as_slice()).unwrap_or_default() {
            for stage in pipeline["stages"].as_array().map(|v| v.as_slice()).unwrap_or_default() {
                stages.push(Stage {
                    id: stage["id"].as_str().unwrap_or_default().to_string(),
                    pipeline_id: pipeline["id"].as_str().unwrap_or_default().to_string(),
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
                source: props["source_type"].as_str().unwrap_or_default().to_string(),
                contact_ids: ids_from_associations(&item["associations"]["contacts"]),
                // Threads are not returned inline; fetched separately.
                thread_ids: Vec::new(),
            });
        }
        Ok(tickets)
    }

    /// The working set for a pipeline: every open ticket, plus the most
    /// recently touched closed ones for context and search.
    ///
    /// Fetching by recency alone is not enough — a ticket parked "on hold"
    /// months ago would fall off the end — and the plain list endpoint is
    /// worse still, returning oldest first, which on a real portal means
    /// pages of tickets closed years ago.
    pub async fn tickets_for_pipeline(
        &self,
        pipeline_id: &str,
        closed_stages: &[String],
        recent_closed: usize,
    ) -> Result<Vec<Ticket>> {
        let mut out = Vec::new();
        // Everything not in a closed stage, however old.
        let open_filter = serde_json::json!([{
            "filters": [
                { "propertyName": "hs_pipeline", "operator": "EQ", "value": pipeline_id },
                { "propertyName": "hs_pipeline_stage", "operator": "NOT_IN", "values": closed_stages },
            ]
        }]);
        out.extend(self.search_tickets(&open_filter, usize::MAX).await?);

        if recent_closed > 0 && !closed_stages.is_empty() {
            let closed_filter = serde_json::json!([{
                "filters": [
                    { "propertyName": "hs_pipeline", "operator": "EQ", "value": pipeline_id },
                    { "propertyName": "hs_pipeline_stage", "operator": "IN", "values": closed_stages },
                ]
            }]);
            out.extend(self.search_tickets(&closed_filter, recent_closed).await?);
        }

        // Search does not return associations; fetch them for what we kept.
        let ids: Vec<String> = out.iter().map(|t| t.id.clone()).collect();
        let contacts = self.ticket_contacts(&ids).await.unwrap_or_default();
        for ticket in &mut out {
            if let Some(found) = contacts.get(&ticket.id) {
                ticket.contact_ids = found.clone();
            }
        }
        Ok(out)
    }

    /// Search, newest activity first, following pages up to `limit`.
    async fn search_tickets(&self, filter_groups: &Value, limit: usize) -> Result<Vec<Ticket>> {
        let mut out = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let mut payload = serde_json::json!({
                "filterGroups": filter_groups,
                "sorts": [{ "propertyName": "hs_lastmodifieddate", "direction": "DESCENDING" }],
                "properties": ["subject", "content", "hs_pipeline", "hs_pipeline_stage",
                               "createdate", "hs_lastmodifieddate", "source_type"],
                "limit": 100,
            });
            if let Some(cursor) = &after {
                payload["after"] = Value::String(cursor.clone());
            }
            let data = self.post("/crm/v3/objects/tickets/search", &payload).await?;
            for item in data["results"].as_array().map(|v| v.as_slice()).unwrap_or_default() {
                let props = &item["properties"];
                out.push(Ticket {
                    id: item["id"].as_str().unwrap_or_default().to_string(),
                    subject: props["subject"].as_str().unwrap_or("(no subject)").to_string(),
                    content: props["content"].as_str().unwrap_or_default().to_string(),
                    stage_id: props["hs_pipeline_stage"].as_str().unwrap_or_default().to_string(),
                    created: props["createdate"].as_str().unwrap_or_default().to_string(),
                    updated: props["hs_lastmodifieddate"].as_str().unwrap_or_default().to_string(),
                    source: props["source_type"].as_str().unwrap_or_default().to_string(),
                    contact_ids: Vec::new(),
                    thread_ids: Vec::new(),
                });
                if out.len() >= limit {
                    return Ok(out);
                }
            }
            match data["paging"]["next"]["after"].as_str() {
                Some(cursor) => after = Some(cursor.to_string()),
                None => return Ok(out),
            }
        }
    }

    /// Contact ids per ticket, in batches rather than one call each.
    pub async fn ticket_contacts(
        &self,
        ticket_ids: &[String],
    ) -> Result<std::collections::HashMap<String, Vec<String>>> {
        let mut map = std::collections::HashMap::new();
        for chunk in ticket_ids.chunks(100) {
            let payload = serde_json::json!({
                "inputs": chunk.iter().map(|id| serde_json::json!({ "id": id })).collect::<Vec<_>>(),
            });
            let data = self.post("/crm/v4/associations/tickets/contacts/batch/read", &payload).await?;
            for row in data["results"].as_array().map(|v| v.as_slice()).unwrap_or_default() {
                let from = row["from"]["id"].as_str().unwrap_or_default().to_string();
                let to: Vec<String> = row["to"]
                    .as_array()
                    .map(|v| v.as_slice())
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|t| {
                        t["toObjectId"]
                            .as_str()
                            .map(str::to_string)
                            .or_else(|| t["toObjectId"].as_i64().map(|n| n.to_string()))
                    })
                    .collect();
                if !from.is_empty() {
                    map.insert(from, to);
                }
            }
        }
        Ok(map)
    }

    /// Contacts in one request rather than one each.
    pub async fn contacts_batch(&self, ids: &[String]) -> Result<Vec<Contact>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let payload = serde_json::json!({
            "properties": ["email", "firstname", "lastname"],
            "inputs": ids.iter().map(|id| serde_json::json!({ "id": id })).collect::<Vec<_>>(),
        });
        let data = self.post("/crm/v3/objects/contacts/batch/read", &payload).await?;
        Ok(data["results"]
            .as_array()
            .map(|v| v.as_slice())
            .unwrap_or_default()
            .iter()
            .map(|c| {
                let props = &c["properties"];
                let name = [props["firstname"].as_str().unwrap_or(""), props["lastname"].as_str().unwrap_or("")]
                    .join(" ")
                    .trim()
                    .to_string();
                Contact {
                    id: c["id"].as_str().unwrap_or_default().to_string(),
                    email: props["email"].as_str().unwrap_or_default().to_string(),
                    name,
                }
            })
            .collect())
    }

    /// Conversation threads associated with a ticket. A web-form ticket has
    /// none until someone replies.
    pub async fn ticket_threads(&self, ticket_id: &str) -> Result<Vec<String>> {
        let path = format!("/crm/v4/objects/tickets/{ticket_id}/associations/conversations");
        let data = self.get(&path).await?;
        Ok(data["results"]
            .as_array()
            .map(|v| v.as_slice())
            .unwrap_or_default()
            .iter()
            .filter(|entry| {
                entry["associationTypes"]
                    .as_array()
                    .map(|types| {
                        types.iter().any(|t| t["typeId"].as_i64() == Some(THREAD_ASSOCIATION))
                    })
                    .unwrap_or(false)
            })
            .filter_map(|entry| {
                entry["toObjectId"]
                    .as_str()
                    .map(str::to_string)
                    .or_else(|| entry["toObjectId"].as_i64().map(|n| n.to_string()))
            })
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

/// Build the reading-pane document for a ticket: its description when it
/// has one, then each associated thread in turn. Threads stay labelled
/// rather than being merged into one stream, because a ticket routinely
/// has several and it matters which exchange a message belongs to.
pub fn assemble_ticket_html(description: &str, threads: &[(String, Vec<ThreadMessage>)]) -> String {
    let mut html = String::new();
    let has_messages = threads.iter().any(|(_, m)| !m.is_empty());
    if !description.trim().is_empty() {
        html.push_str(&format!(
            "<div class='ol-block'><div class='ol-head'>Ticket description</div>{}</div>",
            description
        ));
    }
    let labelled = threads.iter().filter(|(_, m)| !m.is_empty()).count() > 1;
    let mut shown = 0;
    for (_thread_id, messages) in threads.iter().filter(|(_, m)| !m.is_empty()) {
        shown += 1;
        if labelled {
            let started = messages.first().map(|m| m.sent_at.as_str()).unwrap_or("");
            html.push_str(&format!(
                "<div class='ol-thread'>Thread {shown} · started {}</div>",
                crate::util::escape_html(started)
            ));
        }
        for m in messages {
            let who = if m.sender.trim().is_empty() { &m.sender_email } else { &m.sender };
            let attachments = if m.attachments.is_empty() {
                String::new()
            } else {
                format!(" · {} attachment(s): {}", m.attachments.len(), m.attachments.join(", "))
            };
            html.push_str(&format!(
                "<div class='ol-block'><div class='ol-head'>{} {} · {}{}</div>{}</div>",
                if m.outgoing { "&rarr;" } else { "&larr;" },
                crate::util::escape_html(who),
                crate::util::escape_html(&m.sent_at),
                crate::util::escape_html(&attachments),
                if m.is_html {
                    m.body.clone()
                } else {
                    format!("<pre>{}</pre>", crate::util::escape_html(&m.body))
                }
            ));
        }
    }
    if !has_messages && description.trim().is_empty() {
        html.push_str("<p>No correspondence on this ticket yet.</p>");
    }
    html
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(sender: &str, outgoing: bool, body: &str) -> ThreadMessage {
        ThreadMessage {
            id: "m".into(),
            sender: sender.into(),
            sender_email: format!("{sender}@example.com"),
            sent_at: "2026-09-09T10:00:00Z".into(),
            body: body.into(),
            is_html: true,
            outgoing,
            attachments: Vec::new(),
        }
    }

    #[test]
    fn several_threads_stay_labelled() {
        let threads = vec![
            ("t1".to_string(), vec![message("customer", false, "<p>first</p>")]),
            ("t2".to_string(), vec![message("support", true, "<p>second</p>")]),
        ];
        let html = assemble_ticket_html("", &threads);
        assert_eq!(html.matches("ol-thread").count(), 2, "each thread is labelled");
        assert!(html.contains("first") && html.contains("second"));
    }

    #[test]
    fn a_single_thread_needs_no_label() {
        let threads = vec![("t1".to_string(), vec![message("customer", false, "<p>hello</p>")])];
        let html = assemble_ticket_html("", &threads);
        assert!(!html.contains("ol-thread"), "one thread reads as a plain conversation");
        assert!(html.contains("hello"));
    }

    #[test]
    fn a_web_form_ticket_shows_its_description() {
        // No thread yet: the description is all there is to read.
        let html = assemble_ticket_html("<p>printer on fire</p>", &[]);
        assert!(html.contains("Ticket description"));
        assert!(html.contains("printer on fire"));
        assert!(!html.contains("No correspondence"));
    }

    #[test]
    fn an_empty_ticket_says_so_rather_than_rendering_blank() {
        assert!(assemble_ticket_html("", &[]).contains("No correspondence"));
    }
}

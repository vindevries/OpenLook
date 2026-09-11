//! HubSpot tickets, as an OpenLook plugin.
//!
//! Reads one JSON request per line on standard input and answers on
//! standard output; see src/plugin.rs for the conversation. Nothing about
//! HubSpot is compiled into OpenLook itself — this program is the whole of
//! it, and a different helpdesk is a different program.

use std::io::Write;

use openlook::connector::{Connector, Error};
use openlook::hubspot::{HubSpot, TicketPipeline};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut client: Option<HubSpot> = None;
    let mut pipeline: Option<TicketPipeline> = None;

    while let Some(line) = lines.next_line().await? {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        let Ok(request) = serde_json::from_str::<Value>(&line) else {
            eprintln!("ignoring a request that is not JSON");
            continue;
        };
        let id = request["id"].as_u64().unwrap_or(0);
        let method = request["method"].as_str().unwrap_or_default().to_string();
        let params = request["params"].clone();

        let answer = match method.as_str() {
            "initialize" => {
                let token = params["credential"].as_str().unwrap_or_default().to_string();
                let scope = params["scope"].as_str().unwrap_or_default().to_string();
                let http = reqwest::Client::builder().build()?;
                let connected = HubSpot::new(http, token);
                if !scope.is_empty() {
                    pipeline = Some(TicketPipeline::new(connected.clone(), scope));
                }
                client = Some(connected);
                Ok(json!({ "version": openlook::plugin::PROTOCOL_VERSION, "name": "HubSpot" }))
            }
            "scopes" => match &client {
                // Each ticket pipeline is something that can be added.
                Some(client) => client
                    .pipelines()
                    .await
                    .map(|pipelines| {
                        Value::Array(
                            pipelines
                                .into_iter()
                                .map(|(id, name)| json!({ "id": id, "name": name }))
                                .collect(),
                        )
                    })
                    .map_err(Error::from),
                None => Err(Error::Permanent("not initialized".into())),
            },
            "sections" => match &pipeline {
                Some(pipeline) => pipeline.sections().await.map(|sections| {
                    Value::Array(
                        sections
                            .into_iter()
                            .map(|s| json!({ "id": s.id, "name": s.name }))
                            .collect(),
                    )
                }),
                None => Err(Error::Permanent("no pipeline chosen".into())),
            },
            "items" => match &pipeline {
                Some(pipeline) => pipeline.items().await.map(|items| {
                    Value::Array(
                        items
                            .into_iter()
                            .map(|item| {
                                json!({
                                    "id": item.id,
                                    "section": item.folder_id,
                                    "subject": item.subject,
                                    "from_name": item.from.name,
                                    "from_address": item.from.address,
                                    "received": item.received,
                                    "preview": item.preview,
                                    "read": item.is_read,
                                })
                            })
                            .collect(),
                    )
                }),
                None => Err(Error::Permanent("no pipeline chosen".into())),
            },
            "body" => match &pipeline {
                Some(pipeline) => {
                    // Only the parts a body is assembled from are needed.
                    let mut item = openlook::model::MessageSummary::empty();
                    item.id = params["id"].as_str().unwrap_or_default().to_string();
                    item.preview = params["preview"].as_str().unwrap_or_default().to_string();
                    pipeline.body(&item).await.map(|content| {
                        json!({
                            "is_html": content.body.is_html,
                            "content": content.body.content,
                            "threads": content.threads,
                        })
                    })
                }
                None => Err(Error::Permanent("no pipeline chosen".into())),
            },
            other => Err(Error::Permanent(format!("no such method: {other}"))),
        };

        let reply = match answer {
            Ok(result) => json!({ "id": id, "result": result }),
            Err(e) => json!({ "id": id, "error": { "kind": kind_of(&e), "message": e.to_string() } }),
        };
        let mut out = std::io::stdout().lock();
        writeln!(out, "{reply}")?;
        out.flush()?;
    }
    Ok(())
}

/// Say which sort of failure this is, so OpenLook knows whether to wait,
/// ask for a new key, or give up.
fn kind_of(e: &Error) -> &'static str {
    match e {
        Error::Offline(_) => "offline",
        Error::Auth(_) => "auth",
        Error::NotFound => "not_found",
        Error::Transient(_) => "transient",
        Error::Permanent(_) => "permanent",
    }
}

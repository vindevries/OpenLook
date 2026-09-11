//! Connectors that live outside the application.
//!
//! A plugin is a program OpenLook starts and talks to over its standard
//! input and output, one JSON object per line. That keeps a plugin at
//! arm's length: it can be written in any language, updated on its own,
//! and a crash or a hang in it costs a pane rather than the mail client.
//!
//! The conversation is small enough to read in one go. OpenLook sends
//!
//! ```text
//! {"id":1,"method":"initialize","params":{"version":1,"credential":"…","scope":"…"}}
//! {"id":2,"method":"scopes"}      what can be added (pipelines, queues, boards)
//! {"id":3,"method":"sections"}    the folder-pane entries
//! {"id":4,"method":"items"}       every row worth showing
//! {"id":5,"method":"body","params":{"id":"…","preview":"…"}}
//! ```
//!
//! and the plugin answers `{"id":n,"result":…}` or `{"id":n,"error":{"kind":
//! "offline|auth|not_found|transient|permanent","message":"…"}}`. Anything
//! a plugin writes to standard error is logged, so it can say what it is
//! doing without disturbing the protocol.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

use crate::connector::{Connector, Error, ItemBody, Result, Section, Task};
use crate::model::{Address, Body, MessageSummary, Pending};

/// The version of the conversation described above. A plugin that answers
/// `initialize` with a different one is not talked to further.
pub const PROTOCOL_VERSION: u32 = 1;

/// What a plugin says about itself, read from `plugin.json` beside it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    /// Short name, used in settings and in the cache's file name.
    pub id: String,
    /// What to call it on screen.
    pub name: String,
    /// What its items are called: "tickets", "issues", "conversations".
    #[serde(default = "default_items_called")]
    pub items_called: String,
    /// The program to run, and any arguments it needs. A bare name is
    /// looked up on PATH; a relative path is taken from the manifest's
    /// own directory, so a plugin can ship as one self-contained folder.
    pub exec: Vec<String>,
    /// What the user has to paste in to connect, if anything.
    #[serde(default)]
    pub credential: Option<Credential>,
    /// Filled in when the manifest is read.
    #[serde(skip)]
    pub directory: PathBuf,
}

fn default_items_called() -> String {
    "items".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Credential {
    /// The prompt: "Private app key", "API token".
    pub label: String,
    /// Where to find it, shown under the field.
    #[serde(default)]
    pub help: String,
}

impl Manifest {
    /// The command to run, resolved against the plugin's own directory.
    fn program(&self) -> PathBuf {
        let first = self.exec.first().map(String::as_str).unwrap_or_default();
        let path = Path::new(first);
        if path.is_absolute() || first.contains('/') {
            self.directory.join(path)
        } else {
            PathBuf::from(first)
        }
    }
}

/// Where plugins are looked for, nearest first: the user's own, then those
/// installed system-wide, then any sitting beside the running binary,
/// which is what makes a plugin usable straight from a build directory.
fn search_path() -> Vec<PathBuf> {
    let mut places = vec![
        crate::config::config_dir().join("plugins"),
        PathBuf::from("/usr/share/openlook/plugins"),
        PathBuf::from("/usr/local/share/openlook/plugins"),
    ];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            places.push(dir.join("plugins"));
        }
    }
    places
}

/// Every plugin that can be found, one entry per id — the first copy
/// found wins, so a user's own build shadows the packaged one.
pub fn manifests() -> Vec<Manifest> {
    let mut found: Vec<Manifest> = Vec::new();
    for place in search_path() {
        let Ok(entries) = std::fs::read_dir(&place) else { continue };
        for entry in entries.flatten() {
            let manifest = entry.path().join("plugin.json");
            let Ok(raw) = std::fs::read_to_string(&manifest) else { continue };
            let Ok(mut parsed) = serde_json::from_str::<Manifest>(&raw) else {
                eprintln!("openlook: {} is not a readable plugin manifest", manifest.display());
                continue;
            };
            parsed.directory = entry.path();
            if !found.iter().any(|m| m.id == parsed.id) {
                found.push(parsed);
            }
        }
    }
    found
}

pub fn manifest(id: &str) -> Option<Manifest> {
    manifests().into_iter().find(|m| m.id == id)
}

/// A running plugin: the process, and the conversation with it.
struct Session {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

/// One configured plugin — a manifest, the credential it was given, and
/// which scope of it to show.
pub struct Plugin {
    manifest: Manifest,
    credential: String,
    scope: String,
    session: Mutex<Option<Session>>,
}

impl Plugin {
    pub fn new(manifest: Manifest, credential: String, scope: String) -> Self {
        Self { manifest, credential, scope, session: Mutex::new(None) }
    }

    /// Ask a plugin what can be added, without keeping it running. Used by
    /// the dialog that sets one up.
    pub async fn scopes(
        manifest: &Manifest,
        credential: &str,
    ) -> Result<Vec<(String, String)>> {
        let plugin = Plugin::new(manifest.clone(), credential.to_string(), String::new());
        let listed = plugin.call("scopes", json!({})).await?;
        plugin.shutdown().await;
        Ok(listed
            .as_array()
            .map(|v| v.as_slice())
            .unwrap_or_default()
            .iter()
            .filter_map(|scope| {
                Some((
                    scope["id"].as_str()?.to_string(),
                    scope["name"].as_str().unwrap_or_default().to_string(),
                ))
            })
            .collect())
    }

    async fn shutdown(&self) {
        if let Some(mut session) = self.session.lock().await.take() {
            let _ = session.stdin.shutdown().await;
            let _ = session.child.kill().await;
        }
    }

    /// Send one request and wait for its answer, starting the plugin if it
    /// is not running. A plugin that has died is started again on the next
    /// request rather than putting the pane out of action for good.
    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let mut guard = self.session.lock().await;
        if guard.is_none() {
            *guard = Some(self.start().await?);
        }
        let session = guard.as_mut().expect("just started");
        match Self::exchange(session, method, params.clone()).await {
            Ok(value) => Ok(value),
            Err(e) if matches!(e, Error::Offline(_)) => {
                // The pipe broke: the plugin is gone. Start it once more,
                // since a plugin that crashed on one request often serves
                // the next one.
                *guard = None;
                let mut session = self.start().await?;
                let answer = Self::exchange(&mut session, method, params).await;
                *guard = Some(session);
                answer
            }
            Err(e) => Err(e),
        }
    }

    async fn exchange(session: &mut Session, method: &str, params: Value) -> Result<Value> {
        session.next_id += 1;
        let id = session.next_id;
        let request = json!({ "id": id, "method": method, "params": params });
        let line = format!("{request}\n");
        session
            .stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| Error::Offline(format!("the plugin stopped listening: {e}")))?;
        session
            .stdin
            .flush()
            .await
            .map_err(|e| Error::Offline(format!("the plugin stopped listening: {e}")))?;

        // Answers carry the id they belong to, so anything else — a late
        // answer to a call that timed out, say — is passed over.
        loop {
            let mut line = String::new();
            let read = session
                .stdout
                .read_line(&mut line)
                .await
                .map_err(|e| Error::Offline(format!("the plugin stopped answering: {e}")))?;
            if read == 0 {
                return Err(Error::Offline("the plugin stopped answering".into()));
            }
            let Ok(message) = serde_json::from_str::<Value>(line.trim()) else {
                eprintln!("openlook: plugin said something that is not JSON: {}", line.trim());
                continue;
            };
            if message["id"].as_u64() != Some(id) {
                continue;
            }
            if let Some(error) = message.get("error") {
                return Err(read_error(error));
            }
            return Ok(message.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    async fn start(&self) -> Result<Session> {
        let program = self.manifest.program();
        let mut command = Command::new(&program);
        command
            .args(self.manifest.exec.iter().skip(1))
            .current_dir(&self.manifest.directory)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|e| {
            Error::Permanent(format!("could not start {}: {e}", program.display()))
        })?;
        let stdin = child.stdin.take().expect("piped");
        let stdout = BufReader::new(child.stdout.take().expect("piped"));
        // Whatever the plugin says on stderr goes to the log, named, so it
        // is obvious which plugin is complaining.
        if let Some(stderr) = child.stderr.take() {
            let id = self.manifest.id.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    eprintln!("openlook: [{id}] {line}");
                }
            });
        }
        let mut session = Session { child, stdin, stdout, next_id: 0 };

        let hello = Self::exchange(
            &mut session,
            "initialize",
            json!({
                "version": PROTOCOL_VERSION,
                "credential": self.credential,
                "scope": self.scope,
            }),
        )
        .await?;
        let speaks = hello["version"].as_u64().unwrap_or(PROTOCOL_VERSION as u64) as u32;
        if speaks != PROTOCOL_VERSION {
            let _ = session.child.kill().await;
            return Err(Error::Permanent(format!(
                "{} speaks plugin protocol {speaks}, this OpenLook speaks {PROTOCOL_VERSION}",
                self.manifest.name
            )));
        }
        Ok(session)
    }
}

/// A plugin's own account of what went wrong, in the terms the sync engine
/// acts on. An unrecognised kind is treated as permanent: better to show
/// the message than to retry something that will never work.
fn read_error(error: &Value) -> Error {
    let message = error["message"].as_str().unwrap_or("the plugin gave no reason").to_string();
    match error["kind"].as_str().unwrap_or("permanent") {
        "offline" => Error::Offline(message),
        "auth" => Error::Auth(message),
        "not_found" => Error::NotFound,
        "transient" => Error::Transient(message),
        _ => Error::Permanent(message),
    }
}

/// One row as a plugin describes it.
fn read_item(value: &Value) -> Option<MessageSummary> {
    let id = value["id"].as_str()?.to_string();
    let name = value["from_name"].as_str().unwrap_or_default();
    let address = value["from_address"].as_str().unwrap_or_default();
    Some(MessageSummary {
        conversation_id: value["thread"].as_str().unwrap_or(&id).to_string(),
        thread_count: 1,
        folder_id: value["section"].as_str().unwrap_or_default().to_string(),
        id,
        subject: value["subject"].as_str().unwrap_or_default().to_string(),
        from: if name.is_empty() {
            Address::bare(address.to_string())
        } else {
            Address::new(name.to_string(), address.to_string())
        },
        received: value["received"].as_str().unwrap_or_default().to_string(),
        preview: value["preview"].as_str().unwrap_or_default().to_string(),
        is_read: value["read"].as_bool().unwrap_or(true),
        has_attachments: value["has_attachments"].as_bool().unwrap_or(false),
        pending: Pending::None,
    })
}

impl Connector for Plugin {
    fn items_called(&self) -> &str {
        &self.manifest.items_called
    }

    fn sections(&self) -> Task<'_, Vec<Section>> {
        Box::pin(async move {
            let listed = self.call("sections", json!({})).await?;
            Ok(listed
                .as_array()
                .map(|v| v.as_slice())
                .unwrap_or_default()
                .iter()
                .filter_map(|section| {
                    Some(Section {
                        id: section["id"].as_str()?.to_string(),
                        name: section["name"].as_str().unwrap_or_default().to_string(),
                    })
                })
                .collect())
        })
    }

    fn items(&self) -> Task<'_, Vec<MessageSummary>> {
        Box::pin(async move {
            let listed = self.call("items", json!({})).await?;
            Ok(listed
                .as_array()
                .map(|v| v.as_slice())
                .unwrap_or_default()
                .iter()
                .filter_map(read_item)
                .collect())
        })
    }

    fn body<'a>(&'a self, item: &'a MessageSummary) -> Task<'a, ItemBody> {
        Box::pin(async move {
            let content = self
                .call("body", json!({ "id": item.id, "preview": item.preview }))
                .await?;
            let threads = content["threads"]
                .as_array()
                .map(|v| v.as_slice())
                .unwrap_or_default()
                .iter()
                .filter_map(|t| t.as_str().map(str::to_string))
                .collect();
            Ok(ItemBody {
                body: Body {
                    is_html: content["is_html"].as_bool().unwrap_or(true),
                    content: content["content"].as_str().unwrap_or_default().to_string(),
                },
                threads,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A plugin says what went wrong in its own terms; the engine needs to
    /// know only whether to wait, ask for credentials, or give up.
    #[test]
    fn a_plugins_reasons_map_onto_the_engines() {
        assert!(read_error(&json!({"kind": "offline", "message": "no route"})).is_offline());
        assert!(read_error(&json!({"kind": "transient", "message": "429"})).should_retry());
        assert!(read_error(&json!({"kind": "auth", "message": "expired"})).should_retry());
        let gone = read_error(&json!({"kind": "not_found"}));
        assert!(!gone.should_retry());
        // Anything unrecognised is shown rather than retried forever.
        let odd = read_error(&json!({"kind": "banana", "message": "?"}));
        assert!(!odd.should_retry());
        assert_eq!(odd.to_string(), "?");
    }

    #[test]
    fn a_row_survives_a_sparse_description() {
        let item = read_item(&json!({ "id": "t-1", "section": "open" })).expect("an id is enough");
        assert_eq!(item.id, "t-1");
        assert_eq!(item.folder_id, "open");
        // A row with no thread of its own stands for itself.
        assert_eq!(item.conversation_id, "t-1");
        assert!(item.is_read, "nothing to mark unread");
        let described = read_item(&json!({
            "id": "t-2", "section": "open", "subject": "Printer", "from_name": "Ana",
            "from_address": "ana@example.com", "received": "2026-09-11T10:00:00Z", "read": false
        }))
        .expect("a full row");
        assert_eq!(described.subject, "Printer");
        assert_eq!(described.from.display(), "Ana");
        assert!(!described.is_read);
    }

    #[test]
    fn a_plugin_without_an_id_is_not_a_row() {
        assert!(read_item(&json!({ "section": "open" })).is_none());
    }
}

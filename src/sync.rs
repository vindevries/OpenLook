//! Background sync engine.
//!
//! The UI never talks to the network. It reads the local database and sends
//! commands here; this engine fetches from Graph, writes to the database and
//! reports back with events. Changes made while offline are applied locally
//! and queued in the outbox, then replayed when connectivity returns.

use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use tokio::runtime::Runtime;
use tokio::sync::Mutex;

use crate::auth::Auth;
use crate::config::{account_key, db_path};
use crate::db::Db;
use crate::graph::{Change, Graph, GraphError};
use crate::hubspot::HubSpot;
use crate::model::{AccountInfo, Address, Body, Folder, MessageSummary, Op, Pending, Status};
use crate::util::now_unix;

/// How many messages the first sync of a folder pulls down.
const WINDOW: u32 = 100;
/// Delta pages fetched per folder per sync. Bounds one tick's work while
/// still letting a large folder finish its first enumeration over a few
/// passes, after which each sync is a single cheap request.
const DELTA_PAGES_PER_SYNC: usize = 10;
/// Total bytes of embedded pictures worth storing with one message. A
/// signature logo is a few kilobytes; a pasted screenshot can be far more,
/// and the base64 of it lands in the cache.
const INLINE_IMAGE_BUDGET: i64 = 4 * 1024 * 1024;
/// How many bodies to prefetch per folder so mail is readable offline.
const PREFETCH: usize = 40;
/// Concurrent body downloads.
const PREFETCH_LANES: usize = 4;
const SYNC_INTERVAL: Duration = Duration::from_secs(120);

static RUNTIME: OnceLock<Runtime> = OnceLock::new();

pub fn runtime() -> &'static Runtime {
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("failed to start async runtime")
    })
}

/// A message body with the pictures it carries embedded.
///
/// Mail refers to its own attachments as `cid:` URLs, which nothing outside
/// the message can resolve — WebKit shows them as broken images. Fetch the
/// referenced pictures and inline them, so a body renders the same offline
/// as it does online. This is the only way bodies are fetched, so mail read
/// straight from the cache is complete too.
async fn fetch_body(graph: &Graph, id: &str) -> Result<(Vec<Address>, Vec<Address>, Body), GraphError> {
    let (to, cc, mut body) = graph.detail(id).await?;
    if body.content.contains("cid:") {
        let wanted = crate::util::cid_references(&body.content);
        match graph.inline_images(id, &wanted, INLINE_IMAGE_BUDGET).await {
            Ok(images) => body.content = crate::util::inline_cid_images(&body.content, &images),
            // Losing the pictures is better than losing the message.
            Err(e) => eprintln!("openlook: inline images: {e}"),
        }
    }
    Ok((to, cc, body))
}

#[derive(Debug, Clone)]
pub enum Mode {
    Demo,
    Account(AccountInfo),
    /// A HubSpot ticket pipeline, shown as its own section of folders.
    Tickets { pipeline_id: String, pipeline_label: String },
}

impl Mode {
    pub fn key(&self) -> String {
        match self {
            Mode::Demo => "demo".into(),
            Mode::Account(a) => account_key(&a.username),
            Mode::Tickets { pipeline_id, .. } => format!("hubspot-{pipeline_id}"),
        }
    }

    /// Human-readable mailbox name, used in error messages.
    pub fn label(&self) -> String {
        match self {
            Mode::Demo => "demo mailbox".into(),
            Mode::Account(a) if !a.username.is_empty() => a.username.clone(),
            Mode::Account(a) => a.name.clone(),
            Mode::Tickets { pipeline_label, .. } => format!("HubSpot · {pipeline_label}"),
        }
    }
}

#[derive(Debug, Clone)]
pub enum Cmd {
    /// Refresh folder list plus the given folder (or the default one).
    SyncAll(Option<String>),
    SyncFolder(String),
    /// Make sure a message body is cached, downloading it if needed.
    OpenMessage(String),
    /// Make sure an appointment's body is cached.
    OpenEvent(String),
    /// Refresh the calendar for a window, given as RFC3339 UTC bounds.
    SyncCalendar { start: String, end: String },
    /// Try to push queued changes now.
    Flush,
    SetOnline(bool),
}

#[derive(Debug, Clone)]
pub enum Event {
    FoldersChanged,
    MessagesChanged(String),
    BodyReady(String),
    CalendarChanged,
    EventReady(String),
    StatusChanged(Status),
    Failed(String),
    Notice(String),
}

/// A signed-in (or demo) mailbox: its cache, plus the engine driving it.
pub struct Session {
    pub db: Db,
    pub mode: Mode,
    cmd_tx: async_channel::Sender<Cmd>,
    pub events: async_channel::Receiver<Event>,
}

impl Session {
    pub fn new(mode: Mode, auth: Arc<Mutex<Auth>>, http: reqwest::Client) -> anyhow::Result<Session> {
        let db = Db::open(&db_path(&mode.key()))?;
        if matches!(mode, Mode::Demo) && db.is_empty() {
            crate::demo::seed(&db)?;
        }
        let (cmd_tx, cmd_rx) = async_channel::unbounded::<Cmd>();
        let (event_tx, events) = async_channel::unbounded::<Event>();

        let backend = match &mode {
            Mode::Demo => Backend::Demo,
            Mode::Account(account) => {
                Backend::Mail(Graph::new(http, auth, account.username.clone()))
            }
            Mode::Tickets { pipeline_id, .. } => match crate::config::hubspot_token() {
                Some(token) => Backend::Tickets {
                    client: HubSpot::new(http, token),
                    pipeline: pipeline_id.clone(),
                },
                // Without a token there is nothing to sync; the cache still
                // serves whatever was fetched before.
                None => Backend::Demo,
            },
        };
        let engine = Engine {
            db: db.clone(),
            backend,
            events: event_tx,
            status: Status::default(),
            last_folder: None,
            label: mode.label(),
        };
        runtime().spawn(engine.run(cmd_rx));
        Ok(Session { db, mode, cmd_tx, events })
    }

    pub fn send(&self, cmd: Cmd) {
        let _ = self.cmd_tx.try_send(cmd);
    }

    pub fn is_demo(&self) -> bool {
        matches!(self.mode, Mode::Demo)
    }

    pub fn account(&self) -> Option<&AccountInfo> {
        match &self.mode {
            Mode::Account(a) => Some(a),
            Mode::Demo | Mode::Tickets { .. } => None,
        }
    }

    /// Ticket sessions have no mailbox behind them, so mail-only actions
    /// (archive, reply-as-mail) do not apply.
    pub fn is_tickets(&self) -> bool {
        matches!(self.mode, Mode::Tickets { .. })
    }

    /// Label for this mailbox in the folder pane.
    pub fn title(&self) -> String {
        match &self.mode {
            Mode::Account(a) if !a.username.is_empty() => a.username.clone(),
            Mode::Account(a) => a.name.clone(),
            Mode::Demo => "Demo mailbox".to_string(),
            Mode::Tickets { pipeline_label, .. } => pipeline_label.clone(),
        }
    }

    /// Apply a mailbox change: immediately to the local database (so the UI
    /// updates at once, online or not), then queue it for the server.
    pub fn apply(&self, op: Op) -> anyhow::Result<()> {
        apply_local(&self.db, &op)?;
        if self.is_demo() {
            if let Op::Send { local_id, .. } = &op {
                self.db.clear_pending_flag(local_id)?;
            }
        } else {
            self.db.enqueue(&op)?;
            self.send(Cmd::Flush);
        }
        Ok(())
    }

    pub fn queued_count(&self) -> i64 {
        self.db.queued_count()
    }
}

/// The local half of an operation — what the user sees happen instantly.
pub fn apply_local(db: &Db, op: &Op) -> anyhow::Result<()> {
    match op {
        Op::MarkRead { message_id, is_read } => db.set_read(message_id, *is_read)?,
        Op::Delete { message_id, purge } => match (purge, db.folder_id_by_name("Deleted Items")) {
            (false, Some(bin)) => db.move_message(message_id, &bin)?,
            _ => db.remove_message(message_id)?,
        },
        Op::Move { message_id, folder_id } => db.move_message(message_id, folder_id)?,
        Op::Send { local_id, message } => {
            let folder = db
                .folder_id_by_name("Sent Items")
                .or_else(|| db.folder_id_by_name("Drafts"))
                .unwrap_or_default();
            let body = Body { is_html: false, content: message.body.clone() };
            let summary = MessageSummary {
                conversation_id: local_id.clone(),
                thread_count: 1,
                id: local_id.clone(),
                folder_id: folder,
                subject: message.subject.clone(),
                from: crate::model::Address::new("You", ""),
                received: chrono::Utc::now().to_rfc3339(),
                preview: message.body.chars().take(120).collect(),
                is_read: true,
                has_attachments: false,
                pending: Pending::Queued,
            };
            let to: Vec<_> = message.to.iter().map(crate::model::Address::bare).collect();
            let cc: Vec<_> = message.cc.iter().map(crate::model::Address::bare).collect();
            db.insert_local_message(&summary, &to, &cc, &body)?;
        }
    }
    Ok(())
}

/// What a session talks to. Demo talks to nothing: the cache is the world.
enum Backend {
    Demo,
    Mail(Graph),
    Tickets { client: HubSpot, pipeline: String },
}

impl Backend {
    /// The Graph client, for the mail-only paths.
    fn mail(&self) -> Option<Graph> {
        match self {
            Backend::Mail(graph) => Some(graph.clone()),
            _ => None,
        }
    }
}

struct Engine {
    db: Db,
    backend: Backend,
    events: async_channel::Sender<Event>,
    status: Status,
    last_folder: Option<String>,
    /// Which mailbox this engine serves, for error messages.
    label: String,
}

impl Engine {
    async fn run(mut self, cmd_rx: async_channel::Receiver<Cmd>) {
        let mut ticker = tokio::time::interval(SYNC_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await; // the first tick fires immediately; ignore it
        loop {
            tokio::select! {
                cmd = cmd_rx.recv() => match cmd {
                    Ok(cmd) => self.handle(cmd).await,
                    // The window is gone: shut the engine down.
                    Err(_) => break,
                },
                _ = ticker.tick() => {
                    self.flush().await;
                    let folder = self.last_folder.clone();
                    self.sync_all(folder).await;
                }
            }
        }
    }

    fn emit(&self, event: Event) {
        let _ = self.events.try_send(event);
    }

    fn publish_status(&mut self) {
        self.status.queued = self.db.queued_count();
        self.status.last_sync =
            self.db.meta("last_sync").and_then(|v| v.parse::<i64>().ok());
        self.emit(Event::StatusChanged(self.status.clone()));
    }

    async fn handle(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::SyncAll(folder) => {
                if folder.is_some() {
                    self.last_folder = folder.clone();
                }
                self.flush().await;
                self.sync_all(folder).await;
            }
            Cmd::SyncFolder(id) => {
                self.last_folder = Some(id.clone());
                if self.backend.mail().is_some() {
                    self.status.syncing = true;
                    self.publish_status();
                    self.sync_folder(&id).await;
                    self.status.syncing = false;
                    self.publish_status();
                }
            }
            Cmd::OpenMessage(id) => self.ensure_body(&id).await,
            Cmd::SyncCalendar { start, end } => self.sync_calendar(&start, &end).await,
            Cmd::OpenEvent(id) => self.ensure_event_body(&id).await,
            Cmd::Flush => {
                self.flush().await;
                self.publish_status();
            }
            Cmd::SetOnline(online) => {
                let was_offline = !self.status.online;
                self.status.online = online;
                self.publish_status();
                if online && was_offline {
                    self.flush().await;
                    let folder = self.last_folder.clone();
                    self.sync_all(folder).await;
                }
            }
        }
    }

    /// Record the outcome of a network call. Anything other than "we are
    /// offline" is surfaced to the user and logged — an empty mailbox with
    /// no explanation is the worst possible outcome.
    fn note_result<T>(&mut self, result: &Result<T, GraphError>) {
        match result {
            Ok(_) => {
                self.status.online = true;
                self.status.detail = None;
            }
            Err(e) if e.is_offline() => {
                self.status.online = false;
                self.status.detail = Some(format!(
                    "Offline — showing cached mail ({})",
                    e.detail().unwrap_or("no network")
                ));
            }
            Err(e) => {
                let message = format!("{}: {e}", self.label);
                eprintln!("openlook: {message}");
                if let Some(detail) = e.detail() {
                    eprintln!("openlook:   {detail}");
                }
                self.status.detail = Some(e.to_string());
                self.emit(Event::Failed(message));
            }
        }
    }

    /// Stages become folders and tickets become rows, so the existing mail
    /// panes show tickets without knowing anything about HubSpot.
    async fn sync_tickets(&mut self) {
        let Backend::Tickets { client, pipeline } = &self.backend else { return };
        let (client, pipeline) = (client.clone(), pipeline.clone());
        self.status.syncing = true;
        self.publish_status();

        let stages = client.stages_of(&pipeline).await;
        self.note_hubspot(&stages);
        let closed_stages: Vec<String> = stages
            .as_ref()
            .map(|s| s.iter().filter(|s| s.closed).map(|s| s.id.clone()).collect())
            .unwrap_or_default();
        if let Ok(stages) = &stages {
            let folders: Vec<Folder> = stages
                .iter()
                .map(|s| Folder {
                    id: s.id.clone(),
                    display_name: s.label.clone(),
                    unread_count: 0,
                    total_count: 0,
                })
                .collect();
            if self.db.upsert_folders(&folders).is_ok() {
                // Keep HubSpot's own stage sequence: New … Closed, not the
                // alphabetical order the mail ranking falls back to.
                for (position, stage) in stages.iter().enumerate() {
                    let _ = self.db.set_folder_sort(&stage.id, position as i64);
                }
                self.emit(Event::FoldersChanged);
            }
        }

        // Every open ticket, plus a slice of recent closed ones.
        let tickets = client.tickets_for_pipeline(&pipeline, &closed_stages, 200).await;
        self.note_hubspot(&tickets);
        let Ok(tickets) = tickets else {
            self.status.syncing = false;
            self.publish_status();
            return;
        };

        // One request for every contact, rather than one per ticket.
        let contact_ids: Vec<String> = {
            let mut ids: Vec<String> =
                tickets.iter().filter_map(|t| t.contact_ids.first().cloned()).collect();
            ids.sort();
            ids.dedup();
            ids
        };
        let mut names = std::collections::HashMap::new();
        for chunk in contact_ids.chunks(100) {
            if let Ok(contacts) = client.contacts_batch(chunk).await {
                for contact in contacts {
                    names.insert(contact.id.clone(), contact);
                }
            }
        }

        let rows: Vec<MessageSummary> = tickets
            .iter()
            .map(|t| {
                let contact = t.contact_ids.first().and_then(|id| names.get(id));
                MessageSummary {
                    // A ticket is its own thread.
                    conversation_id: t.id.clone(),
                    thread_count: 1,
                    id: t.id.clone(),
                    folder_id: t.stage_id.clone(),
                    subject: t.subject.clone(),
                    from: match contact {
                        Some(c) if !c.name.trim().is_empty() => {
                            crate::model::Address::new(c.name.clone(), c.email.clone())
                        }
                        Some(c) => crate::model::Address::bare(c.email.clone()),
                        None => crate::model::Address::new("(no contact)", ""),
                    },
                    received: t.updated.clone(),
                    preview: crate::util::html_to_text(&t.content).chars().take(140).collect(),
                    is_read: true,
                    has_attachments: false,
                    pending: Pending::None,
                }
            })
            .collect();
        let _ = self.db.upsert_messages(&rows);
        let _ = self.db.recompute_counts();
        self.emit(Event::FoldersChanged);
        for folder in rows.iter().map(|r| r.folder_id.clone()).collect::<std::collections::HashSet<_>>() {
            self.emit(Event::MessagesChanged(folder));
        }
        let _ = self.db.set_meta("last_sync", &now_unix().to_string());
        self.status.syncing = false;
        self.publish_status();
    }

    /// Assemble a ticket's correspondence: its description when it has one,
    /// then each associated thread in turn. Threads stay labelled rather
    /// than being silently merged, because a ticket often has several.
    async fn ensure_ticket_body(&mut self, ticket_id: &str) {
        let Backend::Tickets { client, .. } = &self.backend else { return };
        let client = client.clone();
        let cached = matches!(self.db.message(ticket_id), Ok(Some(m)) if m.body.is_some());
        if cached {
            return;
        }
        let threads = client.ticket_threads(ticket_id).await;
        self.note_hubspot(&threads);
        let Ok(threads) = threads else { return };

        let mut collected: Vec<(String, Vec<crate::hubspot::ThreadMessage>)> = Vec::new();
        for thread in &threads {
            match client.thread_messages(thread).await {
                Ok(messages) => {
                    self.status.online = true;
                    collected.push((thread.clone(), messages));
                }
                // A thread that has gone is simply not shown; the rest of
                // the ticket still reads fine.
                Err(crate::hubspot::Error::NotFound) => continue,
                Err(e) => self.note_hubspot(&Err::<(), _>(e)),
            }
        }
        // A ticket raised through a web form has no thread; its own text is
        // the only thing to show until someone answers.
        let description = self
            .db
            .message(ticket_id)
            .ok()
            .flatten()
            .map(|m| crate::util::escape_html(&m.summary.preview))
            .unwrap_or_default();
        let html = crate::hubspot::assemble_ticket_html(&description, &collected);

        let body = Body { is_html: true, content: html };
        if self.db.set_body(ticket_id, &body, &[], &[]).is_ok() {
            let _ = self.db.set_thread_ids(ticket_id, &threads);
            self.emit(Event::BodyReady(ticket_id.to_string()));
        }
    }

    /// HubSpot failures get the same treatment as Graph ones.
    fn note_hubspot<T>(&mut self, result: &std::result::Result<T, crate::hubspot::Error>) {
        match result {
            Ok(_) => {
                self.status.online = true;
                self.status.detail = None;
            }
            Err(e) if e.is_offline() => {
                self.status.online = false;
                self.status.detail = Some("Offline — showing cached tickets".into());
            }
            Err(e) => {
                let message = format!("{}: {e}", self.label);
                eprintln!("openlook: {message}");
                self.status.detail = Some(e.to_string());
                self.emit(Event::Failed(message));
            }
        }
    }

    async fn sync_all(&mut self, folder: Option<String>) {
        if matches!(self.backend, Backend::Tickets { .. }) {
            let _ = folder;
            self.sync_tickets().await;
            return;
        }
        let Some(graph) = self.backend.mail() else {
            // Demo mode: the cache is the whole world.
            let _ = self.db.recompute_counts();
            self.emit(Event::FoldersChanged);
            return;
        };
        self.status.syncing = true;
        self.publish_status();

        let folders = graph.folders().await;
        self.note_result(&folders);
        if let Ok(folders) = folders {
            if self.db.upsert_folders(&folders).is_ok() {
                self.emit(Event::FoldersChanged);
            }
            let target = folder
                .or_else(|| self.db.folder_id_by_name("Inbox"))
                .or_else(|| folders.first().map(|f| f.id.clone()));
            if let Some(id) = target {
                self.last_folder = Some(id.clone());
                self.sync_folder(&id).await;
            }
            // Keep the inbox warm even while another folder is open.
            if let Some(inbox) = self.db.folder_id_by_name("Inbox") {
                if self.last_folder.as_deref() != Some(inbox.as_str()) {
                    self.sync_folder(&inbox).await;
                }
            }
            let _ = self.db.set_meta("last_sync", &now_unix().to_string());
        }
        self.status.syncing = false;
        self.publish_status();
    }

    async fn sync_folder(&mut self, folder_id: &str) {
        if matches!(self.backend, Backend::Tickets { .. }) {
            let _ = folder_id;
            self.sync_tickets().await;
            return;
        }
        let Some(graph) = self.backend.mail() else { return };
        let cursor = self.db.delta_link(folder_id);

        // First contact with a folder: pull the newest messages in order so
        // the list fills immediately. The delta enumeration that follows is
        // unordered, so on its own it would populate the view raggedly.
        if cursor.is_none() {
            let window = graph.messages_window(folder_id, WINDOW).await;
            self.note_result(&window);
            let Ok(messages) = window else { return };
            let ids: Vec<String> = messages.iter().map(|m| m.id.clone()).collect();
            let oldest =
                messages.iter().map(|m| m.received.as_str()).min().unwrap_or("").to_string();
            let _ = self.db.upsert_messages(&messages);
            let _ = self.db.reconcile_window(folder_id, &ids, &oldest);
            self.emit(Event::MessagesChanged(folder_id.to_string()));
        }

        let page = graph.delta(folder_id, cursor.as_deref(), DELTA_PAGES_PER_SYNC).await;
        self.note_result(&page);
        match page {
            Ok(page) => {
                let mut upserts = Vec::new();
                let mut patches = Vec::new();
                let mut removed = Vec::new();
                for change in page.changes {
                    match change {
                        Change::Upsert(m) => upserts.push(m),
                        Change::Patch(p) => patches.push(p),
                        Change::Removed(id) => removed.push(id),
                    }
                }
                let changed = !upserts.is_empty() || !patches.is_empty() || !removed.is_empty();
                let _ = self.db.upsert_messages(&upserts);
                let _ = self.db.patch_messages(&patches);
                for id in &removed {
                    let _ = self.db.remove_message(id);
                }
                // Keep whatever came back — a deltaLink once caught up, or a
                // nextLink partway through. Storing the nextLink is the point:
                // without it a folder too big to enumerate in one pass would
                // start from scratch every time and never earn a token.
                if let Some(cursor) = page.cursor {
                    let _ = self.db.set_delta_link(folder_id, Some(&cursor));
                }
                if changed {
                    self.emit(Event::MessagesChanged(folder_id.to_string()));
                }
            }
            Err(e) if e.is_offline() => return,
            Err(_) => {
                // Cursor rejected or expired: start the chain again next time.
                let _ = self.db.set_delta_link(folder_id, None);
            }
        }
        self.backfill_conversations(folder_id).await;
        self.prefetch_bodies(folder_id).await;
    }

    /// Fetch an appointment's body once, then serve it from the cache.
    async fn ensure_event_body(&mut self, id: &str) {
        let cached = matches!(self.db.event(id), Ok(Some((_, Some(_), _))));
        if cached {
            self.emit(Event::EventReady(id.to_string()));
            return;
        }
        let Some(graph) = self.backend.mail() else { return };
        let detail = graph.event_detail(id).await;
        self.note_result(&detail);
        if let Ok((body, attendees)) = detail {
            if self.db.set_event_body(id, &body, &attendees).is_ok() {
                self.emit(Event::EventReady(id.to_string()));
            }
        }
    }

    /// Refresh the cached calendar for a window. The server view is the
    /// truth for that range, so cancellations disappear too.
    async fn sync_calendar(&mut self, start: &str, end: &str) {
        let Some(graph) = self.backend.mail() else { return };
        let events = graph.calendar_view(start, end).await;
        self.note_result(&events);
        if let Ok(events) = events {
            if self.db.replace_events(start, end, &events).is_ok() {
                self.emit(Event::CalendarChanged);
            }
        }
    }

    /// Mail cached before threading existed carries no conversation, and
    /// delta only revisits messages that change — so those rows would never
    /// group. Sweep the folder once to fill them in.
    async fn backfill_conversations(&mut self, folder_id: &str) {
        if self.db.messages_missing_conversation(folder_id) == 0 {
            return;
        }
        let Some(graph) = self.backend.mail() else { return };
        let pairs = graph.conversation_ids(folder_id, 8).await;
        self.note_result(&pairs);
        if let Ok(pairs) = pairs {
            if let Ok(filled) = self.db.backfill_conversations(&pairs) {
                if filled > 0 {
                    eprintln!("openlook: threaded {filled} cached message(s) in one folder");
                    self.emit(Event::MessagesChanged(folder_id.to_string()));
                }
            }
        }
    }

    /// Download the newest bodies so they are readable without a network.
    async fn prefetch_bodies(&mut self, folder_id: &str) {
        let Some(graph) = self.backend.mail() else { return };
        let Ok(ids) = self.db.missing_bodies(folder_id, PREFETCH) else { return };
        if ids.is_empty() {
            return;
        }
        for chunk in ids.chunks(PREFETCH_LANES) {
            let mut set = tokio::task::JoinSet::new();
            for id in chunk {
                let graph = graph.clone();
                let db = self.db.clone();
                let id = id.clone();
                set.spawn(async move {
                    match fetch_body(&graph, &id).await {
                        Ok((to, cc, body)) => {
                            let _ = db.set_body(&id, &body, &to, &cc);
                            Ok(id)
                        }
                        Err(e) => Err(e),
                    }
                });
            }
            let mut offline = false;
            while let Some(joined) = set.join_next().await {
                match joined {
                    Ok(Ok(id)) => self.emit(Event::BodyReady(id)),
                    Ok(Err(e)) => {
                        if e.is_offline() {
                            offline = true;
                        }
                    }
                    Err(_) => {}
                }
            }
            if offline {
                self.status.online = false;
                self.status.detail = Some("Offline — showing cached mail".into());
                self.publish_status();
                return;
            }
        }
    }

    async fn ensure_body(&mut self, id: &str) {
        if matches!(self.backend, Backend::Tickets { .. }) {
            self.ensure_ticket_body(id).await;
            return;
        }
        let cached = matches!(self.db.message(id), Ok(Some(m)) if m.body.is_some());
        if cached {
            return;
        }
        let Some(graph) = self.backend.mail() else {
            self.emit(Event::Failed("This message is not available offline.".into()));
            return;
        };
        let detail = fetch_body(&graph, id).await;
        self.note_result(&detail);
        match detail {
            Ok((to, cc, body)) => {
                if self.db.set_body(id, &body, &to, &cc).is_ok() {
                    self.emit(Event::BodyReady(id.to_string()));
                }
            }
            Err(e) if e.is_offline() => {
                self.publish_status();
                self.emit(Event::Failed(
                    "That message hasn't been downloaded yet, and you're offline.".into(),
                ));
            }
            Err(e) => self.emit(Event::Failed(e.to_string())),
        }
    }

    /// A server-side move renames the message; keep the cache in step so a
    /// later sync of the destination folder does not insert a duplicate.
    fn adopt_new_id(&self, old_id: &str, new_id: Option<String>) {
        if let Some(new_id) = new_id {
            if new_id != old_id {
                let _ = self.db.rename_message(old_id, &new_id);
            }
        }
    }

    /// Replay queued changes. Anything that fails for a retryable reason
    /// stays in the queue for the next attempt.
    async fn flush(&mut self) {
        let Some(graph) = self.backend.mail() else { return };
        let Ok(queued) = self.db.queued() else { return };
        if queued.is_empty() {
            return;
        }
        let mut sent_any = false;
        for (row_id, op, attempts) in queued {
            let result = match &op {
                Op::MarkRead { message_id, is_read } => graph.set_read(message_id, *is_read).await,
                Op::Delete { message_id, purge } => graph
                    .delete(message_id, *purge)
                    .await
                    .map(|new_id| self.adopt_new_id(message_id, new_id)),
                Op::Move { message_id, folder_id } => graph
                    .move_message(message_id, folder_id)
                    .await
                    .map(|new_id| self.adopt_new_id(message_id, new_id)),
                Op::Send { message, .. } => graph.send_mail(message).await,
            };
            match result {
                Ok(()) => {
                    let _ = self.db.dequeue(row_id);
                    if let Op::Send { local_id, .. } = &op {
                        // The server files its own copy in Sent Items; drop
                        // the local placeholder so it does not show twice.
                        let _ = self.db.remove_message(local_id);
                        sent_any = true;
                    }
                    self.status.online = true;
                }
                Err(GraphError::NotFound) => {
                    // Already gone server-side; nothing left to do.
                    let _ = self.db.dequeue(row_id);
                }
                Err(e) if e.should_retry() => {
                    let _ = self.db.record_failure(row_id, &e.to_string());
                    if e.is_offline() {
                        self.status.online = false;
                        self.status.detail = Some("Offline — changes will sync later".into());
                        self.publish_status();
                        return;
                    }
                    if attempts >= 8 {
                        self.emit(Event::Failed(format!("Still retrying: {e}")));
                    }
                }
                Err(e) => {
                    let _ = self.db.dequeue(row_id);
                    if matches!(op, Op::Send { .. }) {
                        self.emit(Event::Failed(format!("Message not sent: {e}")));
                    }
                }
            }
        }
        if sent_any {
            if let Some(sent) = self.db.folder_id_by_name("Sent Items") {
                self.sync_folder(&sent).await;
                self.emit(Event::MessagesChanged(sent));
            }
            self.emit(Event::Notice("Message sent".into()));
        }
        self.publish_status();
    }
}

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
use crate::connector::Connector;
use crate::db::Db;
use crate::graph::{Change, Graph, GraphError};
use crate::invite::{self, Invite, Response};
use crate::model::{AccountInfo, Address, Body, Folder, MessageSummary, Op, Pending, Status};
use crate::util::{now_unix, safe_name, short_key};

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
async fn fetch_body(
    graph: &Graph,
    db: &Db,
    id: &str,
) -> Result<(Vec<Address>, Vec<Address>, Body), GraphError> {
    let (to, cc, mut body, meeting) = graph.detail(id).await?;
    // The list is what the reading pane offers to open, so record it even
    // when the body refers to none of it.
    let items = match graph.attachments(id).await {
        Ok(items) => {
            let _ = db.set_attachments(id, &items);
            items
        }
        // Losing the list is better than losing the message.
        Err(e) => {
            eprintln!("openlook: attachments: {e}");
            Vec::new()
        }
    };
    // An invitation is recognised while the body is being fetched, so a
    // message read offline later still offers to put it on the calendar.
    if let Err(e) = cache_invite(graph, db, id, meeting, &items).await {
        eprintln!("openlook: invitation: {e}");
    }
    if body.content.contains("cid:") {
        let wanted = crate::util::cid_references(&body.content);
        match graph.inline_images(id, &items, &wanted, INLINE_IMAGE_BUDGET).await {
            Ok(images) => {
                // A picture embedded in the body is part of the message as
                // it reads, not a file to offer separately.
                let embedded: Vec<String> = images.iter().map(|i| i.1.clone()).collect();
                let _ = db.mark_attachments_inline(id, &embedded);
                let images: Vec<(String, String, String)> = images
                    .into_iter()
                    .map(|(content_id, _, content_type, bytes)| (content_id, content_type, bytes))
                    .collect();
                body.content = crate::util::inline_cid_images(&body.content, &images);
            }
            Err(e) => eprintln!("openlook: inline images: {e}"),
        }
    }
    Ok((to, cc, body))
}

/// Send an answer to an invitation, by whichever route the invitation has.
///
/// Exchange can only RSVP for a meeting it holds. For a `.ics` that
/// arrived as an ordinary attachment there is no such meeting and no
/// organiser expecting a reply through the server, so accepting it means
/// creating the appointment — and declining it means there is nothing to
/// send at all.
async fn send_invite_response(
    graph: &Graph,
    message_id: &str,
    invite: &Invite,
    response: Response,
) -> Result<(), GraphError> {
    if invite.can_rsvp() {
        return graph.respond_to_invite(message_id, response).await;
    }
    if response == Response::Declined {
        return Ok(());
    }
    graph.create_event(invite).await.map(|_| ())
}

/// Work out whether a message is a calendar invitation, and cache what it
/// is inviting you to.
///
/// The two shapes are handled the same way from here on: Exchange's own
/// reading of a meeting request, or the `text/calendar` part of a message
/// it did not recognise as one.
async fn cache_invite(
    graph: &Graph,
    db: &Db,
    id: &str,
    meeting: bool,
    items: &[crate::model::Attachment],
) -> Result<(), GraphError> {
    if meeting {
        if let Some(found) = graph.meeting_invite(id).await? {
            let _ = db.set_invite(id, &found);
            return Ok(());
        }
    }
    let Some(part) = items
        .iter()
        .find(|item| invite::is_calendar_part(&item.content_type, &item.name))
    else {
        return Ok(());
    };
    // An .ics is a few kilobytes, so it is fetched with the body rather
    // than waiting for someone to ask — which is what lets the invitation
    // be answered with no network.
    let bytes = graph.attachment_bytes(id, &part.id).await?;
    let text = String::from_utf8_lossy(&bytes);
    if let Some(found) = invite::parse(&text) {
        let _ = db.set_invite(id, &found);
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub enum Mode {
    Demo,
    Account(AccountInfo),
    /// One connector — a plugin and the part of it to show — as its own
    /// section of folders.
    Connector { plugin: String, scope: String, label: String },
}

impl Mode {
    pub fn key(&self) -> String {
        match self {
            Mode::Demo => "demo".into(),
            Mode::Account(a) => account_key(&a.username),
            // The cache is named for the plugin and what it is showing,
            // which is what keeps two queues of the same plugin apart.
            Mode::Connector { plugin, scope, .. } => format!("{plugin}-{scope}"),
        }
    }

    /// Human-readable mailbox name, used in error messages.
    pub fn label(&self) -> String {
        match self {
            Mode::Demo => "demo mailbox".into(),
            Mode::Account(a) if !a.username.is_empty() => a.username.clone(),
            Mode::Account(a) => a.name.clone(),
            Mode::Connector { plugin, label, .. } => {
                let name = crate::plugin::manifest(plugin).map(|m| m.name).unwrap_or_else(|| plugin.clone());
                format!("{name} · {label}")
            }
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
    /// Make sure the list of what a message carries is cached.
    ListAttachments(String),
    /// Work out whether a message carries an invitation, for mail that was
    /// cached before invitations were read.
    EnsureInvite(String),
    /// Download an attachment if it is not already on disk, ready to open.
    OpenAttachment { message_id: String, attachment_id: String },
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
    /// Mail that has just landed in the Inbox, for the desktop to announce.
    NewMail(Vec<MessageSummary>),
    /// The list of files a message carries has arrived.
    AttachmentsChanged(String),
    /// A message turned out to carry a calendar invitation.
    InviteChanged(String),
    /// An attachment is on disk at this path, ready to be opened.
    AttachmentReady { message_id: String, attachment_id: String, path: String },
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
            Mode::Connector { plugin, scope, .. } => {
                match crate::plugin::manifest(plugin) {
                    Some(manifest) => Backend::Items(Arc::new(crate::plugin::Plugin::new(
                        manifest,
                        crate::config::credential(plugin).unwrap_or_default(),
                        scope.clone(),
                    ))),
                    // The plugin is not installed any more; the cache still
                    // serves whatever it fetched before.
                    None => Backend::Demo,
                }
            }
        };
        let engine = Engine {
            db: db.clone(),
            backend,
            events: event_tx,
            status: Status::default(),
            last_folder: None,
            label: mode.label(),
            key: mode.key(),
            seen: std::collections::HashSet::new(),
            invite_checked: std::collections::HashSet::new(),
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
            Mode::Demo | Mode::Connector { .. } => None,
        }
    }

    /// Ticket sessions have no mailbox behind them, so mail-only actions
    /// (archive, reply-as-mail) do not apply.
    pub fn is_tickets(&self) -> bool {
        matches!(self.mode, Mode::Connector { .. })
    }

    /// Label for this mailbox in the folder pane.
    pub fn title(&self) -> String {
        match &self.mode {
            Mode::Account(a) if !a.username.is_empty() => a.username.clone(),
            Mode::Account(a) => a.name.clone(),
            Mode::Demo => "Demo mailbox".to_string(),
            Mode::Connector { label, .. } => label.clone(),
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
            let body = Body { is_html: message.is_html, content: message.body.clone() };
            // The list shows a line of the message, not its markup.
            let preview: String = if message.is_html {
                crate::util::html_to_text(&message.body)
            } else {
                message.body.clone()
            };
            let summary = MessageSummary {
                conversation_id: local_id.clone(),
                thread_count: 1,
                id: local_id.clone(),
                folder_id: folder,
                subject: message.subject.clone(),
                from: crate::model::Address::new("You", ""),
                received: chrono::Utc::now().to_rfc3339(),
                preview: preview.chars().take(120).collect(),
                is_read: true,
                has_attachments: false,
                answered: crate::model::Answered::No,
                pending: Pending::Queued,
            };
            let to: Vec<_> = message.to.iter().map(crate::model::Address::bare).collect();
            let cc: Vec<_> = message.cc.iter().map(crate::model::Address::bare).collect();
            db.insert_local_message(&summary, &to, &cc, &body)?;
        }
        Op::RespondToInvite { message_id, invite, response } => {
            db.set_invite_response(message_id, *response)?;
            // A meeting request is already on the calendar — Exchange put
            // it there as tentative when it arrived — so answering it
            // moves an appointment that exists rather than making one.
            // A bare .ics has no such appointment, so accepting it shows
            // one straight away and the server is told afterwards.
            if !invite.meeting_request {
                let event = invite.as_event("");
                match response {
                    Response::Declined => db.remove_event(&event.id)?,
                    _ => db.upsert_event(&event)?,
                }
            }
        }
    }
    Ok(())
}

/// What a session talks to. Demo talks to nothing: the cache is the world.
enum Backend {
    Demo,
    Mail(Graph),
    /// Anything that is not mail: tickets, queues, whatever a connector
    /// offers. The engine knows only the trait.
    Items(Arc<dyn Connector>),
}

impl Backend {
    /// The connector, for the paths that are not mail.
    fn items(&self) -> Option<Arc<dyn Connector>> {
        match self {
            Backend::Items(connector) => Some(connector.clone()),
            _ => None,
        }
    }

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
    /// Filesystem-safe name for this mailbox, for its downloads.
    key: String,
    /// Folders this run has already been through. The first pass over a
    /// folder is the cache catching up, not mail arriving, so it is quiet.
    seen: std::collections::HashSet<String>,
    /// Messages already looked at for an invitation. Mail that carries
    /// none is the common case, and the answer does not change, so it is
    /// worth asking only once.
    invite_checked: std::collections::HashSet<String>,
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
            Cmd::ListAttachments(id) => self.ensure_attachment_list(&id).await,
            Cmd::EnsureInvite(id) => self.ensure_invite(&id).await,
            Cmd::OpenAttachment { message_id, attachment_id } => {
                self.ensure_attachment(&message_id, &attachment_id).await
            }
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

    /// Sections become folders and items become rows, so the panes built
    /// for mail show a connector's contents without knowing what they are.
    async fn sync_items(&mut self) {
        let Some(connector) = self.backend.items() else { return };
        self.status.syncing = true;
        self.publish_status();

        let sections = connector.sections().await;
        self.note_items(&sections);
        if let Ok(sections) = &sections {
            let folders: Vec<Folder> = sections
                .iter()
                .map(|section| Folder {
                    id: section.id.clone(),
                    display_name: section.name.clone(),
                    unread_count: 0,
                    total_count: 0,
                    // A connector's sections are a flat list for now.
                    parent_id: None,
                })
                .collect();
            if self.db.upsert_folders(&folders).is_ok() {
                // Keep the connector's own order — New … Closed, say —
                // rather than the alphabetical ranking mail falls back to.
                for (position, section) in sections.iter().enumerate() {
                    let _ = self.db.set_folder_sort(&section.id, position as i64);
                }
                self.emit(Event::FoldersChanged);
            }
        }

        let items = connector.items().await;
        self.note_items(&items);
        let Ok(rows) = items else {
            self.status.syncing = false;
            self.publish_status();
            return;
        };
        let _ = self.db.upsert_messages(&rows);
        let _ = self.db.recompute_counts();
        self.emit(Event::FoldersChanged);
        for folder in
            rows.iter().map(|r| r.folder_id.clone()).collect::<std::collections::HashSet<_>>()
        {
            self.emit(Event::MessagesChanged(folder));
        }
        let _ = self.db.set_meta("last_sync", &now_unix().to_string());
        self.status.syncing = false;
        self.publish_status();
    }

    /// Fetch an item's body once, then serve it from the cache.
    async fn ensure_item_body(&mut self, id: &str) {
        let Some(connector) = self.backend.items() else { return };
        let Ok(Some(item)) = self.db.message(id) else { return };
        if item.body.is_some() {
            return;
        }
        let content = connector.body(&item.summary).await;
        self.note_items(&content);
        let Ok(content) = content else { return };
        if self.db.set_body(id, &content.body, &[], &[]).is_ok() {
            let _ = self.db.set_thread_ids(id, &content.threads);
            self.emit(Event::BodyReady(id.to_string()));
        }
    }

    /// A connector's failures get the same treatment as Graph's.
    fn note_items<T>(&mut self, result: &std::result::Result<T, crate::connector::Error>) {
        match result {
            Ok(_) => {
                self.status.online = true;
                self.status.detail = None;
            }
            Err(e) if e.is_offline() => {
                let called = self.backend.items().map(|c| c.items_called().to_string());
                self.status.online = false;
                self.status.detail =
                    Some(format!("Offline — showing cached {}", called.as_deref().unwrap_or("items")));
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
        if self.backend.items().is_some() {
            let _ = folder;
            self.sync_items().await;
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
        if self.backend.items().is_some() {
            let _ = folder_id;
            self.sync_items().await;
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
                // Arrivals, before the cache is written: mail the cache has
                // not seen, still unread, in the folder mail arrives in.
                let announce: Vec<MessageSummary> = if self.seen.contains(folder_id)
                    && self.db.folder_id_by_name("Inbox").as_deref() == Some(folder_id)
                {
                    upserts
                        .iter()
                        .filter(|m| !m.is_read)
                        .filter(|m| matches!(self.db.message(&m.id), Ok(None)))
                        .cloned()
                        .collect()
                } else {
                    Vec::new()
                };
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
                if !announce.is_empty() {
                    self.emit(Event::NewMail(announce));
                }
                // The delta feed will not carry "replied" or "forwarded", so
                // the folder's newest messages are asked about separately.
                if let Ok(verbs) = graph.answered(folder_id, WINDOW).await {
                    if self.db.set_answered(&verbs).is_ok() && !verbs.is_empty() {
                        self.emit(Event::MessagesChanged(folder_id.to_string()));
                    }
                }
                self.seen.insert(folder_id.to_string());
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
        // An invitation accepted a moment ago is a local event until the
        // server has been told about it, and the fetch below is the
        // server's word on this window — so push first, or accepting one
        // and looking at the calendar would make it disappear.
        self.flush().await;
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
                    match fetch_body(&graph, &db, &id).await {
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
        if self.backend.items().is_some() {
            self.ensure_item_body(id).await;
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
        let detail = fetch_body(&graph, &self.db, id).await;
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

    /// Mail cached before attachments were listed carries none in the
    /// cache, and its body is already downloaded, so nothing would fetch
    /// the list. Fetch it once, on the message the reading pane shows.
    async fn ensure_attachment_list(&mut self, message_id: &str) {
        if self.db.attachments(message_id).map(|a| !a.is_empty()).unwrap_or(false) {
            return;
        }
        let Some(graph) = self.backend.mail() else { return };
        let items = graph.attachments(message_id).await;
        self.note_result(&items);
        if let Ok(items) = items {
            if self.db.set_attachments(message_id, &items).is_ok() && !items.is_empty() {
                self.emit(Event::AttachmentsChanged(message_id.to_string()));
            }
        }
    }

    /// Look at a message that was cached before invitations were read, and
    /// work out whether it carries one.
    ///
    /// Mail fetched from now on is checked as its body arrives, so this is
    /// only for what the cache already held — and the answer never changes,
    /// so a message that turns out to carry nothing is not asked about
    /// again this run.
    async fn ensure_invite(&mut self, message_id: &str) {
        if self.db.invite(message_id).is_some() || !self.invite_checked.insert(message_id.to_string())
        {
            return;
        }
        let Some(graph) = self.backend.mail() else { return };
        let items = self.db.attachments(message_id).unwrap_or_default();
        let result = cache_invite(&graph, &self.db, message_id, true, &items).await;
        // A failure here is worth another try, unlike an honest "no".
        if result.is_err() {
            self.invite_checked.remove(message_id);
        }
        self.note_result(&result);
        if self.db.invite(message_id).is_some() {
            self.emit(Event::InviteChanged(message_id.to_string()));
        }
    }

    /// Put an attachment on disk so the desktop can open it. Already
    /// downloaded, it is served straight from the cache — which is what
    /// makes an attachment readable again with no network.
    async fn ensure_attachment(&mut self, message_id: &str, attachment_id: &str) {
        let Some(item) = self.db.attachment(message_id, attachment_id) else {
            self.emit(Event::Failed("That attachment is no longer listed.".into()));
            return;
        };
        if let Some(path) = item.path.filter(|p| std::path::Path::new(p).exists()) {
            self.emit(Event::AttachmentReady {
                message_id: message_id.to_string(),
                attachment_id: attachment_id.to_string(),
                path,
            });
            return;
        }
        let Some(graph) = self.backend.mail() else {
            self.emit(Event::Failed("Attachments need a signed-in mailbox.".into()));
            return;
        };
        let bytes = graph.attachment_bytes(message_id, attachment_id).await;
        self.note_result(&bytes);
        let bytes = match bytes {
            Ok(bytes) => bytes,
            Err(e) if e.is_offline() => {
                self.publish_status();
                self.emit(Event::Failed(
                    "That attachment hasn't been downloaded yet, and you're offline.".into(),
                ));
                return;
            }
            Err(e) => {
                self.emit(Event::Failed(e.to_string()));
                return;
            }
        };
        // One directory per attachment, so two files of the same name on
        // different mail cannot overwrite one another.
        let dir = crate::config::attachments_dir(&self.key).join(short_key(attachment_id));
        let path = dir.join(safe_name(&item.name));
        let written = std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&path, &bytes));
        if let Err(e) = written {
            self.emit(Event::Failed(format!("Could not save the attachment: {e}")));
            return;
        }
        let path = path.to_string_lossy().to_string();
        let _ = self.db.set_attachment_path(message_id, attachment_id, &path);
        self.emit(Event::AttachmentReady {
            message_id: message_id.to_string(),
            attachment_id: attachment_id.to_string(),
            path,
        });
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
                Op::RespondToInvite { message_id, invite, response } => {
                    send_invite_response(&graph, message_id, invite, *response).await
                }
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
                    if let Op::Send { local_id, .. } = &op {
                        // Keep what was written: it moves to Drafts, marked
                        // as refused, rather than sitting in Sent Items
                        // claiming to be on its way.
                        let _ = self.db.mark_send_failed(local_id);
                        self.emit(Event::MessagesChanged(String::new()));
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

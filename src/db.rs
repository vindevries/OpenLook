//! Local mail cache. The UI reads only from here, which is what makes the
//! app work offline; the sync engine keeps it up to date and replays queued
//! changes when the network comes back.

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};

use crate::graph::folder_rank;
use crate::model::{
    Address, Body, CalendarEvent, Folder, MessageDetail, MessagePatch, MessageSummary, Op, Pending,
};
use crate::util::now_unix;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS folders (
    id           TEXT PRIMARY KEY,
    display_name TEXT NOT NULL,
    unread_count INTEGER NOT NULL DEFAULT 0,
    total_count  INTEGER NOT NULL DEFAULT 0,
    sort_order   INTEGER NOT NULL DEFAULT 100,
    delta_link   TEXT
);
CREATE TABLE IF NOT EXISTS messages (
    id              TEXT PRIMARY KEY,
    folder_id       TEXT NOT NULL,
    subject         TEXT NOT NULL DEFAULT '',
    from_name       TEXT NOT NULL DEFAULT '',
    from_addr       TEXT NOT NULL DEFAULT '',
    to_json         TEXT,
    cc_json         TEXT,
    received        TEXT NOT NULL DEFAULT '',
    preview         TEXT NOT NULL DEFAULT '',
    is_read         INTEGER NOT NULL DEFAULT 1,
    has_attachments INTEGER NOT NULL DEFAULT 0,
    body_is_html    INTEGER,
    body            TEXT,
    pending         INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_messages_folder ON messages(folder_id, received DESC);
CREATE TABLE IF NOT EXISTS outbox (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    kind       TEXT NOT NULL,
    message_id TEXT,
    payload    TEXT NOT NULL,
    attempts   INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS events (
    id        TEXT PRIMARY KEY,
    subject   TEXT NOT NULL DEFAULT '',
    organizer TEXT NOT NULL DEFAULT '',
    location  TEXT NOT NULL DEFAULT '',
    start_utc TEXT NOT NULL,
    end_utc   TEXT NOT NULL,
    all_day   INTEGER NOT NULL DEFAULT 0,
    cancelled INTEGER NOT NULL DEFAULT 0,
    preview   TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS idx_events_start ON events(start_utc);
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
"#;

#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(SCHEMA)?;
        // Caches written before appointments could be opened lack these.
        for statement in [
            "ALTER TABLE events ADD COLUMN body TEXT",
            "ALTER TABLE events ADD COLUMN attendees TEXT",
            "ALTER TABLE messages ADD COLUMN thread_ids TEXT",
        ] {
            let _ = conn.execute(statement, []);
        }
        // Partial delta entries used to be stored as whole messages, which
        // left rows behind with no date, sender or subject. Drop them and
        // let the folders that held them enumerate again, which refetches
        // the real messages.
        let _ = conn.execute(
            "UPDATE folders SET delta_link = NULL
              WHERE id IN (SELECT folder_id FROM messages WHERE received = '' AND pending = 0)",
            [],
        );
        let _ = conn.execute("DELETE FROM messages WHERE received = '' AND pending = 0", []);
        Ok(Db { conn: Arc::new(Mutex::new(conn)) })
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    // -- folders ---------------------------------------------------------

    pub fn folders(&self) -> Result<Vec<Folder>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, display_name, unread_count, total_count
               FROM folders ORDER BY sort_order, display_name COLLATE NOCASE",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(Folder {
                    id: r.get(0)?,
                    display_name: r.get(1)?,
                    unread_count: r.get(2)?,
                    total_count: r.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Replace the folder list, keeping delta tokens for folders we already
    /// know about.
    pub fn upsert_folders(&self, folders: &[Folder]) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO folders (id, display_name, unread_count, total_count, sort_order)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(id) DO UPDATE SET
                     display_name = excluded.display_name,
                     unread_count = excluded.unread_count,
                     total_count  = excluded.total_count,
                     sort_order   = excluded.sort_order",
            )?;
            for f in folders {
                stmt.execute(params![
                    f.id,
                    f.display_name,
                    f.unread_count,
                    f.total_count,
                    folder_rank(&f.display_name)
                ])?;
            }
            if !folders.is_empty() {
                // Drop folders that vanished server-side.
                let keep: Vec<String> = folders.iter().map(|f| f.id.clone()).collect();
                let placeholders = keep.iter().map(|_| "?").collect::<Vec<_>>().join(",");
                tx.execute(
                    &format!("DELETE FROM folders WHERE id NOT IN ({placeholders})"),
                    rusqlite::params_from_iter(keep.iter()),
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Order a folder explicitly. Ticket stages run New → … → Closed,
    /// which is nothing like the well-known-mail-folder ranking.
    pub fn set_folder_sort(&self, folder_id: &str, order: i64) -> Result<()> {
        self.conn()
            .execute("UPDATE folders SET sort_order = ?2 WHERE id = ?1", params![folder_id, order])?;
        Ok(())
    }

    pub fn folder_id_by_name(&self, name: &str) -> Option<String> {
        self.conn()
            .query_row("SELECT id FROM folders WHERE display_name = ?1", params![name], |r| r.get(0))
            .optional()
            .ok()
            .flatten()
    }

    pub fn delta_link(&self, folder_id: &str) -> Option<String> {
        self.conn()
            .query_row("SELECT delta_link FROM folders WHERE id = ?1", params![folder_id], |r| {
                r.get::<_, Option<String>>(0)
            })
            .optional()
            .ok()
            .flatten()
            .flatten()
    }

    pub fn set_delta_link(&self, folder_id: &str, link: Option<&str>) -> Result<()> {
        self.conn()
            .execute("UPDATE folders SET delta_link = ?2 WHERE id = ?1", params![folder_id, link])?;
        Ok(())
    }

    /// Recompute unread/total from cached messages. Used in demo mode, where
    /// there is no server to report counts.
    pub fn recompute_counts(&self) -> Result<()> {
        self.conn().execute_batch(
            "UPDATE folders SET
                 total_count  = (SELECT COUNT(*) FROM messages m WHERE m.folder_id = folders.id),
                 unread_count = (SELECT COUNT(*) FROM messages m WHERE m.folder_id = folders.id AND m.is_read = 0)",
        )?;
        Ok(())
    }

    // -- messages --------------------------------------------------------

    pub fn messages(&self, folder_id: &str, search: &str) -> Result<Vec<MessageSummary>> {
        let conn = self.conn();
        let search = search.trim();
        let like = format!("%{search}%");
        let sql = "SELECT id, folder_id, subject, from_name, from_addr, received, preview,
                          is_read, has_attachments, pending
                     FROM messages WHERE folder_id = ?1"
            .to_string();
        let sql = if search.is_empty() {
            sql + " ORDER BY received DESC"
        } else {
            sql + " AND (subject LIKE ?2 COLLATE NOCASE
                       OR from_name LIKE ?2 COLLATE NOCASE
                       OR from_addr LIKE ?2 COLLATE NOCASE
                       OR preview LIKE ?2 COLLATE NOCASE)
                    ORDER BY received DESC"
        };
        let mut stmt = conn.prepare(&sql)?;
        let map = |r: &rusqlite::Row<'_>| -> rusqlite::Result<MessageSummary> {
            Ok(MessageSummary {
                id: r.get(0)?,
                folder_id: r.get(1)?,
                subject: r.get(2)?,
                from: Address { name: r.get(3)?, address: r.get(4)? },
                received: r.get(5)?,
                preview: r.get(6)?,
                is_read: r.get::<_, i64>(7)? != 0,
                has_attachments: r.get::<_, i64>(8)? != 0,
                pending: if r.get::<_, i64>(9)? != 0 { Pending::Queued } else { Pending::None },
            })
        };
        let rows = if search.is_empty() {
            stmt.query_map(params![folder_id], map)?.collect::<rusqlite::Result<Vec<_>>>()?
        } else {
            stmt.query_map(params![folder_id, like], map)?.collect::<rusqlite::Result<Vec<_>>>()?
        };
        Ok(rows)
    }

    pub fn message(&self, id: &str) -> Result<Option<MessageDetail>> {
        let conn = self.conn();
        let detail = conn
            .query_row(
                "SELECT id, folder_id, subject, from_name, from_addr, received, preview,
                        is_read, has_attachments, pending, to_json, cc_json, body_is_html, body
                   FROM messages WHERE id = ?1",
                params![id],
                |r| {
                    let to_json: Option<String> = r.get(10)?;
                    let cc_json: Option<String> = r.get(11)?;
                    let body_is_html: Option<i64> = r.get(12)?;
                    let body: Option<String> = r.get(13)?;
                    Ok(MessageDetail {
                        summary: MessageSummary {
                            id: r.get(0)?,
                            folder_id: r.get(1)?,
                            subject: r.get(2)?,
                            from: Address { name: r.get(3)?, address: r.get(4)? },
                            received: r.get(5)?,
                            preview: r.get(6)?,
                            is_read: r.get::<_, i64>(7)? != 0,
                            has_attachments: r.get::<_, i64>(8)? != 0,
                            pending: if r.get::<_, i64>(9)? != 0 {
                                Pending::Queued
                            } else {
                                Pending::None
                            },
                        },
                        to: to_json.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default(),
                        cc: cc_json.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default(),
                        body: match (body, body_is_html) {
                            (Some(content), Some(is_html)) => {
                                Some(Body { is_html: is_html != 0, content })
                            }
                            _ => None,
                        },
                    })
                },
            )
            .optional()?;
        Ok(detail)
    }

    /// Insert or refresh message summaries from the server. Messages with
    /// queued local changes are left alone so an in-flight sync cannot undo
    /// what the user just did offline.
    pub fn upsert_messages(&self, messages: &[MessageSummary]) -> Result<()> {
        let pending = self.pending_message_ids()?;
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO messages
                     (id, folder_id, subject, from_name, from_addr, received, preview,
                      is_read, has_attachments)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(id) DO UPDATE SET
                     folder_id       = excluded.folder_id,
                     subject         = excluded.subject,
                     from_name       = excluded.from_name,
                     from_addr       = excluded.from_addr,
                     received        = excluded.received,
                     preview         = excluded.preview,
                     is_read         = excluded.is_read,
                     has_attachments = excluded.has_attachments",
            )?;
            for m in messages {
                if pending.contains(&m.id) {
                    continue;
                }
                stmt.execute(params![
                    m.id,
                    m.folder_id,
                    m.subject,
                    m.from.name,
                    m.from.address,
                    m.received,
                    m.preview,
                    m.is_read as i64,
                    m.has_attachments as i64,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Apply delta entries that carried only the properties that changed:
    /// write those, leave the rest of the cached row alone. A message that
    /// is not cached is skipped — a fields-only entry cannot make a usable
    /// row, and the next enumeration brings the whole message.
    pub fn patch_messages(&self, patches: &[MessagePatch]) -> Result<()> {
        if patches.is_empty() {
            return Ok(());
        }
        let pending = self.pending_message_ids()?;
        for patch in patches {
            if pending.contains(&patch.id) {
                continue;
            }
            let cached: Option<String> = self
                .conn()
                .query_row(
                    "SELECT folder_id FROM messages WHERE id = ?1",
                    params![patch.id],
                    |r| r.get(0),
                )
                .optional()?;
            let Some(folder_id) = cached else { continue };
            // Moves and read changes go through the same paths as local
            // edits, so the folder badges stay in step either way.
            if let Some(target) = &patch.folder_id {
                if *target != folder_id {
                    self.move_message(&patch.id, target)?;
                }
            }
            if let Some(is_read) = patch.is_read {
                self.set_read(&patch.id, is_read)?;
            }
            let (from_name, from_addr) = match &patch.from {
                Some(from) => (Some(&from.name), Some(&from.address)),
                None => (None, None),
            };
            self.conn().execute(
                "UPDATE messages SET
                     subject         = COALESCE(?2, subject),
                     from_name       = COALESCE(?3, from_name),
                     from_addr       = COALESCE(?4, from_addr),
                     received        = COALESCE(?5, received),
                     preview         = COALESCE(?6, preview),
                     has_attachments = COALESCE(?7, has_attachments)
                  WHERE id = ?1",
                params![
                    patch.id,
                    patch.subject,
                    from_name,
                    from_addr,
                    patch.received,
                    patch.preview,
                    patch.has_attachments.map(|v| v as i64),
                ],
            )?;
        }
        Ok(())
    }

    /// Drop cached messages that are no longer in a freshly synced window,
    /// bounded to that window so older cached mail is preserved.
    pub fn reconcile_window(&self, folder_id: &str, present: &[String], oldest: &str) -> Result<()> {
        if present.is_empty() {
            return Ok(());
        }
        let pending = self.pending_message_ids()?;
        let conn = self.conn();
        let keep: HashSet<&String> = present.iter().collect();
        let mut stmt = conn.prepare(
            "SELECT id FROM messages WHERE folder_id = ?1 AND received >= ?2 AND pending = 0",
        )?;
        let cached: Vec<String> = stmt
            .query_map(params![folder_id, oldest], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        for id in cached {
            if !keep.contains(&id) && !pending.contains(&id) {
                conn.execute("DELETE FROM messages WHERE id = ?1", params![id])?;
            }
        }
        Ok(())
    }

    pub fn remove_message(&self, id: &str) -> Result<()> {
        self.conn().execute("DELETE FROM messages WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn set_body(&self, id: &str, body: &Body, to: &[Address], cc: &[Address]) -> Result<()> {
        self.conn().execute(
            "UPDATE messages SET body = ?2, body_is_html = ?3, to_json = ?4, cc_json = ?5
              WHERE id = ?1",
            params![
                id,
                body.content,
                body.is_html as i64,
                serde_json::to_string(to)?,
                serde_json::to_string(cc)?
            ],
        )?;
        Ok(())
    }

    /// Newest cached messages without a body, so they can be prefetched for
    /// offline reading.
    pub fn missing_bodies(&self, folder_id: &str, limit: usize) -> Result<Vec<String>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id FROM messages
              WHERE folder_id = ?1 AND body IS NULL AND pending = 0
              ORDER BY received DESC LIMIT ?2",
        )?;
        let ids = stmt
            .query_map(params![folder_id, limit as i64], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(ids)
    }

    /// Apply a read/unread change locally, adjusting the folder badge.
    pub fn set_read(&self, id: &str, is_read: bool) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let previous: Option<i64> = tx
            .query_row("SELECT is_read FROM messages WHERE id = ?1", params![id], |r| r.get(0))
            .optional()?;
        let Some(previous) = previous else { return Ok(()) };
        if (previous != 0) == is_read {
            return Ok(());
        }
        tx.execute("UPDATE messages SET is_read = ?2 WHERE id = ?1", params![id, is_read as i64])?;
        let delta = if is_read { -1 } else { 1 };
        tx.execute(
            "UPDATE folders SET unread_count = MAX(0, unread_count + ?2)
              WHERE id = (SELECT folder_id FROM messages WHERE id = ?1)",
            params![id, delta],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn move_message(&self, id: &str, folder_id: &str) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE folders SET
                 total_count  = MAX(0, total_count - 1),
                 unread_count = MAX(0, unread_count -
                     (SELECT CASE WHEN is_read = 0 THEN 1 ELSE 0 END FROM messages WHERE id = ?1))
              WHERE id = (SELECT folder_id FROM messages WHERE id = ?1)",
            params![id],
        )?;
        tx.execute("UPDATE messages SET folder_id = ?2 WHERE id = ?1", params![id, folder_id])?;
        tx.execute(
            "UPDATE folders SET total_count = total_count + 1 WHERE id = ?1",
            params![folder_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Store a message that exists only locally: one composed here and
    /// waiting to send, or a seeded demo message. The body is written too,
    /// so it is readable offline straight away.
    pub fn insert_local_message(
        &self,
        summary: &MessageSummary,
        to: &[Address],
        cc: &[Address],
        body: &Body,
    ) -> Result<()> {
        self.conn().execute(
            "INSERT OR REPLACE INTO messages
                 (id, folder_id, subject, from_name, from_addr, received, preview,
                  is_read, has_attachments, to_json, cc_json, body_is_html, body, pending)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                summary.id,
                summary.folder_id,
                summary.subject,
                summary.from.name,
                summary.from.address,
                summary.received,
                summary.preview,
                summary.is_read as i64,
                summary.has_attachments as i64,
                serde_json::to_string(to)?,
                serde_json::to_string(cc)?,
                body.is_html as i64,
                body.content,
                matches!(summary.pending, Pending::Queued) as i64,
            ],
        )?;
        Ok(())
    }

    /// Point a cached row at a new server id. Moving a message server-side
    /// creates a new item with a new id, so without this the next sync of
    /// the destination folder would add a duplicate and leave a stale row.
    pub fn rename_message(&self, old_id: &str, new_id: &str) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "UPDATE OR REPLACE messages SET id = ?2 WHERE id = ?1",
            params![old_id, new_id],
        )?;
        conn.execute(
            "UPDATE outbox SET message_id = ?2 WHERE message_id = ?1",
            params![old_id, new_id],
        )?;
        Ok(())
    }

    /// Conversation threads behind a ticket row, newest last.
    pub fn thread_ids(&self, id: &str) -> Vec<String> {
        self.conn()
            .query_row("SELECT thread_ids FROM messages WHERE id = ?1", params![id], |r| {
                r.get::<_, Option<String>>(0)
            })
            .optional()
            .ok()
            .flatten()
            .flatten()
            .and_then(|j| serde_json::from_str::<Vec<String>>(&j).ok())
            .unwrap_or_default()
    }

    pub fn set_thread_ids(&self, id: &str, threads: &[String]) -> Result<()> {
        self.conn().execute(
            "UPDATE messages SET thread_ids = ?2 WHERE id = ?1",
            params![id, serde_json::to_string(threads)?],
        )?;
        Ok(())
    }

    pub fn clear_pending_flag(&self, id: &str) -> Result<()> {
        self.conn().execute("UPDATE messages SET pending = 0 WHERE id = ?1", params![id])?;
        Ok(())
    }

    // -- outbox ----------------------------------------------------------

    pub fn enqueue(&self, op: &Op) -> Result<i64> {
        let message_id = match op {
            Op::MarkRead { message_id, .. }
            | Op::Delete { message_id, .. }
            | Op::Move { message_id, .. } => Some(message_id.clone()),
            Op::Send { local_id, .. } => Some(local_id.clone()),
        };
        let conn = self.conn();
        conn.execute(
            "INSERT INTO outbox (kind, message_id, payload, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![op.kind(), message_id, serde_json::to_string(op)?, now_unix()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Queued operations, oldest first, with their attempt counts.
    pub fn queued(&self) -> Result<Vec<(i64, Op, i64)>> {
        let conn = self.conn();
        let mut stmt =
            conn.prepare("SELECT id, payload, attempts FROM outbox ORDER BY id LIMIT 100")?;
        let rows = stmt
            .query_map([], |r| {
                let id: i64 = r.get(0)?;
                let payload: String = r.get(1)?;
                let attempts: i64 = r.get(2)?;
                Ok((id, payload, attempts))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows
            .into_iter()
            .filter_map(|(id, payload, attempts)| {
                serde_json::from_str::<Op>(&payload).ok().map(|op| (id, op, attempts))
            })
            .collect())
    }

    pub fn queued_count(&self) -> i64 {
        self.conn()
            .query_row("SELECT COUNT(*) FROM outbox", [], |r| r.get(0))
            .unwrap_or(0)
    }

    pub fn dequeue(&self, id: i64) -> Result<()> {
        self.conn().execute("DELETE FROM outbox WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn record_failure(&self, id: i64, error: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE outbox SET attempts = attempts + 1, last_error = ?2 WHERE id = ?1",
            params![id, error],
        )?;
        Ok(())
    }

    fn pending_message_ids(&self) -> Result<HashSet<String>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT message_id FROM outbox WHERE message_id IS NOT NULL")?;
        let ids = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<HashSet<_>>>()?;
        Ok(ids)
    }

    // -- calendar --------------------------------------------------------

    /// Replace everything cached for a date range. The server view is the
    /// truth for that window, so this also removes events cancelled or
    /// moved away since the last sync.
    pub fn replace_events(&self, start: &str, end: &str, events: &[CalendarEvent]) -> Result<()> {
        let keep: Vec<String> = events.iter().map(|e| e.id.clone()).collect();
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        {
            // Drop only what the server no longer reports in this window, so
            // cancellations disappear but already-fetched bodies survive.
            let placeholders =
                if keep.is_empty() { "''".to_string() } else { keep.iter().map(|_| "?").collect::<Vec<_>>().join(",") };
            let sql = format!(
                "DELETE FROM events WHERE start_utc < ?1 AND end_utc > ?2 AND id NOT IN ({placeholders})"
            );
            let mut args: Vec<Box<dyn rusqlite::ToSql>> =
                vec![Box::new(end.to_string()), Box::new(start.to_string())];
            for id in &keep {
                args.push(Box::new(id.clone()));
            }
            tx.execute(&sql, rusqlite::params_from_iter(args.iter().map(|a| a.as_ref())))?;

            let mut stmt = tx.prepare(
                "INSERT INTO events
                     (id, subject, organizer, location, start_utc, end_utc,
                      all_day, cancelled, preview)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(id) DO UPDATE SET
                     subject = excluded.subject, organizer = excluded.organizer,
                     location = excluded.location, start_utc = excluded.start_utc,
                     end_utc = excluded.end_utc, all_day = excluded.all_day,
                     cancelled = excluded.cancelled, preview = excluded.preview",
            )?;
            for e in events {
                stmt.execute(params![
                    e.id, e.subject, e.organizer, e.location, e.start, e.end,
                    e.all_day as i64, e.cancelled as i64, e.preview,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// One cached appointment, with its body if it has been fetched.
    pub fn event(&self, id: &str) -> Result<Option<(CalendarEvent, Option<String>, Vec<String>)>> {
        let conn = self.conn();
        let row = conn
            .query_row(
                "SELECT id, subject, organizer, location, start_utc, end_utc,
                        all_day, cancelled, preview, body, attendees
                   FROM events WHERE id = ?1",
                params![id],
                |r| {
                    let body: Option<String> = r.get(9)?;
                    let attendees: Option<String> = r.get(10)?;
                    Ok((
                        CalendarEvent {
                            id: r.get(0)?,
                            subject: r.get(1)?,
                            organizer: r.get(2)?,
                            location: r.get(3)?,
                            start: r.get(4)?,
                            end: r.get(5)?,
                            all_day: r.get::<_, i64>(6)? != 0,
                            cancelled: r.get::<_, i64>(7)? != 0,
                            preview: r.get(8)?,
                            mailbox: String::new(),
                        },
                        body,
                        attendees
                            .and_then(|j| serde_json::from_str::<Vec<String>>(&j).ok())
                            .unwrap_or_default(),
                    ))
                },
            )
            .optional()?;
        Ok(row)
    }

    pub fn set_event_body(&self, id: &str, body: &str, attendees: &[String]) -> Result<()> {
        self.conn().execute(
            "UPDATE events SET body = ?2, attendees = ?3 WHERE id = ?1",
            params![id, body, serde_json::to_string(attendees)?],
        )?;
        Ok(())
    }

    /// Events overlapping a range, earliest first.
    pub fn events_between(&self, start: &str, end: &str) -> Result<Vec<CalendarEvent>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, subject, organizer, location, start_utc, end_utc,
                    all_day, cancelled, preview
               FROM events
              WHERE start_utc < ?2 AND end_utc > ?1
              ORDER BY all_day DESC, start_utc",
        )?;
        let rows = stmt
            .query_map(params![start, end], |r| {
                Ok(CalendarEvent {
                    id: r.get(0)?,
                    subject: r.get(1)?,
                    organizer: r.get(2)?,
                    location: r.get(3)?,
                    start: r.get(4)?,
                    end: r.get(5)?,
                    all_day: r.get::<_, i64>(6)? != 0,
                    cancelled: r.get::<_, i64>(7)? != 0,
                    preview: r.get(8)?,
                    mailbox: String::new(),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // -- meta ------------------------------------------------------------

    pub fn meta(&self, key: &str) -> Option<String> {
        self.conn()
            .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| r.get(0))
            .optional()
            .ok()
            .flatten()
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn().execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.conn()
            .query_row("SELECT COUNT(*) FROM folders", [], |r| r.get::<_, i64>(0))
            .unwrap_or(0)
            == 0
    }
}

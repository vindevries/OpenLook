//! Shared data model. Everything the UI shows comes out of the local
//! database as one of these types.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Address {
    pub name: String,
    pub address: String,
}

impl Address {
    pub fn new(name: impl Into<String>, address: impl Into<String>) -> Self {
        Self { name: name.into(), address: address.into() }
    }

    /// Bare address, used when the display name is unknown.
    pub fn bare(address: impl Into<String>) -> Self {
        let address = address.into();
        Self { name: address.clone(), address }
    }

    pub fn display(&self) -> &str {
        if !self.name.is_empty() {
            &self.name
        } else if !self.address.is_empty() {
            &self.address
        } else {
            "(unknown)"
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Folder {
    pub id: String,
    pub display_name: String,
    pub unread_count: i64,
    pub total_count: i64,
}

/// Where a locally-composed message stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pending {
    /// Fully in sync with the server (or a demo message).
    None,
    /// Composed locally, still waiting to be sent.
    Queued,
}

#[derive(Debug, Clone)]
pub struct MessageSummary {
    pub id: String,
    pub folder_id: String,
    pub subject: String,
    pub from: Address,
    /// RFC3339 in UTC, so plain string ordering is chronological.
    pub received: String,
    pub preview: String,
    pub is_read: bool,
    pub has_attachments: bool,
    pub pending: Pending,
}

#[derive(Debug, Clone)]
pub struct MessageDetail {
    pub summary: MessageSummary,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    /// `None` when the body has not been downloaded yet.
    pub body: Option<Body>,
}

#[derive(Debug, Clone)]
pub struct Body {
    pub is_html: bool,
    pub content: String,
}

/// How a composed message relates to an existing one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum SendMode {
    #[default]
    New,
    Reply,
    ReplyAll,
    Forward,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Outgoing {
    pub to: Vec<String>,
    #[serde(default)]
    pub cc: Vec<String>,
    pub subject: String,
    pub body: String,
    /// The message being replied to or forwarded, if any.
    #[serde(default)]
    pub in_reply_to: Option<String>,
    #[serde(default)]
    pub mode: SendMode,
}

/// A mailbox change that must eventually reach the server. Applied to the
/// local database immediately, then replayed from the outbox when online.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum Op {
    MarkRead {
        message_id: String,
        is_read: bool,
    },
    Delete {
        message_id: String,
        /// Already in Deleted Items, so this is a permanent delete rather
        /// than a move to the bin.
        #[serde(default)]
        purge: bool,
    },
    Move {
        message_id: String,
        /// Destination folder id.
        folder_id: String,
    },
    Send {
        local_id: String,
        message: Outgoing,
    },
}

impl Op {
    pub fn kind(&self) -> &'static str {
        match self {
            Op::MarkRead { .. } => "mark_read",
            Op::Delete { purge: true, .. } => "purge",
            Op::Delete { .. } => "delete",
            Op::Move { .. } => "move",
            Op::Send { .. } => "send",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct AccountInfo {
    pub username: String,
    pub name: String,
}

/// Snapshot of sync state, shown in the header bar.
#[derive(Debug, Clone)]
pub struct Status {
    pub online: bool,
    pub syncing: bool,
    pub queued: i64,
    /// Unix seconds of the last successful sync.
    pub last_sync: Option<i64>,
    pub detail: Option<String>,
}

impl Default for Status {
    fn default() -> Self {
        Self { online: true, syncing: false, queued: 0, last_sync: None, detail: None }
    }
}

/// A calendar entry, as shown in the month view. Times are RFC3339 in UTC
/// so they sort correctly; they are converted to local time for display.
#[derive(Debug, Clone)]
pub struct CalendarEvent {
    pub id: String,
    pub subject: String,
    pub organizer: String,
    pub location: String,
    pub start: String,
    pub end: String,
    pub all_day: bool,
    pub cancelled: bool,
    pub preview: String,
    /// Which mailbox it came from, filled in when events are merged.
    pub mailbox: String,
}

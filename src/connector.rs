//! What a source of items other than mail has to provide.
//!
//! The tickets pane is not about HubSpot: it is about anything that has
//! sections to list, items in them, and a body to read — a helpdesk queue,
//! an issue tracker, a shared mailbox behind someone else's API. This is
//! the whole of what the sync engine asks of such a source, so a new one
//! is a new implementation of this trait and nothing else.

use std::future::Future;
use std::pin::Pin;

use crate::model::{Body, MessageSummary};

/// Work a connector is doing. Boxed rather than `async fn` in the trait,
/// because the engine holds connectors as trait objects.
pub type Task<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

pub type Result<T> = std::result::Result<T, Error>;

/// A failure, classified the way the sync engine needs to act on it: keep
/// the cache and wait, ask for credentials, retry later, or give up.
#[derive(Debug)]
pub enum Error {
    /// No usable network; the caller should keep queued work queued.
    Offline(String),
    /// Credentials missing, rejected, or lacking a permission.
    Auth(String),
    /// The item is gone.
    NotFound,
    /// Worth retrying — throttling or a server fault.
    Transient(String),
    /// Permanent rejection.
    Permanent(String),
}

impl Error {
    pub fn is_offline(&self) -> bool {
        matches!(self, Error::Offline(_))
    }

    /// Whether queued work should stay queued for a later attempt.
    pub fn should_retry(&self) -> bool {
        matches!(self, Error::Offline(_) | Error::Transient(_) | Error::Auth(_))
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Offline(_) => write!(f, "You are offline — showing what was last fetched."),
            Error::Auth(m) => write!(f, "{m}"),
            Error::NotFound => write!(f, "That item no longer exists."),
            Error::Transient(m) | Error::Permanent(m) => write!(f, "{m}"),
        }
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

/// One entry in the folder pane: a stage, a queue, a label. They are shown
/// in the order they are given, which is rarely alphabetical.
pub struct Section {
    pub id: String,
    pub name: String,
}

/// An item's content, as the reading pane needs it.
pub struct ItemBody {
    pub body: Body,
    /// The conversations the item is made of, kept so that a reply knows
    /// where it belongs. Empty when the source has no such notion.
    pub threads: Vec<String>,
}

pub trait Connector: Send + Sync {
    /// What this connector's items are called, for lines like
    /// "Offline — showing cached tickets".
    fn items_called(&self) -> &str {
        "items"
    }

    /// The folder-pane entries, in the order they should appear.
    fn sections(&self) -> Task<'_, Vec<Section>>;

    /// Everything worth showing, each row naming the section it is in.
    /// The engine caches these, so a connector returns what it can see
    /// now rather than trying to work out what changed.
    fn items(&self) -> Task<'_, Vec<MessageSummary>>;

    /// One item's body. The cached row is passed in because a source
    /// often holds part of the content itself — the description of a
    /// ticket raised through a web form, say.
    fn body<'a>(&'a self, item: &'a MessageSummary) -> Task<'a, ItemBody>;
}

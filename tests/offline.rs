//! Offline behaviour tests: these exercise the cache and outbox directly,
//! without a display or a network.

use std::path::PathBuf;

use openlook::db::Db;
use openlook::model::{Op, Outgoing};
use openlook::sync::apply_local;

fn temp_db(name: &str) -> (Db, PathBuf) {
    let path = std::env::temp_dir().join(format!("openlook-test-{name}-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let db = Db::open(&path).expect("open db");
    openlook::demo::seed(&db).expect("seed");
    (db, path)
}

fn cleanup(path: &PathBuf) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(path.with_extension("db-wal"));
    let _ = std::fs::remove_file(path.with_extension("db-shm"));
}

#[test]
fn cached_mail_is_readable_without_network() {
    let (db, path) = temp_db("read");

    let inbox = db.folder_id_by_name("Inbox").expect("inbox exists");
    let messages = db.messages(&inbox, "").expect("list");
    assert!(messages.len() >= 5, "inbox should be seeded");

    // Bodies come from the cache, so they are readable with no network.
    let detail = db.message(&messages[0].id).expect("query").expect("message");
    let body = detail.body.expect("body cached for offline reading");
    assert!(!body.content.is_empty());

    // Newest first.
    assert!(messages[0].received >= messages[1].received);
    cleanup(&path);
}

#[test]
fn unread_counts_track_reads() {
    let (db, path) = temp_db("counts");
    let inbox = db.folder_id_by_name("Inbox").unwrap();
    let before = db.folders().unwrap().into_iter().find(|f| f.id == inbox).unwrap();
    assert_eq!(before.unread_count, 3, "demo seeds three unread messages");

    let unread = db
        .messages(&inbox, "")
        .unwrap()
        .into_iter()
        .find(|m| !m.is_read)
        .expect("an unread message");
    apply_local(&db, &Op::MarkRead { message_id: unread.id.clone(), is_read: true }).unwrap();

    let after = db.folders().unwrap().into_iter().find(|f| f.id == inbox).unwrap();
    assert_eq!(after.unread_count, before.unread_count - 1, "badge should drop by one");
    assert!(db.message(&unread.id).unwrap().unwrap().summary.is_read);
    cleanup(&path);
}

#[test]
fn delete_moves_to_bin_then_removes() {
    let (db, path) = temp_db("delete");
    let inbox = db.folder_id_by_name("Inbox").unwrap();
    let bin = db.folder_id_by_name("Deleted Items").unwrap();
    let victim = db.messages(&inbox, "").unwrap().remove(0);

    apply_local(&db, &Op::Delete { message_id: victim.id.clone(), purge: false }).unwrap();
    assert_eq!(db.message(&victim.id).unwrap().unwrap().summary.folder_id, bin);

    // Deleting again from the bin removes it for good.
    apply_local(&db, &Op::Delete { message_id: victim.id.clone(), purge: true }).unwrap();
    assert!(db.message(&victim.id).unwrap().is_none());
    cleanup(&path);
}

#[test]
fn changes_made_offline_are_queued_and_survive_restart() {
    let path = std::env::temp_dir().join(format!("openlook-test-queue-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let db = Db::open(&path).unwrap();
    openlook::demo::seed(&db).unwrap();

    let inbox = db.folder_id_by_name("Inbox").unwrap();
    let target = db.messages(&inbox, "").unwrap().remove(0);

    // Simulate the app being offline: apply locally, then queue for later.
    let op = Op::MarkRead { message_id: target.id.clone(), is_read: true };
    apply_local(&db, &op).unwrap();
    db.enqueue(&op).unwrap();
    let send = Op::Send {
        local_id: "local:test".into(),
        message: Outgoing {
            to: vec!["someone@example.com".into()],
            cc: vec![],
            subject: "Written on a train".into(),
            body: "No signal here.".into(),
            in_reply_to: None,
            mode: openlook::model::SendMode::New,
        },
    };
    apply_local(&db, &send).unwrap();
    db.enqueue(&send).unwrap();
    assert_eq!(db.queued_count(), 2);

    // The queued message is visible right away, flagged as pending.
    let sent_folder = db.folder_id_by_name("Sent Items").unwrap();
    let queued_msg = db
        .messages(&sent_folder, "")
        .unwrap()
        .into_iter()
        .find(|m| m.id == "local:test")
        .expect("queued message shows in Sent Items");
    assert!(matches!(queued_msg.pending, openlook::model::Pending::Queued));

    // Reopen the database, as if the app had been restarted while offline.
    drop(db);
    let db = Db::open(&path).unwrap();
    assert_eq!(db.queued_count(), 2, "queued work must survive a restart");
    let ops = db.queued().unwrap();
    assert_eq!(ops.len(), 2);
    assert!(matches!(ops[0].1, Op::MarkRead { .. }));
    assert!(matches!(ops[1].1, Op::Send { .. }));

    // Draining the queue mimics a successful flush once back online.
    for (row_id, _, _) in ops {
        db.dequeue(row_id).unwrap();
    }
    assert_eq!(db.queued_count(), 0);
    cleanup(&path);
}

#[test]
fn sync_does_not_clobber_pending_local_changes() {
    let (db, path) = temp_db("clobber");
    let inbox = db.folder_id_by_name("Inbox").unwrap();
    let target = db.messages(&inbox, "").unwrap().into_iter().find(|m| !m.is_read).unwrap();

    // Read it offline; the change is queued.
    let op = Op::MarkRead { message_id: target.id.clone(), is_read: true };
    apply_local(&db, &op).unwrap();
    db.enqueue(&op).unwrap();

    // A sync then returns the server's stale view (still unread).
    let stale = openlook::model::MessageSummary { is_read: false, ..target.clone() };
    db.upsert_messages(&[stale]).unwrap();

    assert!(
        db.message(&target.id).unwrap().unwrap().summary.is_read,
        "a stale sync must not undo a change that is still queued"
    );
    cleanup(&path);
}

#[test]
fn search_filters_the_cached_folder() {
    let (db, path) = temp_db("search");
    let inbox = db.folder_id_by_name("Inbox").unwrap();

    let hits = db.messages(&inbox, "pipeline").unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].subject.to_lowercase().contains("pipeline"));

    // Case-insensitive, and covers sender and preview as well as subject:
    // one message is from Anna, another only mentions her in its body.
    let upper = db.messages(&inbox, "ANNA").unwrap();
    let lower = db.messages(&inbox, "anna").unwrap();
    assert_eq!(upper.len(), lower.len(), "search must be case-insensitive");
    assert_eq!(upper.len(), 2);
    assert!(upper.iter().any(|m| m.from.name == "Anna Visser"));
    assert!(upper.iter().any(|m| m.from.name == "Jira" && m.preview.contains("Anna")));

    assert!(db.messages(&inbox, "zzzz-no-match").unwrap().is_empty());
    cleanup(&path);
}

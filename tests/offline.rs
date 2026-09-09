//! Offline behaviour tests: these exercise the cache and outbox directly,
//! without a display or a network.

use std::path::PathBuf;

use openlook::db::Db;
use openlook::model::{MessagePatch, Op, Outgoing};
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

/// Graph reports a message it has already sent once by listing only the
/// properties that changed. Writing that as a whole message blanked the date
/// and sender, which dropped the message to the bottom of the list under
/// "Older" and left it looking as if it had vanished.
#[test]
fn a_partial_delta_entry_only_changes_what_it_carries() {
    let (db, path) = temp_db("patch");
    let inbox = db.folder_id_by_name("Inbox").unwrap();
    let before = db.messages(&inbox, "").unwrap().into_iter().find(|m| !m.is_read).unwrap();

    // What arrives when the message is opened here or read on a phone: an id
    // and an isRead, nothing else.
    db.patch_messages(&[MessagePatch {
        id: before.id.clone(),
        is_read: Some(true),
        ..Default::default()
    }])
    .unwrap();

    let after = db
        .messages(&inbox, "")
        .unwrap()
        .into_iter()
        .find(|m| m.id == before.id)
        .expect("still in the folder");
    assert!(after.is_read, "the property that did change is applied");
    assert_eq!(after.received, before.received, "the date survives");
    assert_eq!(after.subject, before.subject, "the subject survives");
    assert_eq!(after.from, before.from, "the sender survives");

    let folder = db.folders().unwrap().into_iter().find(|f| f.id == inbox).unwrap();
    assert_eq!(folder.unread_count, 2, "the badge follows the change");
    cleanup(&path);
}

/// A fields-only entry for a message that was never cached cannot be turned
/// into a usable row; writing one used to create a blank ghost message.
#[test]
fn a_partial_delta_entry_for_unknown_mail_is_ignored() {
    let (db, path) = temp_db("patch-unknown");
    let inbox = db.folder_id_by_name("Inbox").unwrap();
    let before = db.messages(&inbox, "").unwrap().len();

    db.patch_messages(&[MessagePatch {
        id: "not-in-the-cache".into(),
        is_read: Some(false),
        ..Default::default()
    }])
    .unwrap();

    assert_eq!(db.messages(&inbox, "").unwrap().len(), before, "no ghost row appears");
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

#[test]
fn archiving_is_queued_for_the_server() {
    let (db, path) = temp_db("archive");
    let inbox = db.folder_id_by_name("Inbox").unwrap();
    let archive = db.folder_id_by_name("Archive").unwrap();
    let msg = db.messages(&inbox, "").unwrap().remove(0);

    let op = Op::Move { message_id: msg.id.clone(), folder_id: archive.clone() };
    apply_local(&db, &op).unwrap();
    db.enqueue(&op).unwrap();

    // Moves locally at once, so the list updates immediately...
    assert_eq!(db.message(&msg.id).unwrap().unwrap().summary.folder_id, archive);
    // ...and is queued, so it actually reaches the server. Archive used to
    // write straight to the cache and never enqueue anything.
    assert_eq!(db.queued_count(), 1);
    assert!(matches!(db.queued().unwrap()[0].1, Op::Move { .. }));

    // The other half of that bug: a sync still showing the message in the
    // Inbox must not drag it back out of Archive.
    let stale = openlook::model::MessageSummary { folder_id: inbox.clone(), ..msg.clone() };
    db.upsert_messages(&[stale]).unwrap();
    assert_eq!(
        db.message(&msg.id).unwrap().unwrap().summary.folder_id,
        archive,
        "a queued move must survive a sync that still reports the old folder"
    );
    cleanup(&path);
}

#[test]
fn a_server_move_renames_the_cached_message() {
    let (db, path) = temp_db("rename");
    let inbox = db.folder_id_by_name("Inbox").unwrap();
    let msg = db.messages(&inbox, "").unwrap().remove(0);

    // Graph gives a moved message a new id; the cache has to follow it,
    // otherwise the next sync adds a duplicate beside a stale row.
    db.rename_message(&msg.id, "server-side-new-id").unwrap();
    assert!(db.message(&msg.id).unwrap().is_none());
    let moved = db.message("server-side-new-id").unwrap().expect("row follows the new id");
    assert_eq!(moved.summary.subject, msg.subject);
    cleanup(&path);
}

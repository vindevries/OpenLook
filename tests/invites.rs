//! Reading calendar invitations, and answering them offline.
//!
//! The two samples here are the shapes that actually arrive: what Google
//! Calendar attaches to an invitation mail, and what Outlook attaches.
//! Neither needs a network or a display.

use std::path::PathBuf;

use openlook::db::Db;
use openlook::invite::{self, Method, Response};
use openlook::model::Op;
use openlook::sync::apply_local;

/// What Google Calendar sends: a `text/calendar` part, times in the
/// organiser's zone, and a VTIMEZONE saying what that zone does.
const GOOGLE: &str = "\
BEGIN:VCALENDAR\r
PRODID:-//Google Inc//Google Calendar 70.9054//EN\r
VERSION:2.0\r
CALSCALE:GREGORIAN\r
METHOD:REQUEST\r
BEGIN:VTIMEZONE\r
TZID:Europe/Amsterdam\r
X-LIC-LOCATION:Europe/Amsterdam\r
BEGIN:DAYLIGHT\r
TZOFFSETFROM:+0100\r
TZOFFSETTO:+0200\r
TZNAME:CEST\r
DTSTART:19700329T020000\r
RRULE:FREQ=YEARLY;BYMONTH=3;BYDAY=-1SU\r
END:DAYLIGHT\r
BEGIN:STANDARD\r
TZOFFSETFROM:+0200\r
TZOFFSETTO:+0100\r
TZNAME:CET\r
DTSTART:19701025T030000\r
RRULE:FREQ=YEARLY;BYMONTH=10;BYDAY=-1SU\r
END:STANDARD\r
END:VTIMEZONE\r
BEGIN:VEVENT\r
DTSTART;TZID=Europe/Amsterdam:20260615T140000\r
DTEND;TZID=Europe/Amsterdam:20260615T150000\r
DTSTAMP:20260601T090000Z\r
ORGANIZER;CN=Sofia Lindqvist:mailto:sofia@example.com\r
UID:4kq9h1p2r7@google.com\r
ATTENDEE;CUTYPE=INDIVIDUAL;ROLE=REQ-PARTICIPANT;PARTSTAT=NEEDS-ACTION;CN=You\r
 ;X-NUM-GUESTS=0:mailto:you@example.com\r
CREATED:20260601T085959Z\r
DESCRIPTION:Quarterly catch-up\\, then lunch.\r
LAST-MODIFIED:20260601T085959Z\r
LOCATION:Amsterdam office\\, room 3\r
SEQUENCE:0\r
STATUS:CONFIRMED\r
SUMMARY:Quarterly catch-up\r
TRANSP:OPAQUE\r
END:VEVENT\r
END:VCALENDAR\r
";

/// What Outlook sends: the same idea, a Windows zone name, and a winter
/// date — so the standard-time branch of the rule is the one that counts.
const OUTLOOK: &str = "\
BEGIN:VCALENDAR\r
METHOD:REQUEST\r
PRODID:Microsoft Exchange Server 2010\r
VERSION:2.0\r
BEGIN:VTIMEZONE\r
TZID:W. Europe Standard Time\r
BEGIN:STANDARD\r
DTSTART:16010101T030000\r
TZOFFSETFROM:+0200\r
TZOFFSETTO:+0100\r
RRULE:FREQ=YEARLY;INTERVAL=1;BYDAY=-1SU;BYMONTH=10\r
END:STANDARD\r
BEGIN:DAYLIGHT\r
DTSTART:16010101T020000\r
TZOFFSETFROM:+0100\r
TZOFFSETTO:+0200\r
RRULE:FREQ=YEARLY;INTERVAL=1;BYDAY=-1SU;BYMONTH=3\r
END:DAYLIGHT\r
END:VTIMEZONE\r
BEGIN:VEVENT\r
ORGANIZER;CN=Anna Visser:MAILTO:anna.visser@contoso.com\r
ATTENDEE;ROLE=REQ-PARTICIPANT;PARTSTAT=NEEDS-ACTION;RSVP=TRUE;CN=You:MAILTO:y\r
 ou@example.com\r
DESCRIPTION;LANGUAGE=en-GB:Budget checkpoint\r
SUMMARY;LANGUAGE=en-GB:Q1 budget review\r
DTSTART;TZID=W. Europe Standard Time:20261118T100000\r
DTEND;TZID=W. Europe Standard Time:20261118T113000\r
UID:040000008200E00074C5B7101A82E0080000000\r
CLASS:PUBLIC\r
PRIORITY:5\r
DTSTAMP:20261101T090000Z\r
TRANSP:OPAQUE\r
STATUS:CONFIRMED\r
SEQUENCE:0\r
LOCATION;LANGUAGE=en-GB:Microsoft Teams Meeting\r
END:VEVENT\r
END:VCALENDAR\r
";

#[test]
fn a_google_invitation_reads_in_the_organisers_timezone() {
    let invite = invite::parse(GOOGLE).expect("an invitation");

    assert_eq!(invite.subject, "Quarterly catch-up");
    assert_eq!(invite.uid, "4kq9h1p2r7@google.com");
    assert_eq!(invite.method, Method::Request);
    assert_eq!(invite.organizer.name, "Sofia Lindqvist");
    assert_eq!(invite.organizer.address, "sofia@example.com");
    // Escaped commas come back as commas, not as backslashes.
    assert_eq!(invite.location, "Amsterdam office, room 3");
    assert_eq!(invite.description, "Quarterly catch-up, then lunch.");
    assert!(!invite.all_day);

    // June is summer time there, so 14:00 local is 12:00 UTC.
    assert!(invite.start.utc.starts_with("2026-06-15T12:00:00"), "start: {}", invite.start.utc);
    assert!(invite.end.utc.starts_with("2026-06-15T13:00:00"), "end: {}", invite.end.utc);

    // The zone travels with it, so the server gets the organiser's words.
    assert_eq!(invite.start.tzid, "Europe/Amsterdam");
    assert_eq!(invite.start.local, "2026-06-15T14:00:00");

    // The folded ATTENDEE line is one address, not two.
    assert_eq!(invite.attendees.len(), 1);
    assert_eq!(invite.attendees[0].address, "you@example.com");
}

#[test]
fn an_outlook_invitation_in_winter_uses_standard_time() {
    let invite = invite::parse(OUTLOOK).expect("an invitation");

    assert_eq!(invite.subject, "Q1 budget review");
    assert_eq!(invite.organizer.address, "anna.visser@contoso.com");
    assert_eq!(invite.location, "Microsoft Teams Meeting");
    // November is standard time: 10:00 local is 09:00 UTC. Reading the
    // daylight rule instead would put it an hour early.
    assert!(invite.start.utc.starts_with("2026-11-18T09:00:00"), "start: {}", invite.start.utc);
    assert!(invite.end.utc.starts_with("2026-11-18T10:30:00"), "end: {}", invite.end.utc);
    // A line folded mid-address still yields one attendee, spelled right.
    assert_eq!(invite.attendees[0].address, "you@example.com");
}

/// The same invitation, moved either side of a daylight-saving change.
#[test]
fn the_daylight_saving_change_is_read_from_the_rule() {
    let at = |date: &str| {
        let ics = GOOGLE.replace("20260615T140000", &format!("{date}T140000"));
        // Keep the end in step so the event stays well-formed.
        let ics = ics.replace("20260615T150000", &format!("{date}T150000"));
        invite::parse(&ics).expect("an invitation").start.utc
    };

    // Summer time runs from the last Sunday of March to the last of October.
    assert!(at("20260328").starts_with("2026-03-28T13:00:00"), "{}", at("20260328"));
    assert!(at("20260330").starts_with("2026-03-30T12:00:00"), "{}", at("20260330"));
    assert!(at("20261024").starts_with("2026-10-24T12:00:00"), "{}", at("20261024"));
    assert!(at("20261026").starts_with("2026-10-26T13:00:00"), "{}", at("20261026"));
}

#[test]
fn a_whole_day_invitation_spans_the_day() {
    let ics = "BEGIN:VCALENDAR\nMETHOD:REQUEST\nBEGIN:VEVENT\n\
               UID:offsite@example.com\nSUMMARY:Company day\n\
               DTSTART;VALUE=DATE:20260702\nDTEND;VALUE=DATE:20260703\n\
               END:VEVENT\nEND:VCALENDAR\n";
    let invite = invite::parse(ics).expect("an invitation");
    assert!(invite.all_day);
    assert!(invite.start.utc.starts_with("2026-07-02T00:00:00"));
    // The end is exclusive, which is how the calendar stores whole days.
    assert!(invite.end.utc.starts_with("2026-07-03T00:00:00"));
}

/// A whole-day event with no DTEND is one day long, and a timed one with
/// only a DURATION is as long as the duration says.
#[test]
fn a_length_can_be_given_instead_of_an_end() {
    let ics = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:d@example.com\nSUMMARY:Standup\n\
               DTSTART:20260702T080000Z\nDURATION:PT45M\nEND:VEVENT\nEND:VCALENDAR\n";
    let invite = invite::parse(ics).expect("an invitation");
    assert!(invite.end.utc.starts_with("2026-07-02T08:45:00"), "end: {}", invite.end.utc);

    let ics = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:e@example.com\nSUMMARY:Leave\n\
               DTSTART;VALUE=DATE:20260702\nEND:VEVENT\nEND:VCALENDAR\n";
    let invite = invite::parse(ics).expect("an invitation");
    assert!(invite.end.utc.starts_with("2026-07-03T00:00:00"), "end: {}", invite.end.utc);
}

#[test]
fn a_cancellation_is_not_something_to_accept() {
    let ics = GOOGLE.replace("METHOD:REQUEST", "METHOD:CANCEL");
    let invite = invite::parse(&ics).expect("an invitation");
    assert!(invite.cancelled());
    assert!(!invite.wants_reply());

    // Outlook marks some cancellations on the event rather than the method.
    let ics = GOOGLE.replace("STATUS:CONFIRMED", "STATUS:CANCELLED");
    assert!(invite::parse(&ics).expect("an invitation").cancelled());
}

#[test]
fn something_that_is_not_an_event_is_not_an_invitation() {
    let ics = "BEGIN:VCALENDAR\nBEGIN:VTODO\nUID:t@example.com\nSUMMARY:Buy milk\n\
               END:VTODO\nEND:VCALENDAR\n";
    assert!(invite::parse(ics).is_none());
    assert!(invite::parse("not an ics at all").is_none());
}

#[test]
fn a_calendar_part_is_recognised_by_type_or_by_name() {
    assert!(invite::is_calendar_part("text/calendar; method=REQUEST", "invite.ics"));
    assert!(invite::is_calendar_part("TEXT/CALENDAR", "meeting"));
    // Google names it this even when the type is generic.
    assert!(invite::is_calendar_part("application/octet-stream", "invite.ics"));
    assert!(!invite::is_calendar_part("application/pdf", "agenda.pdf"));
}

/// Exchange holds the whole timezone database and this does not, so an
/// event created from an invitation is handed the organiser's own words.
#[test]
fn the_event_sent_to_the_server_keeps_the_invitations_timezone() {
    let invite = invite::parse(GOOGLE).expect("an invitation");
    let event = invite.as_graph_event();
    assert_eq!(event["start"]["dateTime"], "2026-06-15T14:00:00");
    assert_eq!(event["start"]["timeZone"], "Europe/Amsterdam");
    assert_eq!(event["subject"], "Quarterly catch-up");
    assert_eq!(event["isAllDay"], false);

    // A time written in UTC has no zone of its own to pass on.
    let ics = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:z@example.com\nSUMMARY:Call\n\
               DTSTART:20260702T080000Z\nDTEND:20260702T083000Z\nEND:VEVENT\nEND:VCALENDAR\n";
    let event = invite::parse(ics).expect("an invitation").as_graph_event();
    assert_eq!(event["start"]["dateTime"], "2026-07-02T08:00:00");
    assert_eq!(event["start"]["timeZone"], "UTC");
}

// -- answering ------------------------------------------------------------

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

/// An invitation Exchange never saw has no meeting on the server to
/// answer, so accepting it has to put the appointment on the calendar
/// here — and that has to happen with no network, like every other change.
#[test]
fn accepting_an_ics_invitation_puts_it_on_the_calendar_offline() {
    let (db, path) = temp_db("invite-accept");
    let invite = invite::parse(GOOGLE).expect("an invitation");
    db.set_invite("msg-1", &invite).expect("cache the invitation");

    let op = Op::RespondToInvite {
        message_id: "msg-1".into(),
        invite: invite.clone(),
        response: Response::Accepted,
    };
    apply_local(&db, &op).expect("apply");
    db.enqueue(&op).expect("queue for the server");

    // On the calendar straight away.
    let day = db
        .events_between("2026-06-15T00:00:00Z", "2026-06-16T00:00:00Z")
        .expect("read the calendar");
    let found = day.iter().find(|e| e.subject == "Quarterly catch-up").expect("the appointment");
    assert_eq!(found.location, "Amsterdam office, room 3");
    assert_eq!(found.organizer, "Sofia Lindqvist");

    // And remembered as answered, so reopening the mail says so.
    let (_, response) = db.invite("msg-1").expect("the invitation");
    assert_eq!(response, Some(Response::Accepted));

    // Still queued, because nothing has reached the server yet.
    assert_eq!(db.queued_count(), 1);
    cleanup(&path);
}

/// Declining one of those is the other way round: there is no appointment
/// to keep, and nothing to tell the server either.
#[test]
fn declining_an_ics_invitation_leaves_the_calendar_alone() {
    let (db, path) = temp_db("invite-decline");
    let invite = invite::parse(GOOGLE).expect("an invitation");
    db.set_invite("msg-2", &invite).expect("cache the invitation");

    apply_local(
        &db,
        &Op::RespondToInvite {
            message_id: "msg-2".into(),
            invite: invite.clone(),
            response: Response::Accepted,
        },
    )
    .expect("accept");
    apply_local(
        &db,
        &Op::RespondToInvite {
            message_id: "msg-2".into(),
            invite: invite.clone(),
            response: Response::Declined,
        },
    )
    .expect("then decline");

    let day = db
        .events_between("2026-06-15T00:00:00Z", "2026-06-16T00:00:00Z")
        .expect("read the calendar");
    assert!(!day.iter().any(|e| e.subject == "Quarterly catch-up"));
    assert_eq!(db.invite("msg-2").expect("the invitation").1, Some(Response::Declined));
    cleanup(&path);
}

/// A meeting request is already on the calendar as tentative before anyone
/// touches it, so answering it must not add a second copy.
#[test]
fn answering_a_meeting_request_does_not_duplicate_the_appointment() {
    let (db, path) = temp_db("invite-meeting");
    let mut invite = invite::parse(GOOGLE).expect("an invitation");
    invite.meeting_request = true;
    db.set_invite("msg-3", &invite).expect("cache the invitation");

    apply_local(
        &db,
        &Op::RespondToInvite {
            message_id: "msg-3".into(),
            invite: invite.clone(),
            response: Response::Accepted,
        },
    )
    .expect("accept");

    let day = db
        .events_between("2026-06-15T00:00:00Z", "2026-06-16T00:00:00Z")
        .expect("read the calendar");
    assert!(
        !day.iter().any(|e| e.subject == "Quarterly catch-up"),
        "the server's own copy is the one that counts"
    );
    assert_eq!(db.invite("msg-3").expect("the invitation").1, Some(Response::Accepted));
    cleanup(&path);
}

/// Re-reading the mail refreshes what the invitation says without
/// forgetting that it was answered.
#[test]
fn re_reading_an_invitation_keeps_the_answer_given_to_it() {
    let (db, path) = temp_db("invite-reread");
    let invite = invite::parse(GOOGLE).expect("an invitation");
    db.set_invite("msg-4", &invite).unwrap();
    db.set_invite_response("msg-4", Response::Tentative).unwrap();

    db.set_invite("msg-4", &invite).expect("cached again on the next read");
    assert_eq!(db.invite("msg-4").expect("the invitation").1, Some(Response::Tentative));
    cleanup(&path);
}

/// A server-side move renames the message; the invitation has to follow it
/// or the mail would land in its new folder with the invitation gone.
#[test]
fn an_invitation_follows_its_message_through_a_server_move() {
    let (db, path) = temp_db("invite-move");
    let invite = invite::parse(OUTLOOK).expect("an invitation");
    db.set_invite("old-id", &invite).unwrap();
    db.set_invite_response("old-id", Response::Accepted).unwrap();

    db.rename_message("old-id", "new-id").expect("rename");

    assert!(db.invite("old-id").is_none());
    let (moved, response) = db.invite("new-id").expect("the invitation moved with it");
    assert_eq!(moved.subject, "Q1 budget review");
    assert_eq!(response, Some(Response::Accepted));
    cleanup(&path);
}

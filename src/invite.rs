//! Calendar invitations.
//!
//! An invitation reaches a mailbox in one of two shapes. Exchange
//! recognises the ones it understands — anything sent from Outlook, and
//! usually anything sent from Google Calendar — and turns them into
//! meeting requests: the appointment is already on the calendar as
//! tentative, and replying is an RSVP sent back to the organiser.
//! Everything else arrives as an ordinary message carrying a
//! `text/calendar` part: a forwarded invitation, an invitation to a
//! mailbox with calendar processing turned off, an `.ics` someone
//! exported. Putting one of those on the calendar means reading the file.
//!
//! This module covers both: the shared shape of an invitation, and
//! enough of RFC 5545 to read one out of an `.ics` — including the
//! VTIMEZONE block, which is what says when "10:00" actually is.

use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, Timelike, Utc, Weekday};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::model::{Address, CalendarEvent};

/// What the sender is asking for — RFC 5545 METHOD.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Method {
    /// An invitation, or an update to one. The default, because that is
    /// what a METHOD-less `.ics` attached to a mail almost always is.
    #[default]
    Request,
    /// The meeting is off.
    Cancel,
    /// Someone else's RSVP, which is not ours to act on.
    Reply,
    /// An event offered to keep, with no RSVP expected.
    Publish,
}

impl Method {
    fn parse(value: &str) -> Method {
        match value.trim().to_ascii_uppercase().as_str() {
            "CANCEL" => Method::Cancel,
            "REPLY" => Method::Reply,
            "PUBLISH" => Method::Publish,
            _ => Method::Request,
        }
    }
}

/// An answer to an invitation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    Accepted,
    Tentative,
    Declined,
}

impl Response {
    /// How it is stored in the cache, and read back.
    pub fn as_str(self) -> &'static str {
        match self {
            Response::Accepted => "accepted",
            Response::Tentative => "tentative",
            Response::Declined => "declined",
        }
    }

    pub fn from_str(value: &str) -> Option<Response> {
        match value {
            "accepted" => Some(Response::Accepted),
            "tentative" => Some(Response::Tentative),
            "declined" => Some(Response::Declined),
            _ => None,
        }
    }

    /// The Graph action that sends this RSVP to the organiser.
    pub fn graph_action(self) -> &'static str {
        match self {
            Response::Accepted => "accept",
            Response::Tentative => "tentativelyAccept",
            Response::Declined => "decline",
        }
    }

    /// How the reading pane says it back, once it is done.
    pub fn said(self) -> &'static str {
        match self {
            Response::Accepted => "You accepted this invitation.",
            Response::Tentative => "You answered this invitation tentatively.",
            Response::Declined => "You declined this invitation.",
        }
    }
}

/// One end of an invitation's span.
///
/// Both forms are kept. `utc` is what the calendar is stored and drawn in;
/// `local` and `tzid` are the invitation's own words, which is what gets
/// handed to the server when the event is created — Exchange holds the
/// whole timezone database and we do not, so it resolves them, not us.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct InviteTime {
    /// Wall time as written: `2026-01-05T10:00:00`, or `2026-01-05` for a
    /// whole day.
    pub local: String,
    /// The TZID it was written in. Empty when it was already UTC, or
    /// floating, or a plain date.
    pub tzid: String,
    /// RFC3339 in UTC — best effort, resolved through the VTIMEZONE the
    /// invitation carries.
    pub utc: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Invite {
    /// The event's identity across updates and cancellations.
    pub uid: String,
    pub method: Method,
    /// Bumped by the organiser on every revision.
    pub sequence: i64,
    pub subject: String,
    pub organizer: Address,
    pub location: String,
    pub description: String,
    pub start: InviteTime,
    pub end: InviteTime,
    pub all_day: bool,
    /// The invitation is for a series. Only the first occurrence is read;
    /// the rest come back from the server once it holds the event.
    pub recurring: bool,
    pub attendees: Vec<Address>,
    /// Exchange already made this a meeting request: the appointment is on
    /// the calendar, and answering goes through the message rather than by
    /// creating anything.
    pub meeting_request: bool,
}

impl Invite {
    /// Whether the organiser is waiting for an answer. A published event
    /// or someone else's reply is not ours to answer.
    pub fn wants_reply(&self) -> bool {
        matches!(self.method, Method::Request)
    }

    pub fn cancelled(&self) -> bool {
        self.method == Method::Cancel
    }

    /// Whether the RSVP can actually reach the organiser. Exchange sends
    /// it for a meeting request; for a bare `.ics` there is no meeting on
    /// the server to answer, so the most that can be done is keep it.
    pub fn can_rsvp(&self) -> bool {
        self.meeting_request && self.wants_reply()
    }

    /// The invitation as a calendar entry, for showing it immediately on
    /// the calendar while the server is being told.
    pub fn as_event(&self, mailbox: &str) -> CalendarEvent {
        CalendarEvent {
            // The UID is the organiser's own identifier for the event, so
            // a second copy of the same invitation lands on the same row
            // rather than doubling it.
            id: format!("invite:{}", self.uid),
            subject: if self.subject.is_empty() {
                "(no subject)".to_string()
            } else {
                self.subject.clone()
            },
            organizer: self.organizer.display().to_string(),
            location: self.location.clone(),
            start: self.start.utc.clone(),
            end: self.end.utc.clone(),
            all_day: self.all_day,
            cancelled: self.cancelled(),
            preview: self.description.chars().take(400).collect(),
            mailbox: mailbox.to_string(),
        }
    }

    /// The event as Graph wants it created. The invitation's own timezone
    /// is passed through rather than the UTC we worked out, so the server
    /// gets the authoritative reading.
    pub fn as_graph_event(&self) -> Value {
        let when = |t: &InviteTime| -> Value {
            if self.all_day {
                // Graph insists a whole-day event start and end on
                // midnight in the zone it is given, which is what the
                // date form already is.
                json!({ "dateTime": format!("{}T00:00:00", t.local), "timeZone": "UTC" })
            } else if t.tzid.is_empty() {
                let naive = t.utc.strip_suffix('Z').map(str::to_string).unwrap_or_else(|| {
                    DateTime::parse_from_rfc3339(&t.utc)
                        .map(|dt| dt.naive_utc().format("%Y-%m-%dT%H:%M:%S").to_string())
                        .unwrap_or_else(|_| t.local.clone())
                });
                json!({ "dateTime": naive, "timeZone": "UTC" })
            } else {
                json!({ "dateTime": t.local, "timeZone": t.tzid })
            }
        };
        json!({
            "subject": self.subject,
            "body": { "contentType": "text", "content": self.description },
            "start": when(&self.start),
            "end": when(&self.end),
            "location": { "displayName": self.location },
            "isAllDay": self.all_day,
            "attendees": self
                .attendees
                .iter()
                .filter(|a| !a.address.is_empty())
                .map(|a| json!({
                    "emailAddress": { "address": a.address, "name": a.name },
                    "type": "required",
                }))
                .collect::<Vec<Value>>(),
        })
    }
}

/// Whether a message carries an invitation, judged the way mail has to be
/// judged: by what the part says it is, falling back to its name.
pub fn is_calendar_part(content_type: &str, name: &str) -> bool {
    content_type.split(';').next().unwrap_or_default().trim().eq_ignore_ascii_case("text/calendar")
        || name.to_ascii_lowercase().ends_with(".ics")
}

// -- reading an .ics ------------------------------------------------------

/// One `NAME;PARAM=VALUE:value` line, already unfolded.
struct Line {
    name: String,
    params: Vec<(String, String)>,
    value: String,
}

impl Line {
    fn param(&self, key: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
    }
}

/// Undo RFC 5545 folding: a line starting with a space or tab is the
/// continuation of the one before it.
fn unfold(ics: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in ics.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        match line.strip_prefix([' ', '\t']) {
            Some(rest) => {
                if let Some(last) = out.last_mut() {
                    last.push_str(rest);
                    continue;
                }
                out.push(rest.to_string());
            }
            None => out.push(line.to_string()),
        }
    }
    out
}

/// Split a content line into its name, parameters and value. The colon
/// that ends the name can be inside a quoted parameter — `CN="A: B"` — so
/// quoting has to be tracked rather than looking for the first colon.
fn parse_line(line: &str) -> Option<Line> {
    let mut quoted = false;
    let mut split = None;
    for (index, ch) in line.char_indices() {
        match ch {
            '"' => quoted = !quoted,
            ':' if !quoted => {
                split = Some(index);
                break;
            }
            _ => {}
        }
    }
    let (head, value) = match split {
        Some(index) => (&line[..index], &line[index + 1..]),
        // A line with no value at all is not one we can use.
        None => return None,
    };

    let mut parts: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for ch in head.chars() {
        match ch {
            '"' => quoted = !quoted,
            ';' if !quoted => parts.push(std::mem::take(&mut current)),
            _ => current.push(ch),
        }
    }
    parts.push(current);
    let mut parts = parts.into_iter();
    let name = parts.next().unwrap_or_default().trim().to_ascii_uppercase();
    if name.is_empty() {
        return None;
    }
    let params = parts
        .filter_map(|part| {
            let (key, value) = part.split_once('=')?;
            Some((key.trim().to_ascii_uppercase(), value.trim().to_string()))
        })
        .collect();
    Some(Line { name, params, value: value.to_string() })
}

/// Undo the text escaping of RFC 5545 §3.3.11.
fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') | Some('N') => out.push('\n'),
            Some(escaped) => out.push(escaped),
            None => out.push('\\'),
        }
    }
    out
}

/// `MAILTO:someone@example.com` in any casing, or a bare address.
fn address_of(value: &str) -> String {
    let value = value.trim();
    let bare = value
        .get(..7)
        .filter(|prefix| prefix.eq_ignore_ascii_case("mailto:"))
        .map(|_| &value[7..])
        .unwrap_or(value);
    bare.trim().to_string()
}

fn person(line: &Line) -> Address {
    let address = address_of(&line.value);
    let name = line.param("CN").map(unescape).unwrap_or_default();
    if name.is_empty() {
        Address::bare(address)
    } else {
        Address::new(name, address)
    }
}

/// A naive time already in UTC, written the way the cache stores times.
fn utc_string(naive: NaiveDateTime) -> String {
    DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc).to_rfc3339()
}

// -- timezones ------------------------------------------------------------

/// One STANDARD or DAYLIGHT block: the offset it puts the clock on, and
/// when in the year it takes over.
#[derive(Clone)]
struct Observance {
    offset_seconds: i32,
    /// The transition, as it is written: the local time the clock changes,
    /// plus either a fixed date or a yearly rule.
    month: u32,
    /// Day of the month, when the rule names one.
    monthday: Option<u32>,
    /// Weekday plus its position in the month (1 = first, -1 = last).
    weekday: Option<(Weekday, i32)>,
    hour: u32,
    minute: u32,
    /// The literal DTSTART, used when there is no recurrence rule and the
    /// block therefore applies from a single date onwards.
    from: Option<NaiveDateTime>,
    recurring: bool,
}

impl Observance {
    /// The local instant this observance takes over in a given year.
    fn transition(&self, year: i32) -> Option<NaiveDateTime> {
        if !self.recurring {
            return self.from;
        }
        let date = match (self.monthday, self.weekday) {
            (Some(day), _) => NaiveDate::from_ymd_opt(year, self.month, day)?,
            (None, Some((weekday, ordinal))) if ordinal > 0 => {
                NaiveDate::from_weekday_of_month_opt(year, self.month, weekday, ordinal as u8)?
            }
            (None, Some((weekday, _))) => {
                // Last of the month: walk back from its final day.
                let mut day = last_day_of_month(year, self.month);
                loop {
                    let date = NaiveDate::from_ymd_opt(year, self.month, day)?;
                    if date.weekday() == weekday {
                        break date;
                    }
                    day = day.checked_sub(1)?;
                }
            }
            (None, None) => return self.from,
        };
        date.and_hms_opt(self.hour, self.minute, 0)
    }
}

fn last_day_of_month(year: i32, month: u32) -> u32 {
    let (next_year, next_month) = if month == 12 { (year + 1, 1) } else { (year, month + 1) };
    NaiveDate::from_ymd_opt(next_year, next_month, 1)
        .and_then(|first| first.pred_opt())
        .map(|d| d.day())
        .unwrap_or(28)
}

/// `+0200`, `-0530`, `+020000` — RFC 5545 UTC offsets.
fn parse_offset(value: &str) -> Option<i32> {
    let value = value.trim();
    let (sign, rest) = match value.chars().next()? {
        '+' => (1, &value[1..]),
        '-' => (-1, &value[1..]),
        _ => (1, value),
    };
    if rest.len() < 4 || !rest.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let hours: i32 = rest[0..2].parse().ok()?;
    let minutes: i32 = rest[2..4].parse().ok()?;
    let seconds: i32 = if rest.len() >= 6 { rest[4..6].parse().ok()? } else { 0 };
    Some(sign * (hours * 3600 + minutes * 60 + seconds))
}

/// The VTIMEZONE blocks an invitation carries, which is the only timezone
/// database available here — and, conveniently, the one the organiser was
/// actually using.
#[derive(Default)]
struct Timezones {
    zones: Vec<(String, Vec<Observance>)>,
}

impl Timezones {
    /// The offset in force at a local wall time, by picking the observance
    /// whose transition most recently passed.
    fn offset_at(&self, tzid: &str, local: NaiveDateTime) -> Option<i32> {
        let observances = self
            .zones
            .iter()
            .find(|(id, _)| id.eq_ignore_ascii_case(tzid))
            .map(|(_, o)| o.as_slice())?;
        match observances {
            [] => None,
            [only] => Some(only.offset_seconds),
            many => {
                let year = local.year();
                // The year's transitions, plus the last of the previous
                // year — before the first transition of this year, it is
                // still that one that is in force.
                let mut candidates: Vec<(NaiveDateTime, i32)> = Vec::new();
                for observance in many {
                    for y in [year - 1, year] {
                        if let Some(at) = observance.transition(y) {
                            candidates.push((at, observance.offset_seconds));
                        }
                    }
                }
                candidates.sort_by_key(|(at, _)| *at);
                candidates
                    .iter()
                    .rev()
                    .find(|(at, _)| *at <= local)
                    .or_else(|| candidates.first())
                    .map(|(_, offset)| *offset)
            }
        }
    }
}

/// Read a DATE or DATE-TIME property into both forms.
///
/// Returns the time and whether it was a whole-day date.
fn parse_time(line: &Line, zones: &Timezones) -> Option<(InviteTime, bool)> {
    let value = line.value.trim();
    let is_date = line.param("VALUE").map(|v| v.eq_ignore_ascii_case("DATE")).unwrap_or(false)
        || (value.len() == 8 && !value.contains('T'));
    if is_date {
        let date = NaiveDate::parse_from_str(&value[..value.len().min(8)], "%Y%m%d").ok()?;
        let midnight = date.and_hms_opt(0, 0, 0)?;
        return Some((
            InviteTime {
                local: date.format("%Y-%m-%d").to_string(),
                tzid: String::new(),
                utc: utc_string(midnight),
            },
            true,
        ));
    }

    let utc_marked = value.ends_with('Z');
    let stripped = value.trim_end_matches('Z');
    let naive = NaiveDateTime::parse_from_str(stripped, "%Y%m%dT%H%M%S")
        .or_else(|_| NaiveDateTime::parse_from_str(stripped, "%Y%m%dT%H%M"))
        .ok()?;
    let tzid = line.param("TZID").map(|t| t.trim_matches('"').to_string()).unwrap_or_default();

    // A trailing Z is UTC outright; otherwise the TZID decides, and a time
    // with neither is floating — local wherever it is read, which for a
    // mail client means here.
    let offset = if utc_marked {
        0
    } else if !tzid.is_empty() {
        match zones.offset_at(&tzid, naive) {
            Some(offset) => offset,
            // The invitation named a zone it did not describe. The server
            // still gets the name and resolves it properly; what is shown
            // until then is the wall time as written.
            None => 0,
        }
    } else {
        0
    };
    let utc = naive - Duration::seconds(offset as i64);
    Some((
        InviteTime {
            local: naive.format("%Y-%m-%dT%H:%M:%S").to_string(),
            tzid: if utc_marked { String::new() } else { tzid },
            utc: utc_string(utc),
        },
        false,
    ))
}

/// `PT1H30M`, `P1D`, `-PT15M` — RFC 5545 durations, as far as an event
/// length needs them.
fn parse_duration(value: &str) -> Option<Duration> {
    let value = value.trim();
    let (sign, rest) = match value.strip_prefix('-') {
        Some(rest) => (-1, rest),
        None => (1, value.strip_prefix('+').unwrap_or(value)),
    };
    let rest = rest.strip_prefix('P')?;
    let mut seconds: i64 = 0;
    let mut number = String::new();
    let mut in_time = false;
    for ch in rest.chars() {
        match ch {
            'T' => in_time = true,
            '0'..='9' => number.push(ch),
            unit => {
                let count: i64 = number.parse().ok()?;
                number.clear();
                seconds += match (unit, in_time) {
                    ('W', _) => count * 7 * 86_400,
                    ('D', _) => count * 86_400,
                    ('H', true) => count * 3_600,
                    ('M', true) => count * 60,
                    ('S', true) => count,
                    _ => return None,
                };
            }
        }
    }
    Some(Duration::seconds(sign * seconds))
}

/// Read an invitation out of the text of an `.ics`.
///
/// `None` when there is no event in it — an `.ics` holding only free/busy
/// time or a to-do is not something to put on a calendar here.
pub fn parse(ics: &str) -> Option<Invite> {
    let lines: Vec<Line> = unfold(ics).iter().filter_map(|l| parse_line(l)).collect();

    // Timezones first: the event's times are read through them.
    let mut zones = Timezones::default();
    let mut tzid = String::new();
    let mut observances: Vec<Observance> = Vec::new();
    let mut current: Option<Observance> = None;
    for line in &lines {
        match (line.name.as_str(), line.value.trim().to_ascii_uppercase().as_str()) {
            ("BEGIN", "VTIMEZONE") => {
                tzid = String::new();
                observances = Vec::new();
            }
            ("END", "VTIMEZONE") => {
                if !tzid.is_empty() {
                    zones.zones.push((std::mem::take(&mut tzid), std::mem::take(&mut observances)));
                }
            }
            ("BEGIN", "STANDARD") | ("BEGIN", "DAYLIGHT") => {
                current = Some(Observance {
                    offset_seconds: 0,
                    month: 1,
                    monthday: None,
                    weekday: None,
                    hour: 0,
                    minute: 0,
                    from: None,
                    recurring: false,
                });
            }
            ("END", "STANDARD") | ("END", "DAYLIGHT") => {
                if let Some(observance) = current.take() {
                    observances.push(observance);
                }
            }
            _ => {
                if line.name == "TZID" && current.is_none() {
                    tzid = line.value.trim().to_string();
                    continue;
                }
                let Some(observance) = current.as_mut() else { continue };
                match line.name.as_str() {
                    "TZOFFSETTO" => {
                        observance.offset_seconds = parse_offset(&line.value).unwrap_or(0)
                    }
                    "DTSTART" => {
                        if let Ok(naive) =
                            NaiveDateTime::parse_from_str(line.value.trim(), "%Y%m%dT%H%M%S")
                        {
                            observance.from = Some(naive);
                            observance.month = naive.month();
                            observance.hour = naive.hour();
                            observance.minute = naive.minute();
                        }
                    }
                    "RRULE" => apply_transition_rule(observance, &line.value),
                    _ => {}
                }
            }
        }
    }

    // Then the event itself. Only the first VEVENT is read: a series comes
    // back from the server with its occurrences once it holds the event,
    // and an invitation's later VEVENTs are its exceptions.
    let start_index = lines.iter().position(|l| {
        l.name == "BEGIN" && l.value.trim().eq_ignore_ascii_case("VEVENT")
    })?;
    let end_index = lines
        .iter()
        .skip(start_index)
        .position(|l| l.name == "END" && l.value.trim().eq_ignore_ascii_case("VEVENT"))
        .map(|offset| start_index + offset)
        .unwrap_or(lines.len());

    let method = lines
        .iter()
        .take(start_index)
        .find(|l| l.name == "METHOD")
        .map(|l| Method::parse(&l.value))
        .unwrap_or_default();

    let mut invite = Invite { method, ..Default::default() };
    let mut end_time: Option<InviteTime> = None;
    let mut duration: Option<Duration> = None;
    let mut status_cancelled = false;

    for line in &lines[start_index..end_index] {
        match line.name.as_str() {
            "UID" => invite.uid = line.value.trim().to_string(),
            "SUMMARY" => invite.subject = unescape(&line.value),
            "LOCATION" => invite.location = unescape(&line.value),
            "DESCRIPTION" => invite.description = unescape(&line.value),
            "SEQUENCE" => invite.sequence = line.value.trim().parse().unwrap_or(0),
            "ORGANIZER" => invite.organizer = person(line),
            "ATTENDEE" => invite.attendees.push(person(line)),
            "RRULE" => invite.recurring = true,
            "STATUS" => {
                status_cancelled = line.value.trim().eq_ignore_ascii_case("CANCELLED");
            }
            "DTSTART" => {
                if let Some((time, all_day)) = parse_time(line, &zones) {
                    invite.start = time;
                    invite.all_day = all_day;
                }
            }
            "DTEND" => end_time = parse_time(line, &zones).map(|(time, _)| time),
            "DURATION" => duration = parse_duration(&line.value),
            _ => {}
        }
    }

    if invite.start.utc.is_empty() {
        return None;
    }
    if status_cancelled {
        invite.method = Method::Cancel;
    }
    // A UID is required, but an exported .ics is not always well-formed;
    // the subject and start are enough to tell two invitations apart.
    if invite.uid.is_empty() {
        invite.uid = format!("{}-{}", invite.subject, invite.start.utc);
    }
    invite.end = end_time.unwrap_or_else(|| {
        let span = duration.unwrap_or_else(|| {
            // With neither an end nor a length: a whole day for a date, and
            // an instant for a time, which is what RFC 5545 says they mean.
            if invite.all_day {
                Duration::days(1)
            } else {
                Duration::zero()
            }
        });
        shift(&invite.start, span, invite.all_day)
    });
    Some(invite)
}

/// Move a time by a span, keeping both of its forms in step.
fn shift(time: &InviteTime, span: Duration, all_day: bool) -> InviteTime {
    let Ok(utc) = DateTime::parse_from_rfc3339(&time.utc) else { return time.clone() };
    let moved = utc + span;
    let local = if all_day {
        moved.naive_utc().date().format("%Y-%m-%d").to_string()
    } else {
        NaiveDateTime::parse_from_str(&time.local, "%Y-%m-%dT%H:%M:%S")
            .map(|naive| (naive + span).format("%Y-%m-%dT%H:%M:%S").to_string())
            .unwrap_or_else(|_| moved.naive_utc().format("%Y-%m-%dT%H:%M:%S").to_string())
    };
    InviteTime {
        local,
        tzid: time.tzid.clone(),
        utc: moved.with_timezone(&Utc).to_rfc3339(),
    }
}

/// Read a VTIMEZONE recurrence rule — the yearly "last Sunday in October"
/// kind, which is all a daylight-saving transition ever is.
fn apply_transition_rule(observance: &mut Observance, rule: &str) {
    observance.recurring = true;
    for part in rule.split(';') {
        let Some((key, value)) = part.split_once('=') else { continue };
        match key.trim().to_ascii_uppercase().as_str() {
            "BYMONTH" => {
                if let Ok(month) = value.trim().parse::<u32>() {
                    if (1..=12).contains(&month) {
                        observance.month = month;
                    }
                }
            }
            "BYMONTHDAY" => observance.monthday = value.trim().parse::<u32>().ok(),
            "BYDAY" => {
                let value = value.trim();
                let split = value.len().saturating_sub(2);
                let (ordinal, day) = value.split_at(split);
                let weekday = match day.to_ascii_uppercase().as_str() {
                    "MO" => Weekday::Mon,
                    "TU" => Weekday::Tue,
                    "WE" => Weekday::Wed,
                    "TH" => Weekday::Thu,
                    "FR" => Weekday::Fri,
                    "SA" => Weekday::Sat,
                    "SU" => Weekday::Sun,
                    _ => continue,
                };
                // No ordinal means the rule is for every such weekday,
                // which for a transition rule means the first one.
                let ordinal: i32 = ordinal.parse().unwrap_or(1);
                observance.weekday = Some((weekday, ordinal));
            }
            _ => {}
        }
    }
}

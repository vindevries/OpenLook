//! Month view. Like the mail side, it reads only from the local cache; the
//! sync engine refreshes the window being looked at.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, TimeZone, Utc};
use gtk::pango::EllipsizeMode;

use crate::model::CalendarEvent;
use crate::sync::Cmd;
use crate::ui::widgets::ToolbarView;
use crate::ui::window::State;
use crate::util::html_to_text;

/// Events drawn in a day cell before it collapses into "+N more".
const CHIPS_PER_DAY: usize = 3;

pub struct CalendarUi {
    pub root: gtk::Box,
    grid: gtk::Grid,
    month_label: gtk::Label,
    pub prev: gtk::Button,
    pub next: gtk::Button,
    pub today: gtk::Button,
    day_title: gtk::Label,
    day_list: gtk::ListBox,
    /// First day of the displayed month.
    month: RefCell<NaiveDate>,
    selected: RefCell<NaiveDate>,
    /// Set while rebuilding, so cell clicks are ignored.
    building: Cell<bool>,
    /// The day cells, so selecting a day can restyle them in place rather
    /// than rebuilding the grid — a rebuild would destroy the chip being
    /// double-clicked before the second click landed.
    cells: RefCell<Vec<(NaiveDate, gtk::Box)>>,
}

impl CalendarUi {
    pub fn new() -> CalendarUi {
        let root = gtk::Box::new(gtk::Orientation::Horizontal, 0);

        let left = gtk::Box::new(gtk::Orientation::Vertical, 0);
        left.set_hexpand(true);

        let head = gtk::Box::builder()
            .spacing(6)
            .margin_top(10)
            .margin_bottom(8)
            .margin_start(14)
            .margin_end(14)
            .build();
        let prev = gtk::Button::from_icon_name("go-previous-symbolic");
        prev.add_css_class("flat");
        let next = gtk::Button::from_icon_name("go-next-symbolic");
        next.add_css_class("flat");
        let today = gtk::Button::with_label("Today");
        today.add_css_class("flat");
        let month_label = gtk::Label::new(None);
        month_label.add_css_class("title-3");
        month_label.set_xalign(0.0);
        month_label.set_hexpand(true);
        head.append(&prev);
        head.append(&next);
        head.append(&today);
        head.append(&month_label);
        left.append(&head);

        let weekdays = gtk::Grid::builder()
            .column_homogeneous(true)
            .margin_start(10)
            .margin_end(10)
            .build();
        for (index, name) in ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"].iter().enumerate() {
            let label = gtk::Label::new(Some(name));
            label.add_css_class("cal-weekday");
            weekdays.attach(&label, index as i32, 0, 1, 1);
        }
        left.append(&weekdays);

        let grid = gtk::Grid::builder()
            .column_homogeneous(true)
            .row_homogeneous(true)
            .hexpand(true)
            .vexpand(true)
            .margin_start(10)
            .margin_end(10)
            .margin_bottom(10)
            .build();
        left.append(&grid);
        root.append(&left);
        root.append(&gtk::Separator::new(gtk::Orientation::Vertical));

        // Day detail, to the right of the grid.
        let side = gtk::Box::new(gtk::Orientation::Vertical, 0);
        side.set_size_request(320, -1);
        let day_title = gtk::Label::builder()
            .xalign(0.0)
            .margin_top(14)
            .margin_bottom(8)
            .margin_start(14)
            .margin_end(14)
            .build();
        day_title.add_css_class("heading");
        side.append(&day_title);
        side.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        let day_list = gtk::ListBox::new();
        day_list.set_selection_mode(gtk::SelectionMode::None);
        let placeholder = gtk::Label::builder().label("Nothing scheduled").margin_top(20).build();
        placeholder.add_css_class("dim-label");
        day_list.set_placeholder(Some(&placeholder));
        side.append(
            &gtk::ScrolledWindow::builder()
                .child(&day_list)
                .vexpand(true)
                .hscrollbar_policy(gtk::PolicyType::Never)
                .build(),
        );
        root.append(&side);

        let today_date = Local::now().date_naive();
        CalendarUi {
            root,
            grid,
            month_label,
            prev,
            next,
            today,
            day_title,
            day_list,
            month: RefCell::new(first_of_month(today_date)),
            selected: RefCell::new(today_date),
            building: Cell::new(false),
            cells: RefCell::new(Vec::new()),
        }
    }
}

fn first_of_month(date: NaiveDate) -> NaiveDate {
    NaiveDate::from_ymd_opt(date.year(), date.month(), 1).unwrap_or(date)
}

fn add_months(date: NaiveDate, delta: i32) -> NaiveDate {
    let mut year = date.year();
    let mut month = date.month() as i32 + delta;
    while month < 1 {
        month += 12;
        year -= 1;
    }
    while month > 12 {
        month -= 12;
        year += 1;
    }
    NaiveDate::from_ymd_opt(year, month as u32, 1).unwrap_or(date)
}

/// The Monday on or before the first of the month: where the grid starts.
fn grid_start(month: NaiveDate) -> NaiveDate {
    month - Duration::days(month.weekday().num_days_from_monday() as i64)
}

fn local_of(utc: &str) -> Option<DateTime<Local>> {
    DateTime::parse_from_rfc3339(utc).ok().map(|dt| dt.with_timezone(&Local))
}

/// Shift the month, then reload and re-sync.
pub fn step_month(state: &Rc<State>, delta: i32) {
    {
        let ui = &state.calendar;
        let current = *ui.month.borrow();
        *ui.month.borrow_mut() = add_months(current, delta);
    }
    refresh(state);
    request_sync(state);
}

pub fn go_today(state: &Rc<State>) {
    let today = Local::now().date_naive();
    {
        let ui = &state.calendar;
        *ui.month.borrow_mut() = first_of_month(today);
        *ui.selected.borrow_mut() = today;
    }
    refresh(state);
    request_sync(state);
}

/// Ask every mailbox to refresh the window currently on screen, padded so
/// events just outside the grid are there when the month is stepped.
pub fn request_sync(state: &Rc<State>) {
    let month = *state.calendar.month.borrow();
    let start = grid_start(month) - Duration::days(7);
    let end = start + Duration::days(56);
    let to_utc = |d: NaiveDate| -> String {
        Local
            .from_local_datetime(&d.and_hms_opt(0, 0, 0).unwrap())
            .single()
            .map(|dt| dt.with_timezone(&Utc).format("%Y-%m-%dT%H:%M:%SZ").to_string())
            .unwrap_or_default()
    };
    let (start, end) = (to_utc(start), to_utc(end));
    for session in state.sessions.borrow().iter() {
        session.send(Cmd::SyncCalendar { start: start.clone(), end: end.clone() });
    }
}

/// Every cached event overlapping the grid, tagged with its mailbox.
fn events_for_grid(state: &Rc<State>, from: NaiveDate, to: NaiveDate) -> Vec<(usize, CalendarEvent)> {
    let to_utc = |d: NaiveDate| -> String {
        Local
            .from_local_datetime(&d.and_hms_opt(0, 0, 0).unwrap())
            .single()
            .map(|dt| dt.with_timezone(&Utc).to_rfc3339())
            .unwrap_or_default()
    };
    let (start, end) = (to_utc(from), to_utc(to));
    let mut out = Vec::new();
    for (index, session) in state.sessions.borrow().iter().enumerate() {
        let title = session.title();
        if let Ok(events) = session.db.events_between(&start, &end) {
            for mut event in events {
                if event.mailbox.is_empty() {
                    event.mailbox = title.clone();
                }
                out.push((index, event));
            }
        }
    }
    out.sort_by(|a, b| a.1.start.cmp(&b.1.start));
    out
}

fn occurs_on(event: &CalendarEvent, day: NaiveDate) -> bool {
    if event.all_day {
        // All-day events are stored on UTC midnight boundaries with an
        // exclusive end. Converting those to local time would smear them
        // across two days for anyone east of Greenwich.
        let start = DateTime::parse_from_rfc3339(&event.start).ok().map(|d| d.naive_utc().date());
        let end = DateTime::parse_from_rfc3339(&event.end).ok().map(|d| d.naive_utc().date());
        return match (start, end) {
            (Some(start), Some(end)) => {
                day >= start && day < end.max(start + Duration::days(1))
            }
            _ => false,
        };
    }
    let (Some(start), Some(end)) = (local_of(&event.start), local_of(&event.end)) else {
        return false;
    };
    // An event ending exactly at midnight belongs to the previous day.
    let last = if end.time() == chrono::NaiveTime::MIN && end.date_naive() > start.date_naive() {
        end.date_naive() - Duration::days(1)
    } else {
        end.date_naive()
    };
    day >= start.date_naive() && day <= last
}

pub fn refresh(state: &Rc<State>) {
    let ui = &state.calendar;
    let month = *ui.month.borrow();
    let selected = *ui.selected.borrow();
    let start = grid_start(month);
    let events = events_for_grid(state, start, start + Duration::days(42));

    ui.month_label.set_text(&month.format("%B %Y").to_string());
    ui.building.set(true);
    while let Some(child) = ui.grid.first_child() {
        ui.grid.remove(&child);
    }

    let today = Local::now().date_naive();
    let mut cells: Vec<(NaiveDate, gtk::Box)> = Vec::with_capacity(42);
    for cell in 0..42 {
        let day = start + Duration::days(cell);
        let day_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
        day_box.add_css_class("cal-day");
        if day.month() != month.month() {
            day_box.add_css_class("cal-outside");
        }
        if day == today {
            day_box.add_css_class("cal-today");
        }
        if day == selected {
            day_box.add_css_class("cal-selected");
        }

        let number = gtk::Label::new(Some(&day.day().to_string()));
        number.set_xalign(0.0);
        number.add_css_class("cal-daynum");
        day_box.append(&number);

        let today_events: Vec<&(usize, CalendarEvent)> =
            events.iter().filter(|(_, e)| occurs_on(e, day)).collect();
        for (session, event) in today_events.iter().take(CHIPS_PER_DAY) {
            let text = if event.all_day {
                event.subject.clone()
            } else {
                match local_of(&event.start) {
                    Some(dt) => format!("{} {}", dt.format("%H:%M"), event.subject),
                    None => event.subject.clone(),
                }
            };
            let chip = gtk::Label::builder()
                .label(&text)
                .xalign(0.0)
                .ellipsize(EllipsizeMode::End)
                .tooltip_text(&format!("{}\n{}", event.subject, event.mailbox))
                .build();
            chip.add_css_class("event-chip");
            chip.add_css_class(&format!("mailbox-{}", session % 6));
            if event.cancelled {
                chip.add_css_class("event-cancelled");
            }
            // Double-click opens the appointment, as in Outlook.
            let gesture = gtk::GestureClick::new();
            let s = state.clone();
            let event_id = event.id.clone();
            let owner = *session;
            gesture.connect_pressed(move |_, presses, _, _| {
                if presses >= 2 {
                    open_event(&s, owner, &event_id);
                }
            });
            chip.add_controller(gesture);
            day_box.append(&chip);
        }
        if today_events.len() > CHIPS_PER_DAY {
            let more = gtk::Label::new(Some(&format!("+{} more", today_events.len() - CHIPS_PER_DAY)));
            more.set_xalign(0.0);
            more.add_css_class("cal-more");
            day_box.append(&more);
        }

        // Clicking a day shows it in the panel on the right.
        let gesture = gtk::GestureClick::new();
        let s = state.clone();
        gesture.connect_pressed(move |_, _, _, _| {
            if s.calendar.building.get() {
                return;
            }
            select_day(&s, day);
        });
        day_box.add_controller(gesture);

        cells.push((day, day_box.clone()));
        ui.grid.attach(&day_box, (cell % 7) as i32, (cell / 7) as i32, 1, 1);
    }
    *ui.cells.borrow_mut() = cells;
    ui.building.set(false);

    refresh_day_panel(state);
}

/// Select a day without rebuilding the grid.
pub fn select_day(state: &Rc<State>, day: NaiveDate) {
    {
        let mut selected = state.calendar.selected.borrow_mut();
        if *selected == day {
            return;
        }
        *selected = day;
    }
    for (date, cell) in state.calendar.cells.borrow().iter() {
        if *date == day {
            cell.add_css_class("cal-selected");
        } else {
            cell.remove_css_class("cal-selected");
        }
    }
    refresh_day_panel(state);
}

/// Fill the panel beside the grid with the selected day's appointments.
pub fn refresh_day_panel(state: &Rc<State>) {
    let ui = &state.calendar;
    let selected = *ui.selected.borrow();
    ui.day_title.set_text(&selected.format("%A %-d %B %Y").to_string());
    while let Some(row) = ui.day_list.row_at_index(0) {
        ui.day_list.remove(&row);
    }
    let events = events_for_grid(state, selected, selected + Duration::days(1));
    for (session, event) in events.iter().filter(|(_, e)| occurs_on(e, selected)) {
        ui.day_list.append(&event_row(state, *session, event));
    }
}

fn event_row(state: &Rc<State>, session: usize, event: &CalendarEvent) -> gtk::ListBoxRow {
    let row = gtk::ListBoxRow::new();
    row.set_selectable(false);

    let gesture = gtk::GestureClick::new();
    let s = state.clone();
    let event_id = event.id.clone();
    gesture.connect_pressed(move |_, presses, _, _| {
        if presses >= 2 {
            open_event(&s, session, &event_id);
        }
    });
    row.add_controller(gesture);
    let row_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .margin_top(8)
        .margin_bottom(8)
        .margin_start(14)
        .margin_end(14)
        .build();

    let when = if event.all_day {
        "All day".to_string()
    } else {
        match (local_of(&event.start), local_of(&event.end)) {
            (Some(a), Some(b)) => format!("{} – {}", a.format("%H:%M"), b.format("%H:%M")),
            _ => String::new(),
        }
    };
    let time = gtk::Label::builder().label(&when).xalign(0.0).build();
    time.add_css_class("caption");
    time.add_css_class(&format!("mailbox-text-{}", session % 6));

    let subject =
        gtk::Label::builder().label(&event.subject).xalign(0.0).wrap(true).build();
    subject.add_css_class("heading");
    if event.cancelled {
        subject.add_css_class("event-cancelled");
    }

    row_box.append(&time);
    row_box.append(&subject);
    for (text, dim) in [(event.location.clone(), true), (event.organizer.clone(), true)] {
        if text.is_empty() {
            continue;
        }
        let label = gtk::Label::builder()
            .label(&text)
            .xalign(0.0)
            .ellipsize(EllipsizeMode::End)
            .build();
        label.add_css_class("caption");
        if dim {
            label.add_css_class("dim-label");
        }
        row_box.append(&label);
    }
    row.set_child(Some(&row_box));
    row
}

/// An open appointment. Kept so the body can be filled in when it arrives.
pub struct AppointmentWindow {
    pub id: String,
    window: adw::Window,
    body_label: gtk::Label,
    attendees_label: gtk::Label,
}

/// Open an appointment: cached details right away, body fetched if missing.
pub fn open_event(state: &Rc<State>, session: usize, id: &str) {
    // Already open? Just bring it forward.
    if let Some(existing) = state.appointments.borrow().iter().find(|a| a.id == id) {
        existing.window.present();
        return;
    }
    let Some(db) = state.sessions.borrow().get(session).map(|s| s.db.clone()) else { return };
    let Ok(Some((event, body, attendees))) = db.event(id) else { return };

    let window = adw::Window::builder()
        .transient_for(&state.window)
        .modal(false)
        .default_width(580)
        .default_height(520)
        .title(&event.subject)
        .build();
    let view = ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .margin_top(16)
        .margin_bottom(12)
        .margin_start(20)
        .margin_end(20)
        .build();

    let subject = gtk::Label::builder().label(&event.subject).xalign(0.0).wrap(true).build();
    subject.add_css_class("mail-subject");
    if event.cancelled {
        subject.add_css_class("event-cancelled");
    }
    content.append(&subject);

    let when = if event.all_day {
        match DateTime::parse_from_rfc3339(&event.start) {
            Ok(dt) => format!("All day · {}", dt.naive_utc().date().format("%A %-d %B %Y")),
            Err(_) => "All day".to_string(),
        }
    } else {
        match (local_of(&event.start), local_of(&event.end)) {
            (Some(a), Some(b)) => format!(
                "{} · {} – {}",
                a.format("%A %-d %B %Y"),
                a.format("%H:%M"),
                b.format("%H:%M")
            ),
            _ => String::new(),
        }
    };
    let when_label = gtk::Label::builder().label(&when).xalign(0.0).wrap(true).build();
    when_label.add_css_class(&format!("mailbox-text-{}", session % 6));
    content.append(&when_label);

    for (prefix, value) in [
        ("Where", event.location.clone()),
        ("Organiser", event.organizer.clone()),
        ("Mailbox", state.sessions.borrow().get(session).map(|s| s.title()).unwrap_or_default()),
    ] {
        if value.is_empty() {
            continue;
        }
        let label =
            gtk::Label::builder().label(&format!("{prefix}: {value}")).xalign(0.0).wrap(true).build();
        label.add_css_class("dim-label");
        label.add_css_class("caption");
        content.append(&label);
    }

    let attendees_label = gtk::Label::builder().xalign(0.0).wrap(true).visible(false).build();
    attendees_label.add_css_class("dim-label");
    attendees_label.add_css_class("caption");
    content.append(&attendees_label);

    content.append(&gtk::Separator::builder().margin_top(8).margin_bottom(8).build());

    let body_label = gtk::Label::builder()
        .xalign(0.0)
        .yalign(0.0)
        .wrap(true)
        .selectable(true)
        // Selectable labels grab focus and highlight themselves on open;
        // the text stays selectable with the mouse without that.
        .can_focus(false)
        .vexpand(true)
        .build();
    content.append(
        &gtk::ScrolledWindow::builder()
            .child(&body_label)
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build(),
    );

    view.set_content(Some(&content));
    window.set_content(Some(view.widget()));

    let entry = AppointmentWindow {
        id: id.to_string(),
        window: window.clone(),
        body_label,
        attendees_label,
    };
    fill_body(&entry, body.as_deref(), &attendees, &event.preview);
    state.appointments.borrow_mut().push(entry);

    // Forget it once closed, so a later reopen builds a fresh window.
    {
        let s = state.clone();
        let id = id.to_string();
        window.connect_close_request(move |_| {
            s.appointments.borrow_mut().retain(|a| a.id != id);
            gtk::glib::Propagation::Proceed
        });
    }

    // Ask for the body; harmless when it is already cached.
    if let Some(session) = state.sessions.borrow().get(session) {
        session.send(Cmd::OpenEvent(id.to_string()));
    }
    window.present();
}

fn fill_body(entry: &AppointmentWindow, body: Option<&str>, attendees: &[String], preview: &str) {
    let text = match body {
        Some(body) if !body.trim().is_empty() => html_to_text(body),
        _ if !preview.trim().is_empty() => preview.to_string(),
        _ => "No description.".to_string(),
    };
    entry.body_label.set_text(&text);
    if attendees.is_empty() {
        entry.attendees_label.set_visible(false);
    } else {
        entry.attendees_label.set_text(&format!("Invited: {}", attendees.join(", ")));
        entry.attendees_label.set_visible(true);
    }
}

/// Called when a mailbox reports that an appointment body has arrived.
pub fn event_ready(state: &Rc<State>, id: &str) {
    if !state.appointments.borrow().iter().any(|a| a.id == id) {
        return;
    }
    // The appointment can belong to any mailbox; take the first cache that
    // holds it.
    let caches: Vec<_> = state.sessions.borrow().iter().map(|s| s.db.clone()).collect();
    for db in caches {
        let Ok(Some((event, body, attendees))) = db.event(id) else { continue };
        let appointments = state.appointments.borrow();
        if let Some(entry) = appointments.iter().find(|a| a.id == id) {
            fill_body(entry, body.as_deref(), &attendees, &event.preview);
        }
        return;
    }
}

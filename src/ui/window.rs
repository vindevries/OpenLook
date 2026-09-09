//! The main window: command bar, folder pane (one tree per mailbox),
//! date-grouped message list, reading pane and status bar.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use chrono::{DateTime, Local};
use gtk::pango::EllipsizeMode;
use gtk::{gdk, gio, glib};
use tokio::sync::Mutex;
#[cfg(feature = "html-view")]
use webkit::prelude::*;

use crate::auth::Auth;
use crate::model::{
    AccountInfo, Folder, MessageDetail, MessageSummary, Op, Pending, SendMode, Status,
};
use crate::sync::{runtime, Cmd, Event, Mode, Session};
use crate::ui::compose::ComposeWindow;
use crate::ui::dialogs;
use crate::ui::widgets::ToolbarView;
use crate::util::{fmt_full_time, fmt_since, fmt_time, wrap_body};

/// Folders shown under Favorites for a single-mailbox setup.
const FAVOURITES: [&str; 4] = ["Inbox", "Sent Items", "Drafts", "Deleted Items"];
/// Folders whose badge is a total count in brackets, as Outlook shows them.
const BRACKET_COUNT: [&str; 3] = ["Drafts", "Outbox", "Junk Email"];

fn folder_icon(name: &str) -> &'static str {
    match name {
        "Inbox" => "mail-inbox-symbolic",
        "Drafts" => "document-edit-symbolic",
        "Sent Items" => "mail-send-symbolic",
        "Outbox" => "mail-outbox-symbolic",
        "Deleted Items" => "user-trash-symbolic",
        "Junk Email" => "mail-mark-junk-symbolic",
        "Archive" => "folder-symbolic",
        "Conversation History" => "user-available-symbolic",
        _ => "folder-symbolic",
    }
}

/// Which kind of mailbox the panes are showing. Tickets get their own
/// pane rather than sitting among the mail folders.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Mail,
    Tickets,
}

/// One row of the folder pane.
#[derive(Clone)]
enum PaneRow {
    /// "Favorites", or a mailbox address. `key` identifies it for collapsing.
    Header { key: String, title: String },
    Folder { session: usize, folder: Folder, favourite: bool },
    /// Shown under a mailbox that has nothing cached yet, so an empty
    /// section always says why.
    Notice { text: String, error: bool },
}

/// One row of the message list.
enum ListEntry {
    /// A date separator; its text lives in the row widget.
    DateHeader,
    Message(MessageSummary),
}

/// Outlook-style date buckets.
fn date_bucket(received: &str) -> String {
    let Ok(parsed) = DateTime::parse_from_rfc3339(received) else { return "Older".into() };
    let dt = parsed.with_timezone(&Local);
    let now = Local::now();
    let days = (now.date_naive() - dt.date_naive()).num_days();
    match days {
        d if d <= 0 => "Today".into(),
        1 => "Yesterday".into(),
        2..=6 => dt.format("%A").to_string(),
        7..=13 => "Last Week".into(),
        14..=30 => "Earlier This Month".into(),
        _ => "Older".into(),
    }
}

fn initials(name: &str) -> String {
    let parts: Vec<&str> = name.split_whitespace().filter(|p| !p.is_empty()).collect();
    match parts.len() {
        0 => "?".into(),
        1 => parts[0].chars().take(2).collect::<String>().to_uppercase(),
        _ => {
            let first = parts[0].chars().next().unwrap_or('?');
            let last = parts[parts.len() - 1].chars().next().unwrap_or('?');
            format!("{first}{last}").to_uppercase()
        }
    }
}

/// Stable colour per correspondent, like Outlook's contact circles.
fn avatar_class(seed: &str) -> String {
    let sum: u32 = seed.bytes().map(u32::from).sum();
    format!("avatar-{}", sum % 6)
}

pub struct State {
    pub window: adw::ApplicationWindow,
    pub toasts: adw::ToastOverlay,
    folder_list: gtk::ListBox,
    message_list: gtk::ListBox,
    search: gtk::SearchEntry,
    reading: gtk::Stack,
    subject_label: gtk::Label,
    avatar: gtk::Label,
    from_label: gtk::Label,
    to_label: gtk::Label,
    date_label: gtk::Label,
    pending_label: gtk::Label,
    read_toggle: gtk::Button,
    filter_unread: gtk::ToggleButton,
    sort_button: gtk::Button,
    folder_title: gtk::Label,
    count_label: gtk::Label,
    connection_label: gtk::Label,
    pub calendar: crate::ui::calendar::CalendarUi,
    /// Appointment windows currently open, so a body can be filled in.
    pub appointments: RefCell<Vec<crate::ui::calendar::AppointmentWindow>>,
    view_stack: gtk::Stack,
    #[cfg(feature = "html-view")]
    webview: webkit::WebView,
    #[cfg(not(feature = "html-view"))]
    body_label: gtk::Label,

    pub auth: Arc<Mutex<Auth>>,
    pub http: reqwest::Client,
    /// One per mailbox (or a single demo mailbox when nothing is signed in).
    pub sessions: RefCell<Vec<Session>>,
    statuses: RefCell<Vec<Status>>,
    pane_rows: RefCell<Vec<PaneRow>>,
    entries: RefCell<Vec<ListEntry>>,
    collapsed: RefCell<HashSet<String>>,
    current: RefCell<Option<(usize, String)>>,
    current_message: RefCell<Option<String>>,
    scope: Cell<Scope>,
    /// Selection is remembered per scope, so switching panes returns you
    /// to where you were.
    mail_selection: RefCell<Option<(usize, String)>>,
    ticket_selection: RefCell<Option<(usize, String)>>,
    unread_only: Cell<bool>,
    newest_first: Cell<bool>,
    rebuilding: Cell<bool>,
}

impl State {
    pub fn scope(&self) -> Scope {
        self.scope.get()
    }

    /// Whether a session belongs in the pane currently on show.
    fn in_scope(&self, index: usize) -> bool {
        let tickets = self.sessions.borrow().get(index).map(Session::is_tickets).unwrap_or(false);
        match self.scope.get() {
            Scope::Mail => !tickets,
            Scope::Tickets => tickets,
        }
    }

    /// Index of the mailbox whose folder is open (falls back to the first).
    pub fn active_session(&self) -> usize {
        self.current.borrow().as_ref().map(|(index, _)| *index).unwrap_or(0)
    }
}

pub fn build(app: &adw::Application) {
    let http = reqwest::Client::builder()
        .user_agent(concat!("OpenLook/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap_or_default();

    let auth_raw = Auth::load(http.clone());
    let accounts = auth_raw.accounts();
    let auth = Arc::new(Mutex::new(auth_raw));

    let mut sessions = Vec::new();
    for account in &accounts {
        match Session::new(Mode::Account(account.clone()), auth.clone(), http.clone()) {
            Ok(session) => sessions.push(session),
            Err(e) => eprintln!("openlook: cannot open cache for {}: {e}", account.username),
        }
    }
    // HubSpot ticket pipelines sit alongside the mailboxes, each as its
    // own section, and only when a token has been provided.
    if crate::config::hubspot_token().is_some() {
        for pipeline in crate::config::Settings::load().hubspot_pipelines {
            let mode = Mode::Tickets {
                pipeline_id: pipeline.id.clone(),
                pipeline_label: pipeline.label.clone(),
            };
            match Session::new(mode, auth.clone(), http.clone()) {
                Ok(session) => sessions.push(session),
                Err(e) => eprintln!("openlook: cannot open cache for {}: {e}", pipeline.label),
            }
        }
    }

    if sessions.is_empty() {
        match Session::new(Mode::Demo, auth.clone(), http.clone()) {
            Ok(session) => sessions.push(session),
            Err(e) => {
                eprintln!("openlook: cannot open mail cache: {e}");
                return;
            }
        }
    }
    let session_count = sessions.len();

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("OpenLook")
        .default_width(1420)
        .default_height(900)
        .width_request(940)
        .height_request(580)
        .build();

    // ----- header ------------------------------------------------------
    let header = adw::HeaderBar::new();
    let search = gtk::SearchEntry::builder()
        .placeholder_text("Search current folder")
        .width_request(420)
        .build();
    header.set_title_widget(Some(&search));

    let menu = gio::Menu::new();
    let account_section = gio::Menu::new();
    account_section.append(Some("Add mailbox…"), Some("win.sign-in"));
    account_section.append(Some("Remove this mailbox"), Some("win.sign-out"));
    menu.append_section(None, &account_section);
    let end_section = gio::Menu::new();
    end_section.append(Some("Settings…"), Some("win.settings"));
    end_section.append(Some("About OpenLook"), Some("win.about"));
    menu.append_section(None, &end_section);
    header.pack_end(
        &gtk::MenuButton::builder().icon_name("open-menu-symbolic").menu_model(&menu).build(),
    );

    // ----- command bar (ribbon-style) -----------------------------------
    let command_bar = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(2)
        .margin_top(4)
        .margin_bottom(4)
        .margin_start(8)
        .margin_end(8)
        .build();
    command_bar.add_css_class("command-bar");

    let command = |icon: &str, label: &str, action: &str, tooltip: &str| -> gtk::Button {
        let content = gtk::Box::new(gtk::Orientation::Vertical, 2);
        content.set_halign(gtk::Align::Center);
        let image = gtk::Image::from_icon_name(icon);
        image.set_pixel_size(20);
        let text = gtk::Label::new(Some(label));
        text.add_css_class("command-label");
        content.append(&image);
        content.append(&text);
        let button = gtk::Button::builder()
            .child(&content)
            .tooltip_text(tooltip)
            .action_name(action)
            .build();
        button.add_css_class("flat");
        button.add_css_class("command-button");
        button
    };
    let separator = || {
        let sep = gtk::Separator::new(gtk::Orientation::Vertical);
        sep.set_margin_top(4);
        sep.set_margin_bottom(4);
        sep.set_margin_start(6);
        sep.set_margin_end(6);
        sep
    };

    command_bar.append(&command(
        "mail-message-new-symbolic",
        "New email",
        "win.new-mail",
        "New message (Ctrl+N)",
    ));
    command_bar.append(&separator());
    command_bar.append(&command("user-trash-symbolic", "Delete", "win.delete", "Delete (Del)"));
    command_bar.append(&command("folder-symbolic", "Archive", "win.archive", "Move to Archive"));
    command_bar.append(&separator());
    command_bar.append(&command(
        "mail-reply-sender-symbolic",
        "Reply",
        "win.reply",
        "Reply (Ctrl+Shift+R)",
    ));
    command_bar.append(&command("mail-reply-all-symbolic", "Reply all", "win.reply-all", "Reply all"));
    command_bar.append(&command("mail-forward-symbolic", "Forward", "win.forward", "Forward"));
    command_bar.append(&separator());
    command_bar.append(&command(
        "mail-unread-symbolic",
        "Unread",
        "win.toggle-read",
        "Mark read or unread",
    ));
    command_bar.append(&separator());
    command_bar.append(&command("view-refresh-symbolic", "Send / Receive", "win.refresh", "Refresh (F5)"));

    // ----- three panes --------------------------------------------------
    let body = gtk::Box::new(gtk::Orientation::Horizontal, 0);

    let folder_list = gtk::ListBox::new();
    folder_list.set_selection_mode(gtk::SelectionMode::Single);
    folder_list.add_css_class("navigation-sidebar");
    folder_list.add_css_class("folder-pane");
    let folder_scroll = gtk::ScrolledWindow::builder()
        .child(&folder_list)
        .vexpand(true)
        .width_request(250)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    body.append(&folder_scroll);
    body.append(&gtk::Separator::new(gtk::Orientation::Vertical));

    // Message list, with its own header strip (folder name, All/Unread, sort)
    let list_pane = gtk::Box::new(gtk::Orientation::Vertical, 0);
    list_pane.set_size_request(400, -1);

    let list_head = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .margin_top(8)
        .margin_bottom(6)
        .margin_start(12)
        .margin_end(12)
        .build();
    let folder_title = gtk::Label::builder().xalign(0.0).ellipsize(EllipsizeMode::End).build();
    folder_title.add_css_class("heading");
    list_head.append(&folder_title);

    let filter_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let filter_all = gtk::ToggleButton::builder().label("All").active(true).build();
    filter_all.add_css_class("flat");
    filter_all.add_css_class("filter-tab");
    let filter_unread = gtk::ToggleButton::builder().label("Unread").build();
    filter_unread.add_css_class("flat");
    filter_unread.add_css_class("filter-tab");
    filter_unread.set_group(Some(&filter_all));
    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    let sort_button = gtk::Button::builder().tooltip_text("Sort by date").build();
    sort_button.add_css_class("flat");
    sort_button.set_child(Some(
        &adw::ButtonContent::builder().icon_name("go-down-symbolic").label("By date").build(),
    ));
    filter_row.append(&filter_all);
    filter_row.append(&filter_unread);
    filter_row.append(&spacer);
    filter_row.append(&sort_button);
    list_head.append(&filter_row);
    list_pane.append(&list_head);
    list_pane.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

    let message_list = gtk::ListBox::new();
    message_list.set_selection_mode(gtk::SelectionMode::Single);
    message_list.add_css_class("message-list");
    let placeholder = gtk::Label::builder().label("No messages").margin_top(24).build();
    placeholder.add_css_class("dim-label");
    message_list.set_placeholder(Some(&placeholder));
    list_pane.append(
        &gtk::ScrolledWindow::builder()
            .child(&message_list)
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build(),
    );
    body.append(&list_pane);
    body.append(&gtk::Separator::new(gtk::Orientation::Vertical));

    // ----- reading pane -------------------------------------------------
    let reading = gtk::Stack::builder().hexpand(true).vexpand(true).build();
    reading.add_named(
        &adw::StatusPage::builder()
            .icon_name("mail-unread-symbolic")
            .title("Select a message")
            .description("Nothing is selected")
            .build(),
        Some("empty"),
    );

    let reader = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let head = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .margin_top(16)
        .margin_bottom(12)
        .margin_start(20)
        .margin_end(20)
        .build();

    let subject_label =
        gtk::Label::builder().xalign(0.0).wrap(true).selectable(true).build();
    subject_label.add_css_class("mail-subject");
    head.append(&subject_label);

    let sender_row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    let avatar = gtk::Label::new(None);
    avatar.add_css_class("avatar");
    avatar.set_size_request(40, 40);
    avatar.set_valign(gtk::Align::Center);
    sender_row.append(&avatar);

    let sender_details = gtk::Box::new(gtk::Orientation::Vertical, 2);
    sender_details.set_hexpand(true);
    sender_details.set_valign(gtk::Align::Center);
    let from_label =
        gtk::Label::builder().xalign(0.0).selectable(true).ellipsize(EllipsizeMode::End).build();
    let to_label =
        gtk::Label::builder().xalign(0.0).selectable(true).ellipsize(EllipsizeMode::End).build();
    to_label.add_css_class("dim-label");
    to_label.add_css_class("caption");
    sender_details.append(&from_label);
    sender_details.append(&to_label);
    sender_row.append(&sender_details);

    let actions = gtk::Box::new(gtk::Orientation::Vertical, 4);
    actions.set_valign(gtk::Align::Start);
    let action_buttons = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    action_buttons.set_halign(gtk::Align::End);
    let reply_button = gtk::Button::builder().action_name("win.reply").build();
    reply_button.set_child(Some(
        &adw::ButtonContent::builder().icon_name("mail-reply-sender-symbolic").label("Reply").build(),
    ));
    reply_button.add_css_class("flat");
    let reply_all_button = gtk::Button::builder().action_name("win.reply-all").build();
    reply_all_button.set_child(Some(
        &adw::ButtonContent::builder().icon_name("mail-reply-all-symbolic").label("Reply all").build(),
    ));
    reply_all_button.add_css_class("flat");
    let forward_button = gtk::Button::builder().action_name("win.forward").build();
    forward_button.set_child(Some(
        &adw::ButtonContent::builder().icon_name("mail-forward-symbolic").label("Forward").build(),
    ));
    forward_button.add_css_class("flat");
    let read_toggle = gtk::Button::builder()
        .icon_name("mail-unread-symbolic")
        .tooltip_text("Mark as unread")
        .action_name("win.toggle-read")
        .build();
    read_toggle.add_css_class("flat");
    let delete_button = gtk::Button::builder()
        .icon_name("user-trash-symbolic")
        .tooltip_text("Delete")
        .action_name("win.delete")
        .build();
    delete_button.add_css_class("flat");
    action_buttons.append(&reply_button);
    action_buttons.append(&reply_all_button);
    action_buttons.append(&forward_button);
    action_buttons.append(&read_toggle);
    action_buttons.append(&delete_button);
    actions.append(&action_buttons);
    let date_label = gtk::Label::builder().xalign(1.0).build();
    date_label.add_css_class("dim-label");
    date_label.add_css_class("caption");
    actions.append(&date_label);
    sender_row.append(&actions);
    head.append(&sender_row);

    let pending_label = gtk::Label::builder().xalign(0.0).visible(false).build();
    pending_label.add_css_class("queued-chip");
    head.append(&pending_label);

    reader.append(&head);
    reader.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

    #[cfg(feature = "html-view")]
    let webview = {
        let view = webkit::WebView::new();
        view.set_hexpand(true);
        view.set_vexpand(true);
        if let Some(settings) = webkit::prelude::WebViewExt::settings(&view) {
            // Mail is untrusted content: no scripting, no plugins.
            settings.set_enable_javascript(false);
            settings.set_enable_html5_local_storage(false);
            settings.set_enable_developer_extras(false);
        }
        reader.append(&view);
        view
    };
    #[cfg(not(feature = "html-view"))]
    let body_label = {
        let label = gtk::Label::builder()
            .xalign(0.0)
            .yalign(0.0)
            .wrap(true)
            .selectable(true)
            .margin_top(12)
            .margin_start(20)
            .margin_end(20)
            .build();
        reader.append(&gtk::ScrolledWindow::builder().child(&label).vexpand(true).build());
        label
    };

    reading.add_named(&reader, Some("message"));
    body.append(&reading);

    // ----- status bar ---------------------------------------------------
    let status_bar = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(16)
        .margin_top(4)
        .margin_bottom(4)
        .margin_start(12)
        .margin_end(12)
        .build();
    status_bar.add_css_class("status-bar");
    let count_label = gtk::Label::builder().xalign(0.0).build();
    count_label.add_css_class("caption");
    let status_spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    status_spacer.set_hexpand(true);
    let connection_label = gtk::Label::builder().xalign(1.0).build();
    connection_label.add_css_class("caption");
    status_bar.append(&count_label);
    status_bar.append(&status_spacer);
    status_bar.append(&connection_label);

    // Mail and Calendar as separate pages, switched from a rail on the left
    // the way Outlook's module buttons work.
    let calendar = crate::ui::calendar::CalendarUi::new();
    let view_stack = gtk::Stack::new();
    view_stack.set_hexpand(true);
    view_stack.add_named(&body, Some("mail"));
    view_stack.add_named(&calendar.root, Some("calendar"));

    let rail = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .margin_top(8)
        .margin_start(4)
        .margin_end(4)
        .build();
    rail.add_css_class("view-rail");
    let mail_button = gtk::ToggleButton::builder()
        .icon_name("mail-unread-symbolic")
        .tooltip_text("Mail")
        .active(true)
        .build();
    mail_button.add_css_class("flat");
    let calendar_button = gtk::ToggleButton::builder()
        .icon_name("x-office-calendar-symbolic")
        .tooltip_text("Calendar")
        .build();
    calendar_button.add_css_class("flat");
    calendar_button.set_group(Some(&mail_button));
    let tickets_button = gtk::ToggleButton::builder()
        .icon_name("view-list-symbolic")
        .tooltip_text("Tickets")
        .build();
    tickets_button.add_css_class("flat");
    tickets_button.set_group(Some(&mail_button));
    rail.append(&mail_button);
    rail.append(&calendar_button);
    rail.append(&tickets_button);

    let shell = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    shell.append(&rail);
    shell.append(&gtk::Separator::new(gtk::Orientation::Vertical));
    shell.append(&view_stack);

    let view = ToolbarView::new();
    view.add_top_bar(&header);
    view.add_top_bar(&command_bar);
    view.set_content(Some(&shell));
    view.add_bottom_bar(&status_bar);
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(view.widget()));
    window.set_content(Some(&toasts));

    let state = Rc::new(State {
        window: window.clone(),
        toasts,
        folder_list,
        message_list,
        search,
        reading,
        subject_label,
        avatar,
        from_label,
        to_label,
        date_label,
        pending_label,
        read_toggle,
        filter_unread,
        sort_button,
        folder_title,
        count_label,
        connection_label,
        calendar,
        appointments: RefCell::new(Vec::new()),
        view_stack,
        #[cfg(feature = "html-view")]
        webview,
        #[cfg(not(feature = "html-view"))]
        body_label,
        auth,
        http,
        sessions: RefCell::new(sessions),
        statuses: RefCell::new(vec![Status::default(); session_count]),
        pane_rows: RefCell::new(Vec::new()),
        entries: RefCell::new(Vec::new()),
        collapsed: RefCell::new(HashSet::new()),
        current: RefCell::new(None),
        current_message: RefCell::new(None),
        scope: Cell::new(Scope::Mail),
        mail_selection: RefCell::new(None),
        ticket_selection: RefCell::new(None),
        unread_only: Cell::new(false),
        newest_first: Cell::new(true),
        rebuilding: Cell::new(false),
    });

    {
        // The command bar is mail-only, so it steps aside for the calendar.
        let s = state.clone();
        let command_bar = command_bar.clone();
        calendar_button.connect_toggled(move |button| {
            if button.is_active() {
                s.view_stack.set_visible_child_name("calendar");
                command_bar.set_visible(false);
                crate::ui::calendar::refresh(&s);
                crate::ui::calendar::request_sync(&s);
            } else if !s.view_stack.visible_child_name().is_some_and(|n| n == "mail") {
                // Leaving the calendar for whichever list pane is selected.
                s.view_stack.set_visible_child_name("mail");
                command_bar.set_visible(true);
            }
        });
    }
    {
        // Tickets reuse the three panes, showing the ticket sessions only.
        let s = state.clone();
        tickets_button.connect_toggled(move |button| {
            set_scope(&s, if button.is_active() { Scope::Tickets } else { Scope::Mail });
        });
    }
    {
        let s = state.clone();
        mail_button.connect_toggled(move |button| {
            if button.is_active() {
                set_scope(&s, Scope::Mail);
            }
        });
    }
    for (button, delta) in
        [(state.calendar.prev.clone(), -1), (state.calendar.next.clone(), 1)]
    {
        let s = state.clone();
        button.connect_clicked(move |_| crate::ui::calendar::step_month(&s, delta));
    }
    {
        let s = state.clone();
        state.calendar.today.connect_clicked(move |_| crate::ui::calendar::go_today(&s));
    }

    connect_signals(&state, app);
    for index in 0..state.sessions.borrow().len() {
        listen(&state, index);
    }
    watch_network(&state);

    reload_folders(&state, true);
    render_status(&state);
    for session in state.sessions.borrow().iter() {
        session.send(Cmd::SyncAll(None));
    }

    window.present();

    // No mailbox configured: offer to add one, as Outlook does.
    if state.sessions.borrow().iter().all(Session::is_demo)
        && !crate::config::Settings::load().demo_ack
    {
        let state = state.clone();
        glib::idle_add_local_once(move || dialogs::show_account_dialog(&state, true));
    }
}

fn connect_signals(state: &Rc<State>, app: &adw::Application) {
    // Folder selection
    let s = state.clone();
    state.folder_list.connect_row_selected(move |_, row| {
        if s.rebuilding.get() {
            return;
        }
        let Some(row) = row else { return };
        let Some(PaneRow::Folder { session, folder, .. }) =
            s.pane_rows.borrow().get(row.index() as usize).cloned()
        else {
            return;
        };
        if s.current.borrow().as_ref() == Some(&(session, folder.id.clone())) {
            return;
        }
        *s.current.borrow_mut() = Some((session, folder.id.clone()));
        show_message(&s, None);
        reload_messages(&s);
        render_status(&s);
        if let Some(session) = s.sessions.borrow().get(session) {
            session.send(Cmd::SyncFolder(folder.id.clone()));
        }
    });

    // Collapsing a mailbox section
    let s = state.clone();
    state.folder_list.connect_row_activated(move |_, row| {
        let Some(PaneRow::Header { key, .. }) =
            s.pane_rows.borrow().get(row.index() as usize).cloned()
        else {
            return;
        };
        {
            let mut collapsed = s.collapsed.borrow_mut();
            if !collapsed.remove(&key) {
                collapsed.insert(key);
            }
        }
        reload_folders(&s, false);
    });

    // Message selection
    let s = state.clone();
    state.message_list.connect_row_selected(move |_, row| {
        if s.rebuilding.get() {
            return;
        }
        let Some(row) = row else { return };
        let summary = match s.entries.borrow().get(row.index() as usize) {
            Some(ListEntry::Message(summary)) => summary.clone(),
            _ => return,
        };
        open_message(&s, &summary);
    });

    let s = state.clone();
    state.search.connect_search_changed(move |_| reload_messages(&s));

    let s = state.clone();
    state.filter_unread.connect_toggled(move |button| {
        s.unread_only.set(button.is_active());
        reload_messages(&s);
    });

    let s = state.clone();
    state.sort_button.connect_clicked(move |button| {
        let newest = !s.newest_first.get();
        s.newest_first.set(newest);
        button.set_child(Some(
            &adw::ButtonContent::builder()
                .icon_name(if newest { "go-down-symbolic" } else { "go-up-symbolic" })
                .label("By date")
                .build(),
        ));
        reload_messages(&s);
    });

    let actions: [(&str, Box<dyn Fn(&Rc<State>)>); 10] = [
        ("new-mail", Box::new(|s: &Rc<State>| ComposeWindow::open(s, s.active_session(), None))),
        ("refresh", Box::new(|s: &Rc<State>| {
            let folder = s.current.borrow().as_ref().map(|(_, id)| id.clone());
            for (index, session) in s.sessions.borrow().iter().enumerate() {
                let target = if index == s.active_session() { folder.clone() } else { None };
                session.send(Cmd::SyncAll(target));
            }
            reload_folders(s, false);
            reload_messages(s);
        })),
        ("reply", Box::new(|s: &Rc<State>| respond(s, SendMode::Reply))),
        ("reply-all", Box::new(|s: &Rc<State>| respond(s, SendMode::ReplyAll))),
        ("forward", Box::new(|s: &Rc<State>| respond(s, SendMode::Forward))),
        ("delete", Box::new(delete_current)),
        ("archive", Box::new(archive_current)),
        ("toggle-read", Box::new(toggle_read_current)),
        ("sign-in", Box::new(|s: &Rc<State>| match s.scope() {
            // In the ticket pane, "add" means connecting HubSpot.
            Scope::Tickets => dialogs::show_hubspot_dialog(s),
            Scope::Mail => dialogs::show_account_dialog(s, false),
        })),
        ("sign-out", Box::new(dialogs::sign_out)),
    ];
    for (name, handler) in actions {
        let action = gio::SimpleAction::new(name, None);
        let s = state.clone();
        action.connect_activate(move |_, _| handler(&s));
        state.window.add_action(&action);
    }
    for (name, handler) in [
        ("settings", Box::new(dialogs::show_settings) as Box<dyn Fn(&Rc<State>)>),
        ("about", Box::new(dialogs::show_about)),
    ] {
        let action = gio::SimpleAction::new(name, None);
        let s = state.clone();
        action.connect_activate(move |_, _| handler(&s));
        state.window.add_action(&action);
    }

    app.set_accels_for_action("win.new-mail", &["<Control>n"]);
    app.set_accels_for_action("win.refresh", &["F5", "<Control>r"]);
    app.set_accels_for_action("win.reply", &["<Control><Shift>r"]);
    app.set_accels_for_action("win.reply-all", &["<Control><Alt>r"]);
    app.set_accels_for_action("win.forward", &["<Control><Shift>f"]);
    app.set_accels_for_action("win.delete", &["Delete"]);

    #[cfg(feature = "html-view")]
    {
        // Open links in the user's browser, never inside the mail view.
        let s = state.clone();
        state.webview.connect_decide_policy(move |_, decision, kind| {
            if kind != webkit::PolicyDecisionType::NavigationAction {
                return false;
            }
            let Some(nav) = decision.downcast_ref::<webkit::NavigationPolicyDecision>() else {
                return false;
            };
            let Some(mut action) = nav.navigation_action() else { return false };
            if action.navigation_type() != webkit::NavigationType::LinkClicked {
                return false;
            }
            if let Some(uri) = action.request().and_then(|r| r.uri()) {
                if uri.starts_with("http://") || uri.starts_with("https://") || uri.starts_with("mailto:")
                {
                    // gtk::UriLauncher needs GTK 4.10; this works back to 4.6.
                    #[allow(deprecated)]
                    gtk::show_uri(Some(&s.window), &uri, gdk::CURRENT_TIME);
                }
            }
            decision.ignore();
            true
        });
    }
}

/// Switch between the mail and ticket panes, keeping each one's selection.
pub fn set_scope(state: &Rc<State>, scope: Scope) {
    if state.scope.get() == scope {
        return;
    }
    // Park the current selection so returning here lands in the same place.
    let current = state.current.borrow().clone();
    match state.scope.get() {
        Scope::Mail => *state.mail_selection.borrow_mut() = current,
        Scope::Tickets => *state.ticket_selection.borrow_mut() = current,
    }
    state.scope.set(scope);
    let restored = match scope {
        Scope::Mail => state.mail_selection.borrow().clone(),
        Scope::Tickets => state.ticket_selection.borrow().clone(),
    };
    *state.current.borrow_mut() = restored.clone();
    *state.current_message.borrow_mut() = None;
    show_message(state, None);
    state.view_stack.set_visible_child_name("mail");

    reload_folders(state, restored.is_none());
    reload_messages(state);
    render_status(state);
}

/// Pump one mailbox's sync events into the UI.
pub fn listen(state: &Rc<State>, index: usize) {
    let Some(events) = state.sessions.borrow().get(index).map(|s| s.events.clone()) else {
        return;
    };
    let state = state.clone();
    glib::spawn_future_local(async move {
        while let Ok(event) = events.recv().await {
            match event {
                Event::FoldersChanged => reload_folders(&state, false),
                Event::MessagesChanged(folder) => {
                    if state.current.borrow().as_ref() == Some(&(index, folder.clone())) {
                        reload_messages(&state);
                    }
                    reload_folders(&state, false);
                }
                Event::BodyReady(id) => {
                    if state.current_message.borrow().as_deref() == Some(id.as_str()) {
                        let db = state.sessions.borrow().get(index).map(|s| s.db.clone());
                        if let Some(Ok(Some(detail))) = db.map(|db| db.message(&id)) {
                            render_body(&state, &detail);
                        }
                    }
                }
                Event::StatusChanged(status) => {
                    if let Some(slot) = state.statuses.borrow_mut().get_mut(index) {
                        *slot = status;
                    }
                    render_status(&state);
                    // A mailbox with nothing cached shows its state in the
                    // pane, so that line has to follow the status.
                    let empty = state
                        .sessions
                        .borrow()
                        .get(index)
                        .map(|s| s.db.folders().map(|f| f.is_empty()).unwrap_or(true))
                        .unwrap_or(false);
                    if empty {
                        reload_folders(&state, false);
                    }
                }
                Event::CalendarChanged => crate::ui::calendar::refresh(&state),
                Event::EventReady(id) => crate::ui::calendar::event_ready(&state, &id),
                Event::Failed(message) | Event::Notice(message) => toast(&state, &message),
            }
        }
    });
}

fn watch_network(state: &Rc<State>) {
    let monitor = gio::NetworkMonitor::default();
    let available = monitor.is_network_available();
    for session in state.sessions.borrow().iter() {
        session.send(Cmd::SetOnline(available));
    }
    let s = state.clone();
    monitor.connect_network_changed(move |_, available| {
        for session in s.sessions.borrow().iter() {
            session.send(Cmd::SetOnline(available));
        }
    });
}

// -- folder pane --------------------------------------------------------

pub fn reload_folders(state: &Rc<State>, select_default: bool) {
    let mut rows: Vec<PaneRow> = Vec::new();
    // Only the sessions belonging to the pane on show; tickets and mail
    // never appear in the same tree.
    let per_session: Vec<Vec<Folder>> = state
        .sessions
        .borrow()
        .iter()
        .enumerate()
        .map(|(index, session)| {
            if state.in_scope(index) {
                session.db.folders().unwrap_or_default()
            } else {
                Vec::new()
            }
        })
        .collect();
    let tickets_pane = matches!(state.scope.get(), Scope::Tickets);
    let visible = per_session.iter().filter(|f| !f.is_empty()).count();
    let multiple = visible > 1;

    // Favorites: the usual four for a single mailbox, each inbox when several.
    if !tickets_pane {
        rows.push(PaneRow::Header { key: "fav".into(), title: "Favorites".into() });
    }
    if !tickets_pane && !state.collapsed.borrow().contains("fav") {
        for (index, folders) in per_session.iter().enumerate() {
            if multiple {
                if let Some(inbox) = folders.iter().find(|f| f.display_name == "Inbox") {
                    rows.push(PaneRow::Folder {
                        session: index,
                        folder: inbox.clone(),
                        favourite: true,
                    });
                }
            } else {
                for name in FAVOURITES {
                    if let Some(folder) = folders.iter().find(|f| f.display_name == name) {
                        rows.push(PaneRow::Folder {
                            session: index,
                            folder: folder.clone(),
                            favourite: true,
                        });
                    }
                }
            }
        }
    }

    // One section per mailbox.
    for (index, folders) in per_session.iter().enumerate() {
        if !state.in_scope(index) {
            continue;
        }
        let key = format!("account:{index}");
        let title = state
            .sessions
            .borrow()
            .get(index)
            .map(Session::title)
            .unwrap_or_else(|| "Mailbox".into());
        rows.push(PaneRow::Header { key: key.clone(), title });
        if state.collapsed.borrow().contains(&key) {
            continue;
        }
        if folders.is_empty() {
            // Nothing cached yet: say whether we are still working on it or
            // the server refused, rather than showing an empty mailbox.
            let status = state.statuses.borrow().get(index).cloned().unwrap_or_default();
            let (text, error) = match status.detail {
                Some(detail) => (detail, true),
                None if status.syncing => ("Syncing…".to_string(), false),
                None => ("Waiting to sync…".to_string(), false),
            };
            rows.push(PaneRow::Notice { text, error });
            continue;
        }
        for folder in folders {
            rows.push(PaneRow::Folder { session: index, folder: folder.clone(), favourite: false });
        }
    }

    let selected = state.current.borrow().clone();
    state.rebuilding.set(true);
    while let Some(row) = state.folder_list.row_at_index(0) {
        state.folder_list.remove(&row);
    }
    let mut select_index: Option<i32> = None;
    for (index, row) in rows.iter().enumerate() {
        let widget = match row {
            PaneRow::Header { title, key } => {
                let collapsed = state.collapsed.borrow().contains(key);
                pane_header_row(title, collapsed)
            }
            PaneRow::Folder { session, folder, favourite } => {
                let is_selected = selected.as_ref() == Some(&(*session, folder.id.clone()));
                // With one mailbox the favourites duplicate the tree, so keep
                // the selection on the tree copy.
                if is_selected && !*favourite && select_index.is_none() {
                    select_index = Some(index as i32);
                }
                if select_default
                    && selected.is_none()
                    && *session == 0
                    && folder.display_name == "Inbox"
                    && !*favourite
                    && select_index.is_none()
                {
                    select_index = Some(index as i32);
                }
                folder_row(state, *session, folder, tickets_pane)
            }
            PaneRow::Notice { text, error } => pane_notice_row(text, *error),
        };
        state.folder_list.append(&widget);
    }
    *state.pane_rows.borrow_mut() = rows;
    state.rebuilding.set(false);

    if let Some(index) = select_index {
        if let Some(row) = state.folder_list.row_at_index(index) {
            state.folder_list.select_row(Some(&row));
        }
    }
}

fn pane_header_row(title: &str, collapsed: bool) -> gtk::ListBoxRow {
    let row = gtk::ListBoxRow::new();
    row.set_selectable(false);
    row.set_activatable(true);
    let row_box = gtk::Box::builder()
        .spacing(6)
        .margin_top(8)
        .margin_bottom(2)
        .margin_start(4)
        .margin_end(4)
        .build();
    let arrow = gtk::Image::from_icon_name(if collapsed {
        "pan-end-symbolic"
    } else {
        "pan-down-symbolic"
    });
    let label = gtk::Label::builder()
        .label(title)
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(EllipsizeMode::Middle)
        .build();
    label.add_css_class("pane-header");
    row_box.append(&arrow);
    row_box.append(&label);
    row.set_child(Some(&row_box));
    row
}

fn pane_notice_row(text: &str, error: bool) -> gtk::ListBoxRow {
    let row = gtk::ListBoxRow::new();
    row.set_selectable(false);
    row.set_activatable(false);
    let label = gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .wrap(true)
        .margin_top(4)
        .margin_bottom(6)
        .margin_start(20)
        .margin_end(8)
        .build();
    label.add_css_class("caption");
    label.add_css_class(if error { "error" } else { "dim-label" });
    row.set_child(Some(&label));
    row
}

fn folder_row(
    state: &Rc<State>,
    session: usize,
    folder: &Folder,
    tickets: bool,
) -> gtk::ListBoxRow {
    let row = gtk::ListBoxRow::new();

    let target = gtk::DropTarget::new(glib::Type::STRING, gdk::DragAction::MOVE);
    {
        let state = state.clone();
        let folder_id = folder.id.clone();
        let folder_name = folder.display_name.clone();
        target.connect_drop(move |_, value, _, _| {
            let Ok(payload) = value.get::<String>() else { return false };
            let Some((from_session, message_id)) = parse_drag_payload(&payload) else {
                return false;
            };
            if from_session != session {
                toast(&state, "Messages can only be moved within the same mailbox.");
                return false;
            }
            drop_message_into(&state, session, message_id, &folder_id, &folder_name);
            true
        });
    }
    // Highlight the folder the message is hovering over.
    {
        let row_for_enter = row.clone();
        target.connect_enter(move |_, _, _| {
            row_for_enter.add_css_class("drop-hover");
            gdk::DragAction::MOVE
        });
        let row_for_leave = row.clone();
        target.connect_leave(move |_| row_for_leave.remove_css_class("drop-hover"));
    }
    row.add_controller(target);

    let row_box = gtk::Box::builder()
        .spacing(8)
        .margin_top(5)
        .margin_bottom(5)
        .margin_start(20)
        .margin_end(6)
        .build();
    row_box.append(&gtk::Image::from_icon_name(folder_icon(&folder.display_name)));
    let name = gtk::Label::builder()
        .label(&folder.display_name)
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(EllipsizeMode::End)
        .build();
    row_box.append(&name);

    // A stage shows how many tickets sit in it; unread has no meaning here.
    if tickets {
        if folder.total_count > 0 {
            let badge = gtk::Label::new(Some(&folder.total_count.to_string()));
            badge.add_css_class("count-muted");
            row_box.append(&badge);
        }
        row.set_child(Some(&row_box));
        return row;
    }

    let bracketed = BRACKET_COUNT.contains(&folder.display_name.as_str());
    if bracketed && folder.total_count > 0 {
        let badge = gtk::Label::new(Some(&format!("[{}]", folder.total_count)));
        badge.add_css_class("count-muted");
        row_box.append(&badge);
    } else if !bracketed && folder.unread_count > 0 {
        name.add_css_class("folder-unread");
        let badge = gtk::Label::new(Some(&folder.unread_count.to_string()));
        badge.add_css_class("count-unread");
        row_box.append(&badge);
    }
    row.set_child(Some(&row_box));
    row
}

// -- message list -------------------------------------------------------

pub fn reload_messages(state: &Rc<State>) {
    let Some((session_index, folder_id)) = state.current.borrow().clone() else {
        state.folder_title.set_text("");
        state.count_label.set_text("");
        return;
    };
    let Some(db) = state.sessions.borrow().get(session_index).map(|s| s.db.clone()) else { return };
    let query = state.search.text().to_string();
    let Ok(mut messages) = db.messages(&folder_id, &query) else { return };
    if state.unread_only.get() {
        messages.retain(|m| !m.is_read);
    }
    if !state.newest_first.get() {
        messages.reverse();
    }

    let folder = db.folders().unwrap_or_default().into_iter().find(|f| f.id == folder_id);
    if let Some(folder) = &folder {
        state.folder_title.set_text(&folder.display_name);
    }

    let current = state.current_message.borrow().clone();
    state.rebuilding.set(true);
    while let Some(row) = state.message_list.row_at_index(0) {
        state.message_list.remove(&row);
    }

    let mut entries: Vec<ListEntry> = Vec::new();
    let mut select_index: Option<i32> = None;
    let mut bucket = String::new();
    for message in messages {
        let this_bucket = date_bucket(&message.received);
        if this_bucket != bucket {
            bucket = this_bucket.clone();
            state.message_list.append(&date_header_row(&bucket));
            entries.push(ListEntry::DateHeader);
        }
        state.message_list.append(&message_row(&message, session_index));
        if current.as_deref() == Some(message.id.as_str()) {
            select_index = Some(entries.len() as i32);
        }
        entries.push(ListEntry::Message(message));
    }

    let shown = entries.iter().filter(|e| matches!(e, ListEntry::Message(_))).count();
    *state.entries.borrow_mut() = entries;

    // Putting the selection back is not the user opening the message, so it
    // stays inside the rebuild: opening marks a message read, which would
    // undo "mark as unread" the moment the list refreshed.
    match select_index {
        Some(index) => {
            if let Some(row) = state.message_list.row_at_index(index) {
                state.message_list.select_row(Some(&row));
            }
        }
        None => {
            if state.current_message.borrow().is_some() {
                show_message(state, None);
            }
        }
    }
    state.rebuilding.set(false);

    let unread = folder.as_ref().map(|f| f.unread_count).unwrap_or(0);
    let total = folder.as_ref().map(|f| f.total_count).unwrap_or(shown as i64);
    state.count_label.set_text(&format!("Items: {total}    Unread: {unread}"));
}

fn date_header_row(title: &str) -> gtk::ListBoxRow {
    let row = gtk::ListBoxRow::new();
    row.set_selectable(false);
    row.set_activatable(false);
    let label = gtk::Label::builder()
        .label(title)
        .xalign(0.0)
        .margin_top(8)
        .margin_bottom(4)
        .margin_start(12)
        .build();
    label.add_css_class("date-header");
    row.set_child(Some(&label));
    row
}

/// Payload carried by a drag: which mailbox the message belongs to, and
/// its id. The mailbox travels with it because Graph cannot move a message
/// between mailboxes, so such a drop has to be refused rather than fail
/// halfway.
fn drag_payload(session: usize, message_id: &str) -> String {
    format!("{session}\n{message_id}")
}

fn parse_drag_payload(payload: &str) -> Option<(usize, &str)> {
    let (session, id) = payload.split_once('\n')?;
    Some((session.parse().ok()?, id))
}

fn message_row(message: &MessageSummary, session: usize) -> gtk::ListBoxRow {
    let row = gtk::ListBoxRow::new();

    // Drag a message onto a folder to move it.
    let payload = drag_payload(session, &message.id);
    let source = gtk::DragSource::builder().actions(gdk::DragAction::MOVE).build();
    source.connect_prepare(move |_, _, _| {
        Some(gdk::ContentProvider::for_value(&payload.to_value()))
    });
    row.add_controller(source);

    let row_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
    row_box.add_css_class("message-row");
    if !message.is_read {
        row_box.add_css_class("unread");
    }

    let line = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let sender = gtk::Label::builder()
        .label(message.from.display())
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(EllipsizeMode::End)
        .build();
    sender.add_css_class("m-sender");
    line.append(&sender);
    if matches!(message.pending, Pending::Queued) {
        let chip = gtk::Label::new(Some("Queued"));
        chip.add_css_class("queued-chip");
        line.append(&chip);
    }
    if message.has_attachments {
        line.append(&gtk::Image::from_icon_name("mail-attachment-symbolic"));
    }
    let time = gtk::Label::new(Some(&fmt_time(&message.received)));
    time.add_css_class("m-time");
    line.append(&time);

    let subject = gtk::Label::builder()
        .label(&message.subject)
        .xalign(0.0)
        .ellipsize(EllipsizeMode::End)
        .build();
    subject.add_css_class("m-subject");
    let preview = gtk::Label::builder()
        .label(&message.preview)
        .xalign(0.0)
        .ellipsize(EllipsizeMode::End)
        .build();
    preview.add_css_class("m-preview");

    row_box.append(&line);
    row_box.append(&subject);
    row_box.append(&preview);
    row.set_child(Some(&row_box));
    row
}

// -- reading pane -------------------------------------------------------

fn open_message(state: &Rc<State>, summary: &MessageSummary) {
    let session_index = state.active_session();
    let Some(db) = state.sessions.borrow().get(session_index).map(|s| s.db.clone()) else { return };
    let Ok(Some(detail)) = db.message(&summary.id) else { return };
    show_message(state, Some(&detail));

    if detail.body.is_none() {
        if let Some(session) = state.sessions.borrow().get(session_index) {
            session.send(Cmd::OpenMessage(summary.id.clone()));
        }
    }
    if !detail.summary.is_read {
        let op = Op::MarkRead { message_id: summary.id.clone(), is_read: true };
        let applied =
            state.sessions.borrow().get(session_index).map(|s| s.apply(op)).unwrap_or(Ok(()));
        if applied.is_ok() {
            reload_messages(state);
            reload_folders(state, false);
            update_read_toggle(state, true);
        }
    }
}

fn show_message(state: &Rc<State>, detail: Option<&MessageDetail>) {
    let Some(detail) = detail else {
        *state.current_message.borrow_mut() = None;
        state.reading.set_visible_child_name("empty");
        return;
    };
    *state.current_message.borrow_mut() = Some(detail.summary.id.clone());
    state.subject_label.set_text(&detail.summary.subject);

    let display = detail.summary.from.display().to_string();
    state.avatar.set_text(&initials(&display));
    for class in state.avatar.css_classes() {
        if class.starts_with("avatar-") {
            state.avatar.remove_css_class(&class);
        }
    }
    state.avatar.add_css_class(&avatar_class(&detail.summary.from.address));

    state.from_label.set_markup(&format!(
        "<b>{}</b>  <span alpha='70%'>&lt;{}&gt;</span>",
        glib::markup_escape_text(&display),
        glib::markup_escape_text(&detail.summary.from.address)
    ));
    let mut recipients: String =
        detail.to.iter().map(|a| a.display().to_string()).collect::<Vec<_>>().join(", ");
    if recipients.is_empty() {
        recipients = "—".into();
    }
    if !detail.cc.is_empty() {
        recipients.push_str("   Cc: ");
        recipients.push_str(
            &detail.cc.iter().map(|a| a.display().to_string()).collect::<Vec<_>>().join(", "),
        );
    }
    state.to_label.set_text(&format!("To: {recipients}"));
    state.date_label.set_text(&fmt_full_time(&detail.summary.received));

    let queued = matches!(detail.summary.pending, Pending::Queued);
    state.pending_label.set_visible(queued);
    if queued {
        state.pending_label.set_text("Waiting to send — will go out when you're online");
    }

    update_read_toggle(state, detail.summary.is_read);
    render_body(state, detail);
    state.reading.set_visible_child_name("message");
}

fn render_body(state: &Rc<State>, detail: &MessageDetail) {
    let dark = adw::StyleManager::default().is_dark();
    let (is_html, content) = match &detail.body {
        Some(body) => (body.is_html, body.content.clone()),
        None => {
            let online =
                state.statuses.borrow().get(state.active_session()).map(|s| s.online).unwrap_or(true);
            let notice = if online {
                "<p style='color:#888'>Downloading message…</p>"
            } else {
                "<p style='color:#888'>This message hasn't been downloaded yet.<br>\
                 It will appear once you're back online.</p>"
            };
            (true, notice.to_string())
        }
    };
    #[cfg(feature = "html-view")]
    state.webview.load_html(&wrap_body(is_html, &content, dark), None);
    #[cfg(not(feature = "html-view"))]
    {
        let _ = dark;
        let text = if is_html { crate::util::html_to_text(&content) } else { content };
        state.body_label.set_text(&text);
    }
}

fn update_read_toggle(state: &Rc<State>, is_read: bool) {
    if is_read {
        state.read_toggle.set_icon_name("mail-unread-symbolic");
        state.read_toggle.set_tooltip_text(Some("Mark as unread"));
    } else {
        state.read_toggle.set_icon_name("mail-read-symbolic");
        state.read_toggle.set_tooltip_text(Some("Mark as read"));
    }
}

pub fn render_status(state: &Rc<State>) {
    let index = state.active_session();
    let statuses = state.statuses.borrow();
    let status = statuses.get(index).cloned().unwrap_or_default();
    let demo = state.sessions.borrow().get(index).map(Session::is_demo).unwrap_or(true);

    let text = if demo {
        "Demo mailbox — not connected".to_string()
    } else if status.syncing {
        "Syncing…".to_string()
    } else if !status.online {
        match status.queued {
            0 => "Offline — showing cached mail".to_string(),
            n => format!("Offline — {n} waiting to sync"),
        }
    } else if status.queued > 0 {
        format!("{} waiting to sync", status.queued)
    } else {
        match status.last_sync {
            Some(when) => format!("All folders are up to date · updated {}", fmt_since(when)),
            None => "Connected to Microsoft 365".to_string(),
        }
    };
    state.connection_label.set_text(&text);
    state.connection_label.set_tooltip_text(status.detail.as_deref());
}

pub fn toast(state: &Rc<State>, message: &str) {
    state.toasts.add_toast(adw::Toast::builder().title(message).timeout(4).build());
}

// -- actions ------------------------------------------------------------

fn current_detail(state: &Rc<State>) -> Option<MessageDetail> {
    let id = state.current_message.borrow().clone()?;
    let db = state.sessions.borrow().get(state.active_session()).map(|s| s.db.clone())?;
    db.message(&id).ok().flatten()
}

fn respond(state: &Rc<State>, mode: SendMode) {
    if let Some(detail) = current_detail(state) {
        ComposeWindow::open(state, state.active_session(), Some((detail, mode)));
    }
}

/// Move a dragged message into a folder of the same mailbox.
fn drop_message_into(
    state: &Rc<State>,
    session: usize,
    message_id: &str,
    folder_id: &str,
    folder_name: &str,
) {
    let Some(db) = state.sessions.borrow().get(session).map(|s| s.db.clone()) else { return };
    let already = db.message(message_id).ok().flatten().map(|m| m.summary.folder_id);
    if already.as_deref() == Some(folder_id) {
        return;
    }
    let op = Op::Move { message_id: message_id.to_string(), folder_id: folder_id.to_string() };
    let applied = state.sessions.borrow().get(session).map(|s| s.apply(op)).unwrap_or(Ok(()));
    match applied {
        Ok(()) => {
            if state.current_message.borrow().as_deref() == Some(message_id) {
                show_message(state, None);
            }
            toast(state, &format!("Moved to {folder_name}"));
        }
        Err(e) => toast(state, &e.to_string()),
    }
    reload_messages(state);
    reload_folders(state, false);
    render_status(state);
}

fn apply_op(state: &Rc<State>, op: Op) -> anyhow::Result<()> {
    let index = state.active_session();
    let sessions = state.sessions.borrow();
    match sessions.get(index) {
        Some(session) => session.apply(op),
        None => Ok(()),
    }
}

fn delete_current(state: &Rc<State>) {
    let Some(detail) = current_detail(state) else { return };
    let db = state.sessions.borrow().get(state.active_session()).map(|s| s.db.clone());
    let purge = match (db.as_ref().and_then(|db| db.folder_id_by_name("Deleted Items")), &detail) {
        (Some(bin), detail) => detail.summary.folder_id == bin,
        (None, _) => true,
    };
    match apply_op(state, Op::Delete { message_id: detail.summary.id.clone(), purge }) {
        Ok(()) => {
            show_message(state, None);
            toast(state, if purge { "Deleted" } else { "Moved to Deleted Items" });
        }
        Err(e) => toast(state, &e.to_string()),
    }
    reload_messages(state);
    reload_folders(state, false);
    render_status(state);
}

fn archive_current(state: &Rc<State>) {
    let Some(detail) = current_detail(state) else { return };
    let Some(db) = state.sessions.borrow().get(state.active_session()).map(|s| s.db.clone()) else {
        return;
    };
    let Some(archive) = db.folder_id_by_name("Archive") else {
        toast(state, "This mailbox has no Archive folder.");
        return;
    };
    if detail.summary.folder_id == archive {
        toast(state, "Already in Archive");
        return;
    }
    // Queued like any other change, so it reaches the server and survives
    // being made offline.
    match apply_op(state, Op::Move { message_id: detail.summary.id.clone(), folder_id: archive }) {
        Ok(()) => {
            show_message(state, None);
            toast(state, "Moved to Archive");
        }
        Err(e) => toast(state, &e.to_string()),
    }
    reload_messages(state);
    reload_folders(state, false);
    render_status(state);
}

fn toggle_read_current(state: &Rc<State>) {
    let Some(detail) = current_detail(state) else { return };
    let target = !detail.summary.is_read;
    if let Err(e) = apply_op(state, Op::MarkRead { message_id: detail.summary.id, is_read: target })
    {
        toast(state, &e.to_string());
        return;
    }
    update_read_toggle(state, target);
    reload_messages(state);
    reload_folders(state, false);
    render_status(state);
}

// -- account management -------------------------------------------------

/// Add a freshly signed-in mailbox without disturbing the others.
pub fn add_account(state: &Rc<State>, account: AccountInfo) {
    let username = account.username.clone();
    let session =
        match Session::new(Mode::Account(account), state.auth.clone(), state.http.clone()) {
            Ok(session) => session,
            Err(e) => {
                toast(state, &format!("Could not open the mail cache: {e}"));
                return;
            }
        };

    // Signing in again with a mailbox that is already open — to grant a new
    // permission, say — replaces it. Pushing would list the same mailbox
    // twice and run two sync engines against one cache.
    let existing = state
        .sessions
        .borrow()
        .iter()
        .position(|s| s.account().map(|a| a.username == username).unwrap_or(false));
    if let Some(index) = existing {
        // Dropping the old session closes its command channel, which stops
        // its engine and ends the listener spawned for that slot.
        state.sessions.borrow_mut()[index] = session;
        if let Some(slot) = state.statuses.borrow_mut().get_mut(index) {
            *slot = Status::default();
        }
        listen(state, index);
        if let Some(session) = state.sessions.borrow().get(index) {
            session.send(Cmd::SyncAll(None));
        }
        reload_folders(state, false);
        render_status(state);
        toast(state, &format!("Reconnected {username}"));
        return;
    }

    // The demo mailbox steps aside as soon as a real one is connected.
    let replacing_demo = state.sessions.borrow().iter().all(Session::is_demo);
    if replacing_demo {
        state.sessions.borrow_mut().clear();
        state.statuses.borrow_mut().clear();
        *state.current.borrow_mut() = None;
        show_message(state, None);
    }
    state.sessions.borrow_mut().push(session);
    state.statuses.borrow_mut().push(Status::default());
    let index = state.sessions.borrow().len() - 1;
    listen(state, index);
    if let Some(session) = state.sessions.borrow().get(index) {
        session.send(Cmd::SyncAll(None));
    }
    reload_folders(state, true);
    render_status(state);
    toast(state, &format!("Added {username}"));
}

/// Open sessions for the ticket pipelines just chosen, replacing any that
/// are already open so re-adding does not duplicate them.
pub fn add_ticket_pipelines(state: &Rc<State>, pipelines: &[crate::config::PipelineRef]) {
    state.sessions.borrow_mut().retain(|s| !s.is_tickets());
    let keep = state.sessions.borrow().len();
    state.statuses.borrow_mut().truncate(keep);
    *state.ticket_selection.borrow_mut() = None;

    for pipeline in pipelines {
        let mode = Mode::Tickets {
            pipeline_id: pipeline.id.clone(),
            pipeline_label: pipeline.label.clone(),
        };
        match Session::new(mode, state.auth.clone(), state.http.clone()) {
            Ok(session) => {
                state.sessions.borrow_mut().push(session);
                state.statuses.borrow_mut().push(Status::default());
                let index = state.sessions.borrow().len() - 1;
                listen(state, index);
                if let Some(session) = state.sessions.borrow().get(index) {
                    session.send(Cmd::SyncAll(None));
                }
            }
            Err(e) => toast(state, &format!("Could not open {}: {e}", pipeline.label)),
        }
    }
    set_scope(state, Scope::Tickets);
    reload_folders(state, true);
    toast(state, &format!("Added {} ticket pipeline(s)", pipelines.len()));
}

/// Drop the mailbox whose folder is currently selected.
pub fn remove_active_account(state: &Rc<State>) -> Option<String> {
    let index = state.active_session();
    let username = {
        let sessions = state.sessions.borrow();
        let session = sessions.get(index)?;
        session.account()?.username.clone()
    };
    state.sessions.borrow_mut().remove(index);
    state.statuses.borrow_mut().remove(index);
    *state.current.borrow_mut() = None;
    *state.current_message.borrow_mut() = None;
    show_message(state, None);

    if state.sessions.borrow().is_empty() {
        if let Ok(session) = Session::new(Mode::Demo, state.auth.clone(), state.http.clone()) {
            state.sessions.borrow_mut().push(session);
            state.statuses.borrow_mut().push(Status::default());
            listen(state, 0);
        }
    }
    reload_folders(state, true);
    render_status(state);
    Some(username)
}

/// Runtime handle for dialogs that need to run async work.
pub fn rt() -> &'static tokio::runtime::Runtime {
    runtime()
}

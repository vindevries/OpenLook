//! Compose window: new message, reply, reply all and forward.

use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use crate::model::{MessageDetail, Op, Outgoing, SendMode};
use crate::util::{fmt_full_time, quoted_original};
use crate::ui::widgets::{EntryRow, ToolbarView};
use crate::ui::window::{self, State};

pub struct ComposeWindow;

impl ComposeWindow {
    /// `respond_to` carries the message being answered and how.
    fn quote_source(detail: &MessageDetail) -> String {
        match &detail.body {
            Some(body) if body.is_html => crate::util::html_to_text(&body.content),
            Some(body) => body.content.clone(),
            // Not downloaded — the preview is all the cache holds.
            None => detail.summary.preview.clone(),
        }
    }

    pub fn open(
        state: &Rc<State>,
        session_index: usize,
        respond_to: Option<(MessageDetail, SendMode)>,
    ) {
        let mode = respond_to.as_ref().map(|(_, mode)| *mode).unwrap_or(SendMode::New);
        let original = respond_to.map(|(detail, _)| detail);

        let title = match mode {
            SendMode::New => "New message",
            SendMode::Reply => "Reply",
            SendMode::ReplyAll => "Reply all",
            SendMode::Forward => "Forward",
        };
        let window = adw::Window::builder()
            .transient_for(&state.window)
            .modal(false)
            .default_width(700)
            .default_height(580)
            .title(title)
            .build();

        let view = ToolbarView::new();
        let header = adw::HeaderBar::new();
        let send_button = gtk::Button::with_label("Send");
        send_button.add_css_class("suggested-action");
        header.pack_start(&send_button);
        view.add_top_bar(&header);

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();

        let fields = gtk::ListBox::new();
        fields.set_selection_mode(gtk::SelectionMode::None);
        fields.add_css_class("boxed-list");
        let to_row = EntryRow::new("To");
        let cc_row = EntryRow::new("Cc");
        let subject_row = EntryRow::new("Subject");

        if let Some(original) = &original {
            let subject = original.summary.subject.clone();
            let lower = subject.to_lowercase();
            match mode {
                SendMode::Reply => {
                    to_row.set_text(&original.summary.from.address);
                    to_row.set_sensitive(false);
                    cc_row.set_visible(false);
                }
                SendMode::ReplyAll => {
                    to_row.set_text(&original.summary.from.address);
                    to_row.set_sensitive(false);
                    // Everyone else on the thread stays in the loop.
                    let others: Vec<String> = original
                        .to
                        .iter()
                        .chain(original.cc.iter())
                        .map(|a| a.address.clone())
                        .filter(|a| !a.is_empty() && *a != original.summary.from.address)
                        .collect();
                    cc_row.set_text(&others.join(", "));
                }
                SendMode::Forward => {
                    cc_row.set_visible(false);
                }
                SendMode::New => {}
            }
            let prefix = if matches!(mode, SendMode::Forward) { "FW: " } else { "RE: " };
            let already = lower.starts_with("re:") || lower.starts_with("fw:");
            subject_row.set_text(&if already { subject } else { format!("{prefix}{subject}") });
            subject_row.set_sensitive(false);
        }
        fields.append(to_row.row());
        fields.append(cc_row.row());
        fields.append(subject_row.row());
        content.append(&fields);

        let body_view = gtk::TextView::builder()
            .wrap_mode(gtk::WrapMode::WordChar)
            .top_margin(8)
            .bottom_margin(8)
            .left_margin(8)
            .right_margin(8)
            .build();
        // Quote what is being answered or passed on, the way Outlook does,
        // so it can be read and trimmed before it goes out.
        if let Some(original) = &original {
            if !matches!(mode, SendMode::New) {
                let names = |list: &[crate::model::Address]| {
                    list.iter().map(|a| a.display().to_string()).collect::<Vec<_>>().join(", ")
                };
                let text = quoted_original(
                    matches!(mode, SendMode::Forward),
                    &format!(
                        "{} <{}>",
                        original.summary.from.display(),
                        original.summary.from.address
                    ),
                    &fmt_full_time(&original.summary.received),
                    &names(&original.to),
                    &names(&original.cc),
                    &original.summary.subject,
                    &Self::quote_source(original),
                );
                let buffer = body_view.buffer();
                buffer.set_text(&text);
                // Start where the reply gets typed, above the quote.
                buffer.place_cursor(&buffer.start_iter());
            }
        }

        let body_scroll = gtk::ScrolledWindow::builder().child(&body_view).vexpand(true).build();
        content.append(&gtk::Frame::builder().child(&body_scroll).build());

        let error_label = gtk::Label::builder().xalign(0.0).wrap(true).visible(false).build();
        error_label.add_css_class("error");
        content.append(&error_label);

        view.set_content(Some(&content));
        window.set_content(Some(view.widget()));

        let original_id = original.as_ref().map(|d| d.summary.id.clone());
        let state = state.clone();
        let win = window.clone();
        let to_entry = to_row.clone();
        let cc_entry = cc_row.clone();
        let subject_entry = subject_row.clone();
        let body_entry = body_view.clone();
        let error = error_label.clone();
        send_button.connect_clicked(move |_| {
            let split = |text: String| -> Vec<String> {
                text.replace(';', ",")
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            };
            let to = split(to_entry.text().to_string());
            let cc = split(cc_entry.text().to_string());
            // A forward needs recipients typed in; replies already have them.
            if to.is_empty() && matches!(mode, SendMode::New | SendMode::Forward) {
                error.set_text("Add at least one recipient.");
                error.set_visible(true);
                return;
            }
            let buffer = body_entry.buffer();
            let body = buffer.text(&buffer.start_iter(), &buffer.end_iter(), true).to_string();
            let subject = {
                let s = subject_entry.text().to_string();
                if s.trim().is_empty() { "(no subject)".to_string() } else { s }
            };

            let message = Outgoing {
                to,
                cc,
                subject,
                body,
                in_reply_to: original_id.clone(),
                mode,
            };
            let op =
                Op::Send { local_id: format!("local:{}", glib::uuid_string_random()), message };
            let (result, queued, demo) = {
                let sessions = state.sessions.borrow();
                match sessions.get(session_index) {
                    Some(session) => {
                        (session.apply(op), session.queued_count(), session.is_demo())
                    }
                    None => (Ok(()), 0, true),
                }
            };
            match result {
                Ok(()) => {
                    window::reload_messages(&state);
                    window::reload_folders(&state, false);
                    window::render_status(&state);
                    let note = if demo {
                        "Message sent"
                    } else if queued > 0 {
                        "Message queued — it will send when you're online"
                    } else {
                        "Sending message…"
                    };
                    window::toast(&state, note);
                    win.close();
                }
                Err(e) => {
                    error.set_text(&e.to_string());
                    error.set_visible(true);
                }
            }
        });

        window.present();
        body_view.grab_focus();
    }
}

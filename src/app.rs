//! Application shell: styling and window creation.

use adw::prelude::*;
use gtk::gdk;

use crate::config::APP_ID;

/// Outlook's blue, rather than the desktop's accent colour (orange on
/// stock Ubuntu). The standalone variant is lightened for dark mode so
/// text keeps its contrast.
fn palette(dark: bool) -> String {
    let (background, standalone) =
        if dark { ("#2B88D8", "#78BEFA") } else { ("#0F6CBD", "#0F6CBD") };
    format!(
        "@define-color accent_bg_color {background};\
         @define-color accent_color {standalone};\
         @define-color accent_fg_color #ffffff;"
    )
}

const CSS: &str = r#"
/* Command bar, in the spirit of Outlook's ribbon */
.command-bar {
  border-bottom: 1px solid alpha(currentColor, 0.12);
}
.command-button {
  padding: 4px 10px;
  min-width: 44px;
}
.command-label {
  font-size: 0.78em;
}

/* Folder pane */
.folder-pane .pane-header {
  font-size: 0.82em;
  font-weight: 700;
  text-transform: uppercase;
  letter-spacing: 0.04em;
  opacity: 0.65;
}
.folder-pane .folder-unread {
  font-weight: 700;
}
.count-unread {
  color: @accent_color;
  font-weight: 700;
  font-size: 0.85em;
}
.count-muted {
  opacity: 0.55;
  font-size: 0.85em;
}

/* Message list */
.message-list .date-header {
  font-size: 0.82em;
  font-weight: 700;
  opacity: 0.7;
}
.message-row {
  padding: 8px 12px;
  border-left: 3px solid transparent;
}
.message-row.unread {
  border-left-color: @accent_bg_color;
}
.message-row.unread .m-sender {
  font-weight: 700;
}
.message-row.unread .m-subject {
  font-weight: 700;
  color: @accent_color;
}
.m-preview, .m-time {
  font-size: 0.85em;
  opacity: 0.65;
}
.thread-arrow {
  min-width: 20px;
  min-height: 20px;
  padding: 0;
  margin-right: 2px;
}
.message-row.thread-child {
  padding-left: 30px;
  border-left-color: alpha(currentColor, 0.15);
}
.filter-tab {
  padding: 2px 10px;
  font-size: 0.9em;
}

/* Reading pane */
.mail-subject {
  font-size: 1.35em;
  font-weight: 700;
}
.avatar {
  border-radius: 20px;
  font-weight: 700;
  color: #ffffff;
}
.avatar-0 { background: #C4314B; }
.avatar-1 { background: #0F6CBD; }
.avatar-2 { background: #107C41; }
.avatar-3 { background: #8764B8; }
.avatar-4 { background: #C55100; }
.avatar-5 { background: #00707F; }

.drop-hover {
  background: alpha(@accent_bg_color, 0.25);
  border-radius: 6px;
}

/* Calendar */
.view-rail button { min-width: 34px; padding: 6px; }
.cal-weekday {
  font-size: 0.8em;
  font-weight: 700;
  opacity: 0.6;
  padding: 4px 0;
}
.cal-day {
  border: 1px solid alpha(currentColor, 0.10);
  padding: 3px 4px;
}
.cal-outside { opacity: 0.45; }
.cal-today { background: alpha(@accent_bg_color, 0.10); }
.cal-selected { border: 2px solid @accent_bg_color; }
.cal-daynum { font-size: 0.85em; font-weight: 700; }
.cal-more { font-size: 0.75em; opacity: 0.6; }
.event-chip {
  font-size: 0.78em;
  border-radius: 4px;
  padding: 1px 5px;
  color: #ffffff;
}
.event-cancelled { text-decoration: line-through; opacity: 0.6; }
.mailbox-0 { background: #0F6CBD; }
.mailbox-1 { background: #107C41; }
.mailbox-2 { background: #8764B8; }
.mailbox-3 { background: #C55100; }
.mailbox-4 { background: #00707F; }
.mailbox-5 { background: #C4314B; }
.mailbox-text-0 { color: #0F6CBD; font-weight: 700; }
.mailbox-text-1 { color: #107C41; font-weight: 700; }
.mailbox-text-2 { color: #8764B8; font-weight: 700; }
.mailbox-text-3 { color: #C55100; font-weight: 700; }
.mailbox-text-4 { color: #00707F; font-weight: 700; }
.mailbox-text-5 { color: #C4314B; font-weight: 700; }

/* Status bar */
.status-bar {
  border-top: 1px solid alpha(currentColor, 0.12);
  opacity: 0.8;
}
.queued-chip {
  border-radius: 8px;
  padding: 0 6px;
  font-size: 0.8em;
  background: alpha(currentColor, 0.12);
}
"#;

pub fn run() -> glib::ExitCode {
    let app = adw::Application::builder().application_id(APP_ID).build();

    app.connect_startup(|_| {
        let provider = gtk::CssProvider::new();
        let style = adw::StyleManager::default();
        provider.load_from_data(&format!("{}{CSS}", palette(style.is_dark())));
        if let Some(display) = gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }
        // Follow the desktop switching between light and dark.
        style.connect_dark_notify(move |style| {
            provider.load_from_data(&format!("{}{CSS}", palette(style.is_dark())));
        });
    });

    app.connect_activate(|app| {
        if let Some(window) = app.active_window() {
            window.present();
            return;
        }
        crate::ui::window::build(app);
    });

    app.run()
}

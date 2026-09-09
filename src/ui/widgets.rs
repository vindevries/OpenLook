//! Small widget helpers that keep the UI buildable against the oldest
//! supported GTK/libadwaita (Ubuntu 22.04: GTK 4.6, libadwaita 1.1).
//!
//! `AdwEntryRow` (1.2), `AdwToolbarView` (1.4) and `AdwAboutWindow` (1.2) are
//! all newer than that, so the equivalents live here and are used everywhere;
//! they work unchanged on newer systems too.

use adw::prelude::*;
use gtk::glib;

/// A stand-in for `AdwEntryRow`: a small dim title above a frameless entry,
/// filling the width of a boxed list row. Cloning shares the widgets.
#[derive(Clone)]
pub struct EntryRow {
    row: gtk::ListBoxRow,
    entry: gtk::Entry,
}

impl EntryRow {
    pub fn new(title: &str) -> Self {
        let label = gtk::Label::builder().label(title).xalign(0.0).build();
        label.add_css_class("caption");
        label.add_css_class("dim-label");

        let entry = gtk::Entry::builder().hexpand(true).has_frame(false).build();
        // The entry supplies its own padding; drop it so the text lines up
        // with the title above it.
        entry.set_css_classes(&["flat"]);

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .margin_top(8)
            .margin_bottom(8)
            .margin_start(12)
            .margin_end(12)
            .build();
        content.append(&label);
        content.append(&entry);

        let row = gtk::ListBoxRow::builder().child(&content).activatable(false).build();

        // AdwEntryRow puts the cursor in the entry when the row is clicked
        // anywhere, including on the title; match that.
        let click = gtk::GestureClick::new();
        let target = entry.clone();
        click.connect_pressed(move |_, _, _, _| {
            target.grab_focus();
        });
        row.add_controller(click);

        Self { row, entry }
    }

    /// The widget to add to a `ListBox`, `PreferencesGroup` or `ExpanderRow`.
    pub fn row(&self) -> &gtk::ListBoxRow {
        &self.row
    }

    pub fn text(&self) -> glib::GString {
        self.entry.text()
    }

    /// Mask the contents, for keys and other secrets.
    pub fn set_secret(&self, secret: bool) {
        self.entry.set_visibility(!secret);
        if secret {
            self.entry.set_input_purpose(gtk::InputPurpose::Password);
        }
    }

    pub fn set_text(&self, text: &str) {
        self.entry.set_text(text);
    }

    /// Greys out the whole row, as `AdwEntryRow::set_sensitive` would.
    pub fn set_sensitive(&self, sensitive: bool) {
        self.row.set_sensitive(sensitive);
    }

    pub fn set_visible(&self, visible: bool) {
        self.row.set_visible(visible);
    }
}

/// A stand-in for `AdwToolbarView`: top bars, a content area that takes the
/// remaining height, then bottom bars, stacked in a vertical box.
pub struct ToolbarView {
    root: gtk::Box,
    top: gtk::Box,
    content: gtk::Box,
    bottom: gtk::Box,
}

impl ToolbarView {
    pub fn new() -> Self {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let top = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let bottom = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.set_vexpand(true);
        root.append(&top);
        root.append(&content);
        root.append(&bottom);
        Self { root, top, content, bottom }
    }

    pub fn add_top_bar(&self, widget: &impl IsA<gtk::Widget>) {
        self.top.append(widget);
    }

    pub fn set_content(&self, child: Option<&impl IsA<gtk::Widget>>) {
        while let Some(existing) = self.content.first_child() {
            self.content.remove(&existing);
        }
        if let Some(child) = child {
            self.content.append(child);
        }
    }

    pub fn add_bottom_bar(&self, widget: &impl IsA<gtk::Widget>) {
        self.bottom.append(widget);
    }

    /// The widget to hand to `Window::set_content`.
    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }
}

impl Default for ToolbarView {
    fn default() -> Self {
        Self::new()
    }
}

//! Account setup (first run and from the menu), settings and about.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use adw::prelude::*;
use gtk::{gdk, glib};

use crate::auth::DeviceFlow;
use crate::config::{Settings, DEFAULT_CLIENT_ID};
use crate::model::AccountInfo;
use crate::ui::widgets::{EntryRow, ToolbarView};
use crate::ui::window::{self, State};

/// Outlook-style account setup: a plain "Sign in" button, with the app
/// registration tucked away under Advanced for tenants that need it.
pub fn show_account_dialog(state: &Rc<State>, first_run: bool) {
    let dialog = adw::Window::builder()
        .transient_for(&state.window)
        .modal(true)
        .title(if first_run { "Add account" } else { "Add your account" })
        .default_width(520)
        .default_height(600)
        .build();

    let view = ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    let stack = gtk::Stack::builder().transition_type(gtk::StackTransitionType::Crossfade).build();

    // ---- page 1: welcome ------------------------------------------------
    let settings = Settings::load();
    let setup = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(14)
        .margin_top(20)
        .margin_bottom(24)
        .margin_start(28)
        .margin_end(28)
        .valign(gtk::Align::Center)
        .build();

    let icon = gtk::Image::from_icon_name("mail-unread-symbolic");
    icon.set_pixel_size(56);
    icon.add_css_class("dim-label");
    let title = gtk::Label::new(Some(if first_run {
        "Welcome to OpenLook"
    } else {
        "Add your account"
    }));
    title.add_css_class("title-1");
    let subtitle = gtk::Label::builder()
        .label(
            "Sign in with your Microsoft 365 account. Sign-in happens in your browser, \
             so your password and two-factor prompt stay with Microsoft.",
        )
        .wrap(true)
        .justify(gtk::Justification::Center)
        .build();
    subtitle.add_css_class("dim-label");
    setup.append(&icon);
    setup.append(&title);
    setup.append(&subtitle);

    let sign_in_button = gtk::Button::with_label("Sign in");
    sign_in_button.add_css_class("suggested-action");
    sign_in_button.add_css_class("pill");
    sign_in_button.set_halign(gtk::Align::Center);
    setup.append(&sign_in_button);

    let skip = gtk::Button::with_label(if first_run {
        "Use the demo mailbox for now"
    } else {
        "Cancel"
    });
    skip.add_css_class("flat");
    skip.set_halign(gtk::Align::Center);
    setup.append(&skip);

    let advanced_list = gtk::ListBox::new();
    advanced_list.set_selection_mode(gtk::SelectionMode::None);
    advanced_list.add_css_class("boxed-list");
    advanced_list.set_margin_top(10);
    let expander = adw::ExpanderRow::builder()
        .title("Advanced")
        .subtitle(
            "OpenLook signs in as Microsoft's public \"Graph Command Line Tools\" app. \
             If your organization blocks it, use your own app registration.",
        )
        .build();
    let client_id_row = EntryRow::new("Application (client) ID");
    client_id_row.set_text(&settings.client_id);
    let tenant_row = EntryRow::new("Tenant (organizations, consumers, or tenant ID)");
    tenant_row.set_text(&settings.tenant);
    expander.add_row(client_id_row.row());
    expander.add_row(tenant_row.row());
    advanced_list.append(&expander);
    setup.append(&advanced_list);
    stack.add_named(&setup, Some("setup"));

    // ---- page 2: device code -------------------------------------------
    let code_page = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(16)
        .margin_top(24)
        .margin_bottom(24)
        .margin_start(28)
        .margin_end(28)
        .valign(gtk::Align::Center)
        .build();
    let status = gtk::Label::builder()
        .label("Contacting Microsoft…")
        .wrap(true)
        .justify(gtk::Justification::Center)
        .build();
    status.add_css_class("dim-label");
    let code_label = gtk::Label::builder().selectable(true).build();
    code_label.add_css_class("title-1");
    let copy_button = gtk::Button::with_label("Copy code");
    copy_button.set_sensitive(false);
    let open_button = gtk::Button::with_label("Open sign-in page");
    open_button.add_css_class("suggested-action");
    open_button.set_sensitive(false);
    let buttons = gtk::Box::builder().spacing(12).halign(gtk::Align::Center).build();
    buttons.append(&copy_button);
    buttons.append(&open_button);
    let spinner = gtk::Spinner::builder().halign(gtk::Align::Center).build();
    spinner.start();
    let back_button = gtk::Button::with_label("Back");
    back_button.add_css_class("flat");
    back_button.set_halign(gtk::Align::Center);

    code_page.append(&status);
    code_page.append(&code_label);
    code_page.append(&buttons);
    code_page.append(&spinner);
    code_page.append(&back_button);
    stack.add_named(&code_page, Some("code"));

    view.set_content(Some(&stack));
    dialog.set_content(Some(view.widget()));

    // ---- behaviour ------------------------------------------------------
    let cancel = Arc::new(AtomicBool::new(false));
    let flow_uri: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));

    {
        let dialog = dialog.clone();
        skip.connect_clicked(move |_| {
            if first_run {
                // Remember the choice so we stop asking at every launch.
                let mut settings = Settings::load();
                settings.demo_ack = true;
                let _ = settings.save();
            }
            dialog.close();
        });
    }
    {
        let cancel = cancel.clone();
        dialog.connect_close_request(move |_| {
            cancel.store(true, Ordering::Relaxed);
            glib::Propagation::Proceed
        });
    }
    {
        let stack = stack.clone();
        let cancel = cancel.clone();
        let status = status.clone();
        let code_label = code_label.clone();
        let copy_button = copy_button.clone();
        let open_button = open_button.clone();
        back_button.connect_clicked(move |_| {
            // Abandon the in-flight poll and return to the first page.
            cancel.store(true, Ordering::Relaxed);
            code_label.set_text("");
            copy_button.set_sensitive(false);
            open_button.set_sensitive(false);
            status.remove_css_class("error");
            status.add_css_class("dim-label");
            stack.set_visible_child_name("setup");
        });
    }
    {
        let code_label = code_label.clone();
        copy_button.connect_clicked(move |_| {
            if let Some(display) = gdk::Display::default() {
                display.clipboard().set_text(&code_label.text());
            }
        });
    }
    {
        let dialog = dialog.clone();
        let flow_uri = flow_uri.clone();
        open_button.connect_clicked(move |_| {
            let uri = flow_uri.borrow().clone();
            let uri = if uri.is_empty() { "https://microsoft.com/devicelogin".to_string() } else { uri };
            // gtk::UriLauncher would be tidier but needs GTK 4.10; this works
            // back to 4.6 (Ubuntu 22.04).
            #[allow(deprecated)]
            gtk::show_uri(Some(&dialog), &uri, gdk::CURRENT_TIME);
        });
    }

    let state = state.clone();
    let dialog_for_sign_in = dialog.clone();
    sign_in_button.connect_clicked(move |_| {
        let mut settings = Settings::load();
        settings.client_id = client_id_row.text().trim().to_string();
        settings.tenant = tenant_row.text().trim().to_string();
        let _ = settings.save();

        cancel.store(false, Ordering::Relaxed);
        status.set_text("Contacting Microsoft…");
        status.remove_css_class("error");
        status.add_css_class("dim-label");
        spinner.start();
        stack.set_visible_child_name("code");

        let (tx, rx) = async_channel::bounded::<Result<DeviceFlow, String>>(1);
        let auth = state.auth.clone();
        window::rt().spawn(async move {
            let result = auth.lock().await.start_device_flow().await.map_err(|e| e.to_string());
            let _ = tx.send(result).await;
        });

        let state = state.clone();
        let dialog = dialog_for_sign_in.clone();
        let status = status.clone();
        let code_label = code_label.clone();
        let copy_button = copy_button.clone();
        let open_button = open_button.clone();
        let spinner = spinner.clone();
        let flow_uri = flow_uri.clone();
        let cancel = cancel.clone();
        glib::spawn_future_local(async move {
            let Ok(result) = rx.recv().await else { return };
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            let flow = match result {
                Ok(flow) => flow,
                Err(message) => {
                    spinner.stop();
                    status.set_text(&message);
                    status.remove_css_class("dim-label");
                    status.add_css_class("error");
                    return;
                }
            };
            code_label.set_text(&flow.user_code);
            copy_button.set_sensitive(true);
            open_button.set_sensitive(true);
            *flow_uri.borrow_mut() = flow.verification_uri.clone();
            status.set_text(&format!(
                "Go to {} in your browser and enter this code, then finish signing in \
                 (including your usual two-factor prompt). Waiting…",
                flow.verification_uri
            ));

            // Poll until the browser half completes.
            let (tx, rx) = async_channel::bounded::<Result<AccountInfo, String>>(1);
            let auth = state.auth.clone();
            let poll_cancel = cancel.clone();
            window::rt().spawn(async move {
                let result = auth
                    .lock()
                    .await
                    .poll_device_flow(&flow, poll_cancel)
                    .await
                    .map_err(|e| e.to_string());
                let _ = tx.send(result).await;
            });

            let Ok(result) = rx.recv().await else { return };
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            match result {
                Ok(account) => {
                    // A real account is configured again: prompt next time
                    // if it ever stops working.
                    let mut settings = Settings::load();
                    settings.demo_ack = false;
                    let _ = settings.save();
                    let username = account.username.clone();
                    let _ = username;
                    window::add_account(&state, account);
                    dialog.close();
                }
                Err(message) => {
                    spinner.stop();
                    status.set_text(&message);
                    status.remove_css_class("dim-label");
                    status.add_css_class("error");
                }
            }
        });
    });

    dialog.present();
}

/// Remove the mailbox whose folder is selected, leaving any others signed in.
pub fn sign_out(state: &Rc<State>) {
    let Some(username) = window::remove_active_account(state) else {
        window::toast(state, "Select a folder in the mailbox you want to remove.");
        return;
    };
    let auth = state.auth.clone();
    let removed_username = username.clone();
    window::rt().block_on(async move { auth.lock().await.remove(&removed_username) });

    // Falling back to the demo mailbox is a deliberate choice; don't nag.
    let no_accounts = window::rt()
        .block_on({
            let auth = state.auth.clone();
            async move { auth.lock().await.accounts().is_empty() }
        });
    if no_accounts {
        let mut settings = Settings::load();
        settings.demo_ack = true;
        let _ = settings.save();
    }
    window::toast(state, &format!("Removed {username}"));
}

pub fn show_settings(state: &Rc<State>) {
    let settings = Settings::load();
    let dialog = adw::PreferencesWindow::builder()
        .transient_for(&state.window)
        .modal(true)
        .title("Settings")
        .default_width(560)
        .default_height(420)
        .search_enabled(false)
        .build();

    let page = adw::PreferencesPage::new();
    let group = adw::PreferencesGroup::builder()
        .title("Microsoft account")
        .description(
            "By default OpenLook signs in as Microsoft's public \"Graph Command Line Tools\" \
             app, so no setup is needed. If your organization blocks it, register your own \
             app in Azure (see README.md) and paste its client ID here. Leave blank to \
             restore the default.",
        )
        .build();
    let client_id_row = EntryRow::new("Application (client) ID");
    client_id_row.set_text(&settings.client_id);
    let tenant_row = EntryRow::new("Tenant (organizations, consumers, or tenant ID)");
    tenant_row.set_text(&settings.tenant);
    group.add(client_id_row.row());
    group.add(tenant_row.row());
    page.add(&group);

    let cache_group = adw::PreferencesGroup::builder()
        .title("Offline")
        .description("Mail is cached on this computer so it can be read without a network.")
        .build();
    let db_row = adw::ActionRow::builder()
        .title("Cached mailbox")
        .subtitle(crate::config::data_dir().to_string_lossy().as_ref())
        .build();
    cache_group.add(&db_row);
    page.add(&cache_group);
    dialog.add(&page);

    dialog.connect_close_request(move |_| {
        let mut settings = Settings::load();
        settings.client_id = client_id_row.text().trim().to_string();
        if settings.client_id.is_empty() {
            settings.client_id = DEFAULT_CLIENT_ID.to_string();
        }
        settings.tenant = tenant_row.text().trim().to_string();
        let _ = settings.save();
        glib::Propagation::Proceed
    });
    dialog.present();
}

pub fn show_about(state: &Rc<State>) {
    // adw::AboutWindow needs libadwaita 1.2; this is the 1.1-era equivalent.
    gtk::AboutDialog::builder()
        .transient_for(&state.window)
        .modal(true)
        .program_name("OpenLook")
        .logo_icon_name("openlook")
        .version(env!("CARGO_PKG_VERSION"))
        .comments("An Outlook-style native mail client for Linux, with offline mail.")
        .authors(vec!["Vincent".to_string()])
        .license_type(gtk::License::Agpl30)
        .build()
        .present();
}

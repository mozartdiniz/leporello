//! Main window and controller.
//!
//! Layout follows Reminders.app: sidebar with smart-list tiles (Today,
//! Scheduled, All, Flagged, Urgent, Completed) and "My Lists"; content shows
//! the selected list, grouped by date or list.
//!
//! The controller lives for the whole process (closing the window only hides
//! it, so alerts keep firing). Network calls run on tokio through
//! [`crate::bg`] with a clone of the client and a copy of the store; results
//! come back to the main thread, which owns all UI and state.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;

use adw::prelude::*;
use chrono::{Local, NaiveDate, Utc};
use gtk::{gio, glib};
use remindkit::{Edits, Error, ICloudClient, List, Reminder, SignInOutcome, Store, TrustedPhone, Writer};
use serde_json::Value;

use crate::config::{self, State};
use crate::editor::{self, Action};
use crate::{bg, notifier, secrets};

const SYNC_INTERVAL_SECS: u32 = 60;
/// Completed reminders shown at most (the account has hundreds).
const COMPLETED_LIMIT: usize = 300;

#[derive(Clone, PartialEq, Debug)]
enum View {
    Today,
    Scheduled,
    All,
    Flagged,
    Urgent,
    Completed,
    List(String),
}

const SMART: [(View, &str, &str, &str); 6] = [
    (View::Today, "Today", "x-office-calendar-symbolic", "tile-today"),
    (View::Scheduled, "Scheduled", "document-open-recent-symbolic", "tile-scheduled"),
    (View::All, "All", "view-list-bullet-symbolic", "tile-all"),
    (View::Flagged, "Flagged", "emoji-flags-symbolic", "tile-flagged"),
    (View::Urgent, "Urgent", "alarm-symbolic", "tile-urgent"),
    (View::Completed, "Completed", "object-select-symbolic", "tile-completed"),
];

pub struct Inner {
    app: adw::Application,
    window: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    split: adw::NavigationSplitView,
    stack: gtk::Stack,
    tile_counts: Vec<gtk::Label>,
    tiles: Vec<gtk::ToggleButton>,
    lists_box: gtk::ListBox,
    content: gtk::Box,
    content_page: adw::NavigationPage,
    heading: gtk::Label,
    subheading: gtk::Label,
    search: gtk::SearchEntry,
    search_bar: gtk::SearchBar,
    refresh: gtk::Button,
    spinner: gtk::Spinner,

    client: RefCell<Option<ICloudClient>>,
    signed_in: Cell<bool>,
    store: RefCell<Option<Store>>,
    view: RefCell<View>,
    state: RefCell<State>,
    syncing: Cell<bool>,
    urgent_pending: RefCell<HashSet<String>>,
    phones: RefCell<Vec<TrustedPhone>>,
    progress: RefCell<Option<adw::Dialog>>,
    signin_toast: RefCell<Option<adw::Toast>>,
}

thread_local! {
    static CONTROLLER: RefCell<Option<Rc<Inner>>> = const { RefCell::new(None) };
}

pub fn controller() -> Option<Rc<Inner>> {
    CONTROLLER.with(|c| c.borrow().clone())
}

pub fn present(app: &adw::Application) {
    match controller() {
        Some(c) => c.window.present(),
        None => {
            start(app);
            present(app);
        }
    }
}

const CSS: &str = "
.tile { border-radius: 12px; padding: 8px 12px; min-height: 64px; color: white; }
.tile .count { font-size: 20pt; font-weight: 800; }
.tile .name { font-weight: 700; }
.tile-today { background: #3584e4; }
.tile-scheduled { background: #e66d6d; }
.tile-all { background: #5e5c64; }
.tile-flagged { background: #e5a50a; }
.tile-urgent { background: #d6577f; }
.tile-completed { background: #9a9996; }
.tile:checked { box-shadow: inset 0 0 0 3px alpha(white, .65); }
.big-title { font-size: 26pt; font-weight: 800; }
.section-title { font-size: 15pt; font-weight: 800; margin-top: 18px; }
.overdue { color: @error_color; }
.chip { background: alpha(currentColor, .08); border-radius: 8px; padding: 2px 8px; font-size: 9pt; }
.reminder-row { padding: 6px 4px; }
";

pub fn start(app: &adw::Application) {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(CSS);
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(&display, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    }

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Leporello")
        .default_width(1000)
        .default_height(720)
        .width_request(360)
        .height_request(400)
        .build();

    // ── Sidebar ──
    let tiles_grid = gtk::Grid::builder().row_spacing(8).column_spacing(8).column_homogeneous(true).build();
    let mut tiles = Vec::new();
    let mut tile_counts = Vec::new();
    for (i, (_, name, icon, class)) in SMART.iter().enumerate() {
        let top = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        let image = gtk::Image::from_icon_name(icon);
        image.set_pixel_size(20);
        image.set_halign(gtk::Align::Start);
        image.set_hexpand(true);
        let count = gtk::Label::new(Some("0"));
        count.add_css_class("count");
        top.append(&image);
        top.append(&count);
        let label = gtk::Label::new(Some(name));
        label.add_css_class("name");
        label.set_xalign(0.0);
        let inner = gtk::Box::new(gtk::Orientation::Vertical, 2);
        inner.append(&top);
        inner.append(&label);
        let tile = gtk::ToggleButton::new();
        tile.set_child(Some(&inner));
        tile.add_css_class("tile");
        tile.add_css_class(class);
        if let Some(first) = tiles.first() {
            tile.set_group(Some(first));
        }
        tiles_grid.attach(&tile, (i % 2) as i32, (i / 2) as i32, 1, 1);
        tiles.push(tile);
        tile_counts.push(count);
    }
    let lists_title = gtk::Label::new(Some("My Lists"));
    lists_title.add_css_class("heading");
    lists_title.add_css_class("dim-label");
    lists_title.set_xalign(0.0);
    lists_title.set_margin_top(18);
    lists_title.set_margin_start(6);
    let lists_box = gtk::ListBox::new();
    lists_box.add_css_class("navigation-sidebar");
    let sidebar_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
    sidebar_box.set_margin_top(6);
    sidebar_box.set_margin_bottom(12);
    sidebar_box.set_margin_start(12);
    sidebar_box.set_margin_end(12);
    sidebar_box.append(&tiles_grid);
    sidebar_box.append(&lists_title);
    sidebar_box.append(&lists_box);
    let sidebar_scroll = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).child(&sidebar_box).build();
    let sidebar_header = adw::HeaderBar::new();
    sidebar_header.set_show_title(false);
    let menu = gio::Menu::new();
    menu.append(Some("Refresh"), Some("win.refresh"));
    menu.append(Some("Sign Out of iCloud"), Some("win.sign-out"));
    menu.append(Some("Quit"), Some("app.quit"));
    let menu_button = gtk::MenuButton::builder().icon_name("open-menu-symbolic").menu_model(&menu).build();
    sidebar_header.pack_end(&menu_button);
    let sidebar_view = adw::ToolbarView::new();
    sidebar_view.add_top_bar(&sidebar_header);
    sidebar_view.set_content(Some(&sidebar_scroll));
    let sidebar_page = adw::NavigationPage::new(&sidebar_view, "Leporello");

    // ── Content ──
    let heading = gtk::Label::new(None);
    heading.add_css_class("big-title");
    heading.set_xalign(0.0);
    let subheading = gtk::Label::new(None);
    subheading.add_css_class("dim-label");
    subheading.set_xalign(0.0);
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let column = gtk::Box::new(gtk::Orientation::Vertical, 4);
    column.set_margin_top(6);
    column.set_margin_bottom(24);
    column.set_margin_start(24);
    column.set_margin_end(24);
    column.append(&heading);
    column.append(&subheading);
    column.append(&content);
    let clamp = adw::Clamp::builder().maximum_size(820).child(&column).build();
    let content_scroll = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).child(&clamp).build();

    let signin_button = gtk::Button::with_label("Sign In to iCloud");
    signin_button.add_css_class("suggested-action");
    signin_button.add_css_class("pill");
    signin_button.set_halign(gtk::Align::Center);
    let signin_page = adw::StatusPage::builder()
        .icon_name("network-server-symbolic")
        .title("Your iCloud Reminders")
        .description(
            "Leporello reads and creates reminders in your iCloud account. Your password goes \
             only to Apple; the session is kept in your system keyring.",
        )
        .child(&signin_button)
        .build();
    let loading = adw::StatusPage::builder().title("Loading Reminders…").build();
    let loading_spinner = gtk::Spinner::new();
    loading_spinner.start();
    loading_spinner.set_size_request(32, 32);
    loading.set_child(Some(&loading_spinner));

    let stack = gtk::Stack::new();
    stack.add_named(&content_scroll, Some("content"));
    stack.add_named(&signin_page, Some("signin"));
    stack.add_named(&loading, Some("loading"));
    stack.set_visible_child_name("loading");

    let search = gtk::SearchEntry::new();
    search.set_placeholder_text(Some("Search reminders"));
    let search_bar = gtk::SearchBar::builder().child(&adw::Clamp::builder().maximum_size(500).child(&search).build()).build();
    search_bar.connect_entry(&search);
    search_bar.set_key_capture_widget(Some(&window));

    let content_header = adw::HeaderBar::new();
    let add = gtk::Button::from_icon_name("list-add-symbolic");
    add.set_tooltip_text(Some("New Reminder (Ctrl+N)"));
    add.set_action_name(Some("win.new-reminder"));
    let refresh = gtk::Button::from_icon_name("view-refresh-symbolic");
    refresh.set_tooltip_text(Some("Refresh (Ctrl+R)"));
    refresh.set_action_name(Some("win.refresh"));
    let spinner = gtk::Spinner::new();
    let search_toggle = gtk::ToggleButton::builder().icon_name("system-search-symbolic").tooltip_text("Search (Ctrl+F)").build();
    search_toggle.bind_property("active", &search_bar, "search-mode-enabled").bidirectional().build();
    content_header.pack_start(&add);
    content_header.pack_end(&search_toggle);
    content_header.pack_end(&refresh);
    content_header.pack_end(&spinner);
    let content_view = adw::ToolbarView::new();
    content_view.add_top_bar(&content_header);
    content_view.add_top_bar(&search_bar);
    content_view.set_content(Some(&stack));
    let content_page = adw::NavigationPage::new(&content_view, "Today");

    let split = adw::NavigationSplitView::builder().sidebar(&sidebar_page).content(&content_page).build();
    split.set_min_sidebar_width(260.0);
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&split));
    window.set_content(Some(&toasts));
    let bp = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
        adw::BreakpointConditionLengthType::MaxWidth,
        620.0,
        adw::LengthUnit::Sp,
    ));
    bp.add_setter(&split, "collapsed", Some(&true.to_value()));
    window.add_breakpoint(bp);

    let inner = Rc::new(Inner {
        app: app.clone(),
        window: window.clone(),
        toasts,
        split,
        stack,
        tile_counts,
        tiles: tiles.clone(),
        lists_box: lists_box.clone(),
        content,
        content_page,
        heading,
        subheading,
        search: search.clone(),
        search_bar,
        refresh,
        spinner,
        client: RefCell::new(None),
        signed_in: Cell::new(false),
        store: RefCell::new(config::load_store()),
        view: RefCell::new(View::Today),
        state: RefCell::new(State::load()),
        syncing: Cell::new(false),
        urgent_pending: RefCell::new(HashSet::new()),
        phones: RefCell::new(Vec::new()),
        progress: RefCell::new(None),
        signin_toast: RefCell::new(None),
    });
    CONTROLLER.with(|c| c.replace(Some(inner.clone())));

    // ── Signals ──
    for (i, tile) in tiles.iter().enumerate() {
        tile.connect_toggled(move |t| {
            if t.is_active() {
                if let Some(c) = controller() {
                    c.lists_box.unselect_all();
                    c.select(SMART[i].0.clone());
                }
            }
        });
    }
    tiles[0].set_active(true);
    lists_box.connect_row_activated(|_, row| {
        let Some(c) = controller() else { return };
        let id = row.widget_name().to_string();
        for t in &c.tiles {
            t.set_active(false);
        }
        c.select(View::List(id));
    });
    search.connect_search_changed(|_| {
        if let Some(c) = controller() {
            c.render();
        }
    });
    signin_button.connect_clicked(|_| {
        if let Some(c) = controller() {
            c.show_sign_in(None);
        }
    });
    // Keyboard focus landing in the list selects its first row; the
    // highlight must follow the view instead.
    lists_box.connect_selected_rows_changed(|lb| {
        let Some(c) = controller() else { return };
        let want = match &*c.view.borrow() {
            View::List(id) => id.clone(),
            _ => String::new(),
        };
        if lb.selected_row().is_some_and(|r| r.widget_name() != want.as_str()) {
            lb.unselect_all();
        }
    });
    window.connect_close_request(|w| {
        // Keep running for alerts; Ctrl+Q quits.
        w.set_visible(false);
        glib::Propagation::Stop
    });

    let action = |name: &str, f: fn(&Rc<Inner>)| {
        let a = gio::SimpleAction::new(name, None);
        a.connect_activate(move |_, _| {
            if let Some(c) = controller() {
                f(&c);
            }
        });
        window.add_action(&a);
    };
    action("new-reminder", |c| c.new_reminder());
    action("refresh", |c| c.sync());
    action("search", |c| c.search_bar.set_search_mode(true));
    action("sign-out", |c| c.sign_out());
    let open = gio::SimpleAction::new("open-reminder", Some(glib::VariantTy::STRING));
    open.connect_activate(|_, p| {
        let (Some(c), Some(name)) = (controller(), p.and_then(|p| p.get::<String>())) else { return };
        c.window.present();
        c.open(&name);
    });
    app.add_action(&open);
    let complete = gio::SimpleAction::new("complete-reminder", Some(glib::VariantTy::STRING));
    complete.connect_activate(|_, p| {
        let (Some(c), Some(name)) = (controller(), p.and_then(|p| p.get::<String>())) else { return };
        c.set_completed(&name, true);
    });
    app.add_action(&complete);

    // ── Start ──
    if inner.store.borrow().is_some() {
        inner.stack.set_visible_child_name("content");
    }
    inner.render();
    inner.restore_session();
    glib::timeout_add_seconds_local(SYNC_INTERVAL_SECS, || {
        if let Some(c) = controller() {
            if c.signed_in.get() {
                c.sync();
            }
        }
        glib::ControlFlow::Continue
    });
    glib::timeout_add_seconds_local(notifier::INTERVAL_SECS, || {
        if let Some(c) = controller() {
            c.check_alerts();
        }
        glib::ControlFlow::Continue
    });
}

impl Inner {
    fn toast(&self, msg: &str) {
        self.toasts.add_toast(adw::Toast::new(msg));
    }

    fn lists(&self) -> Vec<List> {
        self.store.borrow().as_ref().map(Store::lists).unwrap_or_default()
    }

    fn reminders(&self) -> Vec<Reminder> {
        self.store.borrow().as_ref().map(Store::reminders).unwrap_or_default()
    }

    fn urgent(&self, r: &Reminder) -> Option<bool> {
        r.urgent_checksum.as_ref().and_then(|c| self.state.borrow().urgent.get(c).copied())
    }

    fn select(&self, view: View) {
        self.view.replace(view);
        self.split.set_show_content(true);
        self.render();
    }

    // ── Session ──

    fn restore_session(self: &Rc<Self>) {
        let this = self.clone();
        glib::spawn_future_local(async move {
            let Some(saved) = secrets::load_session().await else {
                this.show_signed_out();
                return;
            };
            let Ok(mut client) = ICloudClient::restore(&saved) else {
                this.show_signed_out();
                return;
            };
            let (client, res) = bg(async move {
                let r = client.validate().await;
                (client, r)
            })
            .await;
            match res {
                Ok(()) => {
                    this.client.replace(Some(client.clone()));
                    secrets::save_session(client.save()).await;
                    this.on_signed_in();
                }
                Err(Error::Http(e)) => {
                    // Offline: keep the client and the cached store; retry on the next sync.
                    log::warn!("session check failed: {e}");
                    this.client.replace(Some(client));
                    this.signed_in.set(true);
                    this.toast("Offline: showing saved reminders");
                }
                Err(e) => {
                    log::info!("saved session unusable: {e}");
                    // Keep it: its trust token can skip 2FA on the next sign-in.
                    this.client.replace(Some(client));
                    this.show_signed_out();
                }
            }
        });
    }

    fn show_signed_out(&self) {
        self.signed_in.set(false);
        if self.store.borrow().is_none() {
            self.stack.set_visible_child_name("signin");
        } else {
            let toast = adw::Toast::new("Not signed in to iCloud");
            toast.set_button_label(Some("Sign In"));
            toast.connect_button_clicked(|_| {
                if let Some(c) = controller() {
                    c.show_sign_in(None);
                }
            });
            toast.set_timeout(0);
            if let Some(old) = self.signin_toast.replace(Some(toast.clone())) {
                old.dismiss();
            }
            self.toasts.add_toast(toast);
        }
    }

    fn on_signed_in(self: &Rc<Self>) {
        self.signed_in.set(true);
        if self.store.borrow().is_none() {
            self.stack.set_visible_child_name("loading");
        }
        self.sync();
    }

    fn sign_out(self: &Rc<Self>) {
        let dialog = adw::AlertDialog::new(
            Some("Sign Out of iCloud?"),
            Some("Saved reminders are removed from this computer. Nothing changes in iCloud."),
        );
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("out", "Sign Out");
        dialog.set_response_appearance("out", adw::ResponseAppearance::Destructive);
        dialog.set_close_response("cancel");
        dialog.connect_response(Some("out"), |_, _| {
            let Some(c) = controller() else { return };
            if let Some(client) = c.client.borrow_mut().as_mut() {
                client.sign_out();
            }
            c.client.replace(None);
            c.store.replace(None);
            config::delete_store();
            glib::spawn_future_local(secrets::delete_session());
            c.render();
            c.show_signed_out();
        });
        dialog.present(Some(&self.window));
    }

    fn show_sign_in(self: &Rc<Self>, error: Option<String>) {
        self.window.present();
        let group = adw::PreferencesGroup::new();
        let id_row = adw::EntryRow::new();
        id_row.set_title("Apple ID");
        id_row.set_input_purpose(gtk::InputPurpose::Email);
        if let Some(id) = self.client.borrow().as_ref().and_then(|c| c.apple_id().map(str::to_string)) {
            id_row.set_text(&id);
        }
        let pw_row = adw::PasswordEntryRow::new();
        pw_row.set_title("Password");
        pw_row.set_activates_default(true);
        group.add(&id_row);
        group.add(&pw_row);

        let body = error.unwrap_or_else(|| {
            "Your password goes only to Apple; the sign-in session is kept in your system keyring.".into()
        });
        let dialog = adw::AlertDialog::new(Some("Sign In to iCloud"), Some(&body));
        dialog.set_extra_child(Some(&group));
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("signin", "Sign In");
        dialog.set_response_appearance("signin", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("signin"));
        dialog.set_close_response("cancel");
        let focus = if id_row.text().is_empty() { id_row.clone().upcast::<gtk::Widget>() } else { pw_row.clone().upcast() };
        dialog.connect_response(Some("signin"), move |_, _| {
            let Some(c) = controller() else { return };
            let apple_id = id_row.text().trim().to_string();
            let password = pw_row.text().to_string();
            if apple_id.is_empty() || password.is_empty() {
                c.show_sign_in(Some("Enter your Apple ID and password.".into()));
                return;
            }
            c.show_progress("Signing in to iCloud…");
            let mut client = c.client.borrow().clone().unwrap_or_default();
            glib::spawn_future_local(async move {
                let (client, res) = bg(async move {
                    let r = client.sign_in(&apple_id, &password).await;
                    (client, r)
                })
                .await;
                c.client.replace(Some(client));
                c.close_progress();
                match res {
                    Ok(SignInOutcome::SignedIn) => c.finish_sign_in().await,
                    Ok(SignInOutcome::NeedsCode { phones }) => {
                        c.phones.replace(phones);
                        c.show_code(None);
                    }
                    Err(e) => c.show_sign_in(Some(e.to_string())),
                }
            });
        });
        dialog.present(Some(&self.window));
        focus.grab_focus();
    }

    fn show_code(self: &Rc<Self>, message: Option<String>) {
        let phones = self.phones.borrow().clone();
        let entry = adw::EntryRow::new();
        entry.set_title("Verification Code");
        entry.set_input_purpose(gtk::InputPurpose::Digits);
        entry.set_activates_default(true);
        let group = adw::PreferencesGroup::new();
        group.add(&entry);

        let body = message.unwrap_or_else(|| "Enter the code shown on your iPhone, iPad or Mac.".into());
        let dialog = adw::AlertDialog::new(Some("Two-Factor Authentication"), Some(&body));
        dialog.set_extra_child(Some(&group));
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("resend", "Resend Code");
        for (i, p) in phones.iter().enumerate().take(2) {
            dialog.add_response(&format!("sms-{i}"), &format!("Text {}", p.number));
        }
        dialog.add_response("verify", "Verify");
        dialog.set_response_appearance("verify", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("verify"));
        dialog.set_close_response("cancel");
        let focus = entry.clone();
        dialog.connect_response(None, move |_, response| {
            let Some(c) = controller() else { return };
            let Some(mut client) = c.client.borrow().clone() else { return };
            let response = response.to_string();
            let code: String = entry.text().chars().filter(|ch| ch.is_ascii_digit()).collect();
            let phone = response.strip_prefix("sms-").and_then(|i| i.parse::<usize>().ok()).and_then(|i| phones.get(i).cloned());
            let label = match response.as_str() {
                "verify" if code.len() != 6 => {
                    c.show_code(Some("The code has 6 digits.".into()));
                    return;
                }
                "verify" => "Verifying…",
                "resend" => "Requesting a new code…",
                _ if phone.is_some() => "Sending SMS…",
                _ => return,
            };
            c.show_progress(label);
            glib::spawn_future_local(async move {
                let (client, res, step) = bg(async move {
                    let (r, step) = match (response.as_str(), phone) {
                        ("verify", _) => (client.submit_code(&code).await, "verify"),
                        ("resend", _) => (client.resend_code().await, "resend"),
                        (_, Some(p)) => (client.request_sms(p.id).await, "sms"),
                        _ => (Ok(()), "none"),
                    };
                    (client, r, step)
                })
                .await;
                c.client.replace(Some(client));
                c.close_progress();
                match (step, res) {
                    ("verify", Ok(())) => c.finish_sign_in().await,
                    ("verify", Err(Error::BadCode)) => c.show_code(Some("That code was incorrect. Try again.".into())),
                    ("sms", Ok(())) => c.show_code(Some("A code was sent by text message.".into())),
                    (_, Ok(())) => c.show_code(Some("A new code was sent to your devices.".into())),
                    (_, Err(e)) => c.show_sign_in(Some(e.to_string())),
                }
            });
        });
        dialog.present(Some(&self.window));
        focus.grab_focus();
    }

    async fn finish_sign_in(self: &Rc<Self>) {
        let Some(client) = self.client.borrow().clone() else { return };
        secrets::save_session(client.save()).await;
        if let Some(t) = self.signin_toast.take() {
            t.dismiss();
        }
        self.on_signed_in();
    }

    fn show_progress(&self, text: &str) {
        self.close_progress();
        let spinner = gtk::Spinner::new();
        spinner.start();
        let label = gtk::Label::new(Some(text));
        let content = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        content.set_margin_top(24);
        content.set_margin_bottom(24);
        content.set_margin_start(32);
        content.set_margin_end(32);
        content.append(&spinner);
        content.append(&label);
        let dialog = adw::Dialog::new();
        dialog.set_can_close(false);
        dialog.set_child(Some(&content));
        dialog.present(Some(&self.window));
        self.progress.replace(Some(dialog));
    }

    fn close_progress(&self) {
        if let Some(d) = self.progress.take() {
            d.force_close();
        }
    }

    // ── Sync and writes ──

    fn set_busy(&self, busy: bool) {
        self.syncing.set(busy);
        self.spinner.set_spinning(busy);
        self.spinner.set_visible(busy);
        self.refresh.set_visible(!busy);
    }

    fn sync(self: &Rc<Self>) {
        if self.syncing.get() {
            return;
        }
        let Some(client) = self.client.borrow().clone() else {
            self.show_signed_out();
            return;
        };
        self.set_busy(true);
        let store = self.store.borrow().clone();
        let this = self.clone();
        glib::spawn_future_local(async move {
            let res = bg(async move {
                let mut store = match store {
                    Some(s) => s,
                    None => Store::new(client.reminders_zone().await?),
                };
                client.sync(&mut store).await?;
                Ok::<_, Error>(store)
            })
            .await;
            this.set_busy(false);
            match res {
                Ok(store) => {
                    config::save_store(&store);
                    this.store.replace(Some(store));
                    this.signed_in.set(true);
                    this.stack.set_visible_child_name("content");
                    this.render();
                    this.resolve_urgent();
                    this.check_alerts();
                }
                Err(Error::SessionExpired | Error::NotSignedIn) => this.show_signed_out(),
                Err(Error::Http(e)) => log::warn!("sync failed (offline?): {e}"),
                Err(e) => this.toast(&format!("Couldn't sync: {e}")),
            }
        });
    }

    /// Run `ops` (built from the current store) and fold the result in.
    fn write(self: &Rc<Self>, ops: Vec<Value>, done: &'static str) {
        let (Some(client), Some(store)) = (self.client.borrow().clone(), self.store.borrow().clone()) else {
            self.show_sign_in(Some("Sign in to save to iCloud.".into()));
            return;
        };
        self.set_busy(true);
        let this = self.clone();
        glib::spawn_future_local(async move {
            let res = bg(async move {
                let mut store = store;
                client.write(&mut store, ops).await.map(|()| store)
            })
            .await;
            this.set_busy(false);
            match res {
                Ok(store) => {
                    config::save_store(&store);
                    this.store.replace(Some(store));
                    this.render();
                    if !done.is_empty() {
                        this.toast(done);
                    }
                }
                Err(Error::Conflict) => {
                    this.toast("It changed on another device. Refreshed; try again.");
                    this.sync();
                }
                Err(Error::SessionExpired) => this.show_signed_out(),
                Err(e) => this.toast(&format!("Couldn't save: {e}")),
            }
        });
    }

    fn writer<'a>(&self, store: &'a Store) -> Option<Writer<'a>> {
        let client = self.client.borrow();
        Some(Writer {
            zone_id: &store.zone_id,
            replica: client.as_ref()?.replica_uuid(),
            account_identifier: store.account_identifier(),
        })
    }

    fn new_reminder(self: &Rc<Self>) {
        let lists = self.lists();
        if lists.is_empty() {
            self.toast("No lists yet");
            return;
        }
        let default = match &*self.view.borrow() {
            View::List(id) => Some(id.clone()),
            _ => lists.iter().find(|l| l.name == "Reminders").map(|l| l.record_name.clone()),
        };
        let this = self.clone();
        editor::new_reminder(&self.window, &lists, default.as_deref(), move |draft| {
            let ops = {
                let store = this.store.borrow();
                let Some(store) = store.as_ref() else { return };
                let Some(w) = this.writer(store) else { return };
                w.create(&draft).1
            };
            this.write(ops, "Reminder added");
        });
    }

    fn set_completed(self: &Rc<Self>, record_name: &str, done: bool) {
        let ops = {
            let store = self.store.borrow();
            let Some(store) = store.as_ref() else { return };
            let (Some(record), Some(w)) = (store.record(record_name), self.writer(store)) else { return };
            vec![w.set_completed(record, done)]
        };
        self.app.withdraw_notification(record_name);
        self.write(ops, if done { "Completed" } else { "" });
    }

    fn delete(self: &Rc<Self>, record_name: &str) {
        let ops = {
            let store = self.store.borrow();
            let Some(store) = store.as_ref() else { return };
            let (Some(record), Some(w)) = (store.record(record_name), self.writer(store)) else { return };
            vec![w.delete(record)]
        };
        self.write(ops, "Reminder deleted");
    }

    fn edit(self: &Rc<Self>, record_name: &str, edits: &Edits) {
        let ops = {
            let store = self.store.borrow();
            let Some(store) = store.as_ref() else { return };
            let (Some(record), Some(w)) = (store.record(record_name), self.writer(store)) else { return };
            w.edit(store, record, edits)
        };
        match ops {
            Ok(ops) if ops.is_empty() => {}
            Ok(ops) => self.write(ops, "Saved"),
            Err(e) => self.toast(&format!("Couldn't edit: {e}")),
        }
    }

    fn open(self: &Rc<Self>, record_name: &str) {
        let Some(r) = self.reminders().into_iter().find(|r| r.record_name == record_name) else { return };
        let urgent = self.urgent(&r);
        let name = r.record_name.clone();
        let this = self.clone();
        editor::show_reminder(&self.window, &r, &self.lists(), urgent, move |action| match action {
            Action::Complete(done) => this.set_completed(&name, done),
            Action::Delete => this.delete(&name),
            Action::Save(edits) => this.edit(&name, &edits),
        });
    }

    /// Download urgent flags for open reminders whose asset changed.
    fn resolve_urgent(self: &Rc<Self>) {
        let Some(client) = self.client.borrow().clone() else { return };
        let todo: Vec<(String, String)> = self
            .reminders()
            .into_iter()
            .filter(|r| !r.completed)
            .filter_map(|r| Some((r.urgent_checksum.clone()?, r.urgent_asset.clone()?)))
            .filter(|(sum, _)| !self.state.borrow().urgent.contains_key(sum) && !self.urgent_pending.borrow().contains(sum))
            .collect();
        if todo.is_empty() {
            return;
        }
        self.urgent_pending.borrow_mut().extend(todo.iter().map(|(s, _)| s.clone()));
        let this = self.clone();
        glib::spawn_future_local(async move {
            let results = bg(async move {
                let mut out = Vec::new();
                for (sum, url) in todo {
                    match client.fetch_urgent(&url).await {
                        Ok(u) => out.push((sum, Some(u))),
                        Err(e) => {
                            log::warn!("urgent asset: {e}");
                            out.push((sum, None));
                        }
                    }
                }
                out
            })
            .await;
            {
                let mut state = this.state.borrow_mut();
                let mut pending = this.urgent_pending.borrow_mut();
                for (sum, urgent) in results {
                    pending.remove(&sum);
                    if let Some(u) = urgent {
                        state.urgent.insert(sum, u);
                    }
                }
                state.save();
            }
            this.render();
        });
    }

    fn check_alerts(&self) {
        let reminders = self.reminders();
        let mut state = self.state.borrow_mut();
        let urgent_map = state.urgent.clone();
        notifier::check(self.app.upcast_ref(), &reminders, &mut state, |r| {
            r.urgent_checksum.as_ref().and_then(|c| urgent_map.get(c)).copied().unwrap_or(false)
        });
    }

    // ── Rendering ──

    fn render(&self) {
        let reminders = self.reminders();
        let lists = self.lists();
        let today = Local::now().date_naive();
        let state = self.state.borrow();
        let is_urgent =
            |r: &Reminder| r.urgent_checksum.as_ref().and_then(|c| state.urgent.get(c)).copied().unwrap_or(false);

        // Counts.
        let open: Vec<&Reminder> = reminders.iter().filter(|r| !r.completed).collect();
        let counts = [
            open.iter().filter(|r| r.due.as_ref().is_some_and(|d| d.local_date() <= today)).count(),
            open.iter().filter(|r| r.due.is_some()).count(),
            open.len(),
            open.iter().filter(|r| r.flagged).count(),
            open.iter().filter(|r| is_urgent(r)).count(),
            reminders.len() - open.len(),
        ];
        for (label, n) in self.tile_counts.iter().zip(counts) {
            label.set_text(&n.to_string());
        }

        // Sidebar lists.
        let view = self.view.borrow().clone();
        while let Some(row) = self.lists_box.row_at_index(0) {
            self.lists_box.remove(&row);
        }
        for l in &lists {
            let n = open.iter().filter(|r| r.list.as_deref() == Some(&l.record_name)).count();
            let row = gtk::ListBoxRow::new();
            row.set_widget_name(&l.record_name);
            let b = gtk::Box::new(gtk::Orientation::Horizontal, 10);
            b.set_margin_top(6);
            b.set_margin_bottom(6);
            let dot = gtk::Label::new(None);
            dot.set_markup(&format!(
                "<span foreground=\"{}\" size=\"x-large\">●</span>",
                l.color.as_deref().unwrap_or("#3584e4")
            ));
            let name = gtk::Label::new(Some(&l.name));
            name.set_xalign(0.0);
            name.set_hexpand(true);
            name.set_ellipsize(gtk::pango::EllipsizeMode::End);
            let count = gtk::Label::new(Some(&n.to_string()));
            count.add_css_class("dim-label");
            b.append(&dot);
            b.append(&name);
            b.append(&count);
            row.set_child(Some(&b));
            self.lists_box.append(&row);
            if view == View::List(l.record_name.clone()) {
                self.lists_box.select_row(Some(&row));
            }
        }
        if !matches!(view, View::List(_)) {
            self.lists_box.unselect_all();
        }

        // Content.
        let query = self.search.text().to_lowercase();
        let matches = |r: &&Reminder| {
            query.is_empty() || r.title.to_lowercase().contains(&query) || r.notes.to_lowercase().contains(&query)
        };
        let list_name = |r: &Reminder| {
            r.list.as_ref().and_then(|id| lists.iter().find(|l| &l.record_name == id)).map(|l| l.name.clone()).unwrap_or_default()
        };
        let by_due = |a: &&Reminder, b: &&Reminder| {
            let key = |r: &Reminder| (r.due.as_ref().map(|d| d.at), r.created);
            match (key(a).0, key(b).0) {
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                _ => key(a).cmp(&key(b)),
            }
        };
        let date_label = |d: NaiveDate| match (d - today).num_days() {
            0 => "Today".to_string(),
            1 => "Tomorrow".to_string(),
            _ => d.format("%a %-d %b").to_string(),
        };

        let mut sections: Vec<(String, Vec<&Reminder>)> = Vec::new();
        let (title, color) = match &view {
            View::Today => {
                let mut items: Vec<&Reminder> =
                    open.iter().copied().filter(matches).filter(|r| r.due.as_ref().is_some_and(|d| d.local_date() <= today)).collect();
                items.sort_by(by_due);
                let (overdue, now): (Vec<_>, Vec<_>) =
                    items.into_iter().partition(|r| r.due.as_ref().is_some_and(|d| d.local_date() < today));
                if !overdue.is_empty() {
                    sections.push(("Overdue".into(), overdue));
                }
                sections.push((String::new(), now));
                ("Today".to_string(), "#3584e4")
            }
            View::Scheduled => {
                let mut items: Vec<&Reminder> = open.iter().copied().filter(matches).filter(|r| r.due.is_some()).collect();
                items.sort_by(by_due);
                for r in items {
                    let d = r.due.as_ref().map(|d| d.local_date()).unwrap_or(today);
                    let label = if d < today { "Overdue".to_string() } else { date_label(d) };
                    match sections.last_mut() {
                        Some((l, v)) if *l == label => v.push(r),
                        _ => sections.push((label, vec![r])),
                    }
                }
                ("Scheduled".to_string(), "#e66d6d")
            }
            View::All => {
                for l in &lists {
                    let mut items: Vec<&Reminder> =
                        open.iter().copied().filter(matches).filter(|r| r.list.as_deref() == Some(&l.record_name)).collect();
                    items.sort_by(by_due);
                    if !items.is_empty() {
                        sections.push((l.name.clone(), items));
                    }
                }
                ("All".to_string(), "#5e5c64")
            }
            View::Flagged => {
                let mut items: Vec<&Reminder> = open.iter().copied().filter(matches).filter(|r| r.flagged).collect();
                items.sort_by(by_due);
                sections.push((String::new(), items));
                ("Flagged".to_string(), "#e5a50a")
            }
            View::Urgent => {
                let mut items: Vec<&Reminder> = open.iter().copied().filter(matches).filter(|r| is_urgent(r)).collect();
                items.sort_by(by_due);
                sections.push((String::new(), items));
                ("Urgent".to_string(), "#d6577f")
            }
            View::Completed => {
                let mut items: Vec<&Reminder> = reminders.iter().filter(|r| r.completed).filter(matches).collect();
                items.sort_by_key(|r| std::cmp::Reverse(r.completion_date));
                items.truncate(COMPLETED_LIMIT);
                sections.push((String::new(), items));
                ("Completed".to_string(), "#9a9996")
            }
            View::List(id) => {
                let mut items: Vec<&Reminder> =
                    open.iter().copied().filter(matches).filter(|r| r.list.as_deref() == Some(id)).collect();
                items.sort_by(by_due);
                sections.push((String::new(), items));
                let l = lists.iter().find(|l| &l.record_name == id);
                (l.map(|l| l.name.clone()).unwrap_or_default(), l.and_then(|l| l.color.as_deref()).unwrap_or("#3584e4"))
            }
        };
        self.content_page.set_title(&title);
        self.heading.set_markup(&format!("<span foreground=\"{color}\">{}</span>", glib::markup_escape_text(&title)));
        let done = match &view {
            View::List(id) => reminders.iter().filter(|r| r.completed && r.list.as_deref() == Some(id)).count(),
            View::Completed => 0,
            _ => reminders.len() - open.len(),
        };
        self.subheading.set_text(&if done > 0 { format!("{done} Completed") } else { String::new() });
        self.subheading.set_visible(done > 0);

        while let Some(child) = self.content.first_child() {
            self.content.remove(&child);
        }
        let total: usize = sections.iter().map(|(_, v)| v.len()).sum();
        if total == 0 {
            let empty = adw::StatusPage::builder()
                .icon_name("object-select-symbolic")
                .title(if query.is_empty() { "No Reminders" } else { "No Results" })
                .build();
            empty.add_css_class("compact");
            self.content.append(&empty);
            return;
        }
        for (label, items) in sections {
            if items.is_empty() {
                continue;
            }
            if !label.is_empty() {
                let h = gtk::Label::new(Some(&label));
                h.add_css_class("section-title");
                if label == "Overdue" {
                    h.add_css_class("overdue");
                }
                h.set_xalign(0.0);
                self.content.append(&h);
            }
            let list = gtk::ListBox::new();
            list.add_css_class("boxed-list");
            list.set_selection_mode(gtk::SelectionMode::None);
            list.set_margin_top(6);
            for r in items {
                list.append(&self.row(r, &list_name(r), is_urgent(r), today));
            }
            list.connect_row_activated(|_, row| {
                if let Some(c) = controller() {
                    c.open(&row.widget_name());
                }
            });
            self.content.append(&list);
        }
    }

    fn row(&self, r: &Reminder, list: &str, urgent: bool, today: NaiveDate) -> gtk::ListBoxRow {
        let row = gtk::ListBoxRow::new();
        row.set_widget_name(&r.record_name);
        let b = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        b.add_css_class("reminder-row");
        b.set_margin_start(8);
        b.set_margin_end(8);

        let check = gtk::CheckButton::new();
        check.add_css_class("selection-mode");
        check.set_valign(gtk::Align::Start);
        check.set_margin_top(4);
        check.set_active(r.completed);
        check.set_sensitive(self.signed_in.get());
        let name = r.record_name.clone();
        check.connect_toggled(move |c| {
            if let Some(ctrl) = controller() {
                ctrl.set_completed(&name, c.is_active());
            }
        });
        b.append(&check);

        let text = gtk::Box::new(gtk::Orientation::Vertical, 3);
        text.set_hexpand(true);
        let title = gtk::Label::new(Some(if r.title.is_empty() { "New Reminder" } else { &r.title }));
        title.set_xalign(0.0);
        title.set_wrap(true);
        title.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        if r.completed {
            title.add_css_class("dim-label");
        }
        text.append(&title);

        let mut meta = Vec::new();
        if !list.is_empty() {
            meta.push(glib::markup_escape_text(list).to_string());
        }
        if let Some(due) = &r.due {
            let date = due.local_date();
            let mut s = match (date - today).num_days() {
                0 => "Today".to_string(),
                1 => "Tomorrow".to_string(),
                -1 => "Yesterday".to_string(),
                _ => date.format("%-d/%-m/%Y").to_string(),
            };
            if !due.all_day {
                s.push_str(&due.at.with_timezone(&Local).format(", %H:%M").to_string());
            }
            if !r.completed && (due.at < Utc::now() && !due.all_day || date < today) {
                s = format!("<span foreground=\"#f66151\">{s}</span>");
            }
            meta.push(s);
        }
        if r.recurrence.is_some() {
            meta.push("↻".into());
        }
        if !r.notes.is_empty() {
            meta.push(glib::markup_escape_text(r.notes.lines().next().unwrap_or("")).to_string());
        }
        if !meta.is_empty() {
            let m = gtk::Label::new(None);
            m.set_markup(&meta.join("  ·  "));
            m.add_css_class("dim-label");
            m.add_css_class("caption");
            m.set_xalign(0.0);
            m.set_ellipsize(gtk::pango::EllipsizeMode::End);
            text.append(&m);
        }
        if !r.urls.is_empty() || !r.tags.is_empty() {
            let chips = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            for url in &r.urls {
                let host = url.split("://").nth(1).unwrap_or(url).split('/').next().unwrap_or(url).to_string();
                let link = gtk::LinkButton::with_label(url, &host);
                link.add_css_class("chip");
                link.set_has_frame(false);
                chips.append(&link);
            }
            for t in &r.tags {
                let tag = gtk::Label::new(Some(&format!("#{t}")));
                tag.add_css_class("chip");
                chips.append(&tag);
            }
            text.append(&chips);
        }
        b.append(&text);

        if urgent {
            let i = gtk::Image::from_icon_name("alarm-symbolic");
            i.set_tooltip_text(Some("Urgent"));
            i.add_css_class("error");
            b.append(&i);
        }
        if r.flagged {
            let i = gtk::Image::from_icon_name("emoji-flags-symbolic");
            i.set_tooltip_text(Some("Flagged"));
            i.add_css_class("warning");
            b.append(&i);
        }
        if r.priority != remindkit::Priority::None {
            let marks = match r.priority {
                remindkit::Priority::High => "!!!",
                remindkit::Priority::Medium => "!!",
                _ => "!",
            };
            let p = gtk::Label::new(Some(marks));
            p.add_css_class("accent");
            b.append(&p);
        }
        row.set_child(Some(&b));
        row.set_activatable(true);
        row
    }
}

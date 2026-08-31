mod config;
mod greetd;
mod sessions;
mod theme;
mod users;

use greetd::{AuthPrompt, Outcome};
use gtk4::gdk::Key;
use gtk4::glib::Propagation;
use gtk4::prelude::*;
use relm4::prelude::*;
use tokio::sync::mpsc;

/// Extra zoom beyond plain cover-fit — matches breadlock's `KENBURNS_ZOOM`.
const KENBURNS_ZOOM: f32 = 1.06;

#[derive(Debug, Clone)]
enum Stage {
    /// Waiting for a username typed into `entry` — only reached when user
    /// enumeration is off or found nothing (see [`App::typed_mode`]).
    Username,
    /// greetd/PAM asked a question; `entry` holds the answer (masking is
    /// applied imperatively on the entry widget when the prompt arrives).
    Prompt,
    /// A request is in flight — input is disabled so a second Enter can't
    /// race it. Also the initial stage in auto-user mode, while the opening
    /// `CreateSession` is in flight.
    Working,
    /// `StartSession` has been sent — Escape must not cancel.
    Starting,
}

#[derive(Debug)]
enum AppInput {
    ClockTick,
    /// Enter pressed in the entry — behavior depends on `Stage`.
    Submit,
    Outcome(Outcome),
    Error(String),
    SessionStarted,
    /// Picker changed; `u32::MAX` (`INVALID_LIST_POSITION`) is ignored.
    SessionSelected(u32),
    /// User picker changed (multi-user auto mode) — restart auth for that user.
    UserSelected(u32),
    /// Post-init kick in auto-user mode: open the greetd conversation for the
    /// resolved user so the greeter lands straight on the password prompt.
    AutoStart,
    /// Escape — abort the in-progress PAM conversation.
    Cancel,
}

struct App {
    clock_lbl: gtk4::Label,
    date_lbl: gtk4::Label,
    status_lbl: gtk4::Label,
    entry: gtk4::Entry,
    card: gtk4::Box,
    spinner: gtk4::Box,
    stage: Stage,
    /// The name currently being authenticated — typed in [`Stage::Username`],
    /// or the resolved account in auto-user mode.
    username: String,
    /// Enumerated login accounts. Empty ⇒ typed-username mode; one ⇒ straight
    /// to the password prompt; several ⇒ `user_idx` selects among them.
    users: Vec<users::User>,
    user_idx: usize,
    sessions: Vec<sessions::Session>,
    selected: usize,
    clock_format: String,
    date_format: String,
    /// Last status line was a PAM Info/Error — keep it when the next
    /// Secret/Visible prompt arrives.
    pam_status_held: bool,
    cmd_tx: mpsc::UnboundedSender<greetd::Command>,
}

#[relm4::component]
impl SimpleComponent for App {
    type Init = ();
    type Input = AppInput;
    type Output = ();

    view! {
        gtk4::ApplicationWindow {
            add_css_class: "breadgreet",
            set_title: Some("breadgreet"),

            #[name = "overlay"]
            gtk4::Overlay {
                // The relm4 view macro supports a single `set_child` per
                // widget, so `root_box` is declared as the overlay's child
                // here; the wallpaper (main child) and veil layers are
                // stacked in `init` via `set_child` + `add_overlay`.
                #[name = "root_box"]
                gtk4::Box {
                    set_orientation: gtk4::Orientation::Vertical,
                    set_halign: gtk4::Align::Center,
                    set_valign: gtk4::Align::Center,
                }
            }
        }
    }

    fn init(
        _init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        root.fullscreen();

        let config = config::load();
        let sessions = sessions::list(
            &config.sessions.wayland_dirs,
            &config.sessions.xsessions_dirs,
        );
        // Same default rule as `discover()`: configured stem (compiled-in
        // `bos`), else the first listed session.
        let selected = sessions::discover(
            &config.sessions.wayland_dirs,
            &config.sessions.xsessions_dirs,
            &config.sessions.default,
        )
        .and_then(|chosen| sessions.iter().position(|s| s.stem == chosen.stem))
        .unwrap_or(0);

        // Who's logging in. `[user] prompt` keeps the old type-it flow; a set
        // `[user] name` pins one account; otherwise enumerate `/etc/passwd`.
        // A non-empty list means "auto mode": no username field, straight to
        // the password prompt (with a picker if there's more than one).
        let users = if config.user.prompt {
            Vec::new()
        } else if !config.user.name.trim().is_empty() {
            let name = config.user.name.trim().to_string();
            vec![users::User {
                display: name.clone(),
                name,
                uid: 0,
            }]
        } else {
            users::list()
        };
        let auto_user = !users.is_empty();

        if config.appearance.background.blur {
            tracing::warn!(
                "background.blur is not implemented yet (planned v2 feature, needs a wlr-screencopy \
                 capture) — showing the configured background unblurred"
            );
        }

        let clock_lbl = gtk4::Label::new(None);
        clock_lbl.add_css_class("login-clock");

        let date_lbl = gtk4::Label::new(None);
        date_lbl.add_css_class("login-date");
        // Spacing below the clock cluster is the accent rule's own top/bottom
        // margin (see `.login-rule` CSS); only a hair of clock→date gap here.
        if config.appearance.clock.date_format.is_empty() {
            date_lbl.set_visible(false);
        } else {
            clock_lbl.set_margin_bottom(4);
            date_lbl.set_label(&current_time(&config.appearance.clock.date_format));
        }

        let entry = gtk4::Entry::new();
        entry.add_css_class("login-entry");
        entry.set_width_chars(24);
        // A leading glyph that swaps person → key when PAM asks for a secret
        // (kept in sync in show_auth_entry / reset_auth). In auto-user mode the
        // entry is only ever the password field, and starts disabled until the
        // opening `CreateSession` produces a prompt.
        if auto_user {
            entry.set_placeholder_text(Some("Password"));
            entry.set_primary_icon_name(Some("dialog-password-symbolic"));
            entry.set_visibility(false);
            entry.set_sensitive(false);
        } else {
            entry.set_placeholder_text(Some("Username"));
            entry.set_primary_icon_name(Some("avatar-default-symbolic"));
        }
        entry.set_primary_icon_activatable(false);
        entry.set_primary_icon_sensitive(false);
        {
            let sender = sender.clone();
            entry.connect_activate(move |_| sender.input(AppInput::Submit));
        }

        let status_lbl = gtk4::Label::new(None);
        status_lbl.add_css_class("login-status");

        if sessions.is_empty() {
            entry.set_sensitive(false);
            status_lbl.set_label("No session found — cannot log in");
            status_lbl.add_css_class("error");
        }

        let session_widget: gtk4::Widget = if sessions.is_empty() {
            let session_lbl = gtk4::Label::new(Some("No session found — cannot log in"));
            session_lbl.add_css_class("login-session");
            session_lbl.upcast()
        } else {
            let names: Vec<&str> = sessions.iter().map(|s| s.name.as_str()).collect();
            let dropdown = gtk4::DropDown::from_strings(&names);
            dropdown.set_hexpand(true);
            dropdown.set_focusable(true);
            dropdown.set_tooltip_text(Some("Session"));
            dropdown.update_property(&[gtk4::accessible::Property::Label("Session")]);
            dropdown.set_selected(selected as u32);
            {
                let sender = sender.clone();
                dropdown.connect_selected_notify(move |dd| {
                    sender.input(AppInput::SessionSelected(dd.selected()));
                });
            }
            // Wrap the dropdown in the design sketch's `.srow`: a surface pill
            // with a gradient session glyph on the left (drawn purely in CSS).
            let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
            row.add_css_class("login-session");
            row.add_css_class("session-row");
            let glyph = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
            glyph.add_css_class("session-icon");
            glyph.set_valign(gtk4::Align::Center);
            row.append(&glyph);
            row.append(&dropdown);
            row.upcast()
        };

        // The "who's logging in" row, shown above the password field in auto
        // mode: a gradient avatar glyph + either the account name (one user) or
        // a picker (several). Nothing in typed mode — the entry is the field.
        let user_widget: Option<gtk4::Widget> = if !auto_user {
            None
        } else {
            let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
            row.add_css_class("login-user");
            row.add_css_class("user-row");
            let glyph = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
            glyph.add_css_class("user-icon");
            glyph.set_valign(gtk4::Align::Center);
            row.append(&glyph);
            if users.len() == 1 {
                let name = gtk4::Label::new(Some(&users[0].display));
                name.add_css_class("user-name");
                name.set_halign(gtk4::Align::Start);
                name.set_hexpand(true);
                name.set_ellipsize(gtk4::pango::EllipsizeMode::End);
                row.append(&name);
            } else {
                let labels: Vec<&str> = users.iter().map(|u| u.display.as_str()).collect();
                let dropdown = gtk4::DropDown::from_strings(&labels);
                dropdown.set_hexpand(true);
                dropdown.set_focusable(true);
                dropdown.set_tooltip_text(Some("User"));
                dropdown.update_property(&[gtk4::accessible::Property::Label("User")]);
                {
                    let sender = sender.clone();
                    dropdown.connect_selected_notify(move |dd| {
                        sender.input(AppInput::UserSelected(dd.selected()));
                    });
                }
                row.append(&dropdown);
            }
            Some(row.upcast())
        };

        // A CSS-animated ring (not GtkSpinner) so it matches the design
        // sketch exactly: 2px track, accent top, shown only while a greetd
        // request is in flight (see `set_busy`).
        let spinner = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        spinner.add_css_class("login-spinner");
        spinner.set_halign(gtk4::Align::Center);

        let card = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        card.add_css_class("login-card");
        if let Some(w) = &user_widget {
            card.append(w);
        }
        card.append(&entry);
        card.append(&status_lbl);
        card.append(&spinner);
        card.append(&session_widget);

        let widgets = view_output!();

        // Layer the window: wallpaper (main child, bottom) → dim veil → the
        // clock+card cluster (top). Overlay children stack above the main
        // child in `add_overlay` order, so the card ends up on top.
        let bg_area = gtk4::DrawingArea::new();
        bg_area.set_hexpand(true);
        bg_area.set_vexpand(true);

        let veil = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        veil.set_hexpand(true);
        veil.set_vexpand(true);
        veil.set_halign(gtk4::Align::Fill);
        veil.set_valign(gtk4::Align::Fill);
        veil.set_can_focus(false);
        veil.add_css_class("login-veil");

        widgets.overlay.set_child(Some(&bg_area));
        // Overlay children stack above the main child in `add_overlay`
        // order; the last one added is topmost. So the veil goes in first,
        // then the clock+card cluster on top of it.
        widgets.overlay.add_overlay(&veil);
        widgets.overlay.add_overlay(&widgets.root_box);

        // Thin accent rule between the clock cluster and the card — a
        // signature detail shared with the lock screen; it draws itself in
        // (scaleX 0→1) via `.login-rule`.
        let rule = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        rule.add_css_class("login-rule");
        rule.set_halign(gtk4::Align::Center);

        widgets.root_box.append(&clock_lbl);
        widgets.root_box.append(&date_lbl);
        widgets.root_box.append(&rule);
        widgets.root_box.append(&card);

        {
            let tx = sender.input_sender().clone();
            let key = gtk4::EventControllerKey::new();
            key.set_propagation_phase(gtk4::PropagationPhase::Capture);
            key.connect_key_pressed(move |_, keyval, _, _| {
                if keyval == Key::Escape {
                    let _ = tx.send(AppInput::Cancel);
                    Propagation::Stop
                } else {
                    Propagation::Proceed
                }
            });
            root.add_controller(key);
        }

        // Wallpaper behind the card: cover-fit, Ken Burns pan when enabled
        // (driven by a frame-clock tick callback), plus an entrance fade+rise.
        let ken_burns = config.appearance.background.ken_burns;
        let wallpaper_path = if config.appearance.background.mode
            == breadlock_ui::config::BackgroundMode::Image
            && !config.appearance.background.path.is_empty()
        {
            Some(config.appearance.background.path.clone())
        } else {
            None
        };
        setup_wallpaper(&root, &bg_area, wallpaper_path.as_deref(), ken_burns);
        // Entrance motion (clock rise, rule draw, card pop-in + idle breathe)
        // is CSS `@keyframes` in theme.rs — GTK4 plays `animation` when a
        // widget's style is first computed on show.

        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        spawn_greetd_actor(cmd_rx, sender.clone());

        theme::apply(&config.appearance.font.family);
        theme::bind(&root, &config.appearance.font.family);
        spawn_clock_ticker(sender.clone());

        let username = users.first().map(|u| u.name.clone()).unwrap_or_default();
        let model = App {
            clock_lbl,
            date_lbl,
            status_lbl,
            entry,
            card,
            spinner,
            // Auto mode opens in `Working` — the greeter is already waiting on
            // greetd for the first prompt (kicked by `AutoStart` below).
            stage: if auto_user {
                Stage::Working
            } else {
                Stage::Username
            },
            username,
            users,
            user_idx: 0,
            sessions,
            selected,
            clock_format: config.appearance.clock.format.clone(),
            date_format: config.appearance.clock.date_format.clone(),
            pam_status_held: false,
            cmd_tx,
        };
        model
            .clock_lbl
            .set_label(&current_time(&model.clock_format));
        if auto_user {
            sender.input(AppInput::AutoStart);
        } else if !model.sessions.is_empty() {
            model.entry.grab_focus();
        }

        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, _sender: ComponentSender<Self>) {
        match msg {
            AppInput::ClockTick => {
                self.clock_lbl.set_label(&current_time(&self.clock_format));
                if !self.date_format.is_empty() {
                    self.date_lbl.set_label(&current_time(&self.date_format));
                }
            }
            AppInput::Submit => self.handle_submit(),
            AppInput::Outcome(Outcome::Success) => self.start_session(),
            AppInput::Outcome(Outcome::Prompt(prompt)) => self.handle_prompt(prompt),
            AppInput::Error(description) => self.show_error(&description),
            AppInput::SessionStarted => {
                // greetd waits for this process to exit before exec'ing the
                // session (cage + gtkgreet/tuigreet all quit here).
                self.status_lbl.set_label("Starting session…");
                relm4::main_application().quit();
                std::process::exit(0);
            }
            AppInput::SessionSelected(idx) => {
                let idx = idx as usize;
                if idx < self.sessions.len() {
                    self.selected = idx;
                }
            }
            AppInput::UserSelected(idx) => self.switch_user(idx as usize),
            AppInput::AutoStart => {
                if self.sessions.is_empty() {
                    // Nothing to log into — leave the "no session" error up.
                    self.stage = Stage::Username;
                } else {
                    self.status_lbl.set_label("");
                    self.dispatch(greetd::Command::CreateSession(self.username.clone()));
                }
            }
            AppInput::Cancel => self.cancel_auth(),
        }
        self.sync_busy();
    }
}

impl App {
    /// Spinner runs exactly while a greetd request is in flight. Driven from
    /// one place (the end of `update`) so every path — submit, prompt,
    /// cancel, error — stays in sync with `self.stage`.
    fn sync_busy(&self) {
        let busy = matches!(self.stage, Stage::Working | Stage::Starting);
        if busy {
            self.spinner.add_css_class("spinning");
        } else {
            self.spinner.remove_css_class("spinning");
        }
    }

    /// One-shot shake on the card, matching the lock screen's wrong-password
    /// motion. Re-armed each call by dropping the class first (a running
    /// animation won't restart just from re-adding it).
    fn flash_error(&self) {
        self.card.remove_css_class("shake");
        let card = self.card.clone();
        gtk4::glib::timeout_add_local_once(std::time::Duration::from_millis(10), move || {
            card.add_css_class("shake");
            let card2 = card.clone();
            gtk4::glib::timeout_add_local_once(std::time::Duration::from_millis(420), move || {
                card2.remove_css_class("shake");
            });
        });
    }

    fn handle_submit(&mut self) {
        if matches!(self.stage, Stage::Working | Stage::Starting) {
            return;
        }
        let text = self.entry.text().to_string();

        match &self.stage {
            Stage::Username => {
                if self.sessions.is_empty() {
                    self.show_error("No session found — cannot log in");
                    return;
                }
                if text.is_empty() {
                    return;
                }
                self.username = text;
                self.entry.set_text("");
                self.entry.set_sensitive(false);
                self.stage = Stage::Working;
                self.status_lbl.set_label("");
                self.status_lbl.remove_css_class("error");
                self.pam_status_held = false;
                self.dispatch(greetd::Command::CreateSession(self.username.clone()));
            }
            Stage::Prompt => {
                self.entry.set_text("");
                self.entry.set_sensitive(false);
                self.stage = Stage::Working;
                self.dispatch(greetd::Command::Respond(prompt_answer(text)));
            }
            Stage::Working | Stage::Starting => {}
        }
    }

    fn handle_prompt(&mut self, prompt: AuthPrompt) {
        match prompt {
            AuthPrompt::Info(message) => {
                self.status_lbl.remove_css_class("error");
                self.status_lbl.set_label(&message);
                self.pam_status_held = true;
                self.dispatch(greetd::Command::Respond(None));
            }
            AuthPrompt::Error(message) => {
                self.status_lbl.add_css_class("error");
                self.status_lbl.set_label(&message);
                self.flash_error();
                self.pam_status_held = true;
                self.dispatch(greetd::Command::Respond(None));
            }
            AuthPrompt::Visible(message) => self.show_auth_entry(&message, true),
            AuthPrompt::Secret(message) => self.show_auth_entry(&message, false),
        }
    }

    fn show_auth_entry(&mut self, message: &str, visible: bool) {
        // The prompt text goes in the placeholder; the status line stays for
        // Info/Error messages only (a preceding one is kept via
        // `pam_status_held`, otherwise it's cleared — no echoing "Password:"
        // both in the field and under it).
        if !self.pam_status_held {
            self.status_lbl.remove_css_class("error");
            self.status_lbl.set_label("");
        }
        self.pam_status_held = false;
        self.entry.set_visibility(visible);
        self.entry.set_placeholder_text(Some(message));
        self.entry
            .set_primary_icon_name(Some("dialog-password-symbolic"));
        self.entry.set_sensitive(true);
        self.entry.grab_focus();
        self.stage = Stage::Prompt;
    }

    fn start_session(&mut self) {
        let (cmd, env) = match self.sessions.get(self.selected) {
            Some(session) => (session.exec.clone(), session.start_env()),
            None => {
                self.dispatch(greetd::Command::CancelSession);
                self.show_error("No session available to start");
                return;
            }
        };
        self.status_lbl.remove_css_class("error");
        self.status_lbl.set_label("Starting session…");
        self.entry.set_sensitive(false);
        self.stage = Stage::Starting;
        self.dispatch(greetd::Command::StartSession { cmd, env });
    }

    /// No account resolved up front — the greeter asks for the username.
    fn typed_mode(&self) -> bool {
        self.users.is_empty()
    }

    /// Multi-user auto mode: the picker changed. Tear down the current greetd
    /// conversation and open a fresh one for the newly selected account.
    fn switch_user(&mut self, idx: usize) {
        if matches!(self.stage, Stage::Starting) || idx >= self.users.len() || idx == self.user_idx
        {
            return;
        }
        self.user_idx = idx;
        self.username = self.users[idx].name.clone();
        self.status_lbl.set_label("");
        self.status_lbl.remove_css_class("error");
        self.pam_status_held = false;
        self.entry.set_text("");
        self.entry.set_sensitive(false);
        self.entry.set_placeholder_text(Some("Password"));
        self.stage = Stage::Working;
        let _ = self.cmd_tx.send(greetd::Command::CancelSession);
        self.dispatch(greetd::Command::CreateSession(self.username.clone()));
    }

    fn cancel_auth(&mut self) {
        match self.stage {
            Stage::Starting => {}
            Stage::Username => {
                self.entry.set_text("");
            }
            Stage::Prompt | Stage::Working => {
                self.status_lbl.set_label("");
                self.status_lbl.remove_css_class("error");
                // `reset_auth` dispatches the CancelSession itself, so we don't
                // double-send it here.
                self.reset_auth();
            }
        }
    }

    fn dispatch(&mut self, cmd: greetd::Command) {
        if self.cmd_tx.send(cmd).is_err() {
            self.show_error("Cannot reach greetd");
        }
    }

    fn show_error(&mut self, description: &str) {
        self.status_lbl.set_label(description);
        if description.is_empty() {
            self.status_lbl.remove_css_class("error");
        } else {
            self.status_lbl.add_css_class("error");
            self.flash_error();
        }
        self.reset_auth();
    }

    /// Return to the start of the auth flow after an error or an Escape.
    ///
    /// Typed mode goes back to the username field; auto mode re-opens the
    /// password prompt for the same account (there's no username step to
    /// return to). Either way the stale greetd conversation is cancelled
    /// first, so the next `CreateSession` doesn't stack on a half-done one.
    fn reset_auth(&mut self) {
        self.entry.set_text("");
        self.entry.set_visibility(true);
        self.pam_status_held = false;

        // On a broken channel set the failure label directly rather than
        // recursing through `show_error` (which calls back here forever).
        let channel_ok = self.cmd_tx.send(greetd::Command::CancelSession).is_ok();
        if !channel_ok {
            self.status_lbl.set_label("Cannot reach greetd");
            self.status_lbl.add_css_class("error");
        }

        if self.typed_mode() {
            self.entry.set_placeholder_text(Some("Username"));
            self.entry
                .set_primary_icon_name(Some("avatar-default-symbolic"));
            self.entry.set_sensitive(!self.sessions.is_empty());
            self.stage = Stage::Username;
            self.username.clear();
            if !self.sessions.is_empty() {
                self.entry.grab_focus();
            }
        } else {
            // Auto mode: straight back to a fresh password prompt. The entry
            // stays disabled until the reopened conversation's Secret prompt
            // arrives (see `show_auth_entry`).
            self.entry.set_visibility(false);
            self.entry.set_placeholder_text(Some("Password"));
            self.entry
                .set_primary_icon_name(Some("dialog-password-symbolic"));
            self.entry.set_sensitive(false);
            self.stage = Stage::Working;
            if channel_ok && !self.sessions.is_empty() {
                let _ = self
                    .cmd_tx
                    .send(greetd::Command::CreateSession(self.username.clone()));
            }
        }
    }
}

/// Secret/Visible answers are always `Some`, including the empty string.
/// greetd/PAM treat `None` as a conversation cancel.
fn prompt_answer(text: String) -> Option<String> {
    Some(text)
}

/// Paints the configured wallpaper full-screen behind the login card. The
/// image is loaded once as a `gdk_pixbuf::Pixbuf` and drawn by a
/// `GtkDrawingArea` draw callback, so the pan costs no layout passes — the
/// drawing area fills the window and the draw callback applies the cover
/// scale + Ken Burns offset itself. A missing/unreadable file or a non-image
/// background leaves the card on the palette background color.
fn setup_wallpaper(
    window: &gtk4::ApplicationWindow,
    bg_area: &gtk4::DrawingArea,
    path: Option<&str>,
    ken_burns: bool,
) {
    let Some(path) = path else { return };
    let pixbuf = match gtk4::gdk_pixbuf::Pixbuf::from_file(path) {
        Ok(pixbuf) => pixbuf,
        Err(err) => {
            tracing::warn!(%err, "failed to load wallpaper");
            return;
        }
    };
    let (iw, ih) = (pixbuf.width() as f32, pixbuf.height() as f32);
    if iw <= 0.0 || ih <= 0.0 {
        return;
    }

    // Shared pan phase: the tick callback advances it, the draw callback
    // reads it. Using a draw callback (rather than a moving widget) means
    // the wallpaper never feeds the window's minimum size.
    let phase = std::rc::Rc::new(std::cell::Cell::new(0.0f64));

    let draw_pixbuf = pixbuf.clone();
    let draw_phase = phase.clone();
    bg_area.set_draw_func(move |_area, cr, w, h| {
        let (w, h) = (w as f32, h as f32);
        if w <= 0.0 || h <= 0.0 {
            return;
        }
        // Cover scale, then the Ken Burns oversize (leaves room to pan).
        let cover = (w / iw).max(h / ih);
        let scale = if ken_burns {
            cover * KENBURNS_ZOOM
        } else {
            cover
        };
        let dw = iw * scale;
        let dh = ih * scale;
        // Pan within the oversize margin (0..dw-w, 0..dh-h).
        let phase = draw_phase.get();
        let max_x = (dw - w).max(0.0);
        let max_y = (dh - h).max(0.0);
        let x = max_x * (0.5 + 0.5 * phase.sin() as f32);
        let y = max_y * (0.5 + 0.5 * (phase * 0.7).cos() as f32);

        cr.translate(-x as f64, -y as f64);
        cr.scale(scale as f64, scale as f64);
        cr.set_source_pixbuf(&draw_pixbuf, 0.0, 0.0);
        let _ = cr.paint();
    });

    if !ken_burns {
        return;
    }

    let area = bg_area.clone();
    let start = std::time::Instant::now();
    window.add_tick_callback(move |_w, _frame_clock| {
        let elapsed = start.elapsed().as_secs_f64();
        phase.set(elapsed * std::f64::consts::TAU / 90.0);
        area.queue_draw();
        gtk4::glib::ControlFlow::Continue
    });
}

/// Owns the single stateful connection to `$GREETD_SOCK` and translates the
/// UI's [`greetd::Command`]s into greetd IPC round-trips, forwarding each
/// outcome back as an [`AppInput`].
fn spawn_greetd_actor(
    cmd_rx: mpsc::UnboundedReceiver<greetd::Command>,
    sender: ComponentSender<App>,
) {
    let input = sender.input_sender().clone();
    relm4::spawn(async move {
        greetd::run_actor(cmd_rx, move |event| {
            let msg = match event {
                greetd::Event::Outcome(outcome) => AppInput::Outcome(outcome),
                greetd::Event::Error(description) => AppInput::Error(description),
                greetd::Event::SessionStarted => AppInput::SessionStarted,
            };
            let _ = input.send(msg);
        })
        .await;
    });
}

fn spawn_clock_ticker(sender: ComponentSender<App>) {
    let tx = sender.input_sender().clone();
    relm4::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            if tx.send(AppInput::ClockTick).is_err() {
                break;
            }
        }
    });
}

fn current_time(format: &str) -> String {
    chrono::Local::now().format(format).to_string()
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let app = RelmApp::new("sh.breadway.breadgreet");
    app.run::<App>(());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_password_is_some_empty_string() {
        assert_eq!(prompt_answer(String::new()), Some(String::new()));
        assert_eq!(prompt_answer("hunter2".into()), Some("hunter2".into()));
    }
}

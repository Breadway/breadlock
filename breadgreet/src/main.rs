mod config;
mod greetd;
mod sessions;
mod theme;

use greetd::{AuthPrompt, Client, Outcome};
use gtk4::prelude::*;
use relm4::prelude::*;
use tokio::sync::mpsc;

/// Commands sent from the UI thread to the greetd actor task (see
/// [`spawn_greetd_actor`]), which owns the single stateful connection to
/// `$GREETD_SOCK` for the lifetime of one login attempt.
enum GreetdCommand {
    CreateSession(String),
    Respond(Option<String>),
    StartSession { cmd: Vec<String>, env: Vec<String> },
}

#[derive(Debug, Clone)]
enum Stage {
    /// Waiting for a username in `entry`.
    Username,
    /// greetd/PAM asked a question; `entry` holds the answer (masking is
    /// applied imperatively on the entry widget when the prompt arrives).
    Prompt,
    /// A request is in flight — input is disabled so a second Enter can't
    /// race it.
    Working,
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
}

struct App {
    clock_lbl: gtk4::Label,
    status_lbl: gtk4::Label,
    entry: gtk4::Entry,
    stage: Stage,
    username: String,
    sessions: Vec<sessions::Session>,
    selected: usize,
    clock_format: String,
    cmd_tx: mpsc::UnboundedSender<GreetdCommand>,
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

        let clock_lbl = gtk4::Label::new(None);
        clock_lbl.add_css_class("login-clock");

        let entry = gtk4::Entry::new();
        entry.add_css_class("login-entry");
        entry.set_placeholder_text(Some("Username"));
        entry.set_width_chars(24);
        {
            let sender = sender.clone();
            entry.connect_activate(move |_| sender.input(AppInput::Submit));
        }

        let status_lbl = gtk4::Label::new(None);
        status_lbl.add_css_class("login-status");

        let session_widget: gtk4::Widget = if sessions.is_empty() {
            let session_lbl = gtk4::Label::new(Some("No session found — cannot log in"));
            session_lbl.add_css_class("login-session");
            session_lbl.upcast()
        } else {
            let names: Vec<&str> = sessions.iter().map(|s| s.name.as_str()).collect();
            let dropdown = gtk4::DropDown::from_strings(&names);
            dropdown.add_css_class("login-session");
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
            dropdown.upcast()
        };

        let card = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
        card.add_css_class("login-card");
        card.append(&entry);
        card.append(&status_lbl);
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

        widgets.root_box.append(&clock_lbl);
        widgets.root_box.append(&card);

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
        setup_entrance(&root, &widgets.root_box);

        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        spawn_greetd_actor(cmd_rx, sender.clone());

        theme::apply();
        bread_theme::gtk::bind_window_auto(&root);
        spawn_clock_ticker(sender.clone());

        let model = App {
            clock_lbl,
            status_lbl,
            entry,
            stage: Stage::Username,
            username: String::new(),
            sessions,
            selected,
            clock_format: config.appearance.clock.format.clone(),
            cmd_tx,
        };
        model
            .clock_lbl
            .set_label(&current_time(&model.clock_format));
        model.entry.grab_focus();

        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, _sender: ComponentSender<Self>) {
        match msg {
            AppInput::ClockTick => {
                self.clock_lbl.set_label(&current_time(&self.clock_format));
            }
            AppInput::Submit => self.handle_submit(),
            AppInput::Outcome(Outcome::Success) => self.start_session(),
            AppInput::Outcome(Outcome::Prompt(prompt)) => self.handle_prompt(prompt),
            AppInput::Error(description) => {
                self.status_lbl.set_label(&description);
                self.status_lbl.add_css_class("error");
                self.entry.set_text("");
                self.entry.set_visibility(true);
                self.entry.set_placeholder_text(Some("Username"));
                self.entry.set_sensitive(true);
                self.stage = Stage::Username;
                self.username.clear();
            }
            AppInput::SessionStarted => {
                // greetd now owns the VT switch to the started session —
                // nothing left for the greeter to do.
                self.status_lbl.set_label("Starting session…");
            }
            AppInput::SessionSelected(idx) => {
                let idx = idx as usize;
                if idx < self.sessions.len() {
                    self.selected = idx;
                }
            }
        }
    }
}

impl App {
    fn handle_submit(&mut self) {
        if matches!(self.stage, Stage::Working) {
            return;
        }
        let text = self.entry.text().to_string();

        match &self.stage {
            Stage::Username => {
                if text.is_empty() {
                    return;
                }
                self.username = text;
                self.entry.set_text("");
                self.entry.set_sensitive(false);
                self.stage = Stage::Working;
                let _ = self
                    .cmd_tx
                    .send(GreetdCommand::CreateSession(self.username.clone()));
            }
            Stage::Prompt => {
                self.entry.set_text("");
                self.entry.set_sensitive(false);
                self.stage = Stage::Working;
                let answer = if text.is_empty() { None } else { Some(text) };
                let _ = self.cmd_tx.send(GreetdCommand::Respond(answer));
            }
            Stage::Working => {}
        }
    }

    fn handle_prompt(&mut self, prompt: AuthPrompt) {
        self.status_lbl.remove_css_class("error");
        match prompt {
            AuthPrompt::Info(message) | AuthPrompt::Error(message) => {
                // No answer needed — display and immediately continue the
                // conversation with an empty response.
                self.status_lbl.set_label(&message);
                let _ = self.cmd_tx.send(GreetdCommand::Respond(None));
            }
            AuthPrompt::Visible(message) => {
                self.status_lbl.set_label(&message);
                self.entry.set_visibility(true);
                self.entry.set_placeholder_text(Some(&message));
                self.entry.set_sensitive(true);
                self.entry.grab_focus();
                self.stage = Stage::Prompt;
            }
            AuthPrompt::Secret(message) => {
                self.status_lbl.set_label(&message);
                self.entry.set_visibility(false);
                self.entry.set_placeholder_text(Some(&message));
                self.entry.set_sensitive(true);
                self.entry.grab_focus();
                self.stage = Stage::Prompt;
            }
        }
    }

    fn start_session(&mut self) {
        let Some(session) = self.sessions.get(self.selected) else {
            self.status_lbl.set_label("No session available to start");
            self.status_lbl.add_css_class("error");
            return;
        };
        self.status_lbl.set_label("Starting session…");
        let _ = self.cmd_tx.send(GreetdCommand::StartSession {
            cmd: session.exec.clone(),
            env: Vec::new(),
        });
    }
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
        let scale = if ken_burns { cover * 1.08 } else { cover };
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

/// Entrance animation: the clock + card cluster fades in and rises ~24px
/// over ~600ms (ease-out), matching the lock screen's appear motion.
fn setup_entrance(window: &gtk4::ApplicationWindow, root_box: &gtk4::Box) {
    let root_box = root_box.clone();
    let start = std::time::Instant::now();
    const DURATION_MS: f32 = 600.0;
    const RISE_PX: f32 = 24.0;
    window.add_tick_callback(move |_w, _frame_clock| {
        let t = (start.elapsed().as_secs_f32() * 1000.0) / DURATION_MS;
        let t = t.clamp(0.0, 1.0);
        // Ease-out cubic.
        let e = 1.0 - (1.0 - t).powi(3);
        root_box.set_opacity(e as f64);
        root_box.set_margin_top((RISE_PX * (1.0 - e)) as i32);
        if t >= 1.0 {
            gtk4::glib::ControlFlow::Break
        } else {
            gtk4::glib::ControlFlow::Continue
        }
    });
}

/// Owns the single stateful connection to `$GREETD_SOCK` for one login
/// attempt and translates the UI's [`GreetdCommand`]s into greetd IPC
/// round-trips, forwarding each outcome back as an [`AppInput`].
fn spawn_greetd_actor(
    mut cmd_rx: mpsc::UnboundedReceiver<GreetdCommand>,
    sender: ComponentSender<App>,
) {
    relm4::spawn(async move {
        let mut client = match Client::connect().await {
            Ok(client) => client,
            Err(err) => {
                sender.input(AppInput::Error(format!("Cannot reach greetd: {err}")));
                return;
            }
        };

        while let Some(cmd) = cmd_rx.recv().await {
            let result = match cmd {
                GreetdCommand::CreateSession(username) => client.create_session(&username).await,
                GreetdCommand::Respond(answer) => client.respond(answer).await,
                GreetdCommand::StartSession { cmd, env } => {
                    match client.start_session(cmd, env).await {
                        Ok(()) => {
                            sender.input(AppInput::SessionStarted);
                            continue;
                        }
                        Err(err) => Err(err),
                    }
                }
            };

            match result {
                Ok(outcome) => sender.input(AppInput::Outcome(outcome)),
                Err(err) => {
                    tracing::warn!(%err, "greetd reported an error");
                    client.cancel_session().await;
                    sender.input(AppInput::Error(err.to_string()));
                }
            }
        }
    });
}

fn spawn_clock_ticker(sender: ComponentSender<App>) {
    relm4::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            sender.input(AppInput::ClockTick);
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

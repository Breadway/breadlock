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
}

struct App {
    clock_lbl: gtk4::Label,
    status_lbl: gtk4::Label,
    entry: gtk4::Entry,
    stage: Stage,
    username: String,
    session: Option<sessions::Session>,
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

            #[name = "root_box"]
            gtk4::Box {
                set_orientation: gtk4::Orientation::Vertical,
                set_halign: gtk4::Align::Center,
                set_valign: gtk4::Align::Center,
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
        let session = sessions::discover(
            &config.sessions.wayland_dirs,
            &config.sessions.xsessions_dirs,
            &config.sessions.default,
        );

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

        let session_lbl = gtk4::Label::new(session.as_ref().map(|s| s.name.as_str()));
        session_lbl.add_css_class("login-session");
        if session.is_none() {
            session_lbl.set_label("No session found — cannot log in");
        }

        let card = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
        card.add_css_class("login-card");
        card.append(&entry);
        card.append(&status_lbl);
        card.append(&session_lbl);

        let widgets = view_output!();
        widgets.root_box.append(&clock_lbl);
        widgets.root_box.append(&card);

        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        spawn_greetd_actor(cmd_rx, sender.clone());

        theme::apply();
        spawn_clock_ticker(sender.clone());

        let model = App {
            clock_lbl,
            status_lbl,
            entry,
            stage: Stage::Username,
            username: String::new(),
            session,
            clock_format: config.appearance.clock.format.clone(),
            cmd_tx,
        };
        model
            .clock_lbl
            .set_label(&current_time(&model.clock_format));

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
        let Some(session) = &self.session else {
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

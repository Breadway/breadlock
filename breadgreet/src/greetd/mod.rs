mod client;

pub use client::{AuthPrompt, Client, GreetdError, Outcome};

use std::future::Future;
use tokio::sync::mpsc;

/// Commands sent from the UI thread to the greetd actor, which owns the
/// single stateful connection to `$GREETD_SOCK`.
#[derive(Debug)]
pub enum Command {
    CreateSession(String),
    Respond(Option<String>),
    StartSession { cmd: Vec<String>, env: Vec<String> },
    CancelSession,
}

#[derive(Debug)]
pub enum Event {
    Outcome(Outcome),
    Error(String),
    SessionStarted,
}

/// Owns the greetd connection for the life of the greeter. Connect failures
/// and a later-dead socket are reported as [`Event::Error`]; the actor stays
/// alive and reconnects on the next command so the UI cannot freeze with a
/// dropped `cmd_rx`.
pub async fn run_actor<E>(cmd_rx: mpsc::UnboundedReceiver<Command>, emit: E)
where
    E: FnMut(Event) + Send + 'static,
{
    run_actor_with(cmd_rx, emit, Client::connect).await;
}

async fn run_actor_with<E, C, Fut>(
    mut cmd_rx: mpsc::UnboundedReceiver<Command>,
    mut emit: E,
    mut connect: C,
) where
    E: FnMut(Event),
    C: FnMut() -> Fut,
    Fut: Future<Output = Result<Client, GreetdError>>,
{
    let mut client: Option<Client> = match connect().await {
        Ok(c) => Some(c),
        Err(err) => {
            emit(Event::Error(format!("Cannot reach greetd: {err}")));
            None
        }
    };

    while let Some(cmd) = cmd_rx.recv().await {
        if matches!(cmd, Command::CancelSession) {
            if let Some(c) = client.as_mut() {
                c.cancel_session().await;
            }
            continue;
        }
        if client.is_none() {
            match connect().await {
                Ok(c) => client = Some(c),
                Err(err) => {
                    emit(Event::Error(format!("Cannot reach greetd: {err}")));
                    continue;
                }
            }
        }

        let result = exec_cmd(client.as_mut().expect("just connected"), cmd).await;
        match result {
            CmdResult::Idle => {}
            CmdResult::Started => emit(Event::SessionStarted),
            CmdResult::Roundtrip(Ok(outcome)) => emit(Event::Outcome(outcome)),
            CmdResult::Roundtrip(Err(err)) => {
                if is_connection_error(&err) {
                    client = None;
                } else if let Some(c) = client.as_mut() {
                    c.cancel_session().await;
                }
                emit(Event::Error(err.to_string()));
            }
        }
    }
}

enum CmdResult {
    Idle,
    Started,
    Roundtrip(Result<Outcome, GreetdError>),
}

async fn exec_cmd(client: &mut Client, cmd: Command) -> CmdResult {
    match cmd {
        Command::CancelSession => {
            client.cancel_session().await;
            CmdResult::Idle
        }
        Command::CreateSession(username) => {
            CmdResult::Roundtrip(client.create_session(&username).await)
        }
        Command::Respond(answer) => CmdResult::Roundtrip(client.respond(answer).await),
        Command::StartSession { cmd, env } => match client.start_session(cmd, env).await {
            Ok(()) => CmdResult::Started,
            Err(err) => CmdResult::Roundtrip(Err(err)),
        },
    }
}

fn is_connection_error(err: &GreetdError) -> bool {
    matches!(
        err,
        GreetdError::Connect(_) | GreetdError::Codec(_) | GreetdError::NoSocketEnv
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use greetd_ipc::codec::TokioCodec;
    use greetd_ipc::{Request, Response};
    use tokio::net::UnixListener;

    fn sock(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "breadgreet-actor-{name}-{}.sock",
            std::process::id()
        ))
    }

    #[tokio::test]
    async fn connect_failure_does_not_drop_the_actor() {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();

        let path = sock("retry");
        std::fs::remove_file(&path).ok();

        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let attempts_c = attempts.clone();
        let path_c = path.clone();

        let actor = tokio::spawn(async move {
            run_actor_with(
                cmd_rx,
                move |ev| {
                    let _ = ev_tx.send(ev);
                },
                move || {
                    let n = attempts_c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let path = path_c.clone();
                    async move {
                        if n == 0 {
                            Client::connect_to("/no/such/breadgreet-actor.sock").await
                        } else {
                            Client::connect_to(&path).await
                        }
                    }
                },
            )
            .await;
        });

        let ev = ev_rx.recv().await.expect("startup connect error");
        match ev {
            Event::Error(msg) => assert!(
                msg.contains("Cannot reach greetd"),
                "unexpected error: {msg}"
            ),
            other => panic!("expected Error, got {other:?}"),
        }

        let listener = UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = Request::read_from(&mut stream).await;
            Response::Success.write_to(&mut stream).await.unwrap();
        });

        cmd_tx
            .send(Command::CreateSession("bob".into()))
            .unwrap();
        let ev = ev_rx.recv().await.expect("actor should retry after bind");
        match ev {
            Event::Outcome(Outcome::Success) => {}
            other => panic!("expected Success, got {other:?}"),
        }

        drop(cmd_tx);
        server.await.unwrap();
        actor.await.unwrap();
        assert!(
            attempts.load(std::sync::atomic::Ordering::SeqCst) >= 2,
            "actor should reconnect after the first failed connect"
        );
        std::fs::remove_file(&path).ok();
    }
}

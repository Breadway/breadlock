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
    run_actor_with(cmd_rx, emit, Client::connect, DEFAULT_ROUNDTRIP_TIMEOUT).await;
}

/// Upper bound for a single greetd roundtrip. Without it, a hung PAM module
/// would leave the actor blocked on a `read_from` forever and the UI stuck on
/// the "Working" spinner. When it fires the conversation is cancelled and an
/// [`Event::Error`] is surfaced. Generous on purpose: a slow disk or a
/// deliberate password iterate shouldn't false-positive, but a wedged peer
/// must not wedge the greeter.
const DEFAULT_ROUNDTRIP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

async fn run_actor_with<E, C, Fut>(
    mut cmd_rx: mpsc::UnboundedReceiver<Command>,
    mut emit: E,
    mut connect: C,
    roundtrip_timeout: std::time::Duration,
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

        let result = match tokio::time::timeout(
            roundtrip_timeout,
            exec_cmd(client.as_mut().expect("just connected"), cmd),
        )
        .await
        {
            Ok(result) => result,
            // Hunger guard: the peer accepted our request but never answered.
            // Treat it as a wedged connection rather than going through the
            // normal `Roundtrip(Err)` handler, whose `cancel_session().await`
            // would itself block on a `read_from` against the same dead peer.
            // Dropping the client makes the next command reconnect fresh.
            Err(_elapsed) => {
                client = None;
                emit(Event::Error(format!(
                    "greetd did not respond within {roundtrip_timeout:?}"
                )));
                continue;
            }
        };
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
                std::time::Duration::from_secs(30),
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

        cmd_tx.send(Command::CreateSession("bob".into())).unwrap();
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

    #[tokio::test]
    async fn wedged_roundtrip_times_out_instead_of_hanging() {
        // A peer that accepts the request but never answers must not leave the
        // actor blocked on `read_from` forever; the roundtrip timeout fires,
        // the connection is dropped, and the UI is told so it can recover.
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();

        let path = sock("timeout");
        std::fs::remove_file(&path).ok();
        let listener = UnixListener::bind(&path).unwrap();

        let server = tokio::spawn(async move {
            // Accept and read the CreateSession request, then stay wedged: the
            // socket stays open (holding `stream`) but no response is ever
            // written, so the client hits its roundtrip timeout rather than
            // an EOF.
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = Request::read_from(&mut stream).await;
            std::future::pending::<()>().await;
        });

        let connect_path = path.clone();
        let actor = tokio::spawn(async move {
            run_actor_with(
                cmd_rx,
                move |ev| {
                    let _ = ev_tx.send(ev);
                },
                move || {
                    let connect_path = connect_path.clone();
                    async move { Client::connect_to(&connect_path).await }
                },
                std::time::Duration::from_millis(200),
            )
            .await;
        });

        // Let the listener bind and the actor connect, then drive the roundtrip.
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        cmd_tx.send(Command::CreateSession("bob".into())).unwrap();

        let ev = tokio::time::timeout(std::time::Duration::from_secs(5), ev_rx.recv())
            .await
            .expect("actor must surface a timeout promptly, not hang")
            .expect("actor must emit an event");
        match ev {
            Event::Error(msg) => assert!(
                msg.contains("did not respond"),
                "expected a roundtrip-timeout error, got {msg:?}"
            ),
            other => panic!("expected Error, got {other:?}"),
        }

        // The actor must not hang on a cancel read against the dead peer.
        drop(cmd_tx);
        actor.await.unwrap();
        // Abort the never-completing wedged server and reap it.
        server.abort();
        let _ = server.await;
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn roundtrip_connection_error_recovers_on_next_request() {
        // A roundtrip that dies mid-conversation (EOF) is a connection error:
        // the actor drops the client, surfaces the Error, and then reconnects
        // on the next command so a transient blip can't wedge the greeter.
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();

        let path = sock("roundtrip-recover");
        std::fs::remove_file(&path).ok();
        let listener = UnixListener::bind(&path).unwrap();

        let server = tokio::spawn(async move {
            // First connection: read the request, then hang up -> the client
            // sees EOF mid-roundtrip.
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = Request::read_from(&mut stream).await;
            drop(stream);
            // Second connection (after the actor reconnects): answer Success.
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = Request::read_from(&mut stream).await;
            let _ = Response::Success.write_to(&mut stream).await;
        });

        let connect_path = path.clone();
        let actor = tokio::spawn(async move {
            run_actor_with(
                cmd_rx,
                move |ev| {
                    let _ = ev_tx.send(ev);
                },
                move || {
                    let connect_path = connect_path.clone();
                    async move { Client::connect_to(&connect_path).await }
                },
                std::time::Duration::from_secs(30),
            )
            .await;
        });

        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        cmd_tx.send(Command::CreateSession("bob".into())).unwrap();
        let ev = ev_rx.recv().await.expect("first roundtrip error event");
        match ev {
            Event::Error(msg) => assert!(
                msg.contains("greetd IPC error"),
                "expected a connection (EOF) error, got {msg:?}"
            ),
            other => panic!("expected Error, got {other:?}"),
        }

        cmd_tx.send(Command::CreateSession("bob".into())).unwrap();
        let ev = ev_rx.recv().await.expect("recovery success event");
        match ev {
            Event::Outcome(Outcome::Success) => {}
            other => panic!("expected Success after reconnect, got {other:?}"),
        }

        drop(cmd_tx);
        server.await.unwrap();
        actor.await.unwrap();
        std::fs::remove_file(&path).ok();
    }
}

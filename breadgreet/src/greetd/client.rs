//! Thin async wrapper around `greetd_ipc`'s wire protocol — connects to
//! `$GREETD_SOCK`, and turns greetd's `Request`/`Response` enums into a
//! small state machine ([`Outcome`]) the UI drives.

use greetd_ipc::codec::TokioCodec;
use greetd_ipc::{AuthMessageType, ErrorType, Request, Response};
use tokio::net::UnixStream;

#[derive(Debug, thiserror::Error)]
pub enum GreetdError {
    #[error("$GREETD_SOCK is not set — breadgreet must be launched by greetd")]
    NoSocketEnv,
    #[error("failed to connect to greetd socket: {0}")]
    Connect(#[source] std::io::Error),
    #[error("greetd IPC error: {0}")]
    Codec(#[from] greetd_ipc::codec::Error),
    #[error("{description}")]
    Greetd {
        description: String,
        is_auth_error: bool,
    },
}

/// One step of a PAM conversation, as relayed by greetd.
#[derive(Debug, Clone)]
pub enum AuthPrompt {
    /// Answer should be shown as typed (e.g. a username confirmation).
    Visible(String),
    /// Answer should be masked (a password).
    Secret(String),
    /// Informational message — no answer needed, just display and continue.
    Info(String),
    /// Non-fatal error message from a PAM module — display and continue.
    Error(String),
}

#[derive(Debug, Clone)]
pub enum Outcome {
    /// Login flow complete — call [`Client::start_session`] next.
    Success,
    /// Another prompt to answer via [`Client::respond`].
    Prompt(AuthPrompt),
}

#[derive(Debug)]
pub struct Client {
    stream: UnixStream,
}

impl Client {
    /// Connects to the Unix socket greetd set in `$GREETD_SOCK` when it
    /// launched this process.
    pub async fn connect() -> Result<Self, GreetdError> {
        let path = std::env::var_os("GREETD_SOCK").ok_or(GreetdError::NoSocketEnv)?;
        Self::connect_to(path).await
    }

    /// Connects to an explicit socket path, bypassing `$GREETD_SOCK` —
    /// used directly by tests against a mock server so they don't need to
    /// mutate process-global environment state (which parallel `cargo test`
    /// threads would race on).
    async fn connect_to(path: impl AsRef<std::path::Path>) -> Result<Self, GreetdError> {
        let stream = UnixStream::connect(path)
            .await
            .map_err(GreetdError::Connect)?;
        Ok(Self { stream })
    }

    /// Starts a login attempt for `username`. Answer any resulting
    /// [`Outcome::Prompt`] via [`Self::respond`] until [`Outcome::Success`].
    pub async fn create_session(&mut self, username: &str) -> Result<Outcome, GreetdError> {
        let req = Request::CreateSession {
            username: username.to_string(),
        };
        self.roundtrip(req).await
    }

    /// Answers the most recent [`AuthPrompt`]. `None` for prompts that don't
    /// need an answer (info/error messages).
    pub async fn respond(&mut self, answer: Option<String>) -> Result<Outcome, GreetdError> {
        self.roundtrip(Request::PostAuthMessageResponse { response: answer })
            .await
    }

    /// Hands the chosen session off to greetd, which execs it and owns the
    /// VT switch away from the greeter. Only valid after an [`Outcome::Success`].
    pub async fn start_session(
        &mut self,
        cmd: Vec<String>,
        env: Vec<String>,
    ) -> Result<(), GreetdError> {
        Request::StartSession { cmd, env }
            .write_to(&mut self.stream)
            .await?;
        match Response::read_from(&mut self.stream).await? {
            Response::Success => Ok(()),
            Response::Error {
                error_type,
                description,
            } => Err(GreetdError::Greetd {
                description,
                is_auth_error: matches!(error_type, ErrorType::AuthError),
            }),
            Response::AuthMessage { .. } => Err(GreetdError::Greetd {
                description: "greetd sent an unexpected auth message after StartSession"
                    .to_string(),
                is_auth_error: false,
            }),
        }
    }

    /// Aborts an in-progress login flow (e.g. the user hit Escape). Per the
    /// protocol this can only be called before `StartSession` — best-effort,
    /// errors here don't matter to the caller since the flow is being torn
    /// down either way.
    pub async fn cancel_session(&mut self) {
        let _ = Request::CancelSession.write_to(&mut self.stream).await;
        let _ = Response::read_from(&mut self.stream).await;
    }

    async fn roundtrip(&mut self, req: Request) -> Result<Outcome, GreetdError> {
        req.write_to(&mut self.stream).await?;
        match Response::read_from(&mut self.stream).await? {
            Response::Success => Ok(Outcome::Success),
            Response::AuthMessage {
                auth_message_type,
                auth_message,
            } => Ok(Outcome::Prompt(match auth_message_type {
                AuthMessageType::Visible => AuthPrompt::Visible(auth_message),
                AuthMessageType::Secret => AuthPrompt::Secret(auth_message),
                AuthMessageType::Info => AuthPrompt::Info(auth_message),
                AuthMessageType::Error => AuthPrompt::Error(auth_message),
            })),
            Response::Error {
                error_type,
                description,
            } => Err(GreetdError::Greetd {
                description,
                is_auth_error: matches!(error_type, ErrorType::AuthError),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    //! Exercises the framing/state-machine logic against a mock Unix-socket
    //! server speaking the real greetd_ipc wire format — no real greetd or
    //! PAM involved. This is the safe way to test this module: a bug here
    //! just fails a test, it can never affect a real login.
    use super::*;
    use tokio::net::UnixListener;

    async fn mock_server(path: std::path::PathBuf, script: Vec<Response>) {
        let listener = UnixListener::bind(&path).unwrap();
        let (mut stream, _) = listener.accept().await.unwrap();
        for response in script {
            // Drain the request that prompted this response — we don't need
            // to inspect it, just keep the framing in lockstep.
            let _ = Request::read_from(&mut stream).await;
            response.write_to(&mut stream).await.unwrap();
        }
    }

    fn socket_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "breadgreet-test-{name}-{}.sock",
            std::process::id()
        ))
    }

    #[tokio::test]
    async fn create_session_success_flows_straight_through() {
        let path = socket_path("success");
        std::fs::remove_file(&path).ok();
        let server = tokio::spawn(mock_server(path.clone(), vec![Response::Success]));

        // Give the listener a moment to bind before connecting.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let mut client = Client::connect_to(&path).await.unwrap();
        let outcome = client.create_session("bob").await.unwrap();
        assert!(matches!(outcome, Outcome::Success));

        server.await.unwrap();
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn create_session_prompts_for_password_then_succeeds() {
        let path = socket_path("prompt");
        std::fs::remove_file(&path).ok();
        let server = tokio::spawn(mock_server(
            path.clone(),
            vec![
                Response::AuthMessage {
                    auth_message_type: AuthMessageType::Secret,
                    auth_message: "Password:".to_string(),
                },
                Response::Success,
            ],
        ));

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let mut client = Client::connect_to(&path).await.unwrap();

        let outcome = client.create_session("bob").await.unwrap();
        let Outcome::Prompt(AuthPrompt::Secret(msg)) = outcome else {
            panic!("expected a secret prompt, got {outcome:?}");
        };
        assert_eq!(msg, "Password:");

        let outcome = client.respond(Some("hunter2".to_string())).await.unwrap();
        assert!(matches!(outcome, Outcome::Success));

        server.await.unwrap();
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn auth_error_is_reported_as_such() {
        let path = socket_path("autherr");
        std::fs::remove_file(&path).ok();
        let server = tokio::spawn(mock_server(
            path.clone(),
            vec![Response::Error {
                error_type: ErrorType::AuthError,
                description: "denied".to_string(),
            }],
        ));

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let mut client = Client::connect_to(&path).await.unwrap();

        let err = client.create_session("bob").await.unwrap_err();
        match err {
            GreetdError::Greetd { is_auth_error, .. } => assert!(is_auth_error),
            other => panic!("expected a greetd auth error, got {other:?}"),
        }

        server.await.unwrap();
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn connect_without_greetd_sock_env_fails_cleanly() {
        std::env::remove_var("GREETD_SOCK");
        let err = Client::connect().await.unwrap_err();
        assert!(matches!(err, GreetdError::NoSocketEnv));
    }
}

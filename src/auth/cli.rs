//! Explicit, source-aware sign-in. MCP tool calls never enter this flow.
use crate::{
    auth::signin_instructions,
    config::{AuthSource, Config},
    error::Failure,
    server::TeamsServer,
};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::time::{Instant, sleep, timeout_at};

const LOGIN_WAIT: Duration = Duration::from_secs(900);

pub async fn login(config: &Config, wait: bool) -> Result<Value, Failure> {
    if config.auth_source == AuthSource::Headless {
        let mut result = crate::managed::login(config, wait).await?;
        result["authSource"] = json!("headless");
        result["authentication"] = if result["signedIn"] == true {
            json!(crate::auth::Authentication::verified())
        } else {
            json!({"state":"signing_in","sessionVerified":false})
        };
        return Ok(result);
    }
    eprintln!("{}", signin_instructions(config.auth_source));
    let mut cancellation = Cancellation::new()?;
    tokio::select! {
        biased;
        _ = cancellation.wait() => Err(Failure::new("CANCELLED", "Sign-in verification was cancelled. The existing browser or App was left open.")),
        result = await_existing(config, wait) => result,
    }
}

async fn await_existing(config: &Config, wait: bool) -> Result<Value, Failure> {
    let server = TeamsServer::new(config.clone());
    let deadline = Instant::now() + LOGIN_WAIT;
    let mut last_code = None;
    loop {
        let result = timeout_at(deadline, server.execute("teams_status", json!({}))).await;
        match result {
            Ok(Ok(mut status)) => {
                status["signedIn"] = json!(true);
                return Ok(status);
            }
            Ok(Err(error)) if wait && waitable(&error.code) => {
                if last_code.as_deref() != Some(error.code.as_str()) {
                    eprintln!(
                        "Waiting for the selected Teams session ({}). Complete setup in that browser or App; Ctrl-C cancels.",
                        error.code
                    );
                    last_code = Some(error.code);
                }
            }
            Ok(Err(error)) => return Err(error),
            Err(_) => return Err(login_timeout()),
        }
        if timeout_at(deadline, sleep(Duration::from_secs(2)))
            .await
            .is_err()
        {
            return Err(login_timeout());
        }
    }
}

fn waitable(code: &str) -> bool {
    matches!(
        code,
        "AUTH_REQUIRED"
            | "AUTH_EXPIRED"
            | "BROWSER_SIGNIN_REQUIRED"
            | "SESSION_UNAVAILABLE"
            | "TEAMS_TAB_REQUIRED"
            | "BROWSER_UNAVAILABLE"
            | "BROWSER_TIMEOUT"
            | "PAGE_CHANGED"
            | "APP_NOT_RUNNING"
            | "APP_TEAMS_TAB_REQUIRED"
            | "APP_TIMEOUT"
            | "APP_REQUEST_FAILED"
    )
}

fn login_timeout() -> Failure {
    Failure::new(
        "AUTH_LOGIN_TIMEOUT",
        "The selected Teams session was not verified within 15 minutes. The existing browser or App was left open. Complete sign-in there and retry explicit setup.",
    )
}

pub fn require_managed(config: &Config) -> Result<(), Failure> {
    if config.auth_source != AuthSource::Headless {
        return Err(Failure::new(
            "INVALID_INPUT",
            "This command manages only the dedicated browser. Select --auth-source headless explicitly. The existing browser or App was left unchanged.",
        ));
    }
    Ok(())
}

pub async fn stop(config: &Config) -> Result<Value, Failure> {
    require_managed(config)?;
    crate::managed::stop(config).await
}

pub async fn compact(config: &Config) -> Result<Value, Failure> {
    require_managed(config)?;
    crate::managed::compact(config).await
}

struct Cancellation {
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
}

impl Cancellation {
    fn new() -> Result<Self, Failure> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let failed = |_| {
                Failure::new(
                    "CANCELLED",
                    "Could not install sign-in cancellation handlers.",
                )
            };
            Ok(Self {
                interrupt: signal(SignalKind::interrupt()).map_err(failed)?,
                terminate: signal(SignalKind::terminate()).map_err(failed)?,
            })
        }
        #[cfg(not(unix))]
        Ok(Self {})
    }

    async fn wait(&mut self) {
        #[cfg(unix)]
        tokio::select! { _ = self.interrupt.recv() => {}, _ = self.terminate.recv() => {} }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

use super::state::{
    HEARTBEAT, Running, START_TIMEOUT, private_directory, profile, running, startup_lock,
    worker_stopped, write_private,
};
use super::{Shutdown, State, activity, ensure, failure, selection, stop_worker};
use crate::{config::Config, error::Failure};
use serde_json::{Value, json};
use std::{
    io::IsTerminal,
    path::Path,
    time::{Duration, Instant},
};
use tokio::{
    sync::oneshot,
    time::{sleep, timeout},
};

const LOGIN_WAIT: Duration = Duration::from_secs(900);

pub async fn login(config: &Config, wait: bool) -> Result<Value, Failure> {
    let path = profile(config)?;
    private_directory(&path)?;
    let mut shutdown = Shutdown::new()?;
    let _activity = tokio::select! {
        biased;
        _ = shutdown.wait() => return Err(cancelled()),
        guard = activity::setup_guard(&path, config.request_timeout) => guard?,
    };
    let config = selection::effective(config)?;
    transition_to_setup(&path).await?;
    let state = ensure(&config, &path, true).await?;
    if !wait {
        return Ok(
            json!({"started":true,"mode":"headed","message":"Complete normal Teams sign-in in the opened browser, then run teams-mcp --auth-source headless auth stop. Ordinary calls will reuse this profile headlessly."}),
        );
    }
    finish_login(&config, &path, &state, &mut shutdown).await
}

async fn transition_to_setup(path: &Path) -> Result<(), Failure> {
    let _startup = startup_lock(path).await?;
    if let Running::Ready(state) = settled_worker(path).await?
        && !state.headed
    {
        stop_worker(path, &state).await?;
    }
    Ok(())
}

/// A cancelled cold read can leave its owned browser finishing startup after its
/// activity guard is released. Let that worker publish before switching modes.
async fn settled_worker(path: &Path) -> Result<Running, Failure> {
    timeout(START_TIMEOUT, async {
        loop {
            match running(path)? {
                Running::Starting => sleep(Duration::from_millis(50)).await,
                state => return Ok(state),
            }
        }
    })
    .await
    .map_err(|_| failure("BROWSER_START_FAILED"))?
}

async fn finish_login(
    config: &Config,
    path: &Path,
    state: &State,
    shutdown: &mut Shutdown,
) -> Result<Value, Failure> {
    eprintln!(
        "Complete normal Teams sign-in in the opened browser window. Waiting up to 15 minutes."
    );
    let identity = tokio::select! {
        biased;
        _ = shutdown.wait() => Err(cancelled()),
        identity = await_signin(config, path, state) => identity,
    };
    let saved = identity
        .as_ref()
        .is_ok_and(|(email, tenant)| selection::save(path, email.clone(), tenant.clone()));
    let _startup = startup_lock(path).await?;
    stop_worker(path, state).await?;
    let (email, tenant) = identity?;
    Ok(
        json!({"signedIn":true,"account":email,"tenant":tenant,"selectionSaved":saved,"stopped":true}),
    )
}

fn cancelled() -> Failure {
    Failure::new(
        "CANCELLED",
        "Teams sign-in was cancelled. The setup window was closed without changing the selected account or tenant.",
    )
}

/// On a terminal the owner confirms the identity with Enter, allowing guests to
/// select their intended organization before its account/tenant pair is saved.
async fn await_signin(
    config: &Config,
    path: &Path,
    state: &State,
) -> Result<(String, String), Failure> {
    let interactive = std::io::stdin().is_terminal();
    let deadline = Instant::now() + LOGIN_WAIT;
    let mut announced = None;
    let mut enter = interactive.then(read_line);
    loop {
        if worker_stopped(path)? {
            return Err(failure("AUTH_LOGIN_CLOSED"));
        }
        write_private(&path.join(HEARTBEAT), state.nonce.as_bytes())?;
        let identity = selection::identity(config, &state.endpoint).await;
        let pressed = enter
            .as_mut()
            .is_some_and(|receiver| receiver.try_recv().is_ok());
        match (&identity, interactive, pressed) {
            (Some(current), false, _) | (Some(current), true, true) => return Ok(current.clone()),
            (None, true, true) => {
                eprintln!("Sign-in is not complete yet. Finish it in the browser window first.")
            }
            _ => {}
        }
        if pressed {
            enter = Some(read_line());
        }
        if let Some(current) = identity
            && announced.as_ref() != Some(&current)
        {
            eprintln!(
                "Signed in as {} (tenant {}). Switch organization in the browser window if needed, then press Enter to save and close it.",
                current.0, current.1
            );
            announced = Some(current);
        }
        if Instant::now() >= deadline {
            return Err(failure("AUTH_LOGIN_TIMEOUT"));
        }
        sleep(Duration::from_secs(2)).await;
    }
}

fn read_line() -> oneshot::Receiver<()> {
    let (sender, receiver) = oneshot::channel();
    // A pending terminal read must not delay process exit.
    std::thread::spawn(move || {
        let mut line = String::new();
        if std::io::stdin()
            .read_line(&mut line)
            .is_ok_and(|read| read > 0)
        {
            let _ = sender.send(());
        }
    });
    receiver
}

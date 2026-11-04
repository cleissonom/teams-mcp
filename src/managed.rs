use crate::{
    cdp::Cdp,
    config::{Config, endpoint_from_port_file},
    error::Failure,
};
use fs2::FileExt;
use serde_json::{Value, json};
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{
    process::{Child, Command},
    time::{sleep, timeout},
};
mod restore;
mod selection;
mod state;
use state::{
    ERROR, HEARTBEAT, Running, START_TIMEOUT, STATE, STOP, State, lock_file, modified,
    private_directory, profile, read_small, running, startup_lock, validate_private_profile,
    worker_stopped, write_private,
};

pub fn session_config(config: &Config) -> Result<Config, Failure> {
    selection::effective(config)
}

pub async fn endpoint(config: &Config) -> Result<String, Failure> {
    let config = session_config(config)?;
    let profile = profile(&config)?;
    validate_private_profile(&profile)?;
    if !profile.join("Default").is_dir() {
        return Err(failure("AUTH_SETUP_REQUIRED"));
    }
    Ok(ensure(&config, &profile, false).await?.endpoint)
}

pub async fn login(config: &Config) -> Result<Value, Failure> {
    let config = session_config(config)?;
    let profile = profile(&config)?;
    private_directory(&profile)?;
    ensure(&config, &profile, true).await?;
    Ok(
        json!({"started":true,"mode":"headed","message":"Complete normal Teams sign-in in the opened browser, then run teams-mcp auth stop. Ordinary calls will reuse this profile headlessly."}),
    )
}

pub async fn stop(config: &Config) -> Result<Value, Failure> {
    let profile = profile(config)?;
    if !profile.is_dir() {
        return Ok(json!({"stopped":false}));
    }
    let _startup = startup_lock(&profile).await?;
    let state = match running(&profile)? {
        Running::Ready(state) => state,
        Running::Stopped => return Ok(json!({"stopped":false})),
        Running::Starting => return Err(failure("BROWSER_BUSY")),
    };
    selection::persist(config, &profile, &state.endpoint).await;
    write_private(&profile.join(STOP), state.nonce.as_bytes())?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if worker_stopped(&profile)? {
            return Ok(json!({"stopped":true}));
        }
        if Instant::now() >= deadline {
            return Err(failure("BROWSER_STOP_TIMEOUT"));
        }
        sleep(Duration::from_millis(100)).await;
    }
}

async fn ensure(config: &Config, profile: &Path, headed: bool) -> Result<State, Failure> {
    let _startup = startup_lock(profile).await?;
    match running(profile)? {
        Running::Ready(state) => return touch(profile, state, headed),
        Running::Starting => return wait_ready(profile, headed).await,
        Running::Stopped => {}
    }
    if fs::symlink_metadata(profile.join("SingletonLock")).is_ok() {
        return Err(failure("BROWSER_PROFILE_IN_USE"));
    }
    let binary = browser_binary(config)?;
    let _ = fs::remove_file(profile.join(ERROR));
    spawn_worker(config, profile, &binary, headed)?;
    wait_ready(profile, headed).await
}

async fn wait_ready(profile: &Path, headed: bool) -> Result<State, Failure> {
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        if let Running::Ready(state) = running(profile)? {
            return touch(profile, state, headed);
        }
        if let Some(error) = read_small(&profile.join(ERROR))? {
            let value: Value =
                serde_json::from_str(&error).map_err(|_| failure("BROWSER_STATE_INVALID"))?;
            return Err(failure(
                value["code"].as_str().unwrap_or("BROWSER_START_FAILED"),
            ));
        }
        if Instant::now() >= deadline {
            return Err(failure("BROWSER_START_FAILED"));
        }
        sleep(Duration::from_millis(100)).await;
    }
}

fn touch(profile: &Path, state: State, headed: bool) -> Result<State, Failure> {
    if state.headed != headed {
        return Err(failure("BROWSER_BUSY"));
    }
    write_private(&profile.join(HEARTBEAT), state.nonce.as_bytes())?;
    Ok(state)
}

fn spawn_worker(
    config: &Config,
    profile: &Path,
    binary: &Path,
    headed: bool,
) -> Result<(), Failure> {
    let mut child = worker_command(config, profile, binary, headed)?
        .spawn()
        .map_err(|_| failure("BROWSER_START_FAILED"))?;
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    Ok(())
}

fn worker_command(
    config: &Config,
    profile: &Path,
    binary: &Path,
    headed: bool,
) -> Result<Command, Failure> {
    let mut command =
        Command::new(env::current_exe().map_err(|_| failure("BROWSER_START_FAILED"))?);
    command
        .arg("__browser-worker")
        .env("TEAMS_BROWSER_PROFILE", profile)
        .env("TEAMS_BROWSER_BINARY", binary);
    for (name, value) in [
        ("TEAMS_ACCOUNT", &config.account),
        ("TEAMS_TENANT_ID", &config.tenant),
    ] {
        if let Some(value) = value {
            command.env(name, value);
        } else {
            command.env_remove(name);
        }
    }
    if headed {
        command.arg("--headed");
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    command.process_group(0);
    Ok(command)
}

pub async fn worker(config: &Config, headed: bool) -> Result<(), Failure> {
    let config = session_config(config)?;
    let profile = profile(&config)?;
    if !headed && !profile.join("Default").is_dir() {
        return Err(failure("AUTH_SETUP_REQUIRED"));
    }
    private_directory(&profile)?;
    let result = run_worker(&config, &profile, headed).await;
    if let Err(error) = &result {
        let _ = write_private(
            &profile.join(ERROR),
            json!({"code":error.code}).to_string().as_bytes(),
        );
    }
    result
}

async fn run_worker(config: &Config, profile: &Path, headed: bool) -> Result<(), Failure> {
    let mut shutdown = Shutdown::new()?;
    let ownership = lock_file(&profile.join(".teams-mcp-worker.lock"))?;
    ownership
        .try_lock_exclusive()
        .map_err(|_| failure("BROWSER_BUSY"))?;
    // A newly owned worker must not advertise a previous worker's endpoint while starting.
    for name in [STATE, HEARTBEAT, STOP] {
        match fs::remove_file(profile.join(name)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(failure("BROWSER_STATE_INVALID")),
        }
    }
    if fs::symlink_metadata(profile.join("SingletonLock")).is_ok() {
        return Err(failure("BROWSER_PROFILE_IN_USE"));
    }
    let previous = read_small(&profile.join("DevToolsActivePort"))?;
    let mut child = chrome_command(
        &browser_binary(config)?,
        profile,
        headed,
        &selection::startup_url(config)?,
    )
    .spawn()
    .map_err(|_| failure("BROWSER_START_FAILED"))?;
    let mut known_endpoint = None;
    let startup: Result<Option<State>, Failure> = async {
        let endpoint = tokio::select! {
            endpoint=fresh_endpoint(profile,&mut child,previous.as_deref())=>Some(endpoint?),
            _=shutdown.wait()=>None,
        };
        let Some(endpoint) = endpoint else {
            return Ok(None);
        };
        known_endpoint = Some(endpoint.clone());
        if !headed {
            restore::prepare(&endpoint, &selection::startup_url(config)?).await?;
        }
        state::publish(profile, endpoint, headed).map(Some)
    }
    .await;
    if let Ok(Some(state)) = &startup {
        supervise(profile, state, &mut child, &mut shutdown).await;
    }
    if let Some(endpoint) = known_endpoint {
        close_owned(&endpoint, &mut child).await;
    } else {
        terminate_owned(&mut child).await;
    }
    for name in [STATE, HEARTBEAT, STOP, ERROR] {
        let _ = fs::remove_file(profile.join(name));
    }
    drop(ownership);
    startup.map(|_| ())
}

fn chrome_command(binary: &Path, profile: &Path, headed: bool, url: &str) -> Command {
    let mut command = Command::new(binary);
    command
        .arg(format!("--user-data-dir={}", profile.display()))
        .args([
            "--remote-debugging-address=127.0.0.1",
            "--remote-debugging-port=0",
            "--no-first-run",
            "--no-default-browser-check",
        ]);
    if !headed {
        command.args(["--headless=new", "--restore-last-session"]);
    } else {
        command.arg(url);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    command
}

async fn fresh_endpoint(
    profile: &Path,
    child: &mut Child,
    previous: Option<&str>,
) -> Result<String, Failure> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if child
            .try_wait()
            .map_err(|_| failure("BROWSER_START_FAILED"))?
            .is_some()
        {
            return Err(failure("BROWSER_START_FAILED"));
        }
        if let Some(contents) = read_small(&profile.join("DevToolsActivePort"))?
            && Some(contents.as_str()) != previous
            && let Ok(endpoint) = endpoint_from_port_file(&contents)
        {
            return Ok(endpoint);
        }
        if Instant::now() >= deadline {
            return Err(failure("BROWSER_START_FAILED"));
        }
        sleep(Duration::from_millis(100)).await;
    }
}

async fn supervise(profile: &Path, state: &State, child: &mut Child, shutdown: &mut Shutdown) {
    let idle = Duration::from_secs(if state.headed { 900 } else { 300 });
    let mut last_use = Instant::now();
    let mut seen = modified(&profile.join(HEARTBEAT));
    loop {
        if read_small(&profile.join(STOP)).ok().flatten().as_deref() == Some(&state.nonce)
            || child.try_wait().ok().flatten().is_some()
        {
            break;
        }
        let current = modified(&profile.join(HEARTBEAT));
        if current != seen
            && read_small(&profile.join(HEARTBEAT))
                .ok()
                .flatten()
                .as_deref()
                == Some(&state.nonce)
        {
            seen = current;
            last_use = Instant::now();
        }
        if last_use.elapsed() >= idle {
            break;
        }
        tokio::select! { _=sleep(Duration::from_secs(1))=>{}, _=shutdown.wait()=>break }
    }
}

async fn close_owned(endpoint: &str, child: &mut Child) {
    let _ = timeout(Duration::from_secs(3), async {
        let mut cdp = Cdp::connect(endpoint).await?;
        cdp.call("Browser.close", json!({}), None).await
    })
    .await;
    if timeout(Duration::from_secs(3), child.wait()).await.is_err() {
        let _ = child.kill().await;
    }
}

async fn terminate_owned(child: &mut Child) {
    #[cfg(unix)]
    if matches!(child.try_wait(), Ok(None))
        && let Some(pid) = child
            .id()
            .and_then(|id| libc::pid_t::try_from(id).ok())
            .filter(|pid| *pid > 0)
    {
        // An owned, unreaped child keeps its PID reserved; no PID from disk is trusted.
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
    }
    if timeout(Duration::from_secs(3), child.wait()).await.is_err() {
        let _ = child.start_kill();
        let _ = timeout(Duration::from_secs(3), child.wait()).await;
    }
}

fn browser_binary(config: &Config) -> Result<PathBuf, Failure> {
    let binary = config
        .browser_binary
        .clone()
        .or_else(|| {
            let candidates = if cfg!(target_os = "macos") {
                vec!["/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"]
            } else if cfg!(target_os = "linux") {
                vec![
                    "/usr/bin/google-chrome",
                    "/usr/bin/google-chrome-stable",
                    "/usr/bin/chromium",
                    "/usr/bin/chromium-browser",
                ]
            } else {
                vec![]
            };
            candidates
                .into_iter()
                .map(PathBuf::from)
                .find(|path| path.is_file())
        })
        .ok_or_else(|| failure("BROWSER_NOT_FOUND"))?;
    if !binary.is_absolute() || !binary.is_file() {
        return Err(failure("BROWSER_NOT_FOUND"));
    }
    Ok(binary)
}

struct Shutdown {
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
}

impl Shutdown {
    fn new() -> Result<Self, Failure> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            Ok(Self {
                interrupt: signal(SignalKind::interrupt())
                    .map_err(|_| failure("BROWSER_START_FAILED"))?,
                terminate: signal(SignalKind::terminate())
                    .map_err(|_| failure("BROWSER_START_FAILED"))?,
            })
        }
        #[cfg(not(unix))]
        Ok(Self {})
    }

    async fn wait(&mut self) {
        #[cfg(unix)]
        tokio::select! { _=self.interrupt.recv()=>{}, _=self.terminate.recv()=>{} }
        #[cfg(not(unix))]
        let _ = tokio::signal::ctrl_c().await;
    }
}

fn failure(code: &str) -> Failure {
    let (code, message) = match code {
        "AUTH_SETUP_REQUIRED" => (
            code,
            "Run teams-mcp auth login once to create the dedicated Teams browser session. No browser was started.",
        ),
        "BROWSER_BUSY" => (
            code,
            "The managed browser is starting or running in another mode. Finish sign-in if needed, then run teams-mcp auth stop and retry.",
        ),
        "BROWSER_PROFILE_IN_USE" => (
            code,
            "This browser profile is already open outside the connector. Close that browser normally or select a dedicated TEAMS_BROWSER_PROFILE. Its locks were preserved.",
        ),
        "BROWSER_PROFILE_INSECURE" => (
            code,
            "This browser profile permits access by other users. Select an owner-only private TEAMS_BROWSER_PROFILE or correct its permissions explicitly. No permissions were changed.",
        ),
        "BROWSER_NOT_FOUND" => (
            code,
            "Install Google Chrome or set TEAMS_BROWSER_BINARY to its absolute executable path.",
        ),
        "BROWSER_INVALID_CONFIGURATION" => (
            code,
            "Set an absolute TEAMS_BROWSER_PROFILE path or a valid home directory.",
        ),
        "BROWSER_STATE_INVALID" => (
            code,
            "Cannot validate the managed browser's private ownership state. Stop it and check profile access.",
        ),
        "AUTH_SELECTION_INVALID" => (
            code,
            "The managed account selection is invalid. Provide TEAMS_ACCOUNT and TEAMS_TENANT_ID together to replace it after successful sign-in.",
        ),
        "ACCOUNT_SELECTION_CONFLICT" => (
            code,
            "The supplied account or tenant conflicts with the saved session. Set both TEAMS_ACCOUNT and TEAMS_TENANT_ID explicitly to select another session.",
        ),
        "BROWSER_RESTORE_FAILED" => (
            code,
            "The managed browser could not restore its Teams page. Run explicit authentication setup and retry.",
        ),
        "BROWSER_STOP_TIMEOUT" => (
            code,
            "The owned browser did not stop within ten seconds. Retry teams-mcp auth stop.",
        ),
        _ => (
            "BROWSER_START_FAILED",
            "The managed browser could not start. Check the installed browser and profile access, then retry explicit setup.",
        ),
    };
    Failure::new(code, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn worker_selection_is_explicitly_set_or_removed_instead_of_inheriting_environment() {
        for configured in [true, false] {
            let config = Config {
                account: configured.then(|| "selected@example.test".into()),
                tenant: configured.then(|| "22222222-2222-4222-8222-222222222222".into()),
                ..Default::default()
            };
            let command = worker_command(
                &config,
                Path::new("/fixture/profile"),
                Path::new("/fixture/chrome"),
                false,
            )
            .unwrap();
            for (name, expected) in [
                ("TEAMS_ACCOUNT", config.account.as_deref()),
                ("TEAMS_TENANT_ID", config.tenant.as_deref()),
            ] {
                let value = command
                    .as_std()
                    .get_envs()
                    .find(|(key, _)| *key == name)
                    .unwrap()
                    .1
                    .and_then(std::ffi::OsStr::to_str);
                assert_eq!(value, expected);
            }
        }
    }
}

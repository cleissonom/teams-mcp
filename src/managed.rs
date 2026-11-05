use crate::{
    cdp::{Cdp, select_target},
    config::{BrowserKind, Config, endpoint_from_port_file},
    error::Failure,
};
use serde_json::{Value, json};
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    process::{Child, Command},
    time::{sleep, timeout},
};
mod activity;
mod login;
mod restore;
mod selection;
mod state;
pub use activity::request_guard;
pub use login::login;
use state::{
    ERROR, FROZEN, HEARTBEAT, Running, START_TIMEOUT, STATE, STOP, State, idle_seconds, lock_file,
    log_event, modified, private_directory, profile, profile_in_use, read_small, running,
    startup_lock, validate_private_profile, worker_stopped, write_private,
};

const TEAMS_ORIGINS: [&str; 2] = [
    "https://teams.microsoft.com",
    "https://teams.cloud.microsoft",
];
/// Interactive setup windows close after this much connector inactivity.
const SETUP_IDLE: Duration = Duration::from_secs(900);
/// Chrome features that download models or run services Teams does not use. Security
/// components (Safe Browsing, certificate and CT updates) are deliberately left enabled.
const DISABLED_FEATURES: &str = "--disable-features=OptimizationGuideModelDownloading,OptimizationHintsFetching,OptimizationTargetPrediction,OptimizationHints,Translate,MediaRouter";

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

/// Records tool activity so the supervisor neither freezes nor stops a browser in use.
pub fn keepalive(config: &Config) -> Result<(), Failure> {
    let profile = profile(config)?;
    if !profile.is_dir() {
        return Ok(());
    }
    if let Running::Ready(state) = running(&profile)? {
        write_private(&profile.join(HEARTBEAT), state.nonce.as_bytes())?;
    }
    Ok(())
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
    let saved = selection::persist(config, &profile, &state.endpoint).await;
    stop_worker(&profile, &state).await?;
    Ok(json!({"stopped":true,"selectionSaved":saved}))
}

async fn stop_worker(profile: &Path, state: &State) -> Result<(), Failure> {
    write_private(&profile.join(STOP), state.nonce.as_bytes())?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if worker_stopped(profile)? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(failure("BROWSER_STOP_TIMEOUT"));
        }
        sleep(Duration::from_millis(100)).await;
    }
}

/// Local diagnostics only: never starts Chrome or contacts Teams.
pub fn status(config: &Config) -> Result<Value, Failure> {
    let config = session_config(config)?;
    let profile = profile(&config)?;
    validate_private_profile(&profile)?;
    let mut browser = json!({"state":"stopped"});
    if profile.is_dir() {
        match running(&profile)? {
            Running::Stopped => {}
            Running::Starting => browser = json!({"state":"starting"}),
            Running::Ready(state) => {
                let frozen =
                    read_small(&profile.join(FROZEN))?.as_deref() == Some(state.nonce.as_str());
                let stops = if state.headed {
                    SETUP_IDLE.as_secs()
                } else {
                    state.idle_secs
                };
                browser = json!({"state":"running","mode":if state.headed {"headed"} else {"headless"},
                    "frozen":frozen,"idleSeconds":idle_seconds(&profile),"stopsAfterIdleSeconds":stops,
                    "freezesAfterIdleSeconds":state.freeze_secs.filter(|_| !state.headed)});
            }
        }
    }
    Ok(
        json!({"setupComplete":profile.join("Default").is_dir(),"profileExists":profile.is_dir(),
        "authentication":{"state":if activity::setup_pending(&profile)? || browser["mode"] == "headed" {"signing_in"} else if profile.join("Default").is_dir() {"unverified"} else {"sign_in_required"},"sessionVerified":false},
        "account":config.account,"tenant":config.tenant,"profile":profile.display().to_string(),"browser":browser}),
    )
}

/// Clears only the Teams Service Worker cache through Chrome. Cookies, local storage
/// and IndexedDB (where the sign-in lives) are untouched.
pub async fn compact(config: &Config) -> Result<Value, Failure> {
    let _activity = request_guard(config).await?;
    let endpoint = endpoint(config).await?;
    let cdp = Cdp::connect(&endpoint).await?;
    let session = teams_page_session(&cdp, config.target_id.as_deref()).await?;
    let result = async {
        let before = cache_storage_bytes(&cdp, &session).await?;
        clear_cache_storage(&cdp, &session).await?;
        let after = cache_storage_bytes(&cdp, &session).await?;
        Ok::<_, Failure>((before, after))
    }
    .await;
    let _ = cdp
        .call(
            "Target.detachFromTarget",
            json!({"sessionId":session}),
            None,
        )
        .await;
    let (before, after) = result?;
    let mib = |bytes: u64| (bytes as f64 / 1_048_576.0 * 10.0).round() / 10.0;
    Ok(
        json!({"compacted":true,"cacheStorageBeforeMiB":mib(before),"cacheStorageAfterMiB":mib(after),
        "message":"Cleared Teams' Service Worker cache only. Sign-in data was kept; the next cold start downloads Teams files again."}),
    )
}

/// Chrome's Storage domain answers only on a page session, so storage calls attach to the Teams page.
async fn teams_page_session(cdp: &Cdp, selected: Option<&str>) -> Result<String, Failure> {
    let targets = cdp.call("Target.getTargets", json!({}), None).await?;
    let target = select_target(&targets, selected)?;
    let attached = cdp
        .call(
            "Target.attachToTarget",
            json!({"targetId":target,"flatten":true}),
            None,
        )
        .await?;
    attached["sessionId"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| failure("BROWSER_COMPACT_FAILED"))
}

async fn cache_storage_bytes(cdp: &Cdp, session: &str) -> Result<u64, Failure> {
    let mut total = 0;
    for origin in TEAMS_ORIGINS {
        let usage = cdp
            .call(
                "Storage.getUsageAndQuota",
                json!({"origin":origin}),
                Some(session),
            )
            .await
            .map_err(|_| failure("BROWSER_COMPACT_FAILED"))?;
        total += usage["usageBreakdown"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|entry| entry["storageType"] == "cache_storage")
            .filter_map(|entry| entry["usage"].as_f64())
            .sum::<f64>() as u64;
    }
    Ok(total)
}

async fn clear_cache_storage(cdp: &Cdp, session: &str) -> Result<(), Failure> {
    for origin in TEAMS_ORIGINS {
        cdp.call(
            "Storage.clearDataForOrigin",
            json!({"origin":origin,"storageTypes":"cache_storage"}),
            Some(session),
        )
        .await
        .map_err(|_| failure("BROWSER_COMPACT_FAILED"))?;
    }
    Ok(())
}

async fn ensure(config: &Config, profile: &Path, headed: bool) -> Result<State, Failure> {
    let _startup = startup_lock(profile).await?;
    match running(profile)? {
        Running::Ready(state) => return touch(profile, state, headed),
        Running::Starting => return wait_ready(profile, headed).await,
        Running::Stopped => {}
    }
    if profile_in_use(profile) {
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
        log_event(&profile, config.log, &format!("error code={}", error.code));
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
    ownership.try_lock().map_err(|_| failure("BROWSER_BUSY"))?;
    // A newly owned worker must not advertise a previous worker's endpoint while starting.
    for name in [STATE, HEARTBEAT, STOP, FROZEN] {
        match fs::remove_file(profile.join(name)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(failure("BROWSER_STATE_INVALID")),
        }
    }
    if profile_in_use(profile) {
        return Err(failure("BROWSER_PROFILE_IN_USE"));
    }
    let started = Instant::now();
    log_event(
        profile,
        config.log,
        &format!("start headed={headed} lean={}", config.lean),
    );
    let previous = read_small(&profile.join("DevToolsActivePort"))?;
    let mut child = chrome_command(
        &browser_binary(config)?,
        profile,
        headed,
        config.lean,
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
        state::publish(profile, endpoint, headed, config.idle, config.freeze).map(Some)
    }
    .await;
    if let Ok(Some(state)) = &startup {
        log_event(
            profile,
            config.log,
            &format!("ready ms={}", started.elapsed().as_millis()),
        );
        supervise(config, profile, state, &mut child, &mut shutdown).await;
    }
    if let Some(endpoint) = known_endpoint {
        close_owned(config, profile, &endpoint, &mut child).await;
    } else {
        terminate_owned(&mut child).await;
    }
    for name in [STATE, HEARTBEAT, STOP, ERROR, FROZEN] {
        let _ = fs::remove_file(profile.join(name));
    }
    log_event(profile, config.log, "exit");
    drop(ownership);
    startup.map(|_| ())
}

fn chrome_command(binary: &Path, profile: &Path, headed: bool, lean: bool, url: &str) -> Command {
    let mut command = Command::new(binary);
    command
        .arg(format!("--user-data-dir={}", profile.display()))
        .args([
            "--remote-debugging-address=127.0.0.1",
            "--remote-debugging-port=0",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-sync",
            DISABLED_FEATURES,
        ]);
    if !headed {
        command.args(["--headless=new", "--restore-last-session", "--mute-audio"]);
        if lean {
            // Opt-in: skip image decoding and drop Microsoft telemetry hosts in the hidden page.
            command.args([
                "--blink-settings=imagesEnabled=false",
                "--host-resolver-rules=MAP *.events.data.microsoft.com ~NOTFOUND, MAP *.aria.microsoft.com ~NOTFOUND",
            ]);
        }
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

async fn supervise(
    config: &Config,
    profile: &Path,
    state: &State,
    child: &mut Child,
    shutdown: &mut Shutdown,
) {
    let idle = if state.headed {
        SETUP_IDLE
    } else {
        config.idle
    };
    let freeze_after = config.freeze.filter(|_| !state.headed);
    let mut freezer = Freezer::default();
    let mut retry_freeze = Instant::now();
    let mut last_use = Instant::now();
    let mut seen = modified(&profile.join(HEARTBEAT));
    let active = |profile: &Path| {
        read_small(&profile.join(HEARTBEAT))
            .ok()
            .flatten()
            .as_deref()
            == Some(&state.nonce)
    };
    loop {
        if read_small(&profile.join(STOP)).ok().flatten().as_deref() == Some(&state.nonce)
            || child.try_wait().ok().flatten().is_some()
        {
            break;
        }
        let current = modified(&profile.join(HEARTBEAT));
        if current != seen && active(profile) {
            seen = current;
            last_use = Instant::now();
            if freezer.frozen {
                // A client made the page active before calling it.
                freezer.frozen = false;
                let _ = fs::remove_file(profile.join(FROZEN));
                log_event(profile, config.log, "active");
            }
        }
        if last_use.elapsed() >= idle {
            log_event(profile, config.log, "idle-stop");
            break;
        }
        if let Some(after) = freeze_after
            && !freezer.frozen
            && last_use.elapsed() >= after
            && Instant::now() >= retry_freeze
            // Recheck activity immediately before freezing; clients also re-activate while running.
            && modified(&profile.join(HEARTBEAT)) == seen
        {
            match freezer
                .freeze(&state.endpoint, config.target_id.as_deref())
                .await
            {
                Ok(()) => {
                    let _ = write_private(&profile.join(FROZEN), state.nonce.as_bytes());
                    log_event(profile, config.log, "frozen");
                }
                Err(error) => {
                    retry_freeze = Instant::now() + Duration::from_secs(30);
                    log_event(
                        profile,
                        config.log,
                        &format!("freeze-skipped code={}", error.code),
                    );
                }
            }
        }
        tokio::select! { _=sleep(Duration::from_secs(1))=>{}, _=shutdown.wait()=>break }
    }
}

/// Keeps one supervisor CDP session attached to the managed Teams page while it is frozen.
#[derive(Default)]
struct Freezer {
    cdp: Option<Arc<Cdp>>,
    page: Option<(String, String)>,
    frozen: bool,
}

impl Freezer {
    async fn freeze(&mut self, endpoint: &str, selected: Option<&str>) -> Result<(), Failure> {
        let result = timeout(
            Duration::from_secs(5),
            self.freeze_inner(endpoint, selected),
        )
        .await;
        match result {
            Ok(Ok(())) => {
                self.frozen = true;
                Ok(())
            }
            Ok(Err(error)) => {
                self.page = None;
                Err(error)
            }
            Err(_) => {
                self.page = None;
                Err(failure("BROWSER_FREEZE_FAILED"))
            }
        }
    }

    async fn freeze_inner(
        &mut self,
        endpoint: &str,
        selected: Option<&str>,
    ) -> Result<(), Failure> {
        if !self.cdp.as_ref().is_some_and(|cdp| cdp.is_open()) {
            self.cdp = Some(Cdp::connect(endpoint).await?);
            self.page = None;
        }
        let cdp = self
            .cdp
            .clone()
            .ok_or_else(|| failure("BROWSER_FREEZE_FAILED"))?;
        let targets = cdp.call("Target.getTargets", json!({}), None).await?;
        let target = select_target(&targets, selected)?;
        let session = match &self.page {
            Some((attached, session)) if *attached == target => session.clone(),
            _ => {
                let attached = cdp
                    .call(
                        "Target.attachToTarget",
                        json!({"targetId":target,"flatten":true}),
                        None,
                    )
                    .await?;
                let session = attached["sessionId"]
                    .as_str()
                    .ok_or_else(|| failure("BROWSER_FREEZE_FAILED"))?
                    .to_owned();
                self.page = Some((target, session.clone()));
                session
            }
        };
        cdp.call(
            "Page.setWebLifecycleState",
            json!({"state":"frozen"}),
            Some(&session),
        )
        .await?;
        Ok(())
    }
}

async fn close_owned(config: &Config, profile: &Path, endpoint: &str, child: &mut Child) {
    let _ = timeout(Duration::from_secs(6), async {
        let cdp = Cdp::connect(endpoint).await?;
        if let Some(limit) = config.cache_limit_mb {
            trim_cache(&cdp, limit, config, profile).await;
        }
        cdp.call("Browser.close", json!({}), None).await
    })
    .await;
    if timeout(Duration::from_secs(3), child.wait()).await.is_err() {
        let _ = child.kill().await;
    }
}

/// Clears Teams' Service Worker cache at shutdown once it exceeds the configured limit.
async fn trim_cache(cdp: &Cdp, limit_mb: u64, config: &Config, profile: &Path) {
    let Ok(session) = teams_page_session(cdp, config.target_id.as_deref()).await else {
        return;
    };
    if let Ok(bytes) = cache_storage_bytes(cdp, &session).await
        && bytes > limit_mb * 1_048_576
        && clear_cache_storage(cdp, &session).await.is_ok()
    {
        log_event(
            profile,
            config.log,
            &format!("cache-trim mib={}", bytes / 1_048_576),
        );
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

/// Installed Chromium browsers in preference order: Chrome, Edge, Chromium, Brave.
fn browser_candidates(kind: Option<BrowserKind>) -> Vec<PathBuf> {
    use BrowserKind::*;
    let wanted = |candidate: BrowserKind| kind.is_none_or(|kind| kind == candidate);
    let mut paths = Vec::new();
    if cfg!(target_os = "macos") {
        let home = env::var_os("HOME").map(PathBuf::from);
        for (candidate, app) in [
            (Chrome, "Google Chrome.app/Contents/MacOS/Google Chrome"),
            (Edge, "Microsoft Edge.app/Contents/MacOS/Microsoft Edge"),
            (Chromium, "Chromium.app/Contents/MacOS/Chromium"),
            (Brave, "Brave Browser.app/Contents/MacOS/Brave Browser"),
        ] {
            if wanted(candidate) {
                paths.push(Path::new("/Applications").join(app));
                paths.extend(home.iter().map(|home| home.join("Applications").join(app)));
            }
        }
    } else if cfg!(windows) {
        let roots: Vec<PathBuf> = ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"]
            .iter()
            .filter_map(|name| env::var_os(name).map(PathBuf::from))
            .collect();
        for (candidate, executable) in [
            (Chrome, "Google/Chrome/Application/chrome.exe"),
            (Edge, "Microsoft/Edge/Application/msedge.exe"),
            (Chromium, "Chromium/Application/chrome.exe"),
            (Brave, "BraveSoftware/Brave-Browser/Application/brave.exe"),
        ] {
            if wanted(candidate) {
                paths.extend(roots.iter().map(|root| root.join(executable)));
            }
        }
    } else {
        let mut directories: Vec<PathBuf> = env::var_os("PATH")
            .map(|path| env::split_paths(&path).collect())
            .unwrap_or_default();
        directories.extend(["/usr/bin", "/snap/bin"].map(PathBuf::from));
        for (candidate, names) in [
            (Chrome, &["google-chrome", "google-chrome-stable"][..]),
            (Edge, &["microsoft-edge", "microsoft-edge-stable"][..]),
            (Chromium, &["chromium", "chromium-browser"][..]),
            (Brave, &["brave-browser", "brave"][..]),
        ] {
            if wanted(candidate) {
                for name in names {
                    paths.extend(directories.iter().map(|directory| directory.join(name)));
                }
            }
        }
    }
    paths
}

fn browser_binary(config: &Config) -> Result<PathBuf, Failure> {
    let binary = config
        .browser_binary
        .clone()
        .or_else(|| {
            browser_candidates(config.browser)
                .into_iter()
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
            "Run teams-mcp --auth-source headless auth login to create or renew the dedicated Teams browser session. No browser was started.",
        ),
        "BROWSER_BUSY" => (
            code,
            "The managed session is being prepared or explicit sign-in is in progress. Wait for it to finish, then retry the original request.",
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
            "Install Google Chrome, Microsoft Edge, Chromium or Brave, or set TEAMS_BROWSER_BINARY to its absolute executable path.",
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
            "The owned browser did not stop within ten seconds. Retry teams-mcp --auth-source headless auth stop.",
        ),
        "AUTH_LOGIN_TIMEOUT" => (
            code,
            "Sign-in did not complete within 15 minutes. The setup window was closed without changing the selected account or tenant; retry explicit sign-in when ready.",
        ),
        "AUTH_LOGIN_CLOSED" => (
            code,
            "The setup browser closed before sign-in completed. Run teams-mcp --auth-source headless auth login again.",
        ),
        "BROWSER_FREEZE_FAILED" => (
            code,
            "The idle Teams page could not be frozen; it keeps running until idle shutdown.",
        ),
        "BROWSER_COMPACT_FAILED" => (
            code,
            "Chrome could not report or clear the Teams cache. No sign-in data was changed; retry later.",
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

    #[test]
    fn lean_mode_is_opt_in_and_never_weakens_browser_security() {
        for (headed, lean) in [(false, false), (false, true), (true, true)] {
            let command = chrome_command(
                Path::new("/fixture/chrome"),
                Path::new("/fixture/profile"),
                headed,
                lean,
                "https://teams.microsoft.com/v2/",
            );
            let args: Vec<_> = command
                .as_std()
                .get_args()
                .filter_map(|arg| arg.to_str())
                .collect();
            assert!(args.contains(&"--disable-sync"));
            assert_eq!(
                args.iter().any(|arg| arg.starts_with("--blink-settings")),
                lean && !headed
            );
            for weakening in [
                "--no-sandbox",
                "--disable-web-security",
                "--disable-component-update",
                "--disable-background-networking",
                "--disable-extensions",
            ] {
                assert!(!args.contains(&weakening), "{weakening}");
            }
        }
    }

    #[test]
    #[cfg(unix)]
    fn only_a_dead_same_host_singleton_lock_is_treated_as_stale() {
        let profile = std::env::temp_dir().join(format!("teams-lock-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&profile).unwrap();
        let lock = profile.join("SingletonLock");
        assert!(!profile_in_use(&profile));
        let host = state_hostname();
        for (target, in_use) in [
            (format!("{host}-{}", std::process::id()), true),
            (format!("{host}-999999"), false),
            ("other-host.invalid-999999".to_owned(), true),
            ("unparseable".to_owned(), true),
        ] {
            let _ = fs::remove_file(&lock);
            std::os::unix::fs::symlink(&target, &lock).unwrap();
            assert_eq!(profile_in_use(&profile), in_use, "{target}");
            assert!(
                fs::symlink_metadata(&lock).is_ok(),
                "lock must be preserved"
            );
        }
        fs::remove_file(&lock).unwrap();
        fs::write(&lock, "regular file").unwrap();
        assert!(profile_in_use(&profile));
        fs::remove_dir_all(profile).unwrap();
    }

    #[cfg(unix)]
    fn state_hostname() -> String {
        let output = std::process::Command::new("/bin/hostname")
            .output()
            .unwrap();
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }
}

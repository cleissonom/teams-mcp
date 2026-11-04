use super::failure;
use crate::{
    config::{Config, endpoint_from_port_file, validate_endpoint},
    error::Failure,
};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};
use tokio::time::sleep;

pub(super) const STATE: &str = ".teams-mcp-state.json";
pub(super) const HEARTBEAT: &str = ".teams-mcp-heartbeat";
pub(super) const STOP: &str = ".teams-mcp-stop";
pub(super) const ERROR: &str = ".teams-mcp-error.json";
pub(super) const START_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Serialize, Deserialize)]
pub(super) struct State {
    pub(super) version: u8,
    pub(super) nonce: String,
    pub(super) endpoint: String,
    pub(super) headed: bool,
}
pub(super) enum Running {
    Stopped,
    Starting,
    Ready(State),
}

pub(super) fn publish(profile: &Path, endpoint: String, headed: bool) -> Result<State, Failure> {
    let state = State {
        version: 1,
        nonce: uuid::Uuid::new_v4().to_string(),
        endpoint,
        headed,
    };
    write_private(
        &profile.join(STATE),
        &serde_json::to_vec(&state).map_err(|_| failure("BROWSER_STATE_INVALID"))?,
    )?;
    write_private(&profile.join(HEARTBEAT), state.nonce.as_bytes())?;
    Ok(state)
}

pub(super) fn worker_stopped(profile: &Path) -> Result<bool, Failure> {
    let guard = lock_file(&profile.join(".teams-mcp-worker.lock"))?;
    match guard.try_lock_exclusive() {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(false),
        Err(_) => Err(failure("BROWSER_STATE_INVALID")),
    }
}

pub(super) fn running(profile: &Path) -> Result<Running, Failure> {
    if worker_stopped(profile)? {
        return Ok(Running::Stopped);
    }
    let Some(contents) = read_small(&profile.join(STATE))? else {
        return Ok(Running::Starting);
    };
    let state: State =
        serde_json::from_str(&contents).map_err(|_| failure("BROWSER_STATE_INVALID"))?;
    let port = read_small(&profile.join("DevToolsActivePort"))?
        .ok_or_else(|| failure("BROWSER_STATE_INVALID"))?;
    if state.version != 1
        || uuid::Uuid::parse_str(&state.nonce).is_err()
        || validate_endpoint(&state.endpoint).is_err()
        || endpoint_from_port_file(&port).ok().as_deref() != Some(&state.endpoint)
    {
        return Err(failure("BROWSER_STATE_INVALID"));
    }
    Ok(Running::Ready(state))
}

pub(super) async fn startup_lock(profile: &Path) -> Result<File, Failure> {
    let guard = lock_file(&profile.join(".teams-mcp-start.lock"))?;
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        match guard.try_lock_exclusive() {
            Ok(()) => return Ok(guard),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => return Err(failure("BROWSER_STATE_INVALID")),
        }
        if Instant::now() >= deadline {
            return Err(failure("BROWSER_BUSY"));
        }
        sleep(Duration::from_millis(50)).await;
    }
}

pub(super) fn profile(config: &Config) -> Result<PathBuf, Failure> {
    let path = config
        .browser_profile
        .clone()
        .or_else(|| {
            if cfg!(target_os = "windows") {
                return env::var_os("LOCALAPPDATA")
                    .map(|p| PathBuf::from(p).join("teams-mcp/browser"));
            }
            let home = PathBuf::from(env::var_os("HOME")?);
            Some(home.join(if cfg!(target_os = "macos") {
                "Library/Application Support/teams-mcp/browser"
            } else {
                ".local/share/teams-mcp/browser"
            }))
        })
        .ok_or_else(|| failure("BROWSER_INVALID_CONFIGURATION"))?;
    if !path.is_absolute() {
        return Err(failure("BROWSER_INVALID_CONFIGURATION"));
    }
    Ok(path)
}

pub(super) fn private_directory(path: &Path) -> Result<(), Failure> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(path)
        .map_err(|_| failure("BROWSER_STATE_INVALID"))?;
    validate_private_profile(path)
}

pub(super) fn validate_private_profile(path: &Path) -> Result<(), Failure> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(failure("BROWSER_STATE_INVALID")),
    };
    if !metadata.is_dir() {
        return Err(failure("BROWSER_INVALID_CONFIGURATION"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(failure("BROWSER_PROFILE_INSECURE"));
        }
    }
    Ok(())
}
pub(super) fn lock_file(path: &Path) -> Result<File, Failure> {
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .map_err(|_| failure("BROWSER_STATE_INVALID"))
}
pub(super) fn read_small(path: &Path) -> Result<Option<String>, Failure> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(failure("BROWSER_STATE_INVALID")),
    };
    let mut value = String::new();
    file.take(4097)
        .read_to_string(&mut value)
        .map_err(|_| failure("BROWSER_STATE_INVALID"))?;
    if value.len() > 4096 {
        return Err(failure("BROWSER_STATE_INVALID"));
    }
    Ok(Some(value))
}
pub(super) fn write_private(path: &Path, bytes: &[u8]) -> Result<(), Failure> {
    let temporary = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        options.open(&temporary)?.write_all(bytes)?;
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result.map_err(|_| failure("BROWSER_STATE_INVALID"))
}
pub(super) fn modified(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).ok()?.modified().ok()
}

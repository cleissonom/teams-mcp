use super::{
    failure,
    state::{lock_file, profile, validate_private_profile},
};
use crate::{config::Config, error::Failure};
use std::{
    fs::File,
    path::Path,
    time::{Duration, Instant},
};
use tokio::time::sleep;

const GATE: &str = ".teams-mcp-auth.lock";
const ACTIVITY: &str = ".teams-mcp-activity.lock";

/// Held across a complete managed read, including leased HTTPS requests. Missing
/// profiles stay untouched so discovery and missing-setup errors remain lazy.
pub async fn request_guard(config: &Config) -> Result<Option<File>, Failure> {
    let path = profile(config)?;
    validate_private_profile(&path)?;
    if !path.is_dir() {
        return Ok(None);
    }
    let deadline = Instant::now() + config.request_timeout;
    let gate = acquire(&path.join(GATE), true, deadline).await?;
    let activity = acquire(&path.join(ACTIVITY), true, deadline).await?;
    drop(gate);
    Ok(Some(activity))
}

pub(super) struct SetupGuard {
    _gate: File,
    _activity: File,
}

/// Closing the gate first lets active readers finish while new requests wait.
pub(super) async fn setup_guard(path: &Path, wait: Duration) -> Result<SetupGuard, Failure> {
    let deadline = Instant::now() + wait;
    Ok(SetupGuard {
        _gate: acquire(&path.join(GATE), false, deadline).await?,
        _activity: acquire(&path.join(ACTIVITY), false, deadline).await?,
    })
}

async fn acquire(path: &Path, shared: bool, deadline: Instant) -> Result<File, Failure> {
    let guard = lock_file(path)?;
    loop {
        let result = if shared {
            guard.try_lock_shared()
        } else {
            guard.try_lock()
        };
        match result {
            Ok(()) => return Ok(guard),
            Err(std::fs::TryLockError::WouldBlock) => {}
            Err(_) => return Err(failure("BROWSER_STATE_INVALID")),
        }
        if Instant::now() >= deadline {
            return Err(failure("BROWSER_BUSY"));
        }
        sleep(Duration::from_millis(50)).await;
    }
}

pub(super) fn setup_pending(path: &Path) -> Result<bool, Failure> {
    let guard = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path.join(GATE))
    {
        Ok(guard) => guard,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(_) => return Err(failure("BROWSER_STATE_INVALID")),
    };
    match guard.try_lock_shared() {
        Ok(()) => Ok(false),
        Err(std::fs::TryLockError::WouldBlock) => Ok(true),
        Err(_) => Err(failure("BROWSER_STATE_INVALID")),
    }
}

use std::{env, path::PathBuf, time::Duration};
use url::Url;

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub enum AuthSource {
    #[default]
    Headless,
    Browser,
    App,
}

impl std::str::FromStr for AuthSource {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "headless" => Ok(Self::Headless),
            "browser" => Ok(Self::Browser),
            "app" => Ok(Self::App),
            _ => Err("TEAMS_AUTH_SOURCE must be headless, browser or app.".into()),
        }
    }
}

/// How tool requests reach Teams once authenticated.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub enum Transport {
    /// Every request runs inside the Teams page through the bridge.
    Page,
    /// The browser only mints short-lived access tokens; Rust calls Teams over HTTPS.
    #[default]
    Lease,
}

impl std::str::FromStr for Transport {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "page" => Ok(Self::Page),
            "lease" => Ok(Self::Lease),
            _ => Err("TEAMS_TRANSPORT must be page or lease.".into()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrowserKind {
    Chrome,
    Edge,
    Chromium,
    Brave,
}

impl std::str::FromStr for BrowserKind {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "chrome" => Ok(Self::Chrome),
            "edge" => Ok(Self::Edge),
            "chromium" => Ok(Self::Chromium),
            "brave" => Ok(Self::Brave),
            _ => Err("TEAMS_BROWSER must be chrome, edge, chromium or brave.".into()),
        }
    }
}

#[derive(Clone)]
pub struct Config {
    pub auth_source: AuthSource,
    pub browser_binary: Option<PathBuf>,
    pub browser_profile: Option<PathBuf>,
    pub app_window_id: Option<i64>,
    pub app_cdp_url: Option<String>,
    pub cdp_url: Option<String>,
    pub port_file: Option<PathBuf>,
    pub target_id: Option<String>,
    pub account: Option<String>,
    pub tenant: Option<String>,
    pub transport: Transport,
    /// Upper bound for one tool call, including a cold browser start.
    pub request_timeout: Duration,
    /// Stop the managed headless browser after this much inactivity.
    pub idle: Duration,
    /// Freeze the managed Teams page after this much inactivity; `None` keeps it running.
    pub freeze: Option<Duration>,
    pub cache_limit_mb: Option<u64>,
    pub browser: Option<BrowserKind>,
    pub lean: bool,
    pub prewarm: bool,
    pub log: bool,
    /// Existing-browser mode only: use a connector-owned hidden Teams target, never a visible tab.
    pub hidden_tab: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            auth_source: AuthSource::default(),
            browser_binary: None,
            browser_profile: None,
            app_window_id: None,
            app_cdp_url: None,
            cdp_url: None,
            port_file: None,
            target_id: None,
            account: None,
            tenant: None,
            transport: Transport::Lease,
            request_timeout: Duration::from_secs(65),
            idle: Duration::from_secs(300),
            freeze: Some(Duration::from_secs(15)),
            cache_limit_mb: None,
            browser: None,
            lean: false,
            prewarm: false,
            log: false,
            hidden_tab: false,
        }
    }
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let transport: Transport = env::var("TEAMS_TRANSPORT")
            .ok()
            .map(|value| value.parse())
            .transpose()?
            .unwrap_or_default();
        // A lease only needs the browser while it mints tokens, so it stops soon after.
        let idle_default = if transport == Transport::Lease {
            20
        } else {
            300
        };
        Ok(Self {
            auth_source: env::var("TEAMS_AUTH_SOURCE")
                .unwrap_or_else(|_| "headless".into())
                .parse()?,
            browser_binary: env::var_os("TEAMS_BROWSER_BINARY").map(PathBuf::from),
            browser_profile: env::var_os("TEAMS_BROWSER_PROFILE").map(PathBuf::from),
            app_window_id: env::var("TEAMS_APP_WINDOW_ID")
                .ok()
                .map(|value| {
                    value
                        .parse::<i64>()
                        .map_err(|_| {
                            "TEAMS_APP_WINDOW_ID must be a positive window ID.".to_string()
                        })
                        .and_then(|id| {
                            if id > 0 {
                                Ok(id)
                            } else {
                                Err("TEAMS_APP_WINDOW_ID must be a positive window ID.".into())
                            }
                        })
                })
                .transpose()?,
            app_cdp_url: env::var("TEAMS_APP_CDP_URL").ok(),
            cdp_url: env::var("TEAMS_CDP_URL").ok(),
            port_file: env::var_os("TEAMS_CDP_PORT_FILE").map(PathBuf::from),
            target_id: env::var("TEAMS_TARGET_ID").ok(),
            account: env::var("TEAMS_ACCOUNT").ok(),
            tenant: env::var("TEAMS_TENANT_ID").ok(),
            transport,
            request_timeout: Duration::from_secs(
                seconds("TEAMS_REQUEST_TIMEOUT_SECS", 10, 300)?.unwrap_or(65),
            ),
            idle: Duration::from_secs(
                seconds("TEAMS_BROWSER_IDLE_SECS", 10, 86_400)?.unwrap_or(idle_default),
            ),
            freeze: match env::var("TEAMS_BROWSER_FREEZE_SECS").as_deref() {
                Ok("0") => None,
                _ => Some(Duration::from_secs(
                    seconds("TEAMS_BROWSER_FREEZE_SECS", 5, 3600)?.unwrap_or(15),
                )),
            },
            cache_limit_mb: seconds("TEAMS_BROWSER_CACHE_LIMIT_MB", 50, 100_000)?,
            browser: env::var("TEAMS_BROWSER")
                .ok()
                .map(|value| value.parse())
                .transpose()?,
            lean: flag("TEAMS_BROWSER_LEAN"),
            prewarm: flag("TEAMS_PREWARM"),
            log: flag("TEAMS_MCP_LOG"),
            hidden_tab: flag("TEAMS_HIDDEN_TAB"),
        })
    }

    pub async fn endpoint(&self) -> Result<String, String> {
        if self.auth_source == AuthSource::App {
            return self
                .app_cdp_url
                .as_deref()
                .ok_or_else(|| {
                    "Set TEAMS_APP_CDP_URL to the Teams app's own approved debugging endpoint."
                        .to_string()
                })
                .and_then(validate_endpoint);
        }
        if self.auth_source == AuthSource::Headless {
            return crate::managed::endpoint(self)
                .await
                .map_err(|error| error.message);
        }
        if let Some(endpoint) = &self.cdp_url {
            return validate_endpoint(endpoint);
        }
        let file = self
            .port_file
            .clone()
            .or_else(|| default_port_file(self.browser))
            .ok_or("Set TEAMS_CDP_URL or TEAMS_CDP_PORT_FILE for the existing browser.")?;
        let content = tokio::fs::read_to_string(file).await
            .map_err(|_| "Cannot read DevToolsActivePort. Enable remote debugging in the existing Chrome browser.")?;
        endpoint_from_port_file(&content)
    }
}

fn seconds(name: &str, min: u64, max: u64) -> Result<Option<u64>, String> {
    let Ok(value) = env::var(name) else {
        return Ok(None);
    };
    value
        .parse::<u64>()
        .ok()
        .filter(|value| (min..=max).contains(value))
        .map(Some)
        .ok_or_else(|| format!("{name} must be an integer from {min} to {max}."))
}

fn flag(name: &str) -> bool {
    env::var(name).is_ok_and(|value| !matches!(value.as_str(), "" | "0" | "false"))
}

/// Profile directories of installed Chromium browsers, in preference order.
fn browser_data_directories(kind: Option<BrowserKind>) -> Vec<PathBuf> {
    let Some(home) = env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    let all: &[(BrowserKind, &str, &str)] = &[
        (
            BrowserKind::Chrome,
            "Library/Application Support/Google/Chrome",
            ".config/google-chrome",
        ),
        (
            BrowserKind::Edge,
            "Library/Application Support/Microsoft Edge",
            ".config/microsoft-edge",
        ),
        (
            BrowserKind::Chromium,
            "Library/Application Support/Chromium",
            ".config/chromium",
        ),
        (
            BrowserKind::Brave,
            "Library/Application Support/BraveSoftware/Brave-Browser",
            ".config/BraveSoftware/Brave-Browser",
        ),
    ];
    all.iter()
        .filter(|(candidate, _, _)| kind.is_none_or(|kind| kind == *candidate))
        .filter_map(|(_, macos, linux)| {
            if cfg!(target_os = "macos") {
                Some(home.join(macos))
            } else if cfg!(target_os = "linux") {
                Some(home.join(linux))
            } else {
                None
            }
        })
        .collect()
}

fn default_port_file(kind: Option<BrowserKind>) -> Option<PathBuf> {
    let candidates: Vec<_> = browser_data_directories(kind)
        .into_iter()
        .map(|directory| directory.join("DevToolsActivePort"))
        .collect();
    candidates
        .iter()
        .find(|path| path.is_file())
        .or_else(|| candidates.first())
        .cloned()
}

pub fn validate_endpoint(endpoint: &str) -> Result<String, String> {
    let url = Url::parse(endpoint).map_err(|_| "Invalid CDP WebSocket URL.")?;
    let loopback = matches!(url.host(), Some(url::Host::Ipv4(ip)) if ip.is_loopback())
        || matches!(url.host(), Some(url::Host::Ipv6(ip)) if ip.is_loopback());
    if url.scheme() != "ws"
        || !loopback
        || url.port().is_none_or(|p| p == 0)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.path().starts_with("/devtools/browser/")
        || url
            .path()
            .trim_start_matches("/devtools/browser/")
            .is_empty()
    {
        return Err("CDP requires a literal loopback ws:// address with a port and /devtools/browser/ID path.".into());
    }
    Ok(url.to_string())
}

pub fn endpoint_from_port_file(contents: &str) -> Result<String, String> {
    let mut lines = contents.lines();
    let port: u16 = lines
        .next()
        .unwrap_or("")
        .parse()
        .map_err(|_| "Invalid DevToolsActivePort port.")?;
    let path = lines
        .next()
        .ok_or("DevToolsActivePort is missing its browser path.")?;
    validate_endpoint(&format!("ws://127.0.0.1:{port}{path}"))
}

pub fn is_teams_page(value: &str) -> bool {
    Url::parse(value).is_ok_and(|url| {
        url.scheme() == "https"
            && url.port_or_known_default() == Some(443)
            && url.username().is_empty()
            && url.password().is_none()
            && matches!(
                url.host_str(),
                Some("teams.microsoft.com" | "teams.cloud.microsoft")
            )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_is_the_default_transport_and_page_remains_selectable() {
        assert_eq!(Transport::default(), Transport::Lease);
        assert_eq!(Config::default().transport, Transport::Lease);
        assert_eq!("page".parse::<Transport>().unwrap(), Transport::Page);
        assert!("other".parse::<Transport>().is_err());
    }
}

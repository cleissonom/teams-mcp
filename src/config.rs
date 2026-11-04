use std::{env, path::PathBuf};
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

#[derive(Clone, Default)]
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
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
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
            .or_else(default_port_file)
            .ok_or("Set TEAMS_CDP_URL or TEAMS_CDP_PORT_FILE for the existing browser.")?;
        let content = tokio::fs::read_to_string(file).await
            .map_err(|_| "Cannot read DevToolsActivePort. Enable remote debugging in the existing Chrome browser.")?;
        endpoint_from_port_file(&content)
    }
}

fn default_port_file() -> Option<PathBuf> {
    let home = PathBuf::from(env::var_os("HOME")?);
    if cfg!(target_os = "macos") {
        Some(home.join("Library/Application Support/Google/Chrome/DevToolsActivePort"))
    } else if cfg!(target_os = "linux") {
        Some(home.join(".config/google-chrome/DevToolsActivePort"))
    } else {
        None
    }
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

//! Lease transport: the browser session only mints short-lived access tokens; this
//! process calls Teams over HTTPS with them.
//!
//! Tokens live in this process's memory only. They are never written to disk, logged,
//! returned to MCP clients or included in errors. Refresh tokens and cookies stay in
//! the browser. Requests use the same allowlist, origin checks and size limits as the
//! in-page bridge.

use crate::{error::Failure, operations::ApiRequest};
use futures_util::{StreamExt, stream};
use reqwest::{Client, Method, StatusCode, header::HeaderMap, redirect::Policy};
use serde_json::{Value, json};
use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{sync::Mutex, time::sleep};
use url::Url;

/// Re-mint a token before Teams would reject it.
const MARGIN_SECS: u64 = 300;
/// How long a fresh browser start may take to renew an about-to-expire token.
const FRESHNESS_WAIT: Duration = Duration::from_secs(30);
const MAX_RESPONSE: usize = 2 * 1024 * 1024;
const MAX_BATCH: usize = 3 * 1024 * 1024 + 512 * 1024;
const ORIGINS: [&str; 2] = [
    "https://teams.microsoft.com",
    "https://teams.cloud.microsoft",
];

pub type Mint =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Result<Value, Failure>> + Send>> + Send + Sync>;

/// A bearer credential whose value is never printed.
#[derive(Clone)]
struct Secret {
    token: String,
    expires_at: u64,
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Secret(expires_at={})", self.expires_at)
    }
}

impl Secret {
    fn usable(&self) -> bool {
        self.expires_at > now() + MARGIN_SECS
    }
}

#[derive(Clone, Debug)]
struct Grant {
    profile: Value,
    spaces: Option<Secret>,
    search: Option<Secret>,
    csa: Option<Secret>,
}

/// Which leased token a request needs.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Need {
    Identity,
    Spaces,
    Search,
    Channels,
}

impl Grant {
    fn usable(secret: &Option<Secret>) -> bool {
        secret.as_ref().is_some_and(Secret::usable)
    }

    fn covers(&self, need: Need) -> bool {
        match need {
            Need::Identity | Need::Spaces => Self::usable(&self.spaces),
            Need::Search => Self::usable(&self.search),
            Need::Channels => Self::usable(&self.csa) && Self::usable(&self.spaces),
        }
    }
}

#[derive(Clone, Debug)]
struct Skype {
    secret: Secret,
    regions: Value,
}

/// Fixed service roots. Only tests construct other values.
#[derive(Clone)]
pub(crate) struct Endpoints {
    pub(crate) authz: String,
    pub(crate) substrate: String,
    pub(crate) trusted_origins: Vec<String>,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            authz: "https://authsvc.teams.microsoft.com/v1.0/authz".into(),
            substrate: "https://substrate.office.com".into(),
            trusted_origins: ORIGINS.iter().map(|origin| (*origin).into()).collect(),
        }
    }
}

pub struct Lease {
    mint: Mint,
    log: bool,
    http: Client,
    endpoints: Endpoints,
    grant: Mutex<Option<Grant>>,
    skype: Mutex<Option<Skype>>,
    timezone: String,
}

impl Lease {
    pub fn new(mint: Mint, log: bool) -> Result<Self, Failure> {
        let mut lease = Self::with(mint, Endpoints::default(), true)?;
        lease.log = log;
        Ok(lease)
    }

    pub(crate) fn with(
        mint: Mint,
        endpoints: Endpoints,
        https_only: bool,
    ) -> Result<Self, Failure> {
        let http = Client::builder()
            .redirect(Policy::none())
            .https_only(https_only)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(15))
            .user_agent(concat!("teams-mcp/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|_| failure("LEASE_UNAVAILABLE", "The HTTPS client could not start."))?;
        Ok(Self {
            mint,
            log: false,
            http,
            endpoints,
            grant: Mutex::new(None),
            skype: Mutex::new(None),
            timezone: local_timezone(),
        })
    }

    /// Answers the bridge's actions (`status`, `request`, `batch`) with the same shapes.
    pub async fn evaluate(&self, input: Value) -> Result<Value, Failure> {
        match input["action"].as_str() {
            Some("status" | "profile") => {
                let grant = self.grant(Need::Identity).await?;
                let usable = |secret: &Option<Secret>| Grant::usable(secret);
                Ok(json!({"ok":true,"profile":grant.profile,"capabilities":{
                    "search":usable(&grant.search),"chat":usable(&grant.spaces),
                    "channels":usable(&grant.csa) && usable(&grant.spaces)}}))
            }
            Some("request") => {
                let request: ApiRequest = serde_json::from_value(input["request"].clone())
                    .map_err(|_| failure("INVALID_INPUT", "Unsupported request."))?;
                let grant = self.grant(need(&request)).await?;
                Ok(match self.perform(&grant, &request).await {
                    Ok(data) => json!({"ok":true,"profile":grant.profile,"data":data}),
                    Err(error) => json!({"ok":false,"error":error}),
                })
            }
            Some("batch") => self.batch(&input["requests"]).await,
            _ => Err(failure("INVALID_INPUT", "Unsupported operation.")),
        }
    }

    async fn batch(&self, requests: &Value) -> Result<Value, Failure> {
        let requests: Vec<ApiRequest> = serde_json::from_value(requests.clone())
            .ok()
            .filter(|requests: &Vec<ApiRequest>| (1..=20).contains(&requests.len()))
            .ok_or_else(|| failure("INVALID_INPUT", "A batch requires 1–20 requests."))?;
        let mut grant = self.grant(Need::Identity).await?;
        for request in &requests {
            grant = self.grant(need(request)).await?;
        }
        let pending: Vec<_> = requests
            .iter()
            .map(|request| self.item(&grant, request))
            .collect();
        let results: Vec<Value> = stream::iter(pending).buffered(4).collect().await;
        if serde_json::to_vec(&results).map_or(0, |bytes| bytes.len()) > MAX_BATCH {
            return Ok(
                json!({"ok":false,"error":failure("RESPONSE_TOO_LARGE","Teams responses exceed the size limit. Use a smaller page.")}),
            );
        }
        Ok(json!({"ok":true,"profile":grant.profile,"data":results}))
    }

    async fn item(&self, grant: &Grant, request: &ApiRequest) -> Value {
        match self.perform(grant, request).await {
            Ok(data) => json!({"ok":true,"data":data}),
            Err(error) => json!({"ok":false,"error":error}),
        }
    }

    /// A grant that covers `need`, minting through the browser session only when required.
    async fn grant(&self, need: Need) -> Result<Grant, Failure> {
        let mut current = self.grant.lock().await;
        if let Some(grant) = current.as_ref()
            && grant.covers(need)
        {
            return Ok(grant.clone());
        }
        let started = Instant::now();
        loop {
            let grant = parse_grant((self.mint)().await?)?;
            let fresh = grant.covers(need);
            if self.log {
                // Diagnostics only: remaining minutes per token, never token values.
                let left = |secret: &Option<Secret>| {
                    secret
                        .as_ref()
                        .map_or(-1, |s| (s.expires_at as i64 - now() as i64) / 60)
                };
                eprintln!(
                    "teams-mcp: lease minted spacesMin={} searchMin={} channelsMin={} fresh={fresh}",
                    left(&grant.spaces),
                    left(&grant.search),
                    left(&grant.csa)
                );
            }
            if let Some(previous) = current.as_ref()
                && previous.profile["id"] != grant.profile["id"]
            {
                // A different identity invalidates everything derived from the old one.
                *self.skype.lock().await = None;
            }
            *current = Some(grant.clone());
            // A just-started browser may hold a token about to expire; Teams renews it
            // shortly after loading, so wait briefly for a fresher one.
            if fresh || started.elapsed() >= FRESHNESS_WAIT {
                return Ok(grant);
            }
            sleep(Duration::from_secs(2)).await;
        }
    }

    async fn invalidate(&self) {
        *self.grant.lock().await = None;
        *self.skype.lock().await = None;
    }

    async fn perform(&self, grant: &Grant, request: &ApiRequest) -> Result<Value, Failure> {
        match self.send(grant, request).await {
            Err(error) if error.code == "AUTH_EXPIRED" => {
                // Teams rejected a leased token early; mint a new lease once and retry.
                self.invalidate().await;
                let renewed = self.grant(need(request)).await?;
                self.send(&renewed, request).await
            }
            result => result,
        }
    }

    async fn send(&self, grant: &Grant, request: &ApiRequest) -> Result<Value, Failure> {
        let substrate = request.service == "substrate";
        let skype = if substrate {
            None
        } else {
            Some(self.skype(grant).await?)
        };
        let bearer = match request.service.as_str() {
            "substrate" => grant.search.as_ref(),
            "csa" => grant.csa.as_ref(),
            _ => grant.spaces.as_ref(),
        };
        let bearer = bearer.filter(|secret| secret.expires_at > now()).ok_or_else(|| {
            failure(
                "AUTH_REQUIRED",
                "The Teams session has no valid access token for this operation. Open Teams and retry.",
            )
        })?;
        let regions = skype
            .as_ref()
            .map_or(Value::Null, |skype| skype.regions.clone());
        let url = resolve(request, &regions, &self.endpoints)?;
        let method =
            Method::from_bytes(request.method.as_bytes()).map_err(|_| unsafe_endpoint())?;
        let mut body = request.body.clone();
        if substrate && let Some(object) = body.as_mut().and_then(Value::as_object_mut) {
            object.insert("cvid".into(), json!(uuid::Uuid::new_v4().to_string()));
            object.insert("logicalId".into(), json!(uuid::Uuid::new_v4().to_string()));
            if request.path == "/searchservice/api/v2/query" {
                object.insert("timezone".into(), json!(self.timezone));
            }
        }
        let headers = |skype: Option<&Skype>| {
            let mut headers = vec![("Authorization", format!("Bearer {}", bearer.token))];
            if let Some(skype) = skype {
                headers.push((
                    "Authentication",
                    format!("skypetoken={}", skype.secret.token),
                ));
            }
            headers
        };
        let result = self
            .fetch(
                method.clone(),
                url.clone(),
                &headers(skype.as_ref()),
                body.as_ref(),
            )
            .await;
        match result {
            Err(error) if error.code == "AUTH_EXPIRED" && !substrate => {
                // The Skype token may have been revoked before its stated expiry.
                *self.skype.lock().await = None;
                let renewed = self.skype(grant).await?;
                self.fetch(method, url, &headers(Some(&renewed)), body.as_ref())
                    .await
            }
            result => result,
        }
    }

    /// One Skype token per lease, renewed shortly before Teams says it expires.
    async fn skype(&self, grant: &Grant) -> Result<Skype, Failure> {
        let mut current = self.skype.lock().await;
        if let Some(skype) = current.as_ref()
            && skype.secret.expires_at > now() + 60
        {
            return Ok(skype.clone());
        }
        let spaces = grant
            .spaces
            .as_ref()
            .filter(|secret| secret.expires_at > now())
            .ok_or_else(|| {
                failure(
                    "AUTH_REQUIRED",
                    "The Teams session has no valid Skype Spaces access token. Open Teams and retry.",
                )
            })?;
        let url = Url::parse(&self.endpoints.authz).map_err(|_| unsafe_endpoint())?;
        let data = self
            .fetch(
                Method::POST,
                url,
                &[("Authorization", format!("Bearer {}", spaces.token))],
                Some(&json!({})),
            )
            .await?;
        let token = data["tokens"]["skypeToken"]
            .as_str()
            .filter(|token| !token.is_empty())
            .ok_or_else(|| {
                failure(
                    "AUTH_REQUIRED",
                    "Teams could not supply its chat session token.",
                )
            })?;
        let lifetime = data["tokens"]["expiresIn"]
            .as_u64()
            .filter(|seconds| *seconds > 0)
            .unwrap_or(900)
            .min(86_400);
        let skype = Skype {
            secret: Secret {
                token: token.into(),
                expires_at: now() + lifetime,
            },
            regions: data["regionGtms"].clone(),
        };
        *current = Some(skype.clone());
        Ok(skype)
    }

    async fn fetch(
        &self,
        method: Method,
        url: Url,
        headers: &[(&str, String)],
        body: Option<&Value>,
    ) -> Result<Value, Failure> {
        let mut retried = false;
        loop {
            let mut builder = self
                .http
                .request(method.clone(), url.clone())
                .header("Accept", "application/json");
            for (name, value) in headers {
                builder = builder.header(*name, value);
            }
            if let Some(body) = body {
                builder = builder
                    .header("Content-Type", "application/json")
                    .body(body.to_string());
            }
            let response = match builder.send().await {
                Ok(response) => response,
                Err(_) if !retried => {
                    retried = true;
                    sleep(Duration::from_millis(400)).await;
                    continue;
                }
                Err(_) => {
                    return Err(failure(
                        "NETWORK_ERROR",
                        "Teams request failed or timed out. Check the connection, then retry.",
                    ));
                }
            };
            let status = response.status();
            if status.is_success() {
                let bytes = read_limited(response, MAX_RESPONSE).await?;
                return serde_json::from_slice::<Value>(&bytes)
                    .ok()
                    .filter(Value::is_object)
                    .ok_or_else(|| {
                        failure(
                            "INVALID_RESPONSE",
                            "Teams returned an unsupported response.",
                        )
                    });
            }
            if !retried && let Some(delay) = retry_delay(status, response.headers()) {
                retried = true;
                sleep(delay).await;
                continue;
            }
            if status == StatusCode::BAD_REQUEST && url.path() == "/search/api/v1/suggestions" {
                let body = read_limited(response, 16 * 1024).await.unwrap_or_default();
                let body = String::from_utf8_lossy(&body);
                if body.contains("No providers were enabled") && body.contains("GuestAccess: True")
                {
                    return Err(failure(
                        "FEATURE_UNAVAILABLE",
                        "Teams people search is unavailable for this guest account. Use a known person ID or search in the Teams interface.",
                    ));
                }
            }
            return Err(status_failure(status));
        }
    }
}

fn need(request: &ApiRequest) -> Need {
    match request.service.as_str() {
        "substrate" => Need::Search,
        "csa" => Need::Channels,
        _ => Need::Spaces,
    }
}

fn parse_grant(value: Value) -> Result<Grant, Failure> {
    if value["ok"] != true {
        let error = &value["error"];
        return Err(failure(
            error["code"].as_str().unwrap_or("SESSION_UNAVAILABLE"),
            error["message"]
                .as_str()
                .unwrap_or("Teams session data is unavailable. Finish loading or signing in to Teams, then retry."),
        ));
    }
    let secret = |name: &str| {
        let entry = &value["lease"][name];
        Some(Secret {
            token: entry["token"]
                .as_str()
                .filter(|token| !token.is_empty())?
                .to_owned(),
            expires_at: entry["expiresAt"].as_u64()?,
        })
    };
    if !value["profile"]["id"].is_string() {
        return Err(failure(
            "INVALID_RESPONSE",
            "Teams returned an unsupported session response.",
        ));
    }
    Ok(Grant {
        profile: value["profile"].clone(),
        spaces: secret("spaces"),
        search: secret("search"),
        csa: secret("csa"),
    })
}

/// Builds the exact request URL after checking the service, method, path and region root.
pub(crate) fn resolve(
    request: &ApiRequest,
    regions: &Value,
    endpoints: &Endpoints,
) -> Result<Url, Failure> {
    let segment = |value: &str| {
        !value.is_empty() && !value.contains(['/', '?', '#', '\\']) && !matches!(value, "." | "..")
    };
    let parts: Vec<&str> = request
        .path
        .strip_prefix('/')
        .unwrap_or("")
        .split('/')
        .collect();
    let (method, base, root): (&str, String, fn(&str) -> bool) = match request.service.as_str() {
        "substrate" => {
            let allowed = matches!(
                request.path.as_str(),
                "/searchservice/api/v2/query" | "/search/api/v1/suggestions"
            );
            if !allowed {
                return Err(unsafe_endpoint());
            }
            ("POST", endpoints.substrate.clone(), |_| true)
        }
        "chat" => {
            let allowed = match parts.as_slice() {
                ["v1", "users", "ME", "conversations"] => true,
                ["v1", "users", "ME", "conversations", id] => segment(id),
                ["v1", "users", "ME", "conversations", id, "messages"] => segment(id),
                [
                    "v1",
                    "users",
                    "ME",
                    "conversations",
                    id,
                    "messages",
                    message,
                ] => segment(id) && segment(message),
                ["v1", "threads", id, "consumptionhorizons"] => segment(id),
                _ => false,
            };
            if !allowed {
                return Err(unsafe_endpoint());
            }
            ("GET", region(regions, "chatServiceAfd")?, |path| {
                root_matches(path, "/api/chatsvc/", false)
            })
        }
        "csa" => {
            let allowed = matches!(
                parts.as_slice(),
                ["api", "v3", "teams", "users", "me"]
                    | ["api", "v1", "teams", "users", "me", "conversationFolders"]
            );
            if !allowed {
                return Err(unsafe_endpoint());
            }
            ("GET", region(regions, "chatSvcAggAfd")?, |path| {
                root_matches(path, "/api/csa/", false)
            })
        }
        "profile" => {
            if request.path != "/beta/users/fetchShortProfile" {
                return Err(unsafe_endpoint());
            }
            ("POST", region(regions, "middleTier")?, |path| {
                root_matches(path, "/api/mt/part/", true) || root_matches(path, "/api/mt/", true)
            })
        }
        _ => return Err(unsafe_endpoint()),
    };
    if request.method != method {
        return Err(unsafe_endpoint());
    }
    let root_url = Url::parse(&base).map_err(|_| unsafe_endpoint())?;
    let origin = root_url.origin().ascii_serialization();
    let trusted = if request.service == "substrate" {
        base == endpoints.substrate
    } else {
        endpoints.trusted_origins.contains(&origin)
            && root_url.username().is_empty()
            && root_url.password().is_none()
            && root_url.query().is_none()
            && root_url.fragment().is_none()
            && root(root_url.path())
    };
    if !trusted {
        return Err(unsafe_endpoint());
    }
    let joined = format!("{base}{}", request.path);
    let mut url = Url::parse(&joined).map_err(|_| unsafe_endpoint())?;
    if url.as_str() != joined {
        return Err(unsafe_endpoint());
    }
    if !request.query.is_empty() {
        url.query_pairs_mut().extend_pairs(&request.query);
    }
    Ok(url)
}

/// `/api/chatsvc/emea`-style roots: a fixed prefix plus one lower-case region label,
/// optionally followed by `-<digits>`.
fn root_matches(path: &str, prefix: &str, numbered: bool) -> bool {
    let Some(label) = path.strip_prefix(prefix) else {
        return false;
    };
    let (name, number) = match label.split_once('-') {
        Some((name, number)) if numbered => (name, Some(number)),
        Some(_) => return false,
        None => (label, None),
    };
    !name.is_empty()
        && name.bytes().all(|byte| byte.is_ascii_lowercase())
        && number.is_none_or(|number| {
            !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
        })
}

fn region(regions: &Value, key: &str) -> Result<String, Failure> {
    regions[key].as_str().map(String::from).ok_or_else(|| {
        failure(
            "REGION_UNAVAILABLE",
            "Teams region discovery is unavailable. Retry shortly.",
        )
    })
}

async fn read_limited(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>, Failure> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| failure("NETWORK_ERROR", "Teams response was interrupted. Retry."))?
    {
        if bytes.len() + chunk.len() > limit {
            return Err(failure(
                "RESPONSE_TOO_LARGE",
                "Teams response exceeds the size limit. Use a smaller page.",
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn retry_delay(status: StatusCode, headers: &HeaderMap) -> Option<Duration> {
    if status.is_server_error() {
        return Some(Duration::from_millis(400));
    }
    if status != StatusCode::TOO_MANY_REQUESTS {
        return None;
    }
    let Some(value) = headers
        .get("Retry-After")
        .and_then(|value| value.to_str().ok())
    else {
        return Some(Duration::from_secs(1));
    };
    let seconds: u64 = value.trim().parse().ok()?;
    (seconds <= 5).then(|| Duration::from_secs(seconds))
}

fn status_failure(status: StatusCode) -> Failure {
    let code = match status.as_u16() {
        401 => "AUTH_EXPIRED",
        403 => "ACCESS_DENIED",
        429 => "RATE_LIMITED",
        _ => "API_ERROR",
    };
    let hint = if status == StatusCode::UNAUTHORIZED {
        "The Teams session must be renewed."
    } else {
        "Retry later or check access in Teams."
    };
    failure(
        code,
        &format!("Teams returned HTTP {}. {hint}", status.as_u16()),
    )
}

fn unsafe_endpoint() -> Failure {
    failure("UNSAFE_ENDPOINT", "Unsupported Teams read endpoint.")
}

fn failure(code: &str, message: &str) -> Failure {
    Failure::new(code, message)
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |time| time.as_secs())
}

/// The IANA time zone name used by Teams search, from the OS when available.
fn local_timezone() -> String {
    std::fs::read_link("/etc/localtime")
        .ok()
        .and_then(|path| {
            let path = path.to_string_lossy().into_owned();
            path.split_once("zoneinfo/")
                .map(|(_, zone)| zone.to_owned())
        })
        .filter(|zone| {
            !zone.is_empty()
                && zone
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "/_+-".contains(c))
        })
        .unwrap_or_else(|| "UTC".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Mutex as StdMutex,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    type Handler = Arc<dyn Fn(&str, &str, &str) -> (u16, String, String) + Send + Sync>;
    type Data = Arc<dyn Fn(&str) -> (u16, String, String) + Send + Sync>;

    /// Minimal HTTP/1.1 peer: records `METHOD path` plus headers and answers via `handler`.
    async fn server(handler: Handler) -> (String, Arc<StdMutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let log = Arc::new(StdMutex::new(Vec::new()));
        let seen = log.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let (handler, seen) = (handler.clone(), seen.clone());
                tokio::spawn(async move {
                    let mut buffer = Vec::new();
                    loop {
                        let mut chunk = [0u8; 8192];
                        let Ok(read) = socket.read(&mut chunk).await else {
                            return;
                        };
                        if read == 0 {
                            return;
                        }
                        buffer.extend_from_slice(&chunk[..read]);
                        while let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&buffer[..end]).into_owned();
                            let length = head
                                .lines()
                                .find_map(|line| {
                                    line.to_ascii_lowercase()
                                        .strip_prefix("content-length:")
                                        .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                                })
                                .unwrap_or(0);
                            if buffer.len() < end + 4 + length {
                                break;
                            }
                            buffer.drain(..end + 4 + length);
                            let line = head.lines().next().unwrap_or("").to_owned();
                            let mut parts = line.split(' ');
                            let (method, target) =
                                (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
                            seen.lock()
                                .unwrap()
                                .push(format!("{method} {target}\n{head}"));
                            let (status, headers, body) = handler(method, target, &head);
                            let reply = format!(
                                "HTTP/1.1 {status} X\r\ncontent-length: {}\r\n{headers}\r\n{body}",
                                body.len()
                            );
                            if socket.write_all(reply.as_bytes()).await.is_err() {
                                return;
                            }
                        }
                    }
                });
            }
        });
        (origin, log)
    }

    fn lease_with(origin: &str, grants: Vec<Value>, mints: Arc<AtomicUsize>) -> Lease {
        let grants = Arc::new(StdMutex::new(grants));
        let mint: Mint = Arc::new(move || {
            let (grants, mints) = (grants.clone(), mints.clone());
            Box::pin(async move {
                mints.fetch_add(1, Ordering::SeqCst);
                let mut grants = grants.lock().unwrap();
                Ok(if grants.len() > 1 {
                    grants.remove(0)
                } else {
                    grants[0].clone()
                })
            })
        });
        let endpoints = Endpoints {
            authz: format!("{origin}/v1.0/authz"),
            substrate: origin.to_owned(),
            trusted_origins: vec![origin.to_owned()],
        };
        Lease::with(mint, endpoints, false).unwrap()
    }

    fn grant(seconds: u64) -> Value {
        let expires = now() + seconds;
        json!({"ok":true,"profile":{"id":"user-one","tenantId":"tenant-one"},"lease":{
            "spaces":{"token":"spaces-secret","expiresAt":expires},
            "search":{"token":"search-secret","expiresAt":expires},
            "csa":{"token":"csa-secret","expiresAt":expires}}})
    }

    fn chat(path: &str) -> Value {
        json!({"action":"request","request":{"service":"chat","path":path,"method":"GET","query":[["pageSize","5"]],"body":null}})
    }

    fn teams_handler(origin: String, data: Data) -> Handler {
        Arc::new(move |method, target, head| {
            if target == "/v1.0/authz" {
                assert_eq!(method, "POST");
                assert!(head.contains("Bearer spaces-secret"));
                let body = json!({"tokens":{"skypeToken":"skype-secret","expiresIn":3600},"regionGtms":{"chatServiceAfd":format!("{origin}/api/chatsvc/amer"),"chatSvcAggAfd":format!("{origin}/api/csa/amer"),"middleTier":format!("{origin}/api/mt/part/amer-02")}});
                return (200, String::new(), body.to_string());
            }
            data(target)
        })
    }

    #[tokio::test]
    async fn one_mint_and_one_skype_exchange_serve_many_calls_without_exposing_tokens() {
        let origin_slot = Arc::new(StdMutex::new(String::new()));
        let slot = origin_slot.clone();
        let handler: Handler = Arc::new(move |method, target, head| {
            let origin = slot.lock().unwrap().clone();
            teams_handler(
                origin,
                Arc::new(|_| (200, String::new(), json!({"conversations":[]}).to_string())),
            )(method, target, head)
        });
        let (origin, log) = server(handler).await;
        *origin_slot.lock().unwrap() = origin.clone();
        let mints = Arc::new(AtomicUsize::new(0));
        let lease = lease_with(&origin, vec![grant(3600)], mints.clone());
        for _ in 0..3 {
            let result = lease
                .evaluate(chat("/v1/users/ME/conversations"))
                .await
                .unwrap();
            assert_eq!(result["ok"], true, "{result}");
            assert!(!result.to_string().contains("secret"));
        }
        let status = lease.evaluate(json!({"action":"status"})).await.unwrap();
        assert_eq!(
            status["capabilities"],
            json!({"search":true,"chat":true,"channels":true})
        );
        assert_eq!(mints.load(Ordering::SeqCst), 1);
        let log = log.lock().unwrap();
        assert_eq!(
            log.iter()
                .filter(|entry| entry.starts_with("POST /v1.0/authz"))
                .count(),
            1
        );
        let data: Vec<_> = log
            .iter()
            .filter(|entry| {
                entry.starts_with("GET /api/chatsvc/amer/v1/users/ME/conversations?pageSize=5")
            })
            .collect();
        assert_eq!(data.len(), 3);
        assert!(
            data.iter()
                .all(|entry| entry.contains("skypetoken=skype-secret")
                    && entry.contains("Bearer spaces-secret"))
        );
    }

    #[tokio::test]
    async fn tokens_near_expiry_are_reminted_and_rejected_skype_tokens_renewed_once() {
        let origin_slot = Arc::new(StdMutex::new(String::new()));
        let rejected = Arc::new(AtomicUsize::new(0));
        let (slot, reject) = (origin_slot.clone(), rejected.clone());
        let handler: Handler = Arc::new(move |method, target, head| {
            let origin = slot.lock().unwrap().clone();
            let reject = reject.clone();
            teams_handler(
                origin,
                Arc::new(move |_| {
                    if reject.fetch_add(1, Ordering::SeqCst) == 0 {
                        (401, String::new(), "secret-body".into())
                    } else {
                        (200, String::new(), "{}".into())
                    }
                }),
            )(method, target, head)
        });
        let (origin, log) = server(handler).await;
        *origin_slot.lock().unwrap() = origin.clone();
        let mints = Arc::new(AtomicUsize::new(0));
        // The first grant expires inside the safety margin, so the call mints again.
        let lease = lease_with(&origin, vec![grant(60), grant(3600)], mints.clone());
        let result = lease
            .evaluate(chat("/v1/users/ME/conversations"))
            .await
            .unwrap();
        assert_eq!(result["ok"], true, "{result}");
        assert_eq!(mints.load(Ordering::SeqCst), 2);
        assert_eq!(
            log.lock()
                .unwrap()
                .iter()
                .filter(|entry| entry.starts_with("POST /v1.0/authz"))
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn transient_failures_retry_once_and_redirects_or_large_bodies_fail_closed() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let handler: Handler = Arc::new(move |_, target, _| {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            match target {
                "/searchservice/api/v2/query" if n == 0 => {
                    (429, "retry-after: 0\r\n".into(), String::new())
                }
                "/searchservice/api/v2/query" => {
                    (200, String::new(), json!({"EntitySets":[]}).to_string())
                }
                "/search/api/v1/suggestions" => (
                    302,
                    "location: https://attacker.test/\r\n".into(),
                    String::new(),
                ),
                _ => (500, String::new(), String::new()),
            }
        });
        let (origin, log) = server(handler).await;
        let lease = lease_with(&origin, vec![grant(3600)], Arc::new(AtomicUsize::new(0)));
        let search = |path: &str| json!({"action":"request","request":{"service":"substrate","path":path,"method":"POST","query":[],"body":{"entityRequests":[]}}});
        let ok = lease
            .evaluate(search("/searchservice/api/v2/query"))
            .await
            .unwrap();
        assert_eq!(ok["ok"], true, "{ok}");
        let redirected = lease
            .evaluate(search("/search/api/v1/suggestions"))
            .await
            .unwrap();
        assert_eq!(redirected["error"]["code"], "API_ERROR");
        assert!(
            log.lock()
                .unwrap()
                .iter()
                .all(|entry| !entry.contains("attacker"))
        );
        let entries = log.lock().unwrap().clone();
        assert!(entries[1].contains("\"cvid\"") || entries[1].contains("POST /searchservice"));
    }

    #[tokio::test]
    async fn oversized_and_server_error_responses_return_sanitized_errors() {
        let handler: Handler = Arc::new(move |_, target, _| match target {
            "/searchservice/api/v2/query" => (
                200,
                String::new(),
                format!("{{\"x\":\"{}\"}}", "a".repeat(MAX_RESPONSE)),
            ),
            _ => (500, String::new(), "upstream-secret".into()),
        });
        let (origin, log) = server(handler).await;
        let lease = lease_with(&origin, vec![grant(3600)], Arc::new(AtomicUsize::new(0)));
        let request = |path: &str| json!({"action":"request","request":{"service":"substrate","path":path,"method":"POST","query":[],"body":{}}});
        let large = lease
            .evaluate(request("/searchservice/api/v2/query"))
            .await
            .unwrap();
        assert_eq!(large["error"]["code"], "RESPONSE_TOO_LARGE");
        let failed = lease
            .evaluate(request("/search/api/v1/suggestions"))
            .await
            .unwrap();
        assert_eq!(failed["error"]["code"], "API_ERROR");
        assert!(!failed.to_string().contains("upstream-secret"));
        assert_eq!(
            log.lock()
                .unwrap()
                .iter()
                .filter(|entry| entry.starts_with("POST /search/api/v1/suggestions"))
                .count(),
            2
        );
    }

    #[test]
    fn requests_resolve_only_to_allowlisted_paths_under_trusted_region_roots() {
        let endpoints = Endpoints::default();
        let regions = json!({"chatServiceAfd":"https://teams.microsoft.com/api/chatsvc/emea","chatSvcAggAfd":"https://teams.cloud.microsoft/api/csa/emea","middleTier":"https://teams.microsoft.com/api/mt/part/emea-02"});
        let request = |service: &str, path: &str, method: &str| ApiRequest {
            service: service.into(),
            path: path.into(),
            method: method.into(),
            query: vec![("pageSize".into(), "5".into())],
            body: None,
        };
        let url = resolve(
            &request("chat", "/v1/users/ME/conversations/19%3Aa/messages", "GET"),
            &regions,
            &endpoints,
        )
        .unwrap();
        assert_eq!(
            url.as_str(),
            "https://teams.microsoft.com/api/chatsvc/emea/v1/users/ME/conversations/19%3Aa/messages?pageSize=5"
        );
        assert!(
            resolve(
                &request("csa", "/api/v1/teams/users/me/conversationFolders", "GET"),
                &regions,
                &endpoints
            )
            .is_ok()
        );
        assert!(
            resolve(
                &request("profile", "/beta/users/fetchShortProfile", "POST"),
                &regions,
                &endpoints
            )
            .is_ok()
        );
        assert_eq!(
            resolve(
                &request("substrate", "/searchservice/api/v2/query", "POST"),
                &regions,
                &endpoints
            )
            .unwrap()
            .host_str(),
            Some("substrate.office.com")
        );
        for (service, path, method) in [
            ("chat", "/v1/users/ME/conversations/a/messages/b/c", "GET"),
            ("chat", "/v1/users/ME/conversations/..", "GET"),
            ("chat", "/v1/users/ME/conversations", "POST"),
            ("chat", "/v1/users/other/conversations", "GET"),
            ("csa", "/api/v1/teams/users/me/other", "GET"),
            ("substrate", "/other", "POST"),
            ("unknown", "/", "GET"),
        ] {
            assert_eq!(
                resolve(&request(service, path, method), &regions, &endpoints)
                    .unwrap_err()
                    .code,
                "UNSAFE_ENDPOINT",
                "{service} {path}"
            );
        }
        for root in [
            "https://attacker.test/api/chatsvc/emea",
            "https://teams.microsoft.com/api/other/emea",
            "https://teams.microsoft.com/api/chatsvc/emea?x=1",
            "https://user@teams.microsoft.com/api/chatsvc/emea",
            "http://teams.microsoft.com/api/chatsvc/emea",
        ] {
            let regions = json!({"chatServiceAfd":root});
            assert_eq!(
                resolve(
                    &request("chat", "/v1/users/ME/conversations", "GET"),
                    &regions,
                    &endpoints
                )
                .unwrap_err()
                .code,
                "UNSAFE_ENDPOINT",
                "{root}"
            );
        }
    }

    #[test]
    fn secrets_never_appear_in_debug_output() {
        let secret = Secret {
            token: "bearer-value".into(),
            expires_at: 1,
        };
        assert!(!format!("{secret:?}").contains("bearer-value"));
    }
}

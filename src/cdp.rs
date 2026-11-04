use crate::{
    config::{AuthSource, Config, is_teams_page},
    error::Failure,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::{
    net::TcpStream,
    sync::Mutex,
    time::{Instant, sleep, timeout},
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async_with_config,
    tungstenite::{Message, protocol::WebSocketConfig},
};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[cfg(not(test))]
const READINESS_WAIT: Duration = Duration::from_secs(30);
#[cfg(test)]
const READINESS_WAIT: Duration = Duration::from_millis(500);
#[cfg(not(test))]
const READINESS_POLL: Duration = Duration::from_millis(250);
#[cfg(test)]
const READINESS_POLL: Duration = Duration::from_millis(10);

#[derive(Clone)]
pub struct Browser {
    config: Config,
    connection: Arc<Mutex<Option<Cdp>>>,
}

impl Browser {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            connection: Arc::new(Mutex::new(None)),
        }
    }

    pub async fn evaluate(&self, mut input: Value) -> Result<Value, Failure> {
        let config = if self.config.auth_source == AuthSource::Headless {
            crate::managed::session_config(&self.config)?
        } else {
            self.config.clone()
        };
        input["expectedAccount"] = json!(config.account);
        input["tenantId"] = json!(config.tenant);
        input["nativeApp"] = json!(self.config.auth_source == AuthSource::App);
        timeout(Duration::from_secs(65), self.evaluate_inner(input))
            .await
            .map_err(|_| {
                Failure::new(
                    "BROWSER_TIMEOUT",
                    "Teams did not respond within 65 seconds. Check the open Teams page and retry.",
                )
            })?
    }

    async fn evaluate_inner(&self, input: Value) -> Result<Value, Failure> {
        let mut connection = self.connection.lock().await;
        // Taking ownership ensures a cancelled or timed-out operation drops the socket,
        // rather than caching a connection with an unread response still pending.
        let previous = connection.take();
        let endpoint = if self.config.auth_source == AuthSource::Headless {
            crate::managed::endpoint(&self.config).await?
        } else {
            self.config
                .endpoint()
                .await
                .map_err(|message| Failure::new("BROWSER_UNAVAILABLE", &message))?
        };
        let mut cdp = match previous {
            Some(cdp) if cdp.endpoint == endpoint => cdp,
            _ => Cdp::connect(&endpoint).await?,
        };
        let result = self.evaluate_target(&mut cdp, input).await;
        if result.is_ok() {
            *connection = Some(cdp);
        }
        result
    }

    async fn evaluate_target(&self, cdp: &mut Cdp, input: Value) -> Result<Value, Failure> {
        if self.config.auth_source == AuthSource::Headless {
            return self.evaluate_managed(cdp, input).await;
        }
        let targets = cdp.call("Target.getTargets", json!({}), None).await?;
        let target = select_target(&targets, self.config.target_id.as_deref())?;
        self.evaluate_selected(cdp, input, &target, false)
            .await
            .map(|(value, _)| value)
    }

    async fn evaluate_managed(&self, cdp: &mut Cdp, input: Value) -> Result<Value, Failure> {
        let deadline = Instant::now() + READINESS_WAIT;
        let mut reloaded = false;
        loop {
            let targets = cdp.call("Target.getTargets", json!({}), None).await?;
            let target = match select_target(&targets, self.config.target_id.as_deref()) {
                Ok(target) => target,
                Err(error) if error.code == "TEAMS_TAB_REQUIRED" => {
                    // Session restoration can briefly expose another navigation URL.
                    // Poll discovery only; never attach until the exact Teams origin appears.
                    if wait_for_readiness(deadline).await {
                        continue;
                    }
                    if has_page_target(&targets)
                        && managed_loading(&targets, self.config.target_id.as_deref())?
                    {
                        return Err(setup_required());
                    }
                    return Err(error);
                }
                Err(error) => return Err(error),
            };
            let (value, refreshed) = match self
                .evaluate_selected(cdp, input.clone(), &target, !reloaded)
                .await
            {
                Ok(result) => result,
                Err(error)
                    if error.code == "PAGE_CHANGED" && wait_for_readiness(deadline).await =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            };
            reloaded |= refreshed;
            let code = value["error"]["code"].as_str().unwrap_or("");
            match code {
                "AUTH_EXPIRED" if !refreshed => return Err(setup_required()),
                "AUTH_EXPIRED"
                | "AUTH_REQUIRED"
                | "SESSION_UNAVAILABLE"
                | "REGION_UNAVAILABLE"
                | "WRONG_ORIGIN" => {
                    // The fixed bridge checks the actual origin before reading session
                    // data. Re-query targets after a navigation or authentication race.
                    if wait_for_readiness(deadline).await {
                        continue;
                    }
                    if code != "REGION_UNAVAILABLE" {
                        return Err(setup_required());
                    }
                }
                _ => {}
            }
            return Ok(value);
        }
    }

    async fn evaluate_selected(
        &self,
        cdp: &mut Cdp,
        input: Value,
        target: &str,
        can_reload: bool,
    ) -> Result<(Value, bool), Failure> {
        let attached = cdp
            .call(
                "Target.attachToTarget",
                json!({"targetId":target,"flatten":true}),
                None,
            )
            .await?;
        let session = attached["sessionId"]
            .as_str()
            .ok_or_else(protocol_error)?
            .to_owned();
        let result = evaluate_attached(cdp, input, &session).await;
        let mut reloaded = false;
        let result = match result {
            Ok(value) if can_reload && value["error"]["code"] == "AUTH_EXPIRED" => {
                reloaded = true;
                cdp.call("Page.reload", json!({}), Some(&session))
                    .await
                    .map(|_| value)
            }
            result => result,
        };
        let detached = cdp
            .call(
                "Target.detachFromTarget",
                json!({"sessionId":session}),
                None,
            )
            .await;
        match (result, detached) {
            (Ok(value), Ok(_)) => Ok((value, reloaded)),
            (Err(error), _) | (_, Err(error)) => Err(error),
        }
    }
}

async fn wait_for_readiness(deadline: Instant) -> bool {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return false;
    }
    sleep(READINESS_POLL.min(remaining)).await;
    Instant::now() < deadline
}

fn managed_loading(targets: &Value, selected: Option<&str>) -> Result<bool, Failure> {
    let pages = targets["targetInfos"]
        .as_array()
        .ok_or_else(protocol_error)?;
    if !has_page_target(targets) {
        return Ok(true);
    }
    Ok(pages
        .iter()
        .filter(|page| page["type"] == "page" && selected.is_none_or(|id| page["targetId"] == id))
        .filter_map(|page| page["url"].as_str())
        .any(|url| url == "about:blank" || microsoft_signin(url)))
}

fn has_page_target(targets: &Value) -> bool {
    targets["targetInfos"]
        .as_array()
        .is_some_and(|targets| targets.iter().any(|target| target["type"] == "page"))
}

fn microsoft_signin(value: &str) -> bool {
    url::Url::parse(value).is_ok_and(|url| {
        url.scheme() == "https"
            && url.port_or_known_default() == Some(443)
            && url.username().is_empty()
            && url.password().is_none()
            && matches!(
                url.host_str(),
                Some(
                    "login.microsoftonline.com"
                        | "login.microsoft.com"
                        | "login.live.com"
                        | "login.windows.net"
                )
            )
    })
}

fn setup_required() -> Failure {
    Failure::new(
        "AUTH_SETUP_REQUIRED",
        "The managed Teams session needs interactive sign-in. Run teams-mcp auth stop, then teams-mcp auth login and finish signing in to the intended account and organization. MCP never opens a visible sign-in window automatically.",
    )
}

async fn evaluate_attached(cdp: &mut Cdp, input: Value, session: &str) -> Result<Value, Failure> {
    if input["nativeApp"] == true {
        // The native host bridge is installed in the app's main world only.
        return evaluate_expression(cdp, input, session, None).await;
    }
    let frame = cdp
        .call("Page.getFrameTree", json!({}), Some(session))
        .await?;
    let frame_id = frame["frameTree"]["frame"]["id"]
        .as_str()
        .ok_or_else(protocol_error)?;
    let world = cdp
        .call(
            "Page.createIsolatedWorld",
            json!({"frameId":frame_id,"worldName":"teams-mcp"}),
            Some(session),
        )
        .await?;
    let context = world["executionContextId"]
        .as_i64()
        .ok_or_else(protocol_error)?;
    evaluate_expression(cdp, input, session, Some(context)).await
}

async fn evaluate_expression(
    cdp: &mut Cdp,
    input: Value,
    session: &str,
    context: Option<i64>,
) -> Result<Value, Failure> {
    let expression = format!("({})({input})", include_str!("bridge.js"));
    let mut params = json!({"expression":expression,"awaitPromise":true,"returnByValue":true});
    if let Some(context) = context {
        params["contextId"] = json!(context);
    }
    let result = cdp.call("Runtime.evaluate", params, Some(session)).await?;
    if result.get("exceptionDetails").is_some() {
        return Err(Failure::new(
            "PAGE_CHANGED",
            "Teams changed or closed while the request ran. Retry with the intended Teams tab open.",
        ));
    }
    result["result"]
        .get("value")
        .cloned()
        .ok_or_else(protocol_error)
}

fn select_target(targets: &Value, selected: Option<&str>) -> Result<String, Failure> {
    let targets = targets["targetInfos"]
        .as_array()
        .ok_or_else(protocol_error)?;
    let eligible: Vec<_> = targets
        .iter()
        .filter(|target| {
            target["type"] == "page"
                && target["url"].as_str().is_some_and(is_teams_page)
                && selected.is_none_or(|id| target["targetId"] == id)
        })
        .collect();
    match eligible.as_slice() {
        [target] => target["targetId"]
            .as_str()
            .map(String::from)
            .ok_or_else(protocol_error),
        [] => Err(Failure::new(
            "TEAMS_TAB_REQUIRED",
            "Open Teams in the configured browser or App session and sign in. If TEAMS_TARGET_ID is set, verify that target still exists.",
        )),
        _ => Err(Failure::new(
            "MULTIPLE_TEAMS_TABS",
            "More than one Teams tab is open. Set TEAMS_TARGET_ID to the intended tab ID or leave only that Teams tab open.",
        )),
    }
}

pub(crate) struct Cdp {
    socket: Socket,
    next_id: u64,
    endpoint: String,
}

impl Cdp {
    pub(crate) async fn connect(endpoint: &str) -> Result<Self, Failure> {
        let config = WebSocketConfig::default()
            .max_message_size(Some(4 * 1024 * 1024))
            .max_frame_size(Some(4 * 1024 * 1024));
        let connected = timeout(
            Duration::from_secs(30),
            connect_async_with_config(endpoint, Some(config), false),
        )
        .await;
        let (socket, _) = connected
            .map_err(|_| unavailable())?
            .map_err(|_| unavailable())?;
        Ok(Self {
            socket,
            next_id: 1,
            endpoint: endpoint.to_owned(),
        })
    }

    pub(crate) async fn call(
        &mut self,
        method: &str,
        params: Value,
        session: Option<&str>,
    ) -> Result<Value, Failure> {
        let id = self.next_id;
        self.next_id += 1;
        let mut message = json!({"id":id,"method":method,"params":params});
        if let Some(session) = session {
            message["sessionId"] = json!(session);
        }
        self.socket
            .send(Message::Text(message.to_string().into()))
            .await
            .map_err(|_| unavailable())?;
        loop {
            let message = self
                .socket
                .next()
                .await
                .ok_or_else(unavailable)?
                .map_err(|_| unavailable())?;
            if let Message::Text(text) = message {
                let response: Value = serde_json::from_str(&text).map_err(|_| protocol_error())?;
                if response["id"] != id {
                    continue;
                }
                if response.get("error").is_some() {
                    return Err(protocol_error());
                }
                return response.get("result").cloned().ok_or_else(protocol_error);
            }
            if let Message::Ping(bytes) = message {
                self.socket
                    .send(Message::Pong(bytes))
                    .await
                    .map_err(|_| unavailable())?;
            }
        }
    }
}

fn protocol_error() -> Failure {
    Failure::new(
        "BROWSER_PROTOCOL_ERROR",
        "Could not read the selected Teams page through CDP. It may have closed or navigated; retry.",
    )
}
fn unavailable() -> Failure {
    Failure::new(
        "BROWSER_UNAVAILABLE",
        "Cannot connect to the configured Teams debugging endpoint. For browser mode, approve Chrome's connection prompt and check TEAMS_CDP_URL or TEAMS_CDP_PORT_FILE. For App mode, verify its debugging endpoint and TEAMS_APP_CDP_URL.",
    )
}

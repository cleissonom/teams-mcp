use crate::{
    config::{AuthSource, Config, is_teams_page},
    error::Failure,
};
use futures_util::{SinkExt, StreamExt, stream::SplitSink};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    net::TcpStream,
    sync::{Mutex, oneshot},
    task::JoinHandle,
    time::{Instant, sleep, timeout},
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async_with_config,
    tungstenite::{Message, protocol::WebSocketConfig},
};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
type Pending = Arc<StdMutex<HashMap<u64, oneshot::Sender<Result<Value, Failure>>>>>;

#[cfg(not(test))]
const READINESS_WAIT: Duration = Duration::from_secs(30);
#[cfg(test)]
const READINESS_WAIT: Duration = Duration::from_millis(500);
#[cfg(not(test))]
const READINESS_POLL: Duration = Duration::from_millis(250);
#[cfg(test)]
const READINESS_POLL: Duration = Duration::from_millis(10);
/// Keeps a managed call's lease fresh and its page active while the bridge runs.
const LEASE_INTERVAL: Duration = Duration::from_secs(2);
/// Calls the bridge function installed once per execution context.
const CALL_BRIDGE: &str = "function(input) { return this(input); }";

#[derive(Clone)]
pub struct Browser {
    config: Config,
    shared: Arc<Mutex<Shared>>,
}

#[derive(Default)]
struct Shared {
    cdp: Option<Arc<Cdp>>,
    page: Option<Page>,
    /// The connector-owned hidden Teams target in an existing browser, when enabled.
    hidden: Option<String>,
}

/// A target session that stays attached across calls, with its installed bridge function.
#[derive(Clone)]
struct Page {
    target: String,
    session: String,
    bridge: Option<String>,
}

impl Browser {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            shared: Arc::default(),
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
        let limit = self.config.request_timeout;
        timeout(limit, self.evaluate_inner(input))
            .await
            .map_err(|_| {
                Failure::new(
                    "BROWSER_TIMEOUT",
                    &format!(
                        "Teams did not respond within {} seconds. Check the open Teams page and retry.",
                        limit.as_secs()
                    ),
                )
            })?
    }

    fn managed(&self) -> bool {
        self.config.auth_source == AuthSource::Headless
    }

    fn hidden_tab(&self) -> bool {
        self.config.hidden_tab && self.config.auth_source == AuthSource::Browser
    }

    /// Uses a hidden Teams target that the connector creates in the existing browser.
    /// It shares the browser's normal sign-in but never appears in the tab strip, and the
    /// connector never attaches to the user's own tabs or to sign-in pages.
    async fn evaluate_hidden(&self, cdp: &Arc<Cdp>, input: Value) -> Result<Value, Failure> {
        let target = self.hidden_target(cdp).await?;
        let deadline = Instant::now() + READINESS_WAIT;
        loop {
            let info = cdp
                .call("Target.getTargetInfo", json!({"targetId":target}), None)
                .await?;
            let url = info["targetInfo"]["url"].as_str().unwrap_or("");
            if microsoft_signin(url) {
                return Err(browser_signin_required());
            }
            if is_teams_page(url) {
                match self
                    .evaluate_selected(cdp, input.clone(), &target, false)
                    .await
                {
                    Ok((value, _)) => {
                        let loading = matches!(
                            value["error"]["code"].as_str().unwrap_or(""),
                            "AUTH_REQUIRED"
                                | "SESSION_UNAVAILABLE"
                                | "REGION_UNAVAILABLE"
                                | "WRONG_ORIGIN"
                        );
                        if !(loading && wait_for_readiness(deadline).await) {
                            return Ok(value);
                        }
                        continue;
                    }
                    Err(error)
                        if matches!(
                            error.code.as_str(),
                            "PAGE_CHANGED" | "BROWSER_PROTOCOL_ERROR"
                        ) && cdp.is_open()
                            && wait_for_readiness(deadline).await =>
                    {
                        self.forget(cdp).await;
                        continue;
                    }
                    Err(error) => return Err(error),
                }
            }
            if !wait_for_readiness(deadline).await {
                return Err(Failure::new(
                    "TEAMS_TAB_REQUIRED",
                    "The hidden Teams page did not finish loading in the existing browser. Check that Teams opens there normally, then retry.",
                ));
            }
        }
    }

    async fn hidden_target(&self, cdp: &Arc<Cdp>) -> Result<String, Failure> {
        let mut shared = self.shared.lock().await;
        if let Some(target) = shared.hidden.clone() {
            if cdp
                .call("Target.getTargetInfo", json!({"targetId":target}), None)
                .await
                .is_ok()
            {
                return Ok(target);
            }
            shared.hidden = None;
        }
        let mut url =
            url::Url::parse("https://teams.microsoft.com/v2/").map_err(|_| protocol_error())?;
        if let Some(tenant) = self
            .config
            .tenant
            .as_deref()
            .and_then(|tenant| uuid::Uuid::parse_str(tenant).ok())
        {
            url.query_pairs_mut()
                .append_pair("tenantId", &tenant.to_string());
        }
        let created = cdp
            .call(
                "Target.createTarget",
                json!({"url":url.as_str(),"hidden":true}),
                None,
            )
            .await?;
        let target = created["targetId"]
            .as_str()
            .ok_or_else(protocol_error)?
            .to_owned();
        shared.hidden = Some(target.clone());
        Ok(target)
    }

    /// Closes the connector-owned hidden target, freeing its memory in the existing browser.
    pub async fn release_hidden(&self) {
        let mut shared = self.shared.lock().await;
        let Some(target) = shared.hidden.take() else {
            return;
        };
        if shared
            .page
            .as_ref()
            .is_some_and(|page| page.target == target)
        {
            shared.page = None;
        }
        if let Some(cdp) = shared.cdp.clone() {
            let _ = cdp
                .call("Target.closeTarget", json!({"targetId":target}), None)
                .await;
        }
    }

    async fn evaluate_inner(&self, input: Value) -> Result<Value, Failure> {
        let cdp = self.connection().await?;
        let result = if self.managed() {
            self.evaluate_managed(&cdp, input).await
        } else if self.hidden_tab() {
            self.evaluate_hidden(&cdp, input).await
        } else {
            match cdp.call("Target.getTargets", json!({}), None).await {
                Ok(targets) => match select_target(&targets, self.config.target_id.as_deref()) {
                    Ok(target) => self
                        .evaluate_selected(&cdp, input, &target, false)
                        .await
                        .map(|(value, _)| value),
                    Err(error) => Err(error),
                },
                Err(error) => Err(error),
            }
        };
        if result.is_err() {
            self.forget(&cdp).await;
        }
        if self.managed() {
            let _ = crate::managed::keepalive(&self.config);
        }
        result
    }

    /// Reuses one approved socket per endpoint; a changed endpoint or closed socket reconnects.
    async fn connection(&self) -> Result<Arc<Cdp>, Failure> {
        let endpoint = if self.managed() {
            crate::managed::endpoint(&self.config).await?
        } else {
            self.config
                .endpoint()
                .await
                .map_err(|message| Failure::new("BROWSER_UNAVAILABLE", &message))?
        };
        let mut shared = self.shared.lock().await;
        if let Some(cdp) = &shared.cdp
            && cdp.endpoint == endpoint
            && cdp.is_open()
        {
            return Ok(cdp.clone());
        }
        *shared = Shared::default();
        let cdp = Cdp::connect(&endpoint).await?;
        shared.cdp = Some(cdp.clone());
        Ok(cdp)
    }

    /// Detaches the page after a failure so the next call reattaches; a closed socket is dropped.
    async fn forget(&self, cdp: &Arc<Cdp>) {
        let mut shared = self.shared.lock().await;
        if !shared
            .cdp
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, cdp))
        {
            return;
        }
        if !cdp.is_open() {
            *shared = Shared::default();
            return;
        }
        if let Some(page) = shared.page.take() {
            let _ = cdp
                .call(
                    "Target.detachFromTarget",
                    json!({"sessionId":page.session}),
                    None,
                )
                .await;
        }
    }

    async fn evaluate_managed(&self, cdp: &Arc<Cdp>, input: Value) -> Result<Value, Failure> {
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
                // Restoration can navigate the page (for example to teams.cloud.microsoft)
                // while the bridge is installed; reattach within the readiness deadline.
                Err(error)
                    if matches!(
                        error.code.as_str(),
                        "PAGE_CHANGED" | "BROWSER_PROTOCOL_ERROR"
                    ) && cdp.is_open()
                        && wait_for_readiness(deadline).await =>
                {
                    self.forget(cdp).await;
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
        cdp: &Arc<Cdp>,
        input: Value,
        target: &str,
        can_reload: bool,
    ) -> Result<(Value, bool), Failure> {
        let value = self.call_bridge(cdp, target, input).await?;
        if !(can_reload && value["error"]["code"] == "AUTH_EXPIRED") {
            return Ok((value, false));
        }
        let page = self.page(cdp, target).await?;
        cdp.call("Page.reload", json!({}), Some(&page.session))
            .await?;
        self.reset_bridge(&page.session).await;
        Ok((value, true))
    }

    async fn call_bridge(
        &self,
        cdp: &Arc<Cdp>,
        target: &str,
        input: Value,
    ) -> Result<Value, Failure> {
        for attempt in 0..2 {
            let page = self.page(cdp, target).await?;
            let bridge = page.bridge.clone().ok_or_else(protocol_error)?;
            let params = json!({"functionDeclaration":CALL_BRIDGE,"objectId":bridge,
                "arguments":[{"value":input}],"awaitPromise":true,"returnByValue":true});
            let call = cdp.call("Runtime.callFunctionOn", params, Some(&page.session));
            let result = if self.managed() {
                self.with_lease(cdp, &page.session, call).await
            } else {
                call.await
            };
            match result {
                Ok(result) if result.get("exceptionDetails").is_some() => {
                    self.reset_bridge(&page.session).await;
                    return Err(page_changed());
                }
                Ok(result) => {
                    return result["result"]
                        .get("value")
                        .cloned()
                        .ok_or_else(protocol_error);
                }
                // A navigation destroys the installed context; reinstall once on the same socket.
                Err(_) if attempt == 0 && cdp.is_open() => self.reset_bridge(&page.session).await,
                Err(error) => return Err(error),
            }
        }
        Err(protocol_error())
    }

    /// While a managed call runs, keep the idle lease fresh and the page unfrozen.
    async fn with_lease(
        &self,
        cdp: &Arc<Cdp>,
        session: &str,
        call: impl Future<Output = Result<Value, Failure>>,
    ) -> Result<Value, Failure> {
        tokio::pin!(call);
        loop {
            tokio::select! {
                result = &mut call => return result,
                _ = sleep(LEASE_INTERVAL) => {
                    let _ = crate::managed::keepalive(&self.config);
                    let _ = cdp.call("Page.setWebLifecycleState", json!({"state":"active"}), Some(session)).await;
                }
            }
        }
    }

    /// Returns the attached page for `target`, attaching and installing the bridge when needed.
    async fn page(&self, cdp: &Arc<Cdp>, target: &str) -> Result<Page, Failure> {
        let mut shared = self.shared.lock().await;
        let reusable = shared
            .page
            .as_ref()
            .is_some_and(|page| page.target == target);
        if !reusable {
            if let Some(old) = shared.page.take() {
                let _ = cdp
                    .call(
                        "Target.detachFromTarget",
                        json!({"sessionId":old.session}),
                        None,
                    )
                    .await;
            }
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
            shared.page = Some(Page {
                target: target.into(),
                session,
                bridge: None,
            });
        }
        let page = shared.page.as_mut().ok_or_else(protocol_error)?;
        if self.managed() {
            // The managed supervisor freezes an idle page; every call makes it active first.
            cdp.call(
                "Page.setWebLifecycleState",
                json!({"state":"active"}),
                Some(&page.session),
            )
            .await?;
        }
        if page.bridge.is_none() {
            let native = self.config.auth_source == AuthSource::App;
            page.bridge = Some(install_bridge(cdp, &page.session, native).await?);
        }
        Ok(page.clone())
    }

    async fn reset_bridge(&self, session: &str) {
        let mut shared = self.shared.lock().await;
        if let Some(page) = shared.page.as_mut()
            && page.session == session
        {
            page.bridge = None;
        }
    }
}

/// Installs the fixed bridge once per execution context and returns its function handle.
async fn install_bridge(cdp: &Cdp, session: &str, native: bool) -> Result<String, Failure> {
    let mut params = json!({"expression":include_str!("bridge.js"),"returnByValue":false});
    if !native {
        // The isolated world keeps the bridge away from Teams page scripts. The native
        // App host bridge is installed in its main world only.
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
        params["contextId"] = world["executionContextId"].clone();
        if !params["contextId"].is_i64() {
            return Err(protocol_error());
        }
    }
    let result = cdp.call("Runtime.evaluate", params, Some(session)).await?;
    if result.get("exceptionDetails").is_some() {
        return Err(page_changed());
    }
    result["result"]["objectId"]
        .as_str()
        .map(String::from)
        .ok_or_else(protocol_error)
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

fn browser_signin_required() -> Failure {
    Failure::new(
        "BROWSER_SIGNIN_REQUIRED",
        "This browser is not signed in to Teams for the selected account. Open Teams in it once and sign in normally; the connector never completes sign-in for you.",
    )
}

fn setup_required() -> Failure {
    Failure::new(
        "AUTH_SETUP_REQUIRED",
        "The managed Teams session needs interactive sign-in. Run teams-mcp auth stop, then teams-mcp auth login and finish signing in to the intended account and organization. MCP never opens a visible sign-in window automatically.",
    )
}

pub(crate) fn select_target(targets: &Value, selected: Option<&str>) -> Result<String, Failure> {
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

/// One CDP WebSocket shared by concurrent calls. A reader task routes each response
/// to its caller by ID, so a cancelled call never leaves an unread response behind.
pub(crate) struct Cdp {
    writer: Mutex<SplitSink<Socket, Message>>,
    pending: Pending,
    next_id: AtomicU64,
    open: Arc<AtomicBool>,
    reader: JoinHandle<()>,
    pub(crate) endpoint: String,
}

impl Drop for Cdp {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

/// Removes a call's routing entry when the call completes or is cancelled.
struct Registration<'a> {
    pending: &'a Pending,
    id: u64,
}

impl Drop for Registration<'_> {
    fn drop(&mut self) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.remove(&self.id);
        }
    }
}

impl Cdp {
    pub(crate) async fn connect(endpoint: &str) -> Result<Arc<Self>, Failure> {
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
        let (writer, mut reader) = socket.split();
        let pending: Pending = Arc::default();
        let open = Arc::new(AtomicBool::new(true));
        let task = tokio::spawn({
            let pending = pending.clone();
            let open = open.clone();
            async move {
                while let Some(Ok(message)) = reader.next().await {
                    let Message::Text(text) = message else {
                        continue;
                    };
                    let Ok(mut response) = serde_json::from_str::<Value>(&text) else {
                        continue;
                    };
                    let Some(id) = response["id"].as_u64() else {
                        continue;
                    };
                    let sender = pending.lock().ok().and_then(|mut map| map.remove(&id));
                    if let Some(sender) = sender {
                        let result = if response.get("error").is_some() {
                            Err(protocol_error())
                        } else {
                            response
                                .get_mut("result")
                                .map(Value::take)
                                .ok_or_else(protocol_error)
                        };
                        let _ = sender.send(result);
                    }
                }
                open.store(false, Ordering::SeqCst);
                if let Ok(mut pending) = pending.lock() {
                    pending.clear();
                }
            }
        });
        Ok(Arc::new(Self {
            writer: Mutex::new(writer),
            pending,
            next_id: AtomicU64::new(1),
            open,
            reader: task,
            endpoint: endpoint.to_owned(),
        }))
    }

    pub(crate) fn is_open(&self) -> bool {
        self.open.load(Ordering::SeqCst)
    }

    pub(crate) async fn call(
        &self,
        method: &str,
        params: Value,
        session: Option<&str>,
    ) -> Result<Value, Failure> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (sender, receiver) = oneshot::channel();
        self.pending
            .lock()
            .map_err(|_| unavailable())?
            .insert(id, sender);
        let _registration = Registration {
            pending: &self.pending,
            id,
        };
        // The reader marks the socket closed before clearing routes, so this check
        // cannot miss a response route that was registered after the clear.
        if !self.is_open() {
            return Err(unavailable());
        }
        let mut message = json!({"id":id,"method":method,"params":params});
        if let Some(session) = session {
            message["sessionId"] = json!(session);
        }
        self.writer
            .lock()
            .await
            .send(Message::Text(message.to_string().into()))
            .await
            .map_err(|_| unavailable())?;
        receiver.await.map_err(|_| unavailable())?
    }
}

fn page_changed() -> Failure {
    Failure::new(
        "PAGE_CHANGED",
        "Teams changed or closed while the request ran. Retry with the intended Teams tab open.",
    )
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

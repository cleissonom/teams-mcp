use crate::{
    app::App,
    auth,
    cdp::Browser,
    config::{AuthSource, Config, Transport},
    error::Failure,
    lease::{Lease, Mint},
    operations::{self, Operation},
};
use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock as Content,
        Implementation, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig,
        Tool, ToolAnnotations,
    },
    service::RequestContext,
};
use serde_json::{Value, json};
use std::{
    sync::{Arc, OnceLock},
    time::Instant,
};

#[derive(Clone)]
pub struct TeamsServer {
    browser: Browser,
    app: App,
    config: Config,
    /// Present in lease transport: tokens minted by the session, requests sent from Rust.
    lease: Option<Result<Arc<Lease>, Failure>>,
    /// Lease minting and explicit verification share one temporary hidden target.
    session_gate: Arc<tokio::sync::Mutex<()>>,
}

impl TeamsServer {
    pub fn new(config: Config) -> Self {
        let browser = Browser::new(config.clone());
        let app = App::new(config.clone());
        let session_gate = Arc::new(tokio::sync::Mutex::new(()));
        let lease = (config.transport == Transport::Lease).then(|| {
            let (source, browser, app) = (config.clone(), browser.clone(), app.clone());
            let session_gate = session_gate.clone();
            let mint: Mint = Arc::new(move || {
                let (source, browser, app) = (source.clone(), browser.clone(), app.clone());
                let session_gate = session_gate.clone();
                Box::pin(async move {
                    let _session = session_gate.lock().await;
                    let minted = session(&source, &browser, &app, json!({"action":"lease"})).await;
                    // A hidden Teams page in the user's browser is only needed while minting.
                    browser.release_hidden().await;
                    minted
                })
            });
            Lease::new(mint, config.log).map(Arc::new)
        });
        Self {
            browser,
            app,
            config,
            lease,
            session_gate,
        }
    }

    /// Whether to warm the session after MCP initialization (opt-in).
    pub fn prewarm(&self) -> bool {
        self.config.prewarm
    }

    /// Explicit CLI setup only; MCP tools never open a visible browser tab.
    pub(crate) async fn open_browser_login_tab(&self) -> Result<(), Failure> {
        tokio::time::timeout(self.config.request_timeout, self.browser.open_login_tab())
            .await
            .unwrap_or_else(|_| Err(self.request_timeout()))
    }

    pub async fn execute(&self, name: &str, args: Value) -> Result<Value, Failure> {
        let started = Instant::now();
        let result = tokio::time::timeout(self.config.request_timeout, async {
            let _activity = if self.config.auth_source == AuthSource::Headless {
                crate::managed::request_guard(&self.config).await?
            } else {
                None
            };
            self.dispatch(name, args).await
        })
        .await
        .unwrap_or_else(|_| Err(self.request_timeout()))
        .map_err(|error| auth::recover(error, &self.recovery_config()));
        if self.config.log {
            // Diagnostics only: tool name, duration and error code, never content.
            let code = result
                .as_ref()
                .err()
                .map_or("ok", |error| error.code.as_str());
            eprintln!(
                "teams-mcp: tool={name} ms={} result={code}",
                started.elapsed().as_millis()
            );
        }
        result
    }

    fn request_timeout(&self) -> Failure {
        if self.config.auth_source == AuthSource::Headless
            && crate::managed::status(&self.config)
                .is_ok_and(|status| status["authentication"]["state"] == "signing_in")
        {
            return Failure::new(
                "BROWSER_BUSY",
                "Explicit Teams sign-in is still in progress. Wait for it to complete before verifying the session; do not start another login.",
            );
        }
        Failure::new(
            if self.config.auth_source == AuthSource::App {
                "APP_TIMEOUT"
            } else {
                "BROWSER_TIMEOUT"
            },
            "The Teams request exceeded its configured deadline. Check the selected session and retry.",
        )
    }

    fn recovery_config(&self) -> Config {
        let mut config = if self.config.auth_source == AuthSource::Headless {
            crate::managed::session_config(&self.config).unwrap_or_else(|_| self.config.clone())
        } else {
            self.config.clone()
        };
        let (account, tenant) =
            if config.auth_source == AuthSource::App && config.app_cdp_url.is_none() {
                self.app.selection()
            } else {
                self.browser.selection()
            };
        config.account = account.or(config.account);
        config.tenant = tenant.or(config.tenant);
        config
    }

    async fn verify_session(&self) -> Result<Value, Failure> {
        // A recovery check must observe the selected source, not an older in-memory lease.
        let _session = if self.lease.is_some() {
            Some(self.session_gate.lock().await)
        } else {
            None
        };
        let result = session(
            &self.config,
            &self.browser,
            &self.app,
            json!({"action":"status"}),
        )
        .await;
        if self.lease.is_some() {
            self.browser.release_hidden().await;
        }
        result
    }

    async fn dispatch(&self, name: &str, args: Value) -> Result<Value, Failure> {
        match name {
            "teams_status" | "teams_get_me" | "teams_login" => {
                if !args.as_object().is_some_and(|args| args.is_empty()) {
                    return Err(Failure::new(
                        "INVALID_INPUT",
                        "This tool accepts an empty object. Use explicit setup to sign in or change accounts.",
                    ));
                }
                let mut value = checked(if name == "teams_get_me" {
                    self.evaluate(json!({"action":"status"})).await?
                } else {
                    self.verify_session().await?
                })?;
                if name == "teams_get_me" {
                    return Ok(json!({"profile":value["profile"].take()}));
                }
                let source = match self.config.auth_source {
                    AuthSource::Headless => "headless",
                    AuthSource::App => "existing-app",
                    AuthSource::Browser => "existing-browser",
                };
                let (transport, storage) = match self.config.transport {
                    Transport::Page => ("page", "source-session-only"),
                    Transport::Lease => (
                        "lease",
                        "source-session; short-lived access tokens in memory",
                    ),
                };
                Ok(
                    json!({"authenticated":true,"authentication":auth::Authentication::verified(),"profile":value["profile"].take(),"capabilities":value["capabilities"].take(),"authSource":source,"transport":transport,"credentialStorage":storage}),
                )
            }
            _ => self.execute_operation(name, args).await,
        }
    }

    async fn execute_operation(&self, name: &str, args: Value) -> Result<Value, Failure> {
        let operation = Operation::parse(name, args)
            .map_err(|message| Failure::new("INVALID_INPUT", &message))?;
        let mut responses = Vec::new();
        let mut warnings = Vec::new();
        let mut profile = Value::Null;
        for request in operation.requests() {
            let mut value = checked(
                self.evaluate(json!({"action":"request","request":request}))
                    .await?,
            )?;
            profile = value["profile"].take();
            responses.push(value["data"].take());
        }
        let follow_up = operation
            .follow_up(&responses, &profile)
            .map_err(|message| Failure::new("INVALID_RESPONSE", &message))?;
        if !follow_up.is_empty() {
            // Dependent reads run together inside the page; each item reports its own result.
            let mut value = checked(
                self.evaluate(json!({"action":"batch","requests":follow_up}))
                    .await?,
            )?;
            match value["data"].take() {
                Value::Array(items) => {
                    warnings = auth::batch_warnings(&items, &self.recovery_config());
                    responses.extend(items);
                }
                _ => {
                    return Err(Failure::new(
                        "INVALID_RESPONSE",
                        "Teams returned an unsupported batch response.",
                    ));
                }
            }
        }
        let mut result = operation
            .finish(responses, &profile)
            .map_err(|message| Failure::new("INVALID_RESPONSE", &message))?;
        if !warnings.is_empty() {
            result["warnings"] = json!(warnings);
        }
        Ok(result)
    }

    async fn evaluate(&self, input: Value) -> Result<Value, Failure> {
        match &self.lease {
            Some(Ok(lease)) => lease.evaluate(input).await,
            Some(Err(error)) => Err(error.clone()),
            None => session(&self.config, &self.browser, &self.app, input).await,
        }
    }
}

/// Runs the fixed bridge in the configured browser or App session.
async fn session(
    config: &Config,
    browser: &Browser,
    app: &App,
    input: Value,
) -> Result<Value, Failure> {
    if config.auth_source == AuthSource::App && config.app_cdp_url.is_none() {
        app.evaluate(input).await
    } else {
        browser.evaluate(input).await
    }
}

pub fn tools() -> &'static [Tool] {
    static TOOLS: OnceLock<Vec<Tool>> = OnceLock::new();
    TOOLS.get_or_init(build_tools)
}

fn build_tools() -> Vec<Tool> {
    let mut tools = operations::tools();
    for (name, description) in [
        (
            "teams_status",
            "Verify the selected Teams session after setup or diagnose authentication. Checks the source session and cached API capabilities, not every API permission. Never opens a login window. Follow error.recovery when authentication fails.",
        ),
        (
            "teams_get_me",
            "Get the profile and tenant of the account selected in the configured Teams session.",
        ),
        (
            "teams_login",
            "Compatibility alias for teams_status: only rechecks the session; it does not log in. On failure, follow error.recovery.command for the explicit local sign-in workflow, then verify and retry the original read once. Never opens a login window or asks for credentials.",
        ),
    ] {
        let schema = json!({"type":"object","properties":{},"additionalProperties":false})
            .as_object()
            .unwrap()
            .clone();
        tools.push(
            Tool::new(name, description, schema).with_annotations(
                ToolAnnotations::new()
                    .read_only(true)
                    .destructive(false)
                    .idempotent(true)
                    .open_world(true),
            ),
        );
    }
    tools
}

fn checked(value: Value) -> Result<Value, Failure> {
    if value["ok"] == true {
        return Ok(value);
    }
    let error = &value["error"];
    Err(Failure::new(
        error["code"].as_str().unwrap_or("INVALID_RESPONSE"),
        error["message"]
            .as_str()
            .unwrap_or("Teams returned an unsupported session response."),
    ))
}

pub fn outcome(result: Result<Value, Failure>) -> Value {
    match result {
        Ok(mut value) => {
            value["success"] = json!(true);
            value
        }
        Err(error) => json!({"success":false,"error":error}),
    }
}

impl ServerHandler for TeamsServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("teams-mcp", env!("CARGO_PKG_VERSION")))
            .with_instructions(auth::INSTRUCTIONS)
    }

    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(tools().to_vec()))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        tools().iter().find(|tool| tool.name == name).cloned()
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let args = Value::Object(request.arguments.unwrap_or_default());
        let result = tokio::select! {
            biased;
            _ = context.ct.cancelled() => Err(Failure::new("CANCELLED", "The Teams request was cancelled.")),
            result = self.execute(&request.name, args) => result,
        };
        let failed = result.is_err();
        let data = outcome(result);
        let content = Content::json(data.clone())?;
        let mut result = if failed {
            CallToolResult::error(vec![content])
        } else {
            CallToolResult::success(vec![content])
        };
        result.structured_content = Some(data);
        Ok(result.into())
    }
}

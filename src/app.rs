use crate::{config::Config, error::Failure, identity::IdentityBinding};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::BTreeSet, future::Future, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
    time::timeout,
};

const WEBVIEW: &str =
    "/Applications/Microsoft Teams.app/Contents/Helpers/Microsoft Teams WebView.app";
const MAX_OUTPUT: usize = 4 * 1024 * 1024;

#[derive(Clone)]
pub struct App {
    config: Config,
    identity: IdentityBinding,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AppTarget {
    pub window_id: i64,
    pub tab_id: i64,
}

impl App {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            identity: IdentityBinding::default(),
        }
    }

    pub fn selection(&self) -> (Option<String>, Option<String>) {
        self.identity.selection(&self.config)
    }

    pub async fn evaluate(&self, input: Value) -> Result<Value, Failure> {
        if !cfg!(target_os = "macos") {
            return Err(app_failure("APP_UNSUPPORTED_PLATFORM"));
        }
        timeout(
            Duration::from_secs(65),
            self.evaluate_with(input, run_script),
        )
        .await
        .map_err(|_| app_failure("APP_TIMEOUT"))?
    }

    pub(crate) async fn evaluate_with<F, Fut>(
        &self,
        mut input: Value,
        mut run: F,
    ) -> Result<Value, Failure>
    where
        F: FnMut(String) -> Fut,
        Fut: Future<Output = Result<String, Failure>>,
    {
        if !input.is_object() {
            return Err(app_failure("APP_INVALID_INPUT"));
        }
        input["nativeApp"] = json!(true);
        self.identity.apply(&self.config, &mut input)?;
        let inventory = parse_response(&run(inventory_script()).await?)?;
        let targets: Vec<AppTarget> = serde_json::from_value(inventory["targets"].clone())
            .map_err(|_| app_failure("APP_PROTOCOL_ERROR"))?;
        let target = select_target(&targets, self.config.app_window_id)?;
        let slot = format!("__teams_mcp_{}", uuid::Uuid::new_v4().simple());
        let value = parse_response(&run(execution_script(&target, &input, &slot)).await?)?;
        self.identity.check(&self.config, &value)?;
        Ok(value)
    }
}

pub(crate) fn select_target(
    targets: &[AppTarget],
    selected: Option<i64>,
) -> Result<AppTarget, Failure> {
    if targets
        .iter()
        .any(|target| target.window_id <= 0 || target.tab_id <= 0)
    {
        return Err(app_failure("APP_PROTOCOL_ERROR"));
    }
    let eligible: Vec<_> = targets
        .iter()
        .filter(|target| selected.is_none_or(|id| id == target.window_id))
        .collect();
    if let [target] = eligible.as_slice() {
        return Ok((*target).clone());
    }
    if targets.is_empty() && selected.is_none() {
        return Err(app_failure("APP_TEAMS_TAB_REQUIRED"));
    }
    let ids: BTreeSet<_> = targets.iter().map(|target| target.window_id).collect();
    let choices = ids
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    let message = format!(
        "Select TEAMS_APP_WINDOW_ID for the intended existing Teams window and leave one Teams tab in that window. Available window IDs: {choices}."
    );
    Err(Failure::new("APP_WINDOW_REQUIRED", &message))
}

pub(crate) fn js_string(value: &str) -> String {
    serde_json::to_string(value)
        .expect("strings are JSON serializable")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

fn jxa_prelude() -> String {
    format!(
        r#"
const appPath = {};
function errorResult(code) {{ return JSON.stringify({{ok:false,error:{{code:code}}}}); }}
function isTeams(url) {{ return /^https:\/\/(?:teams\.microsoft\.com|teams\.cloud\.microsoft)(?::443)?(?:[/?#]|$)/i.test(String(url)); }}
function checkedApp() {{ const app=Application(appPath); if(!app.running()) throw {{appCode:'APP_NOT_RUNNING'}}; return app; }}
function safeError(error) {{
    if(error.appCode) return errorResult(error.appCode);
    const number=Number(error.errorNumber || error.number);
    const message=String(error.message || '');
    if(number===-1743 || number===-10004 || /-1743/.test(message)) return errorResult('APP_AUTOMATION_DENIED');
    if(number===12 || number===-12 || /JavaScript.*Apple Events|Apple Events.*JavaScript/i.test(message)) return errorResult('APP_AUTOMATION_DISABLED');
    return errorResult('APP_AUTOMATION_FAILED');
}}
"#,
        js_string(WEBVIEW)
    )
}

pub(crate) fn inventory_script() -> String {
    format!(
        r#"(() => {{
{}
try {{
    const app=checkedApp(), targets=[];
    for(const window of app.windows()) {{
        const windowId=Number(window.id());
        for(const tab of window.tabs()) {{
            const tabId=Number(tab.id());
            if(Number.isSafeInteger(windowId) && windowId>0 && Number.isSafeInteger(tabId) && tabId>0 && isTeams(tab.url())) targets.push({{windowId,tabId}});
        }}
    }}
    return JSON.stringify({{ok:true,targets}});
}} catch(error) {{ return safeError(error); }}
}})()"#,
        jxa_prelude()
    )
}

pub(crate) fn execution_script(target: &AppTarget, input: &Value, slot: &str) -> String {
    let mut input = input.clone();
    input["nativeApp"] = json!(true);
    let launch = launch_expression(&input, slot);
    let poll = format!(
        r#"(() => {{ const value=globalThis[{}]; if(typeof value!=='string') return 'TEAMS_MCP_PENDING'; delete globalThis[{}]; return value; }})()"#,
        js_string(slot),
        js_string(slot)
    );
    let cleanup = format!(
        "delete globalThis[{}]; 'TEAMS_MCP_CLEANED'",
        js_string(slot)
    );
    format!(
        r#"(() => {{
{}
let tab=null;
try {{
    const app=checkedApp();
    const window=app.windows().find(window => Number(window.id())==={});
    if(!window) throw {{appCode:'APP_WINDOW_REQUIRED'}};
    tab=window.tabs().find(tab => Number(tab.id())==={});
    if(!tab || !isTeams(tab.url())) throw {{appCode:'APP_TEAMS_TAB_REQUIRED'}};
    if(String(tab.execute({{javascript:{}}}))!=='TEAMS_MCP_STARTED') throw {{appCode:'APP_PROTOCOL_ERROR'}};
    const deadline=Date.now()+40000;
    while(Date.now()<deadline) {{
        if(!isTeams(tab.url())) throw {{appCode:'APP_TEAMS_TAB_REQUIRED'}};
        const result=String(tab.execute({{javascript:{}}}));
        if(result!=='TEAMS_MCP_PENDING') return result;
        delay(0.25);
    }}
    return errorResult('APP_TIMEOUT');
}} catch(error) {{ return safeError(error); }}
finally {{
    if(tab) {{ try {{ if(isTeams(tab.url())) tab.execute({{javascript:{}}}); }} catch(error) {{ }} }}
}}
}})()"#,
        jxa_prelude(),
        target.window_id,
        target.tab_id,
        js_string(&launch),
        js_string(&poll),
        js_string(&cleanup)
    )
}

fn launch_expression(input: &Value, slot: &str) -> String {
    let input = input
        .to_string()
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029");
    format!(
        r#"(() => {{
const slot={};
Object.defineProperty(globalThis,slot,{{value:null,writable:true,configurable:true}});
const store=value => {{ if(Object.prototype.hasOwnProperty.call(globalThis,slot)) globalThis[slot]=JSON.stringify(value); }};
const failed=() => store({{ok:false,error:{{code:'APP_REQUEST_FAILED'}}}});
setTimeout(() => {{ delete globalThis[slot]; }},45000);
Promise.resolve().then(() => ({})({})).then(value => {{ try {{ store(value); }} catch(error) {{ failed(); }} }}).catch(failed);
return 'TEAMS_MCP_STARTED';
}})()"#,
        js_string(slot),
        include_str!("bridge.js"),
        input
    )
}

pub(crate) fn parse_response(output: &str) -> Result<Value, Failure> {
    if output.len() > MAX_OUTPUT {
        return Err(app_failure("APP_OUTPUT_TOO_LARGE"));
    }
    let value: Value =
        serde_json::from_str(output.trim()).map_err(|_| app_failure("APP_PROTOCOL_ERROR"))?;
    match value["ok"].as_bool() {
        Some(false)
            if value["error"]["code"]
                .as_str()
                .is_some_and(|code| code.starts_with("APP_")) =>
        {
            Err(app_failure(value["error"]["code"].as_str().unwrap()))
        }
        Some(_) => Ok(value),
        None => Err(app_failure("APP_PROTOCOL_ERROR")),
    }
}

pub(crate) fn automation_failure(raw: &str) -> Failure {
    if raw.contains("(-1743)") || raw.contains("(-10004)") {
        return app_failure("APP_AUTOMATION_DENIED");
    }
    if raw.contains("(12)")
        || raw.contains("(-12)")
        || (raw.contains("JavaScript") && raw.contains("Apple Events"))
    {
        return app_failure("APP_AUTOMATION_DISABLED");
    }
    app_failure("APP_AUTOMATION_FAILED")
}

fn app_failure(code: &str) -> Failure {
    let (code, message) = match code {
        "APP_NOT_RUNNING" => (
            code,
            "Open Microsoft Teams and sign in, then retry. The connector does not start the application.",
        ),
        "APP_TEAMS_TAB_REQUIRED" => (
            code,
            "No eligible Teams tab is available in the selected running app window. Open the intended Teams account in the app and retry.",
        ),
        "APP_WINDOW_REQUIRED" => (
            code,
            "Set TEAMS_APP_WINDOW_ID to select one existing Teams window and leave one Teams tab in that window.",
        ),
        "APP_AUTOMATION_DISABLED" => (
            code,
            "Teams JavaScript from Apple Events is disabled. App authentication requires View > Developer > Allow JavaScript from Apple Events to be available and enabled in the Teams WebView.",
        ),
        "APP_AUTOMATION_DENIED" => (
            code,
            "macOS denied automation access to Microsoft Teams WebView. Review the owner's Automation permission for the calling application, then retry.",
        ),
        "APP_TIMEOUT" => (
            code,
            "The existing Teams app did not complete the request within the time limit. Check the selected Teams window and retry.",
        ),
        "APP_OUTPUT_TOO_LARGE" => (
            code,
            "Teams app response exceeded 4 MiB. Use a smaller page.",
        ),
        "APP_UNSUPPORTED_PLATFORM" => (
            code,
            "Native Teams app authentication currently requires macOS. Use browser authentication on other platforms.",
        ),
        "APP_INVALID_INPUT" => (code, "App requests require an object."),
        "APP_PROTOCOL_ERROR" => (
            code,
            "The existing Teams WebView returned an unsupported automation response. Check the selected Teams window and retry.",
        ),
        "APP_REQUEST_FAILED" => (
            code,
            "The existing Teams app could not complete the request. Finish loading or signing in to Teams and retry.",
        ),
        "APP_BRIDGE_UNAVAILABLE" => (
            code,
            "The selected Teams app page does not expose the required native authentication bridge.",
        ),
        _ => (
            "APP_AUTOMATION_FAILED",
            "Could not communicate with the existing Microsoft Teams WebView. Check that the app is open and its automation support is available.",
        ),
    };
    Failure::new(code, message)
}

pub(crate) async fn read_capped<R: AsyncRead + Unpin>(
    reader: R,
    limit: usize,
) -> Result<Vec<u8>, Failure> {
    let mut bytes = Vec::new();
    reader
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| app_failure("APP_AUTOMATION_FAILED"))?;
    if bytes.len() > limit {
        return Err(app_failure("APP_OUTPUT_TOO_LARGE"));
    }
    Ok(bytes)
}

pub(crate) async fn run_script(script: String) -> Result<String, Failure> {
    let mut child = Command::new("/usr/bin/osascript")
        .args(["-l", "JavaScript"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| app_failure("APP_AUTOMATION_FAILED"))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| app_failure("APP_AUTOMATION_FAILED"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| app_failure("APP_AUTOMATION_FAILED"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| app_failure("APP_AUTOMATION_FAILED"))?;
    let write = async move {
        stdin.write_all(script.as_bytes()).await?;
        stdin.shutdown().await?;
        drop(stdin);
        Ok::<(), std::io::Error>(())
    };
    let (written, output, errors, status) = tokio::join!(
        write,
        read_capped(stdout, MAX_OUTPUT),
        read_capped(stderr, 16384),
        child.wait()
    );
    let output = output?;
    let errors = errors?;
    if !status
        .map_err(|_| app_failure("APP_AUTOMATION_FAILED"))?
        .success()
    {
        return Err(automation_failure(&String::from_utf8_lossy(&errors)));
    }
    written.map_err(|_| app_failure("APP_AUTOMATION_FAILED"))?;
    String::from_utf8(output).map_err(|_| app_failure("APP_PROTOCOL_ERROR"))
}

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use teams_mcp::{
    config::{AuthSource, Config, Transport},
    server::{TeamsServer, outcome},
};
use tokio::{net::TcpListener, process::Command, task::JoinHandle, time::timeout};
use tokio_tungstenite::{accept_async, tungstenite::Message};

const ACCOUNT: &str = "fixture@example.test";
const TENANT: &str = "11111111-2222-4333-8444-555555555555";

struct Fixture {
    endpoint: String,
    response: Arc<Mutex<Value>>,
    calls: Arc<Mutex<Vec<Value>>>,
    targets: Arc<Mutex<Vec<Value>>>,
    created: Arc<Mutex<Vec<Value>>>,
    connections: Arc<AtomicUsize>,
    fail_install: Arc<AtomicBool>,
    fail_origin: Arc<AtomicBool>,
    task: JoinHandle<()>,
}

impl Fixture {
    async fn new(code: Option<&str>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!(
            "ws://{}/devtools/browser/recovery",
            listener.local_addr().unwrap()
        );
        let response = Arc::new(Mutex::new(bridge_response(code)));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let targets = Arc::new(Mutex::new(vec![
            json!({"targetId":"teams","type":"page","url":"https://teams.microsoft.com/v2/"}),
        ]));
        let created = Arc::new(Mutex::new(Vec::new()));
        let connections = Arc::new(AtomicUsize::new(0));
        let fail_install = Arc::new(AtomicBool::new(false));
        let fail_origin = Arc::new(AtomicBool::new(false));
        let (state, requests) = (response.clone(), calls.clone());
        let (pages, creations, accepted) = (targets.clone(), created.clone(), connections.clone());
        let installation = fail_install.clone();
        let origin_change = fail_origin.clone();
        let task = tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                accepted.fetch_add(1, Ordering::SeqCst);
                let (state, requests) = (state.clone(), requests.clone());
                let (pages, creations) = (pages.clone(), creations.clone());
                let installation = installation.clone();
                let origin_change = origin_change.clone();
                tokio::spawn(async move {
                    let mut socket = accept_async(socket).await.unwrap();
                    while let Some(Ok(Message::Text(text))) = socket.next().await {
                        let call: Value = serde_json::from_str(&text).unwrap();
                        let result = match call["method"].as_str().unwrap() {
                            "Target.getTargets" => {
                                json!({"targetInfos":*pages.lock().unwrap()})
                            }
                            "Target.createTarget" => {
                                creations.lock().unwrap().push(call["params"].clone());
                                pages.lock().unwrap().push(json!({"targetId":"created-teams","type":"page","url":call["params"]["url"]}));
                                json!({"targetId":"created-teams"})
                            }
                            "Target.attachToTarget" => json!({"sessionId":"session"}),
                            "Page.getFrameTree" => json!({"frameTree":{"frame":{"id":"root"}}}),
                            "Page.createIsolatedWorld" => json!({"executionContextId":42}),
                            "Runtime.evaluate" => json!({"result":{"objectId":"bridge"}}),
                            "Runtime.callFunctionOn" => {
                                let input = call["params"]["arguments"][0]["value"].clone();
                                requests.lock().unwrap().push(input.clone());
                                let mut response = state.lock().unwrap().clone();
                                if origin_change.swap(false, Ordering::SeqCst) {
                                    response = bridge_response(Some("WRONG_ORIGIN"));
                                }
                                if input["action"] == "batch" && response.get("_batch").is_some() {
                                    response["data"] = response["_batch"].take();
                                }
                                json!({"result":{"value":response}})
                            }
                            _ => json!({}),
                        };
                        let response = if call["method"] == "Page.createIsolatedWorld"
                            && installation.swap(false, Ordering::SeqCst)
                        {
                            json!({"id":call["id"],"error":{"code":-32000,"message":"Frame navigated during setup."}})
                        } else {
                            json!({"id":call["id"],"result":result})
                        };
                        if socket
                            .send(Message::Text(response.to_string().into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                });
            }
        });
        Self {
            endpoint,
            response,
            calls,
            targets,
            created,
            connections,
            fail_install,
            fail_origin,
            task,
        }
    }

    fn config(&self, source: AuthSource) -> Config {
        Config {
            auth_source: source,
            transport: Transport::Page,
            cdp_url: Some(self.endpoint.clone()),
            app_cdp_url: Some(self.endpoint.clone()),
            account: Some(ACCOUNT.into()),
            tenant: Some(TENANT.into()),
            ..Config::default()
        }
    }

    fn sign_in(&self) {
        *self.response.lock().unwrap() = bridge_response(None);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn bridge_response(code: Option<&str>) -> Value {
    match code {
        Some(code) => {
            json!({"ok":false,"error":{"code":code,"message":"Synthetic session failure."}})
        }
        None => {
            json!({"ok":true,"profile":{"id":"user-one","email":ACCOUNT,"tenantId":TENANT},"capabilities":{"chat":true},"data":{"conversations":[]}})
        }
    }
}

fn cli() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_teams-mcp"));
    for (key, _) in std::env::vars().filter(|(key, _)| key.starts_with("TEAMS_")) {
        command.env_remove(key);
    }
    command.env("TEAMS_BROWSER_BINARY", "/missing/browser").env(
        "TEAMS_BROWSER_PROFILE",
        std::env::temp_dir().join(format!("teams-cli-isolated-{}", uuid::Uuid::new_v4())),
    );
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

async fn output(mut command: Command) -> Value {
    let output = timeout(Duration::from_secs(5), command.output())
        .await
        .unwrap()
        .unwrap();
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|_| panic!("expected CLI JSON, code {:?}", output.status.code()))
}

#[tokio::test]
async fn absent_managed_session_has_source_specific_executable_recovery_without_starting_chrome() {
    let profile = std::env::temp_dir().join(format!("teams-recovery-{}", uuid::Uuid::new_v4()));
    let config = Config {
        auth_source: AuthSource::Headless,
        browser_profile: Some(profile.clone()),
        account: Some(ACCOUNT.into()),
        tenant: Some(TENANT.into()),
        ..Config::default()
    };
    let failure = outcome(
        TeamsServer::new(config)
            .execute("teams_login", json!({}))
            .await,
    );
    assert_eq!(failure["error"]["code"], "AUTH_SETUP_REQUIRED");
    let recovery = &failure["error"]["recovery"];
    assert_eq!(recovery["authSource"], "headless");
    assert_eq!(recovery["interactionRequired"], true);
    assert!(PathBuf::from(recovery["command"]["executable"].as_str().unwrap()).is_absolute());
    assert_eq!(
        recovery["command"]["args"],
        json!(["--auth-source", "headless", "auth", "login"])
    );
    assert_eq!(
        recovery["command"]["env"]["TEAMS_BROWSER_PROFILE"],
        profile.to_str().unwrap()
    );
    assert_eq!(recovery["command"]["env"]["TEAMS_ACCOUNT"], ACCOUNT);
    assert_eq!(recovery["command"]["env"]["TEAMS_TENANT_ID"], TENANT);
    assert_eq!(
        recovery["verify"],
        json!({"tool":"teams_status","arguments":{}})
    );
    assert_eq!(recovery["retry"]["maxAttempts"], 1);
    assert_eq!(
        failure["error"]["authentication"]["state"],
        "sign_in_required"
    );
    assert!(!profile.exists());
}

#[tokio::test]
async fn browser_is_the_cli_default_without_creating_a_managed_profile() {
    let profile = std::env::temp_dir().join(format!("teams-default-{}", uuid::Uuid::new_v4()));
    let mut command = cli();
    command
        .arg("--status")
        .env("TEAMS_CDP_URL", "ws://127.0.0.1:1/devtools/browser/absent")
        .env("TEAMS_BROWSER_PROFILE", &profile);
    let result = output(command).await;
    assert_eq!(result["error"]["code"], "BROWSER_UNAVAILABLE");
    assert_eq!(result["error"]["recovery"]["authSource"], "browser");
    assert!(!profile.exists());
}

#[tokio::test]
async fn browser_login_uses_chromes_approval_endpoint_without_a_guid() {
    let fixture = Fixture::new(None).await;
    let mut command = cli();
    command
        .args(["auth", "login", "--no-wait"])
        .env("TEAMS_TRANSPORT", "page")
        .env(
            "TEAMS_CDP_URL",
            fixture.endpoint.trim_end_matches("/recovery"),
        )
        .env("TEAMS_ACCOUNT", ACCOUNT)
        .env("TEAMS_TENANT_ID", TENANT);
    let result = output(command).await;
    assert_eq!(result["success"], true, "{result}");
    assert_eq!(result["signedIn"], true);
    assert_eq!(result["authentication"]["sessionVerified"], true);
    assert_eq!(result["authSource"], "existing-browser");
    assert!(fixture.created.lock().unwrap().is_empty());
}

#[tokio::test]
async fn explicit_browser_login_opens_missing_teams_tab_on_the_same_connection() {
    for (fail_install, fail_origin) in [(false, false), (true, false), (false, true)] {
        let fixture = Fixture::new(None).await;
        fixture.targets.lock().unwrap().clear();
        fixture.fail_install.store(fail_install, Ordering::SeqCst);
        fixture.fail_origin.store(fail_origin, Ordering::SeqCst);
        let mut command = cli();
        command
            .args(["auth", "login"])
            .env("TEAMS_TRANSPORT", "page")
            .env("TEAMS_CDP_URL", &fixture.endpoint)
            .env("TEAMS_ACCOUNT", ACCOUNT)
            .env("TEAMS_TENANT_ID", TENANT);
        let result = output(command).await;
        assert_eq!(result["signedIn"], true, "{result}");
        let created = fixture.created.lock().unwrap();
        assert_eq!(created.len(), 1);
        assert_eq!(created[0]["hidden"], false);
        assert_eq!(
            created[0]["url"],
            format!("https://teams.microsoft.com/v2/?tenantId={TENANT}")
        );
        assert_eq!(fixture.connections.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn missing_tab_status_and_no_wait_login_never_open_a_browser_tab() {
    let fixture = Fixture::new(None).await;
    fixture.targets.lock().unwrap().clear();
    let server = TeamsServer::new(fixture.config(AuthSource::Browser));
    for tool in ["teams_status", "teams_login"] {
        let error = server.execute(tool, json!({})).await.unwrap_err();
        assert_eq!(error.code, "TEAMS_TAB_REQUIRED");
    }
    let mut command = cli();
    command
        .args(["auth", "login", "--no-wait"])
        .env("TEAMS_CDP_URL", &fixture.endpoint);
    assert_eq!(output(command).await["error"]["code"], "TEAMS_TAB_REQUIRED");
    assert!(fixture.created.lock().unwrap().is_empty());
}

#[tokio::test]
async fn browser_recovery_command_verifies_and_original_read_can_resume_on_same_server() {
    let fixture = Fixture::new(Some("AUTH_EXPIRED")).await;
    let server = TeamsServer::new(fixture.config(AuthSource::Browser));
    let failed = outcome(server.execute("teams_list_chats", json!({})).await);
    assert_eq!(failed["error"]["code"], "AUTH_EXPIRED");
    let recovery = &failed["error"]["recovery"];
    assert_eq!(recovery["authSource"], "browser");
    assert_eq!(recovery["action"], "sign_in");
    assert_eq!(
        recovery["command"]["env"]["TEAMS_CDP_URL"],
        fixture.endpoint
    );
    assert_eq!(recovery["command"]["env"]["TEAMS_ACCOUNT"], ACCOUNT);
    assert!(
        recovery["command"]["env"]
            .get("TEAMS_BROWSER_PROFILE")
            .is_none()
    );
    fixture.sign_in();
    let mut login = cli();
    login.args(
        recovery["command"]["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|arg| arg.as_str().unwrap()),
    );
    for (key, value) in recovery["command"]["env"].as_object().unwrap() {
        login.env(key, value.as_str().unwrap());
    }
    assert_eq!(output(login).await["signedIn"], true);
    let status = server.execute("teams_status", json!({})).await.unwrap();
    assert_eq!(status["authentication"]["state"], "verified");
    assert_eq!(status["authentication"]["apiAccess"], "not_checked");
    assert!(server.execute("teams_list_chats", json!({})).await.is_ok());
    assert_eq!(
        fixture
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call["action"] == "request")
            .count(),
        2
    );
}

#[tokio::test]
async fn app_recovery_stays_in_the_selected_app_and_access_denials_do_not_recommend_login() {
    let fixture = Fixture::new(Some("AUTH_REQUIRED")).await;
    let server = TeamsServer::new(fixture.config(AuthSource::App));
    let failed = outcome(server.execute("teams_status", json!({})).await);
    let recovery = &failed["error"]["recovery"];
    assert_eq!(recovery["authSource"], "app");
    assert_eq!(
        recovery["command"]["args"],
        json!(["--auth-source", "app", "auth", "login"])
    );
    assert_eq!(
        recovery["command"]["env"]["TEAMS_APP_CDP_URL"],
        fixture.endpoint
    );
    assert!(recovery["command"]["env"].get("TEAMS_CDP_URL").is_none());
    *fixture.response.lock().unwrap() = bridge_response(Some("ACCESS_DENIED"));
    let denied = outcome(server.execute("teams_list_chats", json!({})).await);
    assert_eq!(denied["error"]["code"], "ACCESS_DENIED");
    assert_eq!(denied["error"]["authentication"]["state"], "access_denied");
    assert_eq!(denied["error"]["recovery"]["action"], "request_access");
    assert!(denied["error"]["recovery"].get("command").is_none());
    assert_eq!(denied["error"]["recovery"]["retry"]["maxAttempts"], 0);
}

#[tokio::test]
async fn local_browser_status_does_not_claim_authentication_or_inspect_a_managed_profile() {
    let mut command = cli();
    command
        .args(["auth", "status"])
        .env("TEAMS_BROWSER_PROFILE", "/missing/managed/profile");
    let result = output(command).await;
    assert_eq!(result["authSource"], "browser");
    assert_eq!(result["profileExists"], Value::Null);
    assert_eq!(
        result["authentication"],
        json!({"state":"unverified","sessionVerified":false})
    );
}

#[tokio::test]
async fn partial_unread_results_preserve_authentication_and_permission_recovery() {
    for code in ["AUTH_EXPIRED", "ACCESS_DENIED"] {
        let fixture = Fixture::new(None).await;
        {
            let mut response = fixture.response.lock().unwrap();
            response["profile"]["mri"] = json!("8:orgid:user-one");
            response["data"] = json!({"conversations":[{"id":"19:unread@thread.v2","properties":{"consumptionhorizon":"100;100;100"},"lastMessage":{"id":"200","from":"8:orgid:other"}}]});
            response["_batch"] = json!([{"ok":false,"error":{"code":code,"message":"Synthetic follow-up failure."}}]);
        }
        let server = TeamsServer::new(fixture.config(AuthSource::Browser));
        let result = server
            .execute("teams_get_unread", json!({"includeMessages":true}))
            .await
            .unwrap();
        assert_eq!(result["unreadChats"][0]["messagesError"], code);
        assert_eq!(result["warnings"][0]["code"], code);
        assert_eq!(result["warnings"][0]["recovery"]["authSource"], "browser");
        assert_eq!(
            result["warnings"][0]["recovery"]["retry"]["maxAttempts"],
            if code == "ACCESS_DENIED" { 0 } else { 1 }
        );
    }
}

#[tokio::test]
async fn existing_browser_and_app_login_never_launch_managed_chrome() {
    let fixture = Fixture::new(None).await;
    for (source, endpoint) in [("browser", "TEAMS_CDP_URL"), ("app", "TEAMS_APP_CDP_URL")] {
        let profile =
            std::env::temp_dir().join(format!("teams-no-managed-{}", uuid::Uuid::new_v4()));
        let mut command = cli();
        command
            .args(["--auth-source", source, "auth", "login", "--no-wait"])
            .env(endpoint, &fixture.endpoint)
            .env("TEAMS_TRANSPORT", "page")
            .env("TEAMS_BROWSER_PROFILE", &profile)
            .env("TEAMS_BROWSER_BINARY", "/missing/browser");
        let result = output(command).await;
        assert_eq!(result["signedIn"], true, "source {source}: {result}");
        assert!(!profile.exists());
    }
}

#[cfg(unix)]
#[tokio::test]
async fn cancelling_browser_signin_stops_waiting_and_allows_a_later_login() {
    let fixture = Fixture::new(Some("AUTH_REQUIRED")).await;
    let profile = std::env::temp_dir().join(format!("teams-cancel-{}", uuid::Uuid::new_v4()));
    let mut command = cli();
    command
        .args(["--auth-source", "browser", "auth", "login"])
        .env("TEAMS_CDP_URL", &fixture.endpoint)
        .env("TEAMS_TRANSPORT", "page")
        .env("TEAMS_BROWSER_PROFILE", &profile)
        .env("TEAMS_BROWSER_BINARY", "/missing/browser");
    let child = command.spawn().unwrap();
    timeout(Duration::from_secs(3), async {
        while fixture.calls.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    // Signal only the child process owned by this test.
    assert_eq!(
        unsafe { libc::kill(child.id().unwrap() as i32, libc::SIGINT) },
        0
    );
    let stopped = timeout(Duration::from_secs(3), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    let cancelled: Value = serde_json::from_slice(&stopped.stdout).unwrap();
    assert_eq!(cancelled["error"]["code"], "CANCELLED");
    assert!(cancelled["error"].get("recovery").is_none());
    assert!(!profile.exists());
    fixture.sign_in();
    let mut retry = cli();
    retry
        .args(["--auth-source", "browser", "auth", "login"])
        .env("TEAMS_CDP_URL", &fixture.endpoint)
        .env("TEAMS_TRANSPORT", "page");
    assert_eq!(output(retry).await["signedIn"], true);
}

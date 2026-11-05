use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use teams_mcp::{cdp::Browser, config::Config};
use tokio::net::TcpListener;
use tokio_tungstenite::{accept_async, tungstenite::Message};

pub use teams_mcp::{config, error};
#[path = "../src/identity.rs"]
mod identity;
#[path = "support/readiness.rs"]
mod readiness;

// The readiness fixtures exercise the real transport against a local synthetic CDP
// peer without starting the managed Chrome lifecycle.
mod managed {
    pub fn session_config(
        config: &super::config::Config,
    ) -> Result<super::config::Config, super::error::Failure> {
        let mut config = config.clone();
        config.account = Some("fixture@example.test".into());
        config.tenant = Some("11111111-2222-4333-8444-555555555555".into());
        Ok(config)
    }
    pub async fn endpoint(config: &super::config::Config) -> Result<String, super::error::Failure> {
        super::config::validate_endpoint(config.cdp_url.as_deref().unwrap_or(""))
            .map_err(|message| super::error::Failure::new("BROWSER_UNAVAILABLE", &message))
    }
    pub fn keepalive(_: &super::config::Config) -> Result<(), super::error::Failure> {
        Ok(())
    }
}

type TestSocket = tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>;

async fn receive_call(socket: &mut TestSocket) -> Value {
    let Some(Ok(Message::Text(text))) = socket.next().await else {
        panic!("expected a CDP request");
    };
    serde_json::from_str(&text).unwrap()
}

async fn reply(socket: &mut TestSocket, call: &Value, result: Value) {
    socket
        .send(Message::Text(
            json!({"id":call["id"],"result":result}).to_string().into(),
        ))
        .await
        .unwrap();
}

fn standard_result(method: &str) -> Value {
    match method {
        "Target.createTarget" => json!({"targetId":"teams"}),
        "Target.getTargetInfo" => {
            json!({"targetInfo":{"targetId":"teams","type":"other","url":"https://teams.microsoft.com/v2/"}})
        }
        "Target.closeTarget" => json!({"success":true}),
        "Target.getTargets" => {
            json!({"targetInfos":[{"targetId":"teams","type":"page","url":"https://teams.microsoft.com/v2/"}]})
        }
        "Target.attachToTarget" => json!({"sessionId":"session"}),
        "Page.getFrameTree" => json!({"frameTree":{"frame":{"id":"root"}}}),
        "Page.createIsolatedWorld" => json!({"executionContextId":42}),
        "Runtime.evaluate" => json!({"result":{"type":"function","objectId":"bridge"}}),
        "Runtime.callFunctionOn" => json!({"result":{"value":{"ok":true}}}),
        "Target.detachFromTarget" => json!({}),
        other => panic!("unexpected method: {other}"),
    }
}

const INSTALL: [&str; 6] = [
    "Target.getTargets",
    "Target.attachToTarget",
    "Page.getFrameTree",
    "Page.createIsolatedWorld",
    "Runtime.evaluate",
    "Runtime.callFunctionOn",
];

async fn expect_sequence(socket: &mut TestSocket, methods: &[&str]) {
    for method in methods {
        let call = receive_call(socket).await;
        assert_eq!(call["method"], *method);
        reply(socket, &call, standard_result(method)).await;
    }
}

fn browser(endpoint: String) -> Browser {
    Browser::new(Config {
        auth_source: teams_mcp::config::AuthSource::Browser,
        cdp_url: Some(endpoint),
        ..Default::default()
    })
}

async fn listener(name: &str) -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!(
        "ws://{}/devtools/browser/{name}",
        listener.local_addr().unwrap()
    );
    (listener, endpoint)
}

#[tokio::test]
async fn cdp_attaches_only_to_teams_and_passes_input_as_an_argument() {
    let (listener, endpoint) = listener("test").await;
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(socket).await.unwrap();
        while let Some(Ok(Message::Text(text))) = ws.next().await {
            let call: Value = serde_json::from_str(&text).unwrap();
            let result = match call["method"].as_str().unwrap() {
                "Target.getTargets" => {
                    json!({"targetInfos":[{"targetId":"unrelated","type":"page","url":"https://mail.example.test/"},{"targetId":"teams","type":"page","url":"https://teams.microsoft.com/v2/"}]})
                }
                "Target.attachToTarget" => {
                    assert_eq!(call["params"]["targetId"], "teams");
                    json!({"sessionId":"attached"})
                }
                "Page.getFrameTree" => json!({"frameTree":{"frame":{"id":"root"}}}),
                "Page.createIsolatedWorld" => json!({"executionContextId":42}),
                "Runtime.evaluate" => {
                    assert_eq!(call["params"]["contextId"], 42);
                    assert_eq!(call["sessionId"], "attached");
                    assert_eq!(call["params"]["returnByValue"], false);
                    assert!(
                        !call["params"]["expression"]
                            .as_str()
                            .unwrap()
                            .contains("\"action\""),
                        "input must not be interpolated into the installed source"
                    );
                    json!({"result":{"objectId":"bridge"}})
                }
                "Runtime.callFunctionOn" => {
                    assert_eq!(call["params"]["objectId"], "bridge");
                    assert_eq!(call["sessionId"], "attached");
                    assert_eq!(call["params"]["arguments"][0]["value"]["action"], "status");
                    json!({"result":{"value":{"ok":true,"profile":{"email":"test@example.test"},"capabilities":{"search":true}}}})
                }
                other => panic!("unexpected CDP method: {other}"),
            };
            reply(&mut ws, &call, result).await;
        }
    });
    let browser = browser(endpoint);
    let result = browser.evaluate(json!({"action":"status"})).await.unwrap();
    assert_eq!(result["profile"]["email"], "test@example.test");
    drop(browser);
    server.await.unwrap();
}

#[tokio::test]
async fn browser_clones_reuse_one_socket_target_session_and_installed_bridge() {
    let (listener, endpoint) = listener("reuse").await;
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(socket).await.unwrap();
        expect_sequence(&mut ws, &INSTALL).await;
        expect_sequence(&mut ws, &["Target.getTargets", "Runtime.callFunctionOn"]).await;
        expect_sequence(&mut ws, &["Target.getTargets", "Runtime.callFunctionOn"]).await;
    });
    let browser = browser(endpoint);
    let clone = browser.clone();
    for client in [&browser, &clone, &browser] {
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            client.evaluate(json!({"action":"status"})),
        )
        .await
        .expect("later calls must reuse the attached page without reinstalling");
        assert_eq!(result.unwrap()["ok"], true);
    }
    server.await.unwrap();
}

#[tokio::test]
async fn concurrent_calls_share_the_socket_and_resolve_out_of_order() {
    let (listener, endpoint) = listener("parallel").await;
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(socket).await.unwrap();
        expect_sequence(&mut ws, &INSTALL).await;
        let mut pending = Vec::new();
        while pending.len() < 2 {
            let call = receive_call(&mut ws).await;
            match call["method"].as_str().unwrap() {
                "Target.getTargets" => {
                    reply(&mut ws, &call, standard_result("Target.getTargets")).await
                }
                "Runtime.callFunctionOn" => pending.push(call),
                other => panic!("unexpected method: {other}"),
            }
        }
        // Answer the second call first; each caller must receive its own result.
        for call in pending.iter().rev() {
            let marker = call["params"]["arguments"][0]["value"]["marker"].clone();
            reply(
                &mut ws,
                call,
                json!({"result":{"value":{"ok":true,"marker":marker}}}),
            )
            .await;
        }
    });
    let browser = browser(endpoint);
    browser.evaluate(json!({"action":"status"})).await.unwrap();
    let (a, b) = tokio::join!(
        browser.evaluate(json!({"action":"status","marker":"a"})),
        browser.evaluate(json!({"action":"status","marker":"b"})),
    );
    assert_eq!(a.unwrap()["marker"], "a");
    assert_eq!(b.unwrap()["marker"], "b");
    server.await.unwrap();
}

#[tokio::test]
async fn cancelled_evaluation_ignores_its_late_response_and_keeps_the_socket() {
    let (listener, endpoint) = listener("cancel").await;
    let (started, pending) = tokio::sync::oneshot::channel();
    let (cancelled, aborted) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut first = accept_async(socket).await.unwrap();
        expect_sequence(&mut first, &INSTALL[..5]).await;
        let stale = receive_call(&mut first).await;
        assert_eq!(stale["method"], "Runtime.callFunctionOn");
        started.send(()).unwrap();
        aborted.await.unwrap();
        reply(
            &mut first,
            &stale,
            json!({"result":{"value":{"ok":true,"stale":true}}}),
        )
        .await;
        expect_sequence(&mut first, &["Target.getTargets", "Runtime.callFunctionOn"]).await;
    });
    let browser = browser(endpoint);
    let first = browser.clone();
    let request = tokio::spawn(async move { first.evaluate(json!({"action":"status"})).await });
    tokio::time::timeout(std::time::Duration::from_secs(2), pending)
        .await
        .unwrap()
        .unwrap();
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    cancelled.send(()).unwrap();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        browser.evaluate(json!({"action":"status"})),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result, json!({"ok":true}));
    server.await.unwrap();
}

#[tokio::test]
async fn changed_port_file_invalidates_the_cached_browser_endpoint() {
    let first = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let second = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let first_port = first.local_addr().unwrap().port();
    let second_port = second.local_addr().unwrap().port();
    let path = std::env::temp_dir().join(format!("teams-mcp-cdp-{}.port", uuid::Uuid::new_v4()));
    tokio::fs::write(&path, format!("{first_port}\n/devtools/browser/first\n"))
        .await
        .unwrap();
    let first_server = tokio::spawn(async move {
        let (socket, _) = first.accept().await.unwrap();
        let mut socket = accept_async(socket).await.unwrap();
        expect_sequence(&mut socket, &INSTALL).await;
        assert!(
            !matches!(socket.next().await, Some(Ok(Message::Text(_)))),
            "old endpoint must be dropped before another operation"
        );
    });
    let second_server = tokio::spawn(async move {
        let (socket, _) = second.accept().await.unwrap();
        expect_sequence(&mut accept_async(socket).await.unwrap(), &INSTALL).await;
    });
    let browser = Browser::new(Config {
        auth_source: teams_mcp::config::AuthSource::Browser,
        port_file: Some(path.clone()),
        ..Default::default()
    });
    browser.evaluate(json!({"action":"status"})).await.unwrap();
    tokio::fs::write(&path, format!("{second_port}\n/devtools/browser/second\n"))
        .await
        .unwrap();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        browser.evaluate(json!({"action":"status"})),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result["ok"], true);
    tokio::fs::remove_file(path).await.unwrap();
    first_server.await.unwrap();
    second_server.await.unwrap();
}

#[tokio::test]
async fn page_exception_detaches_and_the_next_call_reinstalls_on_the_same_socket() {
    let (listener, endpoint) = listener("error").await;
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(socket).await.unwrap();
        expect_sequence(&mut ws, &INSTALL[..5]).await;
        let call = receive_call(&mut ws).await;
        assert_eq!(call["method"], "Runtime.callFunctionOn");
        reply(&mut ws, &call, json!({"exceptionDetails":{}})).await;
        expect_sequence(&mut ws, &["Target.detachFromTarget"]).await;
        expect_sequence(&mut ws, &INSTALL).await;
    });
    let browser = browser(endpoint);
    assert_eq!(
        browser
            .evaluate(json!({"action":"status"}))
            .await
            .unwrap_err()
            .code,
        "PAGE_CHANGED"
    );
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        browser.evaluate(json!({"action":"status"})),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result["ok"], true);
    server.await.unwrap();
}

#[tokio::test]
async fn destroyed_context_is_reinstalled_once_within_the_same_call() {
    let (listener, endpoint) = listener("navigated").await;
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(socket).await.unwrap();
        expect_sequence(&mut ws, &INSTALL).await;
        expect_sequence(&mut ws, &["Target.getTargets"]).await;
        let call = receive_call(&mut ws).await;
        assert_eq!(call["method"], "Runtime.callFunctionOn");
        ws.send(Message::Text(
            json!({"id":call["id"],"error":{"code":-32000,"message":"Cannot find context with specified id"}})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
        expect_sequence(
            &mut ws,
            &[
                "Page.getFrameTree",
                "Page.createIsolatedWorld",
                "Runtime.evaluate",
                "Runtime.callFunctionOn",
            ],
        )
        .await;
    });
    let browser = browser(endpoint);
    browser.evaluate(json!({"action":"status"})).await.unwrap();
    let result = browser.evaluate(json!({"action":"status"})).await.unwrap();
    assert_eq!(result["ok"], true);
    server.await.unwrap();
}

#[tokio::test]
async fn app_cdp_uses_its_explicit_endpoint_and_native_host_world() {
    let (listener, endpoint) = listener("native").await;
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(socket).await.unwrap();
        let mut evaluated = false;
        while let Some(Ok(Message::Text(text))) = ws.next().await {
            let call: Value = serde_json::from_str(&text).unwrap();
            let result = match call["method"].as_str().unwrap() {
                "Target.getTargets" => {
                    json!({"targetInfos":[{"targetId":"native","type":"page","url":"https://teams.microsoft.com/v2/"}]})
                }
                "Target.attachToTarget" => json!({"sessionId":"app-session"}),
                "Runtime.evaluate" => {
                    assert!(call["params"].get("contextId").is_none());
                    json!({"result":{"objectId":"native-bridge"}})
                }
                "Runtime.callFunctionOn" => {
                    assert_eq!(call["params"]["arguments"][0]["value"]["nativeApp"], true);
                    evaluated = true;
                    json!({"result":{"value":{"ok":true,"profile":{"id":"native-user"}}}})
                }
                other => panic!("native app host bridge must stay in its main world, got {other}"),
            };
            reply(&mut ws, &call, result).await;
        }
        assert!(evaluated);
    });
    let browser = Browser::new(Config {
        auth_source: teams_mcp::config::AuthSource::App,
        app_cdp_url: Some(endpoint),
        cdp_url: Some("ws://127.0.0.1:1/devtools/browser/wrong-source".into()),
        ..Config::default()
    });
    let result = browser.evaluate(json!({"action":"status"})).await.unwrap();
    assert_eq!(result["profile"]["id"], "native-user");
    drop(browser);
    server.await.unwrap();
}

async fn hidden_fixture(url: &'static str) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    let (listener, endpoint) = listener("hidden").await;
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(socket).await.unwrap();
        let mut methods = Vec::new();
        while let Some(Ok(Message::Text(text))) = ws.next().await {
            let call: Value = serde_json::from_str(&text).unwrap();
            let method = call["method"].as_str().unwrap().to_owned();
            let result = match method.as_str() {
                "Target.createTarget" => {
                    assert_eq!(call["params"]["hidden"], true);
                    assert_eq!(
                        call["params"]["url"],
                        "https://teams.microsoft.com/v2/?tenantId=11111111-2222-4333-8444-555555555555"
                    );
                    json!({"targetId":"hidden"})
                }
                "Target.getTargetInfo" => {
                    json!({"targetInfo":{"targetId":"hidden","type":"other","url":url}})
                }
                "Target.attachToTarget" => {
                    assert_eq!(call["params"]["targetId"], "hidden");
                    json!({"sessionId":"hidden-session"})
                }
                "Target.closeTarget" => {
                    assert_eq!(call["params"]["targetId"], "hidden");
                    json!({"success":true})
                }
                "Target.getTargets" => panic!("hidden-tab mode must not inspect the user's tabs"),
                other => standard_result(other),
            };
            methods.push(method);
            reply(&mut ws, &call, result).await;
        }
        methods
    });
    (endpoint, server)
}

fn hidden_browser(endpoint: String) -> Browser {
    Browser::new(Config {
        auth_source: teams_mcp::config::AuthSource::Browser,
        cdp_url: Some(endpoint),
        hidden_tab: true,
        tenant: Some("11111111-2222-4333-8444-555555555555".into()),
        ..Default::default()
    })
}

#[tokio::test]
async fn browser_binding_survives_expiry_and_rejects_changed_account_or_tenant() {
    for changed in [
        json!({"id":"other-user","email":"other@example.test","tenantId":"original-tenant"}),
        json!({"id":"original-user","email":"original@example.test","tenantId":"other-tenant"}),
    ] {
        let original = json!({"id":"original-user","email":"original@example.test","tenantId":"original-tenant"});
        let outputs = vec![
            json!({"ok":true,"profile":original}),
            json!({"ok":false,"error":{"code":"AUTH_EXPIRED","message":"fixture"}}),
            json!({"ok":true,"profile":changed}),
            json!({"ok":true,"profile":original}),
        ];
        let (endpoint, server) = binding_fixture(outputs).await;
        let browser = browser(endpoint);
        let clone = browser.clone();
        browser.evaluate(json!({"action":"status"})).await.unwrap();
        assert_eq!(
            clone.selection(),
            (
                Some("original@example.test".into()),
                Some("original-tenant".into())
            )
        );
        let expired = clone.evaluate(json!({"action":"status"})).await.unwrap();
        assert_eq!(expired["error"]["code"], "AUTH_EXPIRED");
        let error = clone
            .evaluate(json!({"action":"status"}))
            .await
            .unwrap_err();
        assert_eq!(error.code, "ACCOUNT_MISMATCH");
        let recovered = browser.evaluate(json!({"action":"status"})).await.unwrap();
        assert_eq!(recovered["profile"], original);
        drop((browser, clone));
        server.await.unwrap();
    }
}

#[tokio::test]
async fn first_failed_browser_read_binds_the_known_account_and_tenant() {
    let original =
        json!({"id":"original-user","email":"original@example.test","tenantId":"original-tenant"});
    let changed =
        json!({"id":"other-user","email":"other@example.test","tenantId":"original-tenant"});
    let (endpoint, peer) = binding_fixture(vec![
        json!({"ok":false,"profile":original,"error":{"code":"AUTH_EXPIRED","message":"fixture"}}),
        json!({"ok":true,"profile":changed}),
        json!({"ok":true,"profile":original}),
    ])
    .await;
    let browser = browser(endpoint);
    let failed = browser.evaluate(json!({"action":"request"})).await.unwrap();
    assert_eq!(failed["error"]["code"], "AUTH_EXPIRED");
    assert_eq!(
        browser.selection(),
        (
            Some("original@example.test".into()),
            Some("original-tenant".into())
        )
    );
    assert_eq!(
        browser
            .evaluate(json!({"action":"status"}))
            .await
            .unwrap_err()
            .code,
        "ACCOUNT_MISMATCH"
    );
    assert_eq!(
        browser.evaluate(json!({"action":"status"})).await.unwrap()["profile"],
        original
    );
    drop(browser);
    peer.await.unwrap();
}

#[tokio::test]
async fn first_failed_hidden_attempt_binds_identity_before_its_internal_retry() {
    let original =
        json!({"id":"original-user","email":"original@example.test","tenantId":"original-tenant"});
    let changed =
        json!({"id":"other-user","email":"other@example.test","tenantId":"original-tenant"});
    let (endpoint, peer) = binding_fixture(vec![
        json!({"ok":false,"profile":original,"error":{"code":"AUTH_REQUIRED","message":"fixture"}}),
        json!({"ok":true,"profile":changed}),
    ])
    .await;
    let browser = Browser::new(Config {
        auth_source: config::AuthSource::Browser,
        cdp_url: Some(endpoint),
        hidden_tab: true,
        ..Config::default()
    });
    assert_eq!(
        browser
            .evaluate(json!({"action":"request"}))
            .await
            .unwrap_err()
            .code,
        "ACCOUNT_MISMATCH"
    );
    assert_eq!(
        browser.selection(),
        (
            Some("original@example.test".into()),
            Some("original-tenant".into())
        )
    );
    browser.release_hidden().await;
    drop(browser);
    peer.await.unwrap();
}

#[tokio::test]
async fn concurrent_browser_calls_cannot_replace_the_verified_identity() {
    let original =
        json!({"id":"original-user","email":"original@example.test","tenantId":"original-tenant"});
    let changed =
        json!({"id":"other-user","email":"other@example.test","tenantId":"original-tenant"});
    let (endpoint, server) = binding_fixture(vec![
        json!({"ok":true,"profile":original}),
        json!({"ok":true,"profile":changed}),
        json!({"ok":true,"profile":original}),
    ])
    .await;
    let browser = browser(endpoint);
    browser.evaluate(json!({"action":"status"})).await.unwrap();
    let clone = browser.clone();
    let (a, b) = tokio::join!(
        browser.evaluate(json!({"action":"status"})),
        clone.evaluate(json!({"action":"status"})),
    );
    let (success, failure) = if a.is_ok() { (a, b) } else { (b, a) };
    assert_eq!(success.unwrap()["profile"], original);
    assert_eq!(failure.unwrap_err().code, "ACCOUNT_MISMATCH");
    assert_eq!(
        clone.selection(),
        (
            Some("original@example.test".into()),
            Some("original-tenant".into())
        )
    );
    drop((browser, clone));
    server.await.unwrap();
}

#[tokio::test]
async fn browser_binding_survives_a_reconnected_browser_endpoint() {
    let original =
        json!({"id":"original-user","email":"original@example.test","tenantId":"original-tenant"});
    let changed =
        json!({"id":"other-user","email":"other@example.test","tenantId":"original-tenant"});
    let (first, first_server) = binding_fixture(vec![json!({"ok":true,"profile":original})]).await;
    let (second, second_server) = binding_fixture(vec![json!({"ok":true,"profile":changed})]).await;
    let path = std::env::temp_dir().join(format!("teams-binding-{}.port", uuid::Uuid::new_v4()));
    select_fixture_endpoint(&path, &first).await;
    let browser = Browser::new(Config {
        auth_source: config::AuthSource::Browser,
        port_file: Some(path.clone()),
        ..Default::default()
    });
    browser.evaluate(json!({"action":"status"})).await.unwrap();
    select_fixture_endpoint(&path, &second).await;
    assert_eq!(
        browser
            .evaluate(json!({"action":"status"}))
            .await
            .unwrap_err()
            .code,
        "ACCOUNT_MISMATCH"
    );
    assert_eq!(
        browser.selection(),
        (
            Some("original@example.test".into()),
            Some("original-tenant".into())
        )
    );
    drop(browser);
    tokio::fs::remove_file(path).await.unwrap();
    first_server.await.unwrap();
    second_server.await.unwrap();
}

async fn select_fixture_endpoint(path: &std::path::Path, endpoint: &str) {
    let endpoint = url::Url::parse(endpoint).unwrap();
    tokio::fs::write(
        path,
        format!("{}\n{}\n", endpoint.port().unwrap(), endpoint.path()),
    )
    .await
    .unwrap();
}

async fn binding_fixture(outputs: Vec<Value>) -> (String, tokio::task::JoinHandle<()>) {
    let (listener, endpoint) = listener("binding").await;
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(socket).await.unwrap();
        let original = outputs[0]["profile"].clone();
        let mut calls = 0;
        let mut outputs: std::collections::VecDeque<_> = outputs.into();
        while let Some(Ok(Message::Text(text))) = ws.next().await {
            let call: Value = serde_json::from_str(&text).unwrap();
            let result = if call["method"] == "Runtime.callFunctionOn" {
                if calls > 0 {
                    let input = &call["params"]["arguments"][0]["value"];
                    assert_eq!(input["expectedAccount"], original["email"]);
                    assert_eq!(input["tenantId"], original["tenantId"]);
                }
                calls += 1;
                json!({"result":{"value":outputs.pop_front().expect("unexpected bridge call")}})
            } else {
                standard_result(call["method"].as_str().unwrap())
            };
            reply(&mut ws, &call, result).await;
        }
        assert!(outputs.is_empty());
    });
    (endpoint, server)
}

#[tokio::test]
async fn hidden_tab_mode_uses_only_its_own_hidden_target_and_closes_it() {
    let (endpoint, server) = hidden_fixture("https://teams.microsoft.com/v2/").await;
    let browser = hidden_browser(endpoint);
    let result = browser.evaluate(json!({"action":"lease"})).await.unwrap();
    assert_eq!(result["ok"], true);
    browser.release_hidden().await;
    drop(browser);
    let methods = server.await.unwrap();
    assert_eq!(
        methods.first().map(String::as_str),
        Some("Target.createTarget")
    );
    assert_eq!(
        methods.last().map(String::as_str),
        Some("Target.closeTarget")
    );
}

#[tokio::test]
async fn hidden_tab_redirected_to_signin_is_never_attached() {
    let (endpoint, server) =
        hidden_fixture("https://login.microsoftonline.com/common/oauth2/authorize").await;
    let browser = hidden_browser(endpoint);
    let error = browser
        .evaluate(json!({"action":"lease"}))
        .await
        .unwrap_err();
    assert_eq!(error.code, "BROWSER_SIGNIN_REQUIRED");
    browser.release_hidden().await;
    drop(browser);
    let methods = server.await.unwrap();
    assert!(
        !methods
            .iter()
            .any(|method| method == "Target.attachToTarget")
    );
    assert_eq!(
        methods.last().map(String::as_str),
        Some("Target.closeTarget")
    );
}

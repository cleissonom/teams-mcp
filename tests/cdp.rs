use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use teams_mcp::{cdp::Browser, config::Config};
use tokio::net::TcpListener;
use tokio_tungstenite::{accept_async, tungstenite::Message};

pub use teams_mcp::{config, error};
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
}

#[tokio::test]
async fn cdp_attaches_only_to_teams_and_returns_no_session_secrets() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!(
        "ws://{}/devtools/browser/test",
        listener.local_addr().unwrap()
    );
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
                    json!({"result":{"value":{"ok":true,"profile":{"email":"test@example.test"},"capabilities":{"search":true}}}})
                }
                "Target.detachFromTarget" => {
                    assert_eq!(call["params"]["sessionId"], "attached");
                    json!({})
                }
                other => panic!("unexpected CDP method: {other}"),
            };
            ws.send(Message::Text(
                json!({"id":call["id"],"result":result}).to_string().into(),
            ))
            .await
            .unwrap();
        }
    });
    let browser = Browser::new(Config {
        auth_source: teams_mcp::config::AuthSource::Browser,
        cdp_url: Some(endpoint),
        ..Default::default()
    });
    let result = browser.evaluate(json!({"action":"status"})).await.unwrap();
    assert_eq!(result["profile"]["email"], "test@example.test");
    drop(browser);
    server.await.unwrap();
}

#[tokio::test]
async fn browser_clones_reuse_one_approved_socket_and_detach_each_target_session() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!(
        "ws://{}/devtools/browser/reuse",
        listener.local_addr().unwrap()
    );
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(socket).await.unwrap();
        let mut evaluations = 0;
        let mut detachments = 0;
        while let Some(Ok(Message::Text(text))) = ws.next().await {
            let call: Value = serde_json::from_str(&text).unwrap();
            let result = match call["method"].as_str().unwrap() {
                "Target.getTargets" => {
                    json!({"targetInfos":[{"targetId":"teams","type":"page","url":"https://teams.microsoft.com/v2/"}]})
                }
                "Target.attachToTarget" => {
                    assert_eq!(
                        evaluations, detachments,
                        "previous target session must be detached before another evaluation"
                    );
                    json!({"sessionId":format!("session-{evaluations}")})
                }
                "Page.getFrameTree" => json!({"frameTree":{"frame":{"id":"root"}}}),
                "Page.createIsolatedWorld" => json!({"executionContextId":42}),
                "Runtime.evaluate" => {
                    assert_eq!(call["sessionId"], format!("session-{evaluations}"));
                    evaluations += 1;
                    json!({"result":{"value":{"ok":true,"sequence":evaluations}}})
                }
                "Target.detachFromTarget" => {
                    assert_eq!(
                        call["params"]["sessionId"],
                        format!("session-{detachments}")
                    );
                    detachments += 1;
                    json!({})
                }
                other => panic!("unexpected CDP method: {other}"),
            };
            ws.send(Message::Text(
                json!({"id":call["id"],"result":result}).to_string().into(),
            ))
            .await
            .unwrap();
            if detachments == 2 {
                return (evaluations, detachments);
            }
        }
        (evaluations, detachments)
    });
    let browser = Browser::new(Config {
        auth_source: teams_mcp::config::AuthSource::Browser,
        cdp_url: Some(endpoint),
        ..Default::default()
    });
    let clone = browser.clone();
    assert_eq!(
        browser.evaluate(json!({"action":"status"})).await.unwrap()["sequence"],
        1
    );
    let second = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        clone.evaluate(json!({"action":"status"})),
    )
    .await;
    assert!(
        second.is_ok(),
        "second evaluation must reuse the existing approved connection"
    );
    assert_eq!(second.unwrap().unwrap()["sequence"], 2);
    assert_eq!(server.await.unwrap(), (2, 2));
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
        "Target.getTargets" => {
            json!({"targetInfos":[{"targetId":"teams","type":"page","url":"https://teams.microsoft.com/v2/"}]})
        }
        "Target.attachToTarget" => json!({"sessionId":"session"}),
        "Page.getFrameTree" => json!({"frameTree":{"frame":{"id":"root"}}}),
        "Page.createIsolatedWorld" => json!({"executionContextId":42}),
        "Runtime.evaluate" => json!({"result":{"value":{"ok":true}}}),
        "Target.detachFromTarget" => json!({}),
        other => panic!("unexpected method: {other}"),
    }
}

async fn complete_evaluation(socket: &mut TestSocket) {
    for method in [
        "Target.getTargets",
        "Target.attachToTarget",
        "Page.getFrameTree",
        "Page.createIsolatedWorld",
        "Runtime.evaluate",
        "Target.detachFromTarget",
    ] {
        let call = receive_call(socket).await;
        assert_eq!(call["method"], method);
        reply(socket, &call, standard_result(method)).await;
    }
}

#[tokio::test]
async fn cancelled_evaluation_drops_pending_socket_before_the_next_call() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!(
        "ws://{}/devtools/browser/cancel",
        listener.local_addr().unwrap()
    );
    let (started, pending) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut first = accept_async(socket).await.unwrap();
        for method in [
            "Target.getTargets",
            "Target.attachToTarget",
            "Page.getFrameTree",
            "Page.createIsolatedWorld",
        ] {
            let call = receive_call(&mut first).await;
            assert_eq!(call["method"], method);
            reply(&mut first, &call, standard_result(method)).await;
        }
        assert_eq!(receive_call(&mut first).await["method"], "Runtime.evaluate");
        started.send(()).unwrap();
        assert!(
            !matches!(first.next().await, Some(Ok(Message::Text(_)))),
            "cancelled socket must not receive another request"
        );
        let (socket, _) = listener.accept().await.unwrap();
        let mut second = accept_async(socket).await.unwrap();
        complete_evaluation(&mut second).await;
    });
    let browser = Browser::new(Config {
        auth_source: teams_mcp::config::AuthSource::Browser,
        cdp_url: Some(endpoint),
        ..Default::default()
    });
    let first = browser.clone();
    let request = tokio::spawn(async move { first.evaluate(json!({"action":"status"})).await });
    tokio::time::timeout(std::time::Duration::from_secs(2), pending)
        .await
        .unwrap()
        .unwrap();
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
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
        complete_evaluation(&mut socket).await;
        assert!(
            !matches!(socket.next().await, Some(Ok(Message::Text(_)))),
            "old endpoint must be dropped before another operation"
        );
    });
    let second_server = tokio::spawn(async move {
        let (socket, _) = second.accept().await.unwrap();
        complete_evaluation(&mut accept_async(socket).await.unwrap()).await;
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
async fn failed_evaluation_detaches_the_target_then_invalidates_the_socket() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!(
        "ws://{}/devtools/browser/error",
        listener.local_addr().unwrap()
    );
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut first = accept_async(socket).await.unwrap();
        for method in [
            "Target.getTargets",
            "Target.attachToTarget",
            "Page.getFrameTree",
            "Page.createIsolatedWorld",
            "Runtime.evaluate",
            "Target.detachFromTarget",
        ] {
            let call = receive_call(&mut first).await;
            assert_eq!(call["method"], method);
            let result = if method == "Runtime.evaluate" {
                json!({"exceptionDetails":{}})
            } else {
                standard_result(method)
            };
            reply(&mut first, &call, result).await;
        }
        assert!(!matches!(first.next().await, Some(Ok(Message::Text(_)))));
        let (socket, _) = listener.accept().await.unwrap();
        complete_evaluation(&mut accept_async(socket).await.unwrap()).await;
    });
    let browser = Browser::new(Config {
        auth_source: teams_mcp::config::AuthSource::Browser,
        cdp_url: Some(endpoint),
        ..Default::default()
    });
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
async fn app_cdp_uses_its_explicit_endpoint_and_native_host_world() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!(
        "ws://{}/devtools/browser/native",
        listener.local_addr().unwrap()
    );
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
                    assert!(
                        call["params"]["expression"]
                            .as_str()
                            .unwrap()
                            .contains("\"nativeApp\":true")
                    );
                    evaluated = true;
                    json!({"result":{"value":{"ok":true,"profile":{"id":"native-user"}}}})
                }
                "Target.detachFromTarget" => json!({}),
                other => panic!("native app host bridge must stay in its main world, got {other}"),
            };
            ws.send(Message::Text(
                json!({"id":call["id"],"result":result}).to_string().into(),
            ))
            .await
            .unwrap();
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

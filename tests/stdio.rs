use serde_json::{Value, json};
use std::{process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
    time::timeout,
};

#[tokio::test]
async fn claude_stdio_handshake_works_without_browser_or_credentials() {
    disconnected_handshake("browser", "2024-11-05").await;
}

#[tokio::test]
async fn app_stdio_handshake_and_discovery_do_not_require_app_authentication() {
    disconnected_handshake("app", "2025-11-25").await;
}

#[tokio::test]
async fn headless_handshake_and_discovery_do_not_launch_chrome_or_require_setup() {
    disconnected_handshake("headless", "2025-11-25").await;
}

#[tokio::test]
async fn lease_transport_stays_lazy_and_reports_setup_without_a_browser() {
    disconnected_handshake_with("headless", "2025-11-25", "lease").await;
}

async fn disconnected_handshake(source: &str, protocol: &str) {
    disconnected_handshake_with(source, protocol, "page").await;
}

async fn disconnected_handshake_with(source: &str, protocol: &str, transport: &str) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_teams-mcp"))
        .env("TEAMS_AUTH_SOURCE", source)
        .env("TEAMS_TRANSPORT", transport)
        .env(
            "TEAMS_BROWSER_PROFILE",
            std::env::temp_dir().join(format!("teams-absent-{}", uuid::Uuid::new_v4())),
        )
        .env("TEAMS_BROWSER_BINARY", "/missing/browser")
        .env("TEAMS_CDP_URL", "ws://127.0.0.1:1/devtools/browser/absent")
        .env(
            "TEAMS_APP_CDP_URL",
            "ws://127.0.0.1:1/devtools/browser/absent",
        )
        .env_remove("TEAMS_APP_WINDOW_ID")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap()).lines();
    let initialize = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
        "protocolVersion":protocol,"capabilities":{},"clientInfo":{"name":"compatibility-test","version":"1"}}});
    input
        .write_all(format!("{initialize}\n").as_bytes())
        .await
        .unwrap();
    let line = timeout(Duration::from_secs(3), output.next_line())
        .await
        .unwrap()
        .unwrap();
    assert!(
        line.is_some(),
        "server must respond to initialize even without an authenticated browser"
    );
    let response: Value = serde_json::from_str(&line.unwrap()).unwrap();
    assert_eq!(response["id"], 1);
    assert_eq!(response["result"]["protocolVersion"], protocol);
    input.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n").await.unwrap();
    let line = timeout(Duration::from_secs(3), output.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let response: Value = serde_json::from_str(&line).unwrap();
    let tools = response["result"]["tools"].as_array().unwrap();
    assert!(tools.iter().any(|tool| tool["name"] == "teams_search"));
    assert!(
        tools
            .iter()
            .all(|tool| tool["annotations"]["readOnlyHint"] == true)
    );
    input.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"teams_get_me\",\"arguments\":{}}}\n").await.unwrap();
    let line = timeout(Duration::from_secs(3), output.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let response: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(response["id"], 3);
    assert_eq!(response["result"]["isError"], true);
    assert!(
        response["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains(if source == "headless" {
                "AUTH_SETUP_REQUIRED"
            } else {
                "BROWSER_UNAVAILABLE"
            })
    );
    drop(input);
    timeout(Duration::from_secs(3), child.wait())
        .await
        .unwrap()
        .unwrap();
}

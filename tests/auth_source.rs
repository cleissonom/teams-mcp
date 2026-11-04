use teams_mcp::config::{AuthSource, Config};

#[tokio::test]
async fn app_mode_never_inherits_the_browser_endpoint() {
    let mut config = Config {
        auth_source: AuthSource::App,
        cdp_url: Some("ws://127.0.0.1:1/devtools/browser/browser-only".into()),
        ..Config::default()
    };
    assert!(config.endpoint().await.is_err());
    config.app_cdp_url = Some("ws://127.0.0.1:9223/devtools/browser/native".into());
    assert_eq!(
        config.endpoint().await.unwrap(),
        "ws://127.0.0.1:9223/devtools/browser/native"
    );
}

#[test]
fn cli_rejects_invalid_app_window_ids_before_automation() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_teams-mcp"))
        .args(["--auth-source", "app", "--status"])
        .env("TEAMS_APP_WINDOW_ID", "0")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("TEAMS_APP_WINDOW_ID")
    );
}

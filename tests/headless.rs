use serde_json::Value;
use std::process::Command;

#[test]
fn missing_headless_profile_requires_setup_without_launching_a_browser() {
    let profile = std::env::temp_dir().join(format!("teams-absent-{}", uuid::Uuid::new_v4()));
    let output = Command::new(env!("CARGO_BIN_EXE_teams-mcp"))
        .arg("--status")
        .env("TEAMS_AUTH_SOURCE", "headless")
        .env("TEAMS_BROWSER_PROFILE", &profile)
        .env("TEAMS_BROWSER_BINARY", "/missing/browser")
        .env_remove("TEAMS_APP_WINDOW_ID")
        .output()
        .unwrap();
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["error"]["code"], "AUTH_SETUP_REQUIRED");
    assert_eq!(output.status.code(), Some(1));
    assert!(!profile.exists());
}

use super::{Fixture, output};
use serde_json::{Value, json};
use std::{fs, time::Duration};
use teams_mcp::managed;
use tokio::process::Command;

const TENANT: &str = "22222222-2222-4222-8222-222222222222";

fn signed_in(fixture: &Fixture, email: &str) {
    fs::write(
        fixture.profile.join("fixture-bridge-response"),
        json!({"ok":true,"profile":{"email":email,"tenantId":TENANT},"capabilities":{"chat":true}})
            .to_string(),
    )
    .unwrap();
}

#[tokio::test]
async fn login_transitions_owned_headless_browser_and_the_original_read_can_retry() {
    let fixture = Fixture::new(true);
    signed_in(&fixture, "selected@example.test");
    let mut first = fixture.command();
    first.arg("--status");
    assert_eq!(output(first).await["authenticated"], true);
    let mut login = fixture.command();
    login.args(["auth", "login"]);
    let result = output(login).await;
    assert_eq!(result["signedIn"], true, "{result}");
    assert_eq!(result["stopped"], true);
    assert_eq!(result["account"], "selected@example.test");
    let mut retry = fixture.command();
    retry.arg("--status");
    let result = output(retry).await;
    assert_eq!(result["authenticated"], true, "{result}");
    assert_eq!(result["profile"]["email"], "selected@example.test");
    let args = fs::read_to_string(fixture.root.join("args")).unwrap();
    assert!(args.contains("--headless=new"));
    managed::stop(&fixture.config()).await.unwrap();
}

#[tokio::test]
async fn offline_status_does_not_treat_a_missing_profile_as_a_verified_session() {
    let fixture = Fixture::new(false);
    let result = managed::status(&fixture.config()).unwrap();
    assert_eq!(result["profileExists"], false);
    assert_eq!(result["authentication"]["state"], "sign_in_required");
    assert_eq!(result["authentication"]["sessionVerified"], false);
    assert!(!fixture.profile.exists());
}

async fn wait_for(fixture: &Fixture, field: &str, value: &str) {
    tokio::time::timeout(Duration::from_secs(25), async {
        loop {
            let status = managed::status(&fixture.config()).unwrap();
            if status.pointer(field).and_then(Value::as_str) == Some(value) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap_or_else(|error| panic!("{error}: {}", managed::status(&fixture.config()).unwrap()));
}

fn login_process(fixture: &Fixture) -> tokio::process::Child {
    fixture
        .command()
        .args(["auth", "login"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap()
}

async fn login_result(child: tokio::process::Child) -> Value {
    let result = tokio::time::timeout(Duration::from_secs(15), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    serde_json::from_slice(&result.stdout).unwrap()
}

#[tokio::test]
async fn login_drains_concurrent_reads_and_holds_new_reads_until_setup_finishes() {
    let fixture = Fixture::new(true);
    signed_in(&fixture, "selected@example.test");
    let mut first = fixture.command();
    first.arg("--status");
    assert_eq!(output(first).await["authenticated"], true);
    let active_a = managed::request_guard(&fixture.config()).await.unwrap();
    let active_b = managed::request_guard(&fixture.config()).await.unwrap();
    let login = login_process(&fixture);
    wait_for(&fixture, "/authentication/state", "signing_in").await;
    assert_eq!(
        managed::status(&fixture.config()).unwrap()["browser"]["mode"],
        "headless"
    );
    let config = fixture.config();
    let mut waiting = tokio::spawn(async move { managed::request_guard(&config).await });
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut waiting)
            .await
            .is_err()
    );
    drop(active_a);
    assert_eq!(
        managed::status(&fixture.config()).unwrap()["browser"]["mode"],
        "headless"
    );
    drop(active_b);
    assert_eq!(login_result(login).await["signedIn"], true);
    assert!(waiting.await.unwrap().unwrap().is_some());
    assert_eq!(
        managed::status(&fixture.config()).unwrap()["browser"]["state"],
        "stopped"
    );
}

#[tokio::test]
async fn cancelling_signin_closes_setup_and_preserves_the_saved_identity() {
    for signal in ["-INT", "-TERM"] {
        let fixture = Fixture::new(true);
        let selection = fixture.profile.join(".teams-mcp-selection.json");
        let saved = json!({"email":"selected@example.test","tenant":TENANT}).to_string();
        fs::write(&selection, &saved).unwrap();
        signed_in(&fixture, "other@example.test");
        if signal == "-TERM" {
            fs::write(fixture.profile.join("fixture-bridge-response"), json!({"ok":true,"profile":{"email":"selected@example.test","tenantId":"33333333-3333-4333-8333-333333333333"}}).to_string()).unwrap();
        }
        let login = login_process(&fixture);
        wait_for(&fixture, "/browser/mode", "headed").await;
        let config = fixture.config();
        let waiting = tokio::spawn(async move { managed::request_guard(&config).await });
        waiting.abort();
        assert!(
            Command::new("/bin/kill")
                .args([signal, &login.id().unwrap().to_string()])
                .status()
                .await
                .unwrap()
                .success()
        );
        let result = login_result(login).await;
        assert_eq!(result["error"]["code"], "CANCELLED", "{result}");
        assert_eq!(fs::read_to_string(&selection).unwrap(), saved);
        assert_eq!(
            managed::status(&fixture.config()).unwrap()["browser"]["state"],
            "stopped"
        );
        assert!(
            managed::request_guard(&fixture.config())
                .await
                .unwrap()
                .is_some()
        );
        signed_in(&fixture, "selected@example.test");
        let mut retry = fixture.command();
        retry.arg("--status");
        assert_eq!(output(retry).await["authenticated"], true);
        managed::stop(&fixture.config()).await.unwrap();
    }
}

#[tokio::test]
async fn cancelling_login_while_reads_drain_keeps_the_active_browser_running() {
    let fixture = Fixture::new(true);
    signed_in(&fixture, "selected@example.test");
    let mut first = fixture.command();
    first.arg("--status");
    assert_eq!(output(first).await["authenticated"], true);
    let active = managed::request_guard(&fixture.config()).await.unwrap();
    let login = login_process(&fixture);
    wait_for(&fixture, "/authentication/state", "signing_in").await;
    assert!(
        Command::new("/bin/kill")
            .args(["-TERM", &login.id().unwrap().to_string()])
            .status()
            .await
            .unwrap()
            .success()
    );
    assert_eq!(login_result(login).await["error"]["code"], "CANCELLED");
    assert_eq!(
        managed::status(&fixture.config()).unwrap()["browser"]["mode"],
        "headless"
    );
    drop(active);
    assert!(
        managed::request_guard(&fixture.config())
            .await
            .unwrap()
            .is_some()
    );
    managed::stop(&fixture.config()).await.unwrap();
}

#[tokio::test]
async fn managed_activity_waits_are_bounded_and_do_not_create_missing_profiles() {
    let missing = Fixture::new(false);
    assert!(
        managed::request_guard(&missing.config())
            .await
            .unwrap()
            .is_none()
    );
    assert!(!missing.profile.exists());
    let fixture = Fixture::new(true);
    let login = login_process(&fixture);
    wait_for(&fixture, "/browser/mode", "headed").await;
    let mut config = fixture.config();
    config.request_timeout = Duration::from_millis(100);
    assert_eq!(
        managed::request_guard(&config).await.unwrap_err().code,
        "BROWSER_BUSY"
    );
    assert!(
        Command::new("/bin/kill")
            .args(["-TERM", &login.id().unwrap().to_string()])
            .status()
            .await
            .unwrap()
            .success()
    );
    assert_eq!(login_result(login).await["error"]["code"], "CANCELLED");
}

#[tokio::test]
async fn explicit_login_waits_for_an_in_flight_cli_read_before_switching_modes() {
    let fixture = Fixture::new(true);
    signed_in(&fixture, "selected@example.test");
    let held = fixture.profile.join("fixture-hold-requests");
    fs::write(&held, "hold").unwrap();
    let mut command = fixture.command();
    command.arg("--status");
    let reading = tokio::spawn(output(command));
    tokio::time::timeout(Duration::from_secs(25), async {
        while !fixture.profile.join("fixture-request-started").exists() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    let login = login_process(&fixture);
    wait_for(&fixture, "/authentication/state", "signing_in").await;
    assert_eq!(
        managed::status(&fixture.config()).unwrap()["browser"]["mode"],
        "headless"
    );
    fs::remove_file(held).unwrap();
    assert_eq!(reading.await.unwrap()["authenticated"], true);
    assert_eq!(login_result(login).await["signedIn"], true);
    assert_eq!(
        managed::status(&fixture.config()).unwrap()["browser"]["state"],
        "stopped"
    );
}

#[tokio::test]
async fn login_can_recover_while_a_cancelled_reads_owned_browser_is_still_starting() {
    let fixture = Fixture::new(true);
    signed_in(&fixture, "selected@example.test");
    fs::write(fixture.profile.join("fixture-start-delay"), "delay").unwrap();
    let mut worker = fixture
        .command()
        .arg("__browser-worker")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    wait_for(&fixture, "/browser/state", "starting").await;
    let result = login_result(login_process(&fixture)).await;
    assert_eq!(result["signedIn"], true, "{result}");
    assert!(worker.wait().await.unwrap().success());
}

#[tokio::test]
async fn request_deadline_during_signin_keeps_wait_recovery_without_another_login() {
    let fixture = Fixture::new(true);
    let login = login_process(&fixture);
    wait_for(&fixture, "/browser/mode", "headed").await;
    let mut config = fixture.config();
    config.request_timeout = Duration::from_millis(100);
    let result = teams_mcp::server::TeamsServer::new(config)
        .execute("teams_status", json!({}))
        .await;
    assert!(
        Command::new("/bin/kill")
            .args(["-TERM", &login.id().unwrap().to_string()])
            .status()
            .await
            .unwrap()
            .success()
    );
    assert_eq!(login_result(login).await["error"]["code"], "CANCELLED");
    let error = serde_json::to_value(result.unwrap_err()).unwrap();
    assert_eq!(error["code"], "BROWSER_BUSY", "{error}");
    assert_eq!(error["authentication"]["state"], "signing_in");
    assert_eq!(error["recovery"]["action"], "wait");
    assert!(error["recovery"].get("command").is_none());
}

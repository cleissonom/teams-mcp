#[path = "support/managed_browser.rs"]
mod fake_browser;
use futures_util::SinkExt;
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};
use teams_mcp::{config::Config, managed};
use tokio::process::Command;

struct Fixture {
    root: PathBuf,
    profile: PathBuf,
    browser: PathBuf,
}

impl Fixture {
    fn new(initialized: bool) -> Self {
        let root = std::env::temp_dir().join(format!("teams-managed-{}", uuid::Uuid::new_v4()));
        let profile = root.join("profile with spaces");
        fs::create_dir_all(&root).unwrap();
        if initialized {
            fs::create_dir_all(profile.join("Default")).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&profile, fs::Permissions::from_mode(0o700)).unwrap();
            }
        }
        let browser = root.join("fake-chrome");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let script = format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\nexport TEAMS_FAKE_BROWSER_PROFILE={}\nexec {} --exact fake_browser_process_fixture --nocapture\n",
                quote(&root.join("args")),
                quote(&profile),
                quote(&std::env::current_exe().unwrap())
            );
            fs::write(&browser, script).unwrap();
            fs::set_permissions(&browser, fs::Permissions::from_mode(0o700)).unwrap();
        }
        Self {
            root,
            profile,
            browser,
        }
    }
    fn config(&self) -> Config {
        Config {
            browser_profile: Some(self.profile.clone()),
            browser_binary: Some(self.browser.clone()),
            ..Default::default()
        }
    }
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_teams-mcp"));
        command
            .env("TEAMS_AUTH_SOURCE", "headless")
            .env("TEAMS_BROWSER_PROFILE", &self.profile)
            .env("TEAMS_BROWSER_BINARY", &self.browser);
        for key in [
            "TEAMS_ACCOUNT",
            "TEAMS_TENANT_ID",
            "TEAMS_TARGET_ID",
            "TEAMS_CDP_URL",
            "TEAMS_CDP_PORT_FILE",
        ] {
            command.env_remove(key);
        }
        command
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::process::Command::new(env!("CARGO_BIN_EXE_teams-mcp"))
            .args(["auth", "stop"])
            .env("TEAMS_BROWSER_PROFILE", &self.profile)
            .env("TEAMS_BROWSER_BINARY", &self.browser)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[cfg(unix)]
fn quote(path: &Path) -> String {
    format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"))
}

async fn output(mut command: Command) -> Value {
    let result = tokio::time::timeout(Duration::from_secs(25), command.output())
        .await
        .unwrap()
        .unwrap();
    serde_json::from_slice(&result.stdout).unwrap_or_else(|_| {
        panic!(
            "CLI did not return JSON: {}",
            String::from_utf8_lossy(&result.stderr)
        )
    })
}

#[cfg(unix)]
async fn worker_ready(fixture: &Fixture, marker: &str) -> tokio::process::Child {
    let mut command = fixture.command();
    command
        .arg("__browser-worker")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let child = command.spawn().unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !fixture.profile.join(marker).exists() {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    child
}

#[cfg(unix)]
async fn terminate_worker(worker: &mut tokio::process::Child) -> std::process::ExitStatus {
    assert!(
        Command::new("/bin/kill")
            .args(["-TERM", &worker.id().unwrap().to_string()])
            .status()
            .await
            .unwrap()
            .success()
    );
    tokio::time::timeout(Duration::from_secs(8), worker.wait())
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn missing_profile_fails_without_creating_files_or_launching_browser() {
    let fixture = Fixture::new(false);
    let error = managed::endpoint(&fixture.config()).await.unwrap_err();
    assert_eq!(error.code, "AUTH_SETUP_REQUIRED");
    assert!(!fixture.profile.exists());
    assert!(!fixture.root.join("args").exists());
    let mut config = fixture.config();
    config.browser_binary = Some(fixture.root.join("missing-browser"));
    assert_eq!(
        managed::worker(&config, false).await.unwrap_err().code,
        "AUTH_SETUP_REQUIRED"
    );
    assert!(!fixture.profile.exists());
}

#[tokio::test]
#[cfg(unix)]
async fn simultaneous_clients_share_one_owned_headless_browser_and_stop_it_gracefully() {
    let fixture = Fixture::new(true);
    let mut a = fixture.command();
    a.arg("--status");
    let mut b = fixture.command();
    b.arg("--status");
    let (a, b) = tokio::join!(output(a), output(b));
    assert_eq!(a["error"]["code"], "ACCESS_DENIED");
    assert_eq!(b["error"]["code"], "ACCESS_DENIED");
    assert_eq!(
        fs::read_to_string(fixture.profile.join("fixture-launches"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    let endpoint = managed::endpoint(&fixture.config()).await.unwrap();
    assert_eq!(
        managed::endpoint(&fixture.config()).await.unwrap(),
        endpoint
    );
    let args = fs::read_to_string(fixture.root.join("args")).unwrap();
    assert!(args.lines().any(|arg| arg == "--headless=new"));
    assert!(args.lines().any(|arg| arg == "--restore-last-session"));
    assert!(!args.lines().any(|arg| arg.starts_with("https://")));
    assert!(args.lines().any(|arg| arg == "--remote-debugging-port=0"));
    assert!(
        args.lines()
            .any(|arg| arg == "--remote-debugging-address=127.0.0.1")
    );
    assert!(
        args.lines()
            .any(|arg| arg == format!("--user-data-dir={}", fixture.profile.display()))
    );
    for forbidden in [
        "--no-sandbox",
        "--disable-web-security",
        "--disable-site-isolation-trials",
    ] {
        assert!(!args.contains(forbidden));
    }
    assert_eq!(
        managed::stop(&fixture.config()).await.unwrap()["stopped"],
        true
    );
    assert!(fixture.profile.join("fixture-closed-gracefully").exists());
    assert_eq!(
        managed::stop(&fixture.config()).await.unwrap()["stopped"],
        false
    );
}

#[tokio::test]
#[cfg(unix)]
async fn explicit_login_is_headed_returns_before_signin_and_prevents_mode_conflicts() {
    let fixture = Fixture::new(false);
    let mut login = fixture.command();
    login.args(["auth", "login", "--no-wait"]);
    let result = output(login).await;
    assert_eq!(result["success"], true);
    let args = fs::read_to_string(fixture.root.join("args")).unwrap();
    assert!(!args.contains("--headless"));
    assert!(
        args.lines()
            .any(|arg| arg == "https://teams.microsoft.com/v2/")
    );
    assert_eq!(
        managed::endpoint(&fixture.config()).await.unwrap_err().code,
        "BROWSER_BUSY"
    );
    managed::stop(&fixture.config()).await.unwrap();
}

#[tokio::test]
#[cfg(unix)]
async fn tenant_startup_uses_validated_selection_and_encodes_login_hint() {
    let fixture = Fixture::new(false);
    let mut login = fixture.command();
    login
        .args(["auth", "login", "--no-wait"])
        .env("TEAMS_TENANT_ID", "22222222-2222-4222-8222-222222222222")
        .env("TEAMS_ACCOUNT", "guest+filter&name@example.test");
    assert_eq!(output(login).await["success"], true);
    let args = fs::read_to_string(fixture.root.join("args")).unwrap();
    let url = url::Url::parse(args.lines().last().unwrap()).unwrap();
    let query: std::collections::BTreeMap<_, _> = url.query_pairs().collect();
    managed::stop(&fixture.config()).await.unwrap();
    assert_eq!(
        url.origin().ascii_serialization(),
        "https://teams.microsoft.com"
    );
    assert_eq!(query["tenantId"], "22222222-2222-4222-8222-222222222222");
    assert_eq!(query["login_hint"], "guest+filter&name@example.test");
    assert_eq!(query.len(), 2);
}

#[tokio::test]
#[cfg(unix)]
async fn stop_saves_only_successful_identity_selection_and_preserves_it_on_failed_snapshots() {
    let fixture = Fixture::new(true);
    let tenant = "22222222-2222-4222-8222-222222222222";
    let selection = fixture.profile.join(".teams-mcp-selection.json");
    fs::write(fixture.profile.join("fixture-bridge-response"),json!({"ok":true,"profile":{"email":"guest+tag@example.test","tenantId":tenant,"id":"not-saved","displayName":"not-saved","token":"never-save"}}).to_string()).unwrap();
    let mut login = fixture.command();
    login.args(["auth", "login", "--no-wait"]);
    assert_eq!(output(login).await["success"], true);
    let stopped = managed::stop(&fixture.config()).await.unwrap();
    assert_eq!(stopped["selectionSaved"], true);
    let saved = fs::read_to_string(&selection).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&saved).unwrap(),
        json!({"email":"guest+tag@example.test","tenant":tenant})
    );
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        fs::metadata(&selection).unwrap().permissions().mode() & 0o777,
        0o600
    );
    for response in [
        json!({"ok":false,"error":{"code":"ACCOUNT_SELECTION_REQUIRED"}}),
        json!({"ok":true,"profile":{"email":"malformed","tenantId":tenant}}),
        json!({"ok":true,"profile":{"email":"other@example.test","tenantId":tenant}}),
        json!({"_targets":[{"targetId":"foreign","type":"page","url":"https://example.test/"}]}),
        json!({"_targets":[{"targetId":"signin","type":"page","url":"https://login.microsoftonline.com/"}]}),
        json!({"_targets":[{"targetId":"one","type":"page","url":"https://teams.microsoft.com/"},{"targetId":"two","type":"page","url":"https://teams.microsoft.com/"}]}),
    ] {
        fs::write(
            fixture.profile.join("fixture-bridge-response"),
            response.to_string(),
        )
        .unwrap();
        let mut login = fixture.command();
        login.args(["auth", "login", "--no-wait"]);
        assert_eq!(output(login).await["success"], true);
        assert_eq!(
            managed::stop(&fixture.config()).await.unwrap()["selectionSaved"],
            false
        );
        assert_eq!(fs::read_to_string(&selection).unwrap(), saved);
    }
    let args = fs::read_to_string(fixture.root.join("args")).unwrap();
    let url = url::Url::parse(args.lines().last().unwrap()).unwrap();
    assert!(
        url.query_pairs()
            .any(|(key, value)| key == "tenantId" && value == tenant)
    );
    assert!(
        url.query_pairs()
            .any(|(key, value)| key == "login_hint" && value == "guest+tag@example.test")
    );
}

#[tokio::test]
#[cfg(unix)]
async fn login_waits_for_signin_then_saves_the_account_and_closes_the_window() {
    let fixture = Fixture::new(false);
    let tenant = "22222222-2222-4222-8222-222222222222";
    fs::create_dir_all(&fixture.profile).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&fixture.profile, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(
        fixture.profile.join("fixture-bridge-response"),
        json!({"ok":true,"profile":{"email":"guest@example.test","tenantId":tenant}}).to_string(),
    )
    .unwrap();
    let mut login = fixture.command();
    login
        .args(["auth", "login"])
        .stdin(std::process::Stdio::null());
    let result = output(login).await;
    assert_eq!(result["signedIn"], true);
    assert_eq!(result["account"], "guest@example.test");
    assert_eq!(result["selectionSaved"], true);
    assert_eq!(result["stopped"], true);
    assert!(fixture.profile.join("fixture-closed-gracefully").exists());
    assert_eq!(
        serde_json::from_str::<Value>(
            &fs::read_to_string(fixture.profile.join(".teams-mcp-selection.json")).unwrap()
        )
        .unwrap(),
        json!({"email":"guest@example.test","tenant":tenant})
    );
}

#[tokio::test]
#[cfg(unix)]
async fn offline_status_reports_state_without_launching_a_browser() {
    let fixture = Fixture::new(true);
    let mut status = fixture.command();
    status.args(["auth", "status"]);
    let result = output(status).await;
    assert_eq!(result["setupComplete"], true);
    assert_eq!(result["browser"]["state"], "stopped");
    assert!(
        !fixture.root.join("args").exists(),
        "status must not start Chrome"
    );
    let mut call = fixture.command();
    call.arg("--status");
    assert_eq!(output(call).await["error"]["code"], "ACCESS_DENIED");
    let mut status = fixture.command();
    status.args(["auth", "status"]);
    let result = output(status).await;
    assert_eq!(result["browser"]["state"], "running");
    assert_eq!(result["browser"]["mode"], "headless");
    assert_eq!(result["browser"]["stopsAfterIdleSeconds"], 300);
    assert_eq!(result["browser"]["freezesAfterIdleSeconds"], 15);
    managed::stop(&fixture.config()).await.unwrap();
}

#[tokio::test]
#[cfg(unix)]
async fn idle_page_is_frozen_and_the_next_call_makes_it_active_again() {
    let fixture = Fixture::new(true);
    let lifecycle = fixture.profile.join("fixture-lifecycle");
    let mut call = fixture.command();
    call.arg("--status").env("TEAMS_BROWSER_FREEZE_SECS", "5");
    assert_eq!(output(call).await["error"]["code"], "ACCESS_DENIED");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(12);
    while !fs::read_to_string(&lifecycle)
        .unwrap_or_default()
        .contains("frozen")
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "idle page was not frozen"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let mut status = fixture.command();
    status.args(["auth", "status"]);
    assert_eq!(output(status).await["browser"]["frozen"], true);
    let mut call = fixture.command();
    call.arg("--status");
    assert_eq!(output(call).await["error"]["code"], "ACCESS_DENIED");
    let states = fs::read_to_string(&lifecycle).unwrap();
    assert!(states.trim_end().ends_with("active"), "{states}");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let mut status = fixture.command();
    status.args(["auth", "status"]);
    assert_eq!(output(status).await["browser"]["frozen"], false);
    managed::stop(&fixture.config()).await.unwrap();
}

#[tokio::test]
#[cfg(unix)]
async fn compact_clears_only_teams_cache_storage_through_chrome() {
    let fixture = Fixture::new(true);
    let mut compact = fixture.command();
    compact.args(["auth", "compact"]);
    let result = output(compact).await;
    assert_eq!(result["compacted"], true);
    assert_eq!(result["cacheStorageBeforeMiB"], 1000.0);
    assert_eq!(result["cacheStorageAfterMiB"], 0.0);
    let cleared = fs::read_to_string(fixture.profile.join("fixture-cache-cleared")).unwrap();
    assert_eq!(
        cleared.lines().collect::<Vec<_>>(),
        [
            "https://teams.microsoft.com cache_storage",
            "https://teams.cloud.microsoft cache_storage"
        ]
    );
    assert!(
        fixture.profile.join("Default").is_dir(),
        "sign-in profile must remain"
    );
    managed::stop(&fixture.config()).await.unwrap();
}

#[tokio::test]
async fn chrome_singleton_lock_is_never_removed_or_used_as_our_worker() {
    let fixture = Fixture::new(true);
    fs::write(fixture.profile.join("SingletonLock"), "unrelated-browser").unwrap();
    let mut login = fixture.command();
    login.args(["auth", "login"]);
    let result = output(login).await;
    assert_eq!(result["error"]["code"], "BROWSER_PROFILE_IN_USE");
    assert_eq!(
        fs::read_to_string(fixture.profile.join("SingletonLock")).unwrap(),
        "unrelated-browser"
    );
    assert!(!fixture.root.join("args").exists());
}

#[tokio::test]
#[cfg(unix)]
async fn crashed_worker_state_cannot_select_a_stale_debugging_endpoint_during_restart() {
    let fixture = Fixture::new(true);
    fs::write(fixture.profile.join(".teams-mcp-state.json"),json!({"version":1,"nonce":uuid::Uuid::new_v4().to_string(),"endpoint":"ws://127.0.0.1:9/devtools/browser/stale","headed":false}).to_string()).unwrap();
    fs::write(
        fixture.profile.join("DevToolsActivePort"),
        "9\n/devtools/browser/stale\n",
    )
    .unwrap();
    fs::write(fixture.profile.join("fixture-start-delay"), "delay").unwrap();
    let mut command = fixture.command();
    command.arg("--status");
    let result = output(command).await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    for _ in 0..20 {
        if managed::stop(&fixture.config()).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(result["error"]["code"], "ACCESS_DENIED");
}

#[tokio::test]
#[cfg(unix)]
async fn existing_public_profile_is_rejected_without_launch_or_permission_changes() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new(true);
    fs::set_permissions(&fixture.profile, fs::Permissions::from_mode(0o777)).unwrap();
    let mut command = fixture.command();
    command.arg("--status");
    let result = output(command).await;
    let launched = fixture.root.join("args").exists();
    let _ = managed::stop(&fixture.config()).await;
    assert_eq!(result["error"]["code"], "BROWSER_PROFILE_INSECURE");
    assert!(!launched);
    assert_eq!(
        fs::metadata(&fixture.profile).unwrap().permissions().mode() & 0o777,
        0o777
    );
}

#[tokio::test]
#[cfg(unix)]
async fn sigterm_closes_only_the_owned_browser_and_releases_profile_ownership() {
    let fixture = Fixture::new(true);
    let mut worker = worker_ready(&fixture, ".teams-mcp-state.json").await;
    let state: Value = serde_json::from_str(
        &fs::read_to_string(fixture.profile.join(".teams-mcp-state.json")).unwrap(),
    )
    .unwrap();
    let exit = terminate_worker(&mut worker).await;
    let graceful = fixture.profile.join("fixture-closed-gracefully").exists();
    if !graceful {
        let (mut ws, _) = tokio_tungstenite::connect_async(state["endpoint"].as_str().unwrap())
            .await
            .unwrap();
        ws.send(tokio_tungstenite::tungstenite::Message::Text(
            json!({"id":1,"method":"Browser.close","params":{}})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(600)).await;
    }
    assert!(exit.success(), "supervisor exited without handling SIGTERM");
    assert!(graceful, "owned Chrome was orphaned");
    assert_eq!(
        managed::stop(&fixture.config()).await.unwrap()["stopped"],
        false
    );
}

#[tokio::test]
#[cfg(unix)]
async fn startup_sigterm_allows_owned_browser_to_clean_its_lock_before_restart() {
    for invalid_port in [false, true] {
        let fixture = Fixture::new(true);
        let mode = if invalid_port {
            "fixture-invalid-port"
        } else {
            "fixture-await-startup-signal"
        };
        fs::write(fixture.profile.join(mode), "wait").unwrap();
        let mut worker = worker_ready(&fixture, "fixture-startup-ready").await;
        let exit = if invalid_port {
            tokio::time::timeout(Duration::from_secs(8), worker.wait())
                .await
                .unwrap()
                .unwrap()
        } else {
            terminate_worker(&mut worker).await
        };
        assert_eq!(exit.success(), !invalid_port);
        assert!(
            fixture.profile.join("fixture-closed-gracefully").exists(),
            "startup error or shutdown bypassed owned child cleanup"
        );
        assert!(!fixture.profile.join("SingletonLock").exists());
        if invalid_port {
            let error: Value = serde_json::from_str(
                &fs::read_to_string(fixture.profile.join(".teams-mcp-error.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(error["code"], "BROWSER_STATE_INVALID");
        }
        fs::remove_file(fixture.profile.join(mode)).unwrap();
        let mut restart = fixture.command();
        restart.arg("--status");
        assert_eq!(output(restart).await["error"]["code"], "ACCESS_DENIED");
        managed::stop(&fixture.config()).await.unwrap();
    }
}

// Executed as a separate process by fake-chrome; ordinary test runs do no work here.
#[tokio::test]
async fn fake_browser_process_fixture() {
    fake_browser::run().await;
}

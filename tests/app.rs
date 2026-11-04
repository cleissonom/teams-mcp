pub use teams_mcp::{config, error};
#[path = "../src/app.rs"]
#[allow(dead_code)] // These fixtures use the fake runner without sending Apple Events.
mod app;

use app::{
    App, AppTarget, automation_failure, execution_script, inventory_script, parse_response,
    select_target,
};
use config::Config;
use serde_json::json;
use std::sync::{Arc, Mutex};

#[test]
fn window_selection_requires_a_unique_eligible_numeric_target() {
    let targets = vec![
        AppTarget {
            window_id: 17,
            tab_id: 1,
        },
        AppTarget {
            window_id: 31,
            tab_id: 2,
        },
    ];
    let error = select_target(&targets, None).unwrap_err();
    assert_eq!(error.code, "APP_WINDOW_REQUIRED");
    assert!(error.message.contains("17, 31"));
    assert_eq!(select_target(&targets, Some(31)).unwrap().tab_id, 2);
    assert_eq!(
        select_target(&targets, Some(99)).unwrap_err().code,
        "APP_WINDOW_REQUIRED"
    );
    assert_eq!(
        select_target(&[], None).unwrap_err().code,
        "APP_TEAMS_TAB_REQUIRED"
    );
    assert_eq!(
        select_target(&[targets[0].clone()], None)
            .unwrap()
            .window_id,
        17
    );
}

#[test]
fn script_generation_keeps_user_input_in_quoted_data_and_checks_running_app() {
    let query = "\"; throw new Error('injected'); //\n ";
    let input = json!({"action":"request","query":query});
    let script = execution_script(
        &AppTarget {
            window_id: 17,
            tab_id: 1,
        },
        &input,
        "slot-test",
    );
    assert!(script.contains(".running()"));
    assert!(script.contains("JSON.stringify"));
    assert!(script.contains("teams.microsoft.com"));
    assert!(script.contains("nativeApp"));
    assert!(!script.contains('\u{2028}'));
    assert!(!script.contains("activate("));
    assert!(!script.contains("launch("));
    assert!(!script.contains("doShellScript"));
    assert!(!script.contains("\"; throw new Error('injected'); //\n"));
    assert!(!inventory_script().contains("execute({"));
}

#[test]
fn automation_errors_are_fixed_and_never_echo_raw_error_or_credentials() {
    for (raw, code) in [
        (
            "Error: Not authorized (-1743), secret=do-not-echo",
            "APP_AUTOMATION_DENIED",
        ),
        (
            "Error: executing JavaScript through Apple Events is off (12), secret=do-not-echo",
            "APP_AUTOMATION_DISABLED",
        ),
        (
            "Unknown failure containing secret=do-not-echo",
            "APP_AUTOMATION_FAILED",
        ),
    ] {
        let error = automation_failure(raw);
        assert_eq!(error.code, code);
        assert!(!error.message.contains("secret="));
    }
    assert!(parse_response("not-json").is_err());
    assert!(parse_response(r#"{"secret":"not-a-bridge-result"}"#).is_err());
    let error = parse_response(
        r#"{"ok":false,"error":{"code":"APP_AUTOMATION_DISABLED","message":"secret=do-not-echo"}}"#,
    )
    .unwrap_err();
    assert_eq!(error.code, "APP_AUTOMATION_DISABLED");
    assert!(!error.message.contains("secret="));
}

#[tokio::test]
async fn app_uses_selected_existing_window_and_injects_account_constraints() {
    let scripts = Arc::new(Mutex::new(Vec::new()));
    let captured = scripts.clone();
    let app = App::new(Config {
        app_window_id: Some(31),
        account: Some("expected@example.test".into()),
        tenant: Some("tenant".into()),
        ..Default::default()
    });
    let mut invocation = 0;
    let result = app
        .evaluate_with(json!({"action":"status"}), move |script| {
            captured.lock().unwrap().push(script);
            invocation += 1;
            let result = if invocation == 1 {
                json!({"ok":true,"targets":[{"windowId":17,"tabId":1},{"windowId":31,"tabId":2}]})
            } else {
                json!({"ok":true,"profile":{"email":"expected@example.test"}})
            };
            std::future::ready(Ok(result.to_string()))
        })
        .await
        .unwrap();
    assert_eq!(result["profile"]["email"], "expected@example.test");
    let scripts = scripts.lock().unwrap();
    assert_eq!(scripts.len(), 2);
    assert!(scripts[1].contains("expected@example.test"));
    assert!(scripts[1].contains("tenantId"));
    assert!(scripts[1].contains("31"));
}

#[tokio::test]
async fn failed_inventory_never_attempts_javascript_execution() {
    let app = App::new(Config::default());
    let mut calls = 0;
    let error = app
        .evaluate_with(json!({"action":"status"}), |_| {
            calls += 1;
            std::future::ready(Ok(
                json!({"ok":false,"error":{"code":"APP_NOT_RUNNING"}}).to_string()
            ))
        })
        .await
        .unwrap_err();
    assert_eq!(error.code, "APP_NOT_RUNNING");
    assert_eq!(calls, 1);
}

#[test]
fn script_data_literals_round_trip_untrusted_text() {
    for value in [
        "\"; Application('Terminal').doScript('unsafe'); //",
        "line\n  \\\"'`$()",
        "__proto__",
    ] {
        let literal = app::js_string(value);
        assert_eq!(serde_json::from_str::<String>(&literal).unwrap(), value);
        assert!(!literal.contains('\u{2028}'));
        assert!(!literal.contains('\u{2029}'));
    }
}

#[tokio::test]
async fn automation_output_is_bounded_before_parsing() {
    assert_eq!(app::read_capped(&b"1234"[..], 4).await.unwrap(), b"1234");
    assert_eq!(
        app::read_capped(&b"12345"[..], 4).await.unwrap_err().code,
        "APP_OUTPUT_TOO_LARGE"
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn automation_runner_closes_script_input_before_waiting_for_output() {
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        app::run_script("1 + 1".into()),
    )
    .await
    .expect("osascript must receive EOF and finish evaluating")
    .unwrap();
    assert_eq!(output.trim(), "2");
}

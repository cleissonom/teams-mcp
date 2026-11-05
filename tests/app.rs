pub use teams_mcp::{config, error};
#[path = "../src/app.rs"]
#[allow(dead_code)] // These fixtures use the fake runner without sending Apple Events.
mod app;
#[path = "../src/identity.rs"]
mod identity;

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

#[tokio::test]
async fn app_binding_survives_expiry_and_rejects_another_account_or_tenant() {
    for changed in [
        json!({"id":"other-user","email":"other@example.test","tenantId":"original-tenant"}),
        json!({"id":"original-user","email":"original@example.test","tenantId":"other-tenant"}),
    ] {
        let app = App::new(Config::default());
        let clone = app.clone();
        let original = json!({"id":"original-user","email":"original@example.test","tenantId":"original-tenant"});
        synthetic_app_call(&app, json!({"ok":true,"profile":original}))
            .await
            .unwrap();
        assert_eq!(
            clone.selection(),
            (
                Some("original@example.test".into()),
                Some("original-tenant".into())
            )
        );
        let expired =
            synthetic_app_call(&clone, json!({"ok":false,"error":{"code":"AUTH_EXPIRED"}}))
                .await
                .unwrap();
        assert_eq!(expired["error"]["code"], "AUTH_EXPIRED");
        let error = synthetic_app_call(&clone, json!({"ok":true,"profile":changed}))
            .await
            .unwrap_err();
        assert_eq!(error.code, "ACCOUNT_MISMATCH");
        let recovered = synthetic_app_call(&app, json!({"ok":true,"profile":original}))
            .await
            .unwrap();
        assert_eq!(recovered["profile"], original);
    }
}

#[tokio::test]
async fn first_failed_app_read_binds_the_known_account_and_tenant() {
    let app = App::new(Config::default());
    let original =
        json!({"id":"original-user","email":"original@example.test","tenantId":"original-tenant"});
    let changed =
        json!({"id":"other-user","email":"other@example.test","tenantId":"original-tenant"});
    let failed = synthetic_app_call_with(
        &app,
        json!({"action":"request"}),
        json!({"ok":false,"profile":original,"error":{"code":"AUTH_EXPIRED"}}),
    )
    .await
    .unwrap();
    assert_eq!(failed["error"]["code"], "AUTH_EXPIRED");
    assert_eq!(
        app.selection(),
        (
            Some("original@example.test".into()),
            Some("original-tenant".into())
        )
    );
    assert_eq!(
        synthetic_app_call(&app, json!({"ok":true,"profile":changed}))
            .await
            .unwrap_err()
            .code,
        "ACCOUNT_MISMATCH"
    );
    assert_eq!(
        synthetic_app_call(&app, json!({"ok":true,"profile":original}))
            .await
            .unwrap()["profile"],
        original
    );
}

async fn synthetic_app_call(
    app: &App,
    result: serde_json::Value,
) -> Result<serde_json::Value, error::Failure> {
    synthetic_app_call_with(app, json!({"action":"status"}), result).await
}

async fn synthetic_app_call_with(
    app: &App,
    input: serde_json::Value,
    result: serde_json::Value,
) -> Result<serde_json::Value, error::Failure> {
    let mut invocation = 0;
    app.evaluate_with(input, move |_| {
        invocation += 1;
        let value = if invocation == 1 {
            json!({"ok":true,"targets":[{"windowId":17,"tabId":1}]})
        } else {
            result.clone()
        };
        std::future::ready(Ok(value.to_string()))
    })
    .await
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

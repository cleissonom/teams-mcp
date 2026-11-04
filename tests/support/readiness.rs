#[path = "../../src/cdp.rs"]
#[allow(dead_code)]
mod transport;

use crate::{
    config::{AuthSource, Config},
    error::Failure,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{collections::VecDeque, time::Duration};
use tokio::{net::TcpListener, task::JoinHandle};
use tokio_tungstenite::{accept_async, tungstenite::Message};

const TEAMS: &str = "https://teams.microsoft.com/v2/";

#[derive(Default)]
struct Counts {
    targets: usize,
    attachments: usize,
    installs: usize,
    evaluations: usize,
    reloads: usize,
    detachments: usize,
    activations: usize,
    selection_bound: bool,
}

#[tokio::test]
async fn saved_managed_account_and_tenant_bind_the_bridge_without_affecting_external_browsers() {
    for source in [AuthSource::Headless, AuthSource::Browser] {
        let (result, counts) = run(&[TEAMS], &[ready()], source).await;
        assert!(result.is_ok());
        assert_eq!(counts.selection_bound, source == AuthSource::Headless);
    }
}

#[tokio::test]
async fn headless_waits_for_blank_navigation_and_cold_authentication_to_be_ready() {
    let (result, counts) = run(
        &["about:blank", TEAMS],
        &[error("AUTH_REQUIRED"), ready()],
        AuthSource::Headless,
    )
    .await;
    assert_eq!(result.unwrap()["ok"], true);
    assert_eq!(counts.targets, 3);
    assert_eq!(counts.evaluations, 2);
    assert_eq!((counts.attachments, counts.installs), (1, 1));
    assert_eq!(counts.detachments, 0);
    assert_eq!(counts.reloads, 0);
}

#[tokio::test]
async fn cold_headless_startup_can_create_its_first_page_after_devtools_is_ready() {
    let (result, counts) = run(&["", TEAMS], &[ready()], AuthSource::Headless).await;
    assert_eq!(result.unwrap()["ok"], true);
    assert_eq!(counts.targets, 2);
    assert_eq!(counts.evaluations, 1);
}

#[tokio::test]
async fn headless_restoration_waits_through_transient_navigation_without_trusting_it() {
    for url in ["chrome://newtab/", "https://sso.example.test/redirect"] {
        let (result, counts) = run(&[url, TEAMS], &[ready()], AuthSource::Headless).await;
        assert_eq!(result.unwrap()["ok"], true);
        assert_eq!(counts.targets, 2);
        assert_eq!(counts.evaluations, 1);
        assert_eq!(counts.attachments, 1);
    }
}

#[tokio::test]
async fn headless_login_redirects_require_explicit_setup_without_attaching_to_login_pages() {
    for url in [
        "https://login.microsoftonline.com/common/oauth2/authorize",
        "https://login.microsoft.com/device",
        "https://login.live.com/oauth20_authorize.srf",
    ] {
        let (result, counts) = run(&[url], &[ready()], AuthSource::Headless).await;
        let error = result.unwrap_err();
        assert_eq!(error.code, "AUTH_SETUP_REQUIRED");
        assert!(error.message.contains("auth stop"));
        assert!(error.message.contains("auth login"));
        assert_eq!(counts.evaluations, 0);
    }
}

#[tokio::test]
async fn headless_silent_signin_redirect_can_return_to_teams_without_new_setup() {
    let (result, counts) = run(
        &[
            "https://login.microsoftonline.com/common/oauth2/authorize?prompt=none",
            TEAMS,
        ],
        &[ready()],
        AuthSource::Headless,
    )
    .await;
    assert_eq!(result.unwrap()["ok"], true);
    assert_eq!(counts.evaluations, 1);
    assert_eq!(counts.reloads, 0);
}

#[tokio::test]
async fn managed_navigation_race_rechecks_targets_without_attaching_to_signin_origin() {
    let (result, counts) = run(
        &[
            TEAMS,
            "https://login.microsoftonline.com/common/oauth2/authorize",
        ],
        &[error("WRONG_ORIGIN")],
        AuthSource::Headless,
    )
    .await;
    assert_eq!(result.unwrap_err().code, "AUTH_SETUP_REQUIRED");
    assert_eq!(counts.evaluations, 1);
    let (result, counts) = run(
        &[TEAMS],
        &[error("WRONG_ORIGIN"), ready()],
        AuthSource::Headless,
    )
    .await;
    assert_eq!(result.unwrap()["ok"], true);
    assert_eq!(counts.evaluations, 2);
}

#[tokio::test]
async fn managed_expiry_reloads_once_and_retries_after_normal_authentication_loading() {
    let (result, counts) = run(
        &[TEAMS],
        &[error("AUTH_EXPIRED"), error("AUTH_REQUIRED"), ready()],
        AuthSource::Headless,
    )
    .await;
    assert_eq!(result.unwrap()["ok"], true);
    assert_eq!(counts.reloads, 1);
    assert_eq!(counts.evaluations, 3);
    // Reloading destroys the installed bridge context; the page session stays attached.
    assert_eq!((counts.attachments, counts.installs), (1, 2));
}

#[tokio::test]
async fn managed_session_renewal_is_bounded_and_never_reloads_twice() {
    let (result, counts) = run(&[TEAMS], &[error("AUTH_EXPIRED")], AuthSource::Headless).await;
    assert_eq!(result.unwrap_err().code, "AUTH_SETUP_REQUIRED");
    assert_eq!(counts.reloads, 1);
    assert_eq!(counts.evaluations, 2);
}

#[tokio::test]
async fn managed_authentication_loading_has_a_finite_setup_deadline() {
    let (result, counts) = run(&[TEAMS], &[error("AUTH_REQUIRED")], AuthSource::Headless).await;
    assert_eq!(result.unwrap_err().code, "AUTH_SETUP_REQUIRED");
    assert!(counts.evaluations > 1);
    assert_eq!(counts.reloads, 0);
}

#[tokio::test]
async fn permissions_network_guest_and_account_errors_return_without_retry_or_reload() {
    for code in [
        "ACCESS_DENIED",
        "FEATURE_UNAVAILABLE",
        "NETWORK_ERROR",
        "ACCOUNT_MISMATCH",
        "ACCOUNT_AMBIGUOUS",
    ] {
        let (result, counts) = run(&[TEAMS], &[error(code)], AuthSource::Headless).await;
        assert_eq!(result.unwrap()["error"]["code"], code);
        assert_eq!(counts.evaluations, 1);
        assert_eq!(counts.reloads, 0);
    }
}

#[tokio::test]
async fn untrusted_targets_are_never_attached_and_headless_wait_is_bounded() {
    for url in [
        "https://login.microsoftonline.com.attacker.test/",
        "http://login.microsoftonline.com/",
        "https://user@login.microsoftonline.com/",
    ] {
        let (result, counts) = run(&[url], &[ready()], AuthSource::Headless).await;
        assert_eq!(result.unwrap_err().code, "TEAMS_TAB_REQUIRED");
        assert!(counts.targets > 1);
        assert_eq!(counts.evaluations, 0);
    }
}

#[tokio::test]
async fn managed_restore_navigation_during_bridge_install_is_retried() {
    let (result, counts) = run_with(&[TEAMS], &[ready()], AuthSource::Headless, 1).await;
    assert_eq!(result.unwrap()["ok"], true);
    assert_eq!(counts.attachments, 2, "the failed page session is replaced");
    assert_eq!(counts.detachments, 1);
}

#[tokio::test]
async fn managed_calls_activate_a_frozen_page_but_existing_browsers_are_never_touched() {
    for source in [AuthSource::Headless, AuthSource::Browser] {
        let (result, counts) = run(&[TEAMS], &[ready()], source).await;
        assert!(result.is_ok());
        assert_eq!(counts.activations > 0, source == AuthSource::Headless);
    }
}

#[tokio::test]
async fn existing_browser_authentication_errors_remain_immediate() {
    for code in ["AUTH_REQUIRED", "AUTH_EXPIRED"] {
        let (result, counts) = run(&[TEAMS], &[error(code)], AuthSource::Browser).await;
        assert_eq!(result.unwrap()["error"]["code"], code);
        assert_eq!(counts.evaluations, 1);
        assert_eq!(counts.reloads, 0);
    }
}

fn error(code: &str) -> Value {
    json!({"ok":false,"error":{"code":code,"message":"fixture"}})
}
fn ready() -> Value {
    json!({"ok":true,"profile":{"id":"fixture-user"}})
}

async fn run(
    urls: &[&str],
    outputs: &[Value],
    source: AuthSource,
) -> (Result<Value, Failure>, Counts) {
    run_with(urls, outputs, source, 0).await
}

async fn run_with(
    urls: &[&str],
    outputs: &[Value],
    source: AuthSource,
    install_failures: usize,
) -> (Result<Value, Failure>, Counts) {
    let (endpoint, server) = fixture(urls, outputs, install_failures).await;
    let browser = transport::Browser::new(Config {
        auth_source: source,
        cdp_url: Some(endpoint),
        ..Config::default()
    });
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        browser.evaluate(json!({"action":"status"})),
    )
    .await
    .expect("managed readiness must have a finite deadline");
    drop(browser);
    (result, server.await.unwrap())
}

async fn fixture(
    urls: &[&str],
    outputs: &[Value],
    mut install_failures: usize,
) -> (String, JoinHandle<Counts>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!(
        "ws://{}/devtools/browser/readiness",
        listener.local_addr().unwrap()
    );
    let mut urls: VecDeque<String> = urls.iter().map(|url| (*url).into()).collect();
    let mut outputs: VecDeque<Value> = outputs.iter().cloned().collect();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(socket).await.unwrap();
        let mut counts = Counts::default();
        while let Some(Ok(Message::Text(text))) = ws.next().await {
            let call: Value = serde_json::from_str(&text).unwrap();
            let result = match call["method"].as_str().unwrap() {
                "Target.getTargets" => {
                    counts.targets += 1;
                    let url = next(&mut urls);
                    if url.is_empty() {
                        json!({"targetInfos":[]})
                    } else {
                        json!({"targetInfos":[{"targetId":"teams","type":"page","url":url}]})
                    }
                }
                "Target.attachToTarget" => {
                    counts.attachments += 1;
                    json!({"sessionId":"fixture"})
                }
                "Page.setWebLifecycleState" => {
                    assert_eq!(call["params"]["state"], "active");
                    counts.activations += 1;
                    json!({})
                }
                "Page.getFrameTree" => json!({"frameTree":{"frame":{"id":"root"}}}),
                "Page.createIsolatedWorld" if install_failures > 0 => {
                    // A restoring page navigated away while the bridge was being installed.
                    install_failures -= 1;
                    ws.send(Message::Text(
                        json!({"id":call["id"],"error":{"code":-32000,"message":"Cannot find frame"}})
                            .to_string()
                            .into(),
                    ))
                    .await
                    .unwrap();
                    continue;
                }
                "Page.createIsolatedWorld" => json!({"executionContextId":42}),
                "Runtime.evaluate" => {
                    counts.installs += 1;
                    json!({"result":{"objectId":"bridge"}})
                }
                "Runtime.callFunctionOn" => {
                    counts.evaluations += 1;
                    let input = &call["params"]["arguments"][0]["value"];
                    counts.selection_bound = input["expectedAccount"] == "fixture@example.test"
                        && input["tenantId"] == "11111111-2222-4333-8444-555555555555";
                    json!({"result":{"value":next(&mut outputs)}})
                }
                "Page.reload" => {
                    counts.reloads += 1;
                    json!({})
                }
                "Target.detachFromTarget" => {
                    counts.detachments += 1;
                    json!({})
                }
                method => panic!("unexpected readiness method: {method}"),
            };
            ws.send(Message::Text(
                json!({"id":call["id"],"result":result}).to_string().into(),
            ))
            .await
            .unwrap();
        }
        counts
    });
    (endpoint, server)
}

fn next<T: Clone>(items: &mut VecDeque<T>) -> T {
    if items.len() > 1 {
        items.pop_front().unwrap()
    } else {
        items.front().unwrap().clone()
    }
}

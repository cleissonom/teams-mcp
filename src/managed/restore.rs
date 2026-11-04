use super::failure;
use crate::{cdp::Cdp, config::is_teams_page, error::Failure};
use serde_json::json;
use std::time::Duration;
use tokio::time::timeout;

pub(super) async fn prepare(endpoint: &str, url: &str) -> Result<(), Failure> {
    timeout(Duration::from_secs(5), prepare_inner(endpoint, url))
        .await
        .map_err(|_| failure("BROWSER_RESTORE_FAILED"))?
}

async fn prepare_inner(endpoint: &str, url: &str) -> Result<(), Failure> {
    let cdp = Cdp::connect(endpoint).await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    let targets = loop {
        let targets = cdp.call("Target.getTargets", json!({}), None).await?;
        let pages = targets["targetInfos"]
            .as_array()
            .ok_or_else(|| failure("BROWSER_RESTORE_FAILED"))?;
        if pages.iter().any(|target| target["type"] == "page")
            || tokio::time::Instant::now() >= deadline
        {
            break targets;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let pages: Vec<_> = targets["targetInfos"]
        .as_array()
        .ok_or_else(|| failure("BROWSER_RESTORE_FAILED"))?
        .iter()
        .filter(|target| target["type"] == "page")
        .collect();
    // Restored Teams tabs preserve the user's organization. CDP retains explicit
    // ambiguity errors and foreign-page rejection; bootstrap never alters those tabs.
    if pages
        .iter()
        .any(|target| target["url"].as_str().is_some_and(is_teams_page))
    {
        return Ok(());
    }
    if pages.is_empty() {
        cdp.call("Target.createTarget", json!({"url":url}), None)
            .await?;
        return Ok(());
    }
    let eligible: Vec<_> = pages
        .into_iter()
        .filter(|target| target["url"].as_str().is_some_and(bootstrap_page))
        .collect();
    let [target] = eligible.as_slice() else {
        return Ok(());
    };
    let target = target["targetId"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| failure("BROWSER_RESTORE_FAILED"))?;
    let attached = cdp
        .call(
            "Target.attachToTarget",
            json!({"targetId":target,"flatten":true}),
            None,
        )
        .await?;
    let session = attached["sessionId"]
        .as_str()
        .ok_or_else(|| failure("BROWSER_RESTORE_FAILED"))?;
    let expression = format!(
        "(() => {{ if (location.href !== 'about:blank' && !['https://login.microsoftonline.com','https://login.live.com','https://login.microsoft.com'].includes(location.origin)) return false; location.replace({}); return true; }})()",
        serde_json::to_string(url).expect("string serialization")
    );
    let result = cdp
        .call(
            "Runtime.evaluate",
            json!({"expression":expression,"returnByValue":true}),
            Some(session),
        )
        .await;
    let detached = cdp
        .call(
            "Target.detachFromTarget",
            json!({"sessionId":session}),
            None,
        )
        .await;
    let result = result?;
    detached?;
    if result.get("exceptionDetails").is_some() || result["result"]["value"].as_bool().is_none() {
        return Err(failure("BROWSER_RESTORE_FAILED"));
    }
    Ok(())
}

fn bootstrap_page(value: &str) -> bool {
    if value == "about:blank" {
        return true;
    }
    url::Url::parse(value).is_ok_and(|url| {
        url.scheme() == "https"
            && url.port_or_known_default() == Some(443)
            && url.username().is_empty()
            && url.password().is_none()
            && matches!(
                url.host_str(),
                Some("login.microsoftonline.com" | "login.live.com" | "login.microsoft.com")
            )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use serde_json::Value;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn restoration_bootstraps_only_blank_signin_or_empty_owned_browser_without_duplicate_teams()
     {
        let cases = [
            (
                vec!["https://teams.microsoft.com/v2/?tenantId=existing"],
                vec!["Target.getTargets"],
            ),
            (
                vec![
                    "https://teams.microsoft.com/",
                    "https://teams.cloud.microsoft/",
                ],
                vec!["Target.getTargets"],
            ),
            (vec!["https://example.test/"], vec!["Target.getTargets"]),
            (
                vec!["https://login.microsoftonline.com.evil.test/"],
                vec!["Target.getTargets"],
            ),
            (
                vec!["about:blank"],
                vec![
                    "Target.getTargets",
                    "Target.attachToTarget",
                    "Runtime.evaluate",
                    "Target.detachFromTarget",
                ],
            ),
            (
                vec!["https://login.microsoftonline.com/organizations/oauth2/v2.0/authorize"],
                vec![
                    "Target.getTargets",
                    "Target.attachToTarget",
                    "Runtime.evaluate",
                    "Target.detachFromTarget",
                ],
            ),
            (vec![], vec!["Target.getTargets", "Target.createTarget"]),
        ];
        for (urls, expected) in cases {
            let targets:Vec<_>=urls.iter().enumerate().map(|(index,url)|json!({"type":"page","targetId":format!("target-{index}"),"url":url})).collect();
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!(
                "ws://{}/devtools/browser/fixture",
                listener.local_addr().unwrap()
            );
            let server = tokio::spawn(async move {
                let (socket, _) = listener.accept().await.unwrap();
                let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
                let mut requests = Vec::new();
                while let Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) =
                    ws.next().await
                {
                    let request: Value = serde_json::from_str(&text).unwrap();
                    let result = match request["method"].as_str().unwrap() {
                        "Target.getTargets" => json!({"targetInfos":targets}),
                        "Target.attachToTarget" => json!({"sessionId":"fixture-session"}),
                        "Runtime.evaluate" => json!({"result":{"value":true}}),
                        _ => json!({}),
                    };
                    ws.send(tokio_tungstenite::tungstenite::Message::Text(
                        json!({"id":request["id"],"result":result})
                            .to_string()
                            .into(),
                    ))
                    .await
                    .unwrap();
                    requests.push(request);
                }
                requests
            });
            let url = "https://teams.microsoft.com/v2/?tenantId=22222222-2222-4222-8222-222222222222&login_hint=guest%40example.test";
            prepare(&endpoint, url).await.unwrap();
            let requests = server.await.unwrap();
            let mut methods = requests
                .iter()
                .map(|request| request["method"].as_str().unwrap())
                .collect::<Vec<_>>();
            methods.dedup();
            assert_eq!(methods, expected);
            for request in requests
                .iter()
                .filter(|request| matches!(request["method"].as_str(), Some("Target.createTarget")))
            {
                assert_eq!(request["params"]["url"], url);
            }
        }
    }

    #[tokio::test]
    async fn early_empty_targets_wait_for_restored_teams_and_foreign_redirect_is_guarded_atomically()
     {
        for early_empty in [true, false] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!(
                "ws://{}/devtools/browser/fixture",
                listener.local_addr().unwrap()
            );
            let server = tokio::spawn(async move {
                let (socket, _) = listener.accept().await.unwrap();
                let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
                let mut requests = Vec::new();
                let mut probes = 0;
                while let Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) =
                    ws.next().await
                {
                    let request: Value = serde_json::from_str(&text).unwrap();
                    let result = match request["method"].as_str().unwrap() {
                        "Target.getTargets" => {
                            probes += 1;
                            let targets = if early_empty && probes == 1 {
                                json!([])
                            } else {
                                json!([{"type":"page","targetId":"restored","url":if early_empty {"https://teams.microsoft.com/v2/"}else{"https://login.microsoftonline.com/"}}])
                            };
                            json!({"targetInfos":targets})
                        }
                        "Target.attachToTarget" => json!({"sessionId":"s"}),
                        "Runtime.evaluate" => json!({"result":{"value":false}}),
                        _ => json!({}),
                    };
                    ws.send(tokio_tungstenite::tungstenite::Message::Text(
                        json!({"id":request["id"],"result":result})
                            .to_string()
                            .into(),
                    ))
                    .await
                    .unwrap();
                    requests.push(request);
                }
                requests
            });
            prepare(&endpoint, "https://teams.microsoft.com/v2/")
                .await
                .unwrap();
            let requests = server.await.unwrap();
            let methods: Vec<_> = requests
                .iter()
                .map(|r| r["method"].as_str().unwrap())
                .collect();
            if early_empty {
                assert_eq!(methods, vec!["Target.getTargets", "Target.getTargets"]);
            } else {
                assert_eq!(
                    methods,
                    vec![
                        "Target.getTargets",
                        "Target.attachToTarget",
                        "Runtime.evaluate",
                        "Target.detachFromTarget"
                    ]
                );
                assert!(
                    requests[2]["params"]["expression"]
                        .as_str()
                        .unwrap()
                        .contains("location.origin")
                );
            }
        }
    }
}

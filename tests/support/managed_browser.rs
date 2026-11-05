use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{fs, path::PathBuf, time::Duration};
use tokio::{net::TcpListener, sync::Notify};

pub async fn run() {
    let Some(profile) = std::env::var_os("TEAMS_FAKE_BROWSER_PROFILE").map(PathBuf::from) else {
        return;
    };
    use std::io::Write;
    #[cfg(unix)]
    if profile.join("fixture-await-startup-signal").exists()
        || profile.join("fixture-invalid-port").exists()
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
        fs::write(profile.join("SingletonLock"), "owned fixture").unwrap();
        fs::write(profile.join("fixture-startup-ready"), "ready").unwrap();
        if profile.join("fixture-invalid-port").exists() {
            fs::write(profile.join("DevToolsActivePort"), "x".repeat(5000)).unwrap();
        }
        let _ = tokio::time::timeout(Duration::from_secs(10), terminate.recv()).await;
        fs::remove_file(profile.join("SingletonLock")).unwrap();
        let _ = fs::remove_file(profile.join("DevToolsActivePort"));
        fs::write(profile.join("fixture-closed-gracefully"), "closed").unwrap();
        return;
    }
    if profile.join("fixture-start-delay").exists() {
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    fs::create_dir_all(profile.join("Default")).unwrap();
    writeln!(
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(profile.join("fixture-launches"))
            .unwrap(),
        "launched"
    )
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    fs::write(
        profile.join("DevToolsActivePort"),
        format!(
            "{}\n/devtools/browser/{}\n",
            listener.local_addr().unwrap().port(),
            uuid::Uuid::new_v4()
        ),
    )
    .unwrap();
    let closed = std::sync::Arc::new(Notify::new());
    loop {
        tokio::select! {
            _=closed.notified()=>break,
            connection=listener.accept()=>{
                let (socket,_)=connection.unwrap();let closed=closed.clone();let profile=profile.clone();
                tokio::spawn(async move {
                    let Ok(mut ws)=tokio_tungstenite::accept_async(socket).await else{return};
                    while let Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text)))=ws.next().await {
                        let request:Value=serde_json::from_str(&text).unwrap();
                        if request["method"] == "Runtime.callFunctionOn" && profile.join("fixture-hold-requests").exists() {
                            fs::write(profile.join("fixture-request-started"), "started").unwrap();
                            while profile.join("fixture-hold-requests").exists() {
                                tokio::time::sleep(Duration::from_millis(25)).await;
                            }
                        }
                        let stop=request["method"]=="Browser.close";
                        let response=fs::read_to_string(profile.join("fixture-bridge-response")).ok().and_then(|text|serde_json::from_str::<Value>(&text).ok());
                        let record=|name:&str,line:&str|{let _=writeln!(fs::OpenOptions::new().create(true).append(true).open(profile.join(name)).unwrap(),"{line}");};
                        let cleared=profile.join("fixture-cache-cleared").exists();
                        let result=match request["method"].as_str() {
                            Some("Page.setWebLifecycleState")=>{record("fixture-lifecycle",request["params"]["state"].as_str().unwrap_or(""));json!({})}
                            Some("Storage.getUsageAndQuota")=>json!({"usage":1,"quota":1,"usageBreakdown":[{"storageType":"cache_storage","usage":if cleared {0} else {524_288_000}},{"storageType":"indexeddb","usage":1000}]}),
                            Some("Storage.clearDataForOrigin")=>{record("fixture-cache-cleared",&format!("{} {}",request["params"]["origin"].as_str().unwrap_or(""),request["params"]["storageTypes"].as_str().unwrap_or("")));json!({})}
                            Some("Target.getTargets")=>json!({"targetInfos":response.as_ref().and_then(|value|value.get("_targets")).cloned().unwrap_or_else(||json!([{"targetId":"fixture-page","type":"page","url":"https://teams.microsoft.com/v2/"}]))}),
                            Some("Target.attachToTarget")=>json!({"sessionId":"fixture-session"}),
                            Some("Page.getFrameTree")=>json!({"frameTree":{"frame":{"id":"fixture-frame"}}}),
                            Some("Page.createIsolatedWorld")=>json!({"executionContextId":1}),
                            Some("Runtime.evaluate")=>json!({"result":{"objectId":"fixture-bridge"}}),
                            Some("Runtime.callFunctionOn")=>json!({"result":{"value":response.unwrap_or_else(||json!({"ok":false,"error":{"code":"ACCESS_DENIED","message":"Synthetic account access is restricted."}}))}}),
                            _=>json!({})};
                        ws.send(tokio_tungstenite::tungstenite::Message::Text(json!({"id":request["id"],"result":result}).to_string().into())).await.unwrap();
                        if stop {closed.notify_one();break;}
                    }
                });
            }
        }
    }
    fs::remove_file(profile.join("DevToolsActivePort")).unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    fs::write(profile.join("fixture-closed-gracefully"), "closed").unwrap();
}

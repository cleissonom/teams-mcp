use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    net::{TcpListener, TcpStream},
    process::{ChildStdin, ChildStdout, Command},
    sync::oneshot,
    time::timeout,
};
use tokio_tungstenite::{WebSocketStream, accept_async, tungstenite::Message};

#[tokio::test]
async fn mcp_cancellation_drops_pending_browser_work_and_allows_the_next_request() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!(
        "ws://{}/devtools/browser/cancellation",
        listener.local_addr().unwrap()
    );
    let (started, pending) = oneshot::channel();
    let (closed, closure) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut first = accept_async(socket).await.unwrap();
        reach_evaluation(&mut first).await;
        started.send(()).unwrap();
        assert!(
            matches!(
                first.next().await,
                None | Some(Err(_)) | Some(Ok(Message::Close(_)))
            ),
            "cancelled operation must close its pending CDP socket"
        );
        closed.send(()).unwrap();
        let (socket, _) = listener.accept().await.unwrap();
        let mut second = accept_async(socket).await.unwrap();
        let call = reach_evaluation(&mut second).await;
        reply(&mut second,&call,json!({"result":{"value":{"ok":true,"profile":{"email":"fixture@example.test"},"capabilities":{"search":true}}}})).await;
        let detach = receive(&mut second).await;
        assert_eq!(detach["method"], "Target.detachFromTarget");
        reply(&mut second, &detach, json!({})).await;
    });

    let mut child = Command::new(env!("CARGO_BIN_EXE_teams-mcp"))
        .env("TEAMS_AUTH_SOURCE", "browser")
        .env("TEAMS_CDP_URL", endpoint)
        .env_remove("TEAMS_TARGET_ID")
        .env_remove("TEAMS_ACCOUNT")
        .env_remove("TEAMS_TENANT_ID")
        .env_remove("TEAMS_APP_WINDOW_ID")
        .env_remove("TEAMS_APP_CDP_URL")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap()).lines();
    send(&mut input,json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"cancellation-test","version":"1"}}})).await;
    let initialized = timeout(Duration::from_secs(3), response(&mut output, 1))
        .await
        .unwrap();
    assert!(initialized.get("result").is_some());
    send(
        &mut input,
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    )
    .await;
    send(&mut input, tool_call(2)).await;
    timeout(Duration::from_secs(3), pending)
        .await
        .expect("tool request must reach Runtime.evaluate")
        .unwrap();

    send(&mut input,json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":2,"reason":"fixture cancellation"}})).await;
    timeout(Duration::from_secs(3),closure).await.expect("MCP cancellation must promptly drop the pending CDP socket, rather than wait for the 65-second browser deadline").unwrap();

    send(&mut input, tool_call(3)).await;
    let result = timeout(Duration::from_secs(3), response(&mut output, 3))
        .await
        .expect("next request must reconnect and complete promptly");
    assert_ne!(result["result"]["isError"], true);
    let data: Value =
        serde_json::from_str(result["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(data["profile"]["email"], "fixture@example.test");
    assert_eq!(data["success"], true);
    timeout(Duration::from_secs(3), server)
        .await
        .unwrap()
        .unwrap();
    drop(input);
    timeout(Duration::from_secs(3), child.wait())
        .await
        .unwrap()
        .unwrap();
}

fn tool_call(id: u64) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"teams_status","arguments":{}}})
}

async fn send(input: &mut ChildStdin, value: Value) {
    input
        .write_all(format!("{value}\n").as_bytes())
        .await
        .unwrap();
}

async fn response(output: &mut Lines<BufReader<ChildStdout>>, id: u64) -> Value {
    loop {
        let line = output
            .next_line()
            .await
            .unwrap()
            .expect("MCP stdout ended unexpectedly");
        let value: Value =
            serde_json::from_str(&line).expect("stdout must contain only JSON-RPC messages");
        if value["id"] == id {
            return value;
        }
    }
}

async fn receive(socket: &mut WebSocketStream<TcpStream>) -> Value {
    let Some(Ok(Message::Text(text))) = socket.next().await else {
        panic!("expected a CDP request");
    };
    serde_json::from_str(&text).unwrap()
}

async fn reply(socket: &mut WebSocketStream<TcpStream>, call: &Value, result: Value) {
    socket
        .send(Message::Text(
            json!({"id":call["id"],"result":result}).to_string().into(),
        ))
        .await
        .unwrap();
}

async fn reach_evaluation(socket: &mut WebSocketStream<TcpStream>) -> Value {
    for (method, result) in [
        (
            "Target.getTargets",
            json!({"targetInfos":[{"targetId":"teams","type":"page","url":"https://teams.microsoft.com/v2/"}]}),
        ),
        ("Target.attachToTarget", json!({"sessionId":"attached"})),
        (
            "Page.getFrameTree",
            json!({"frameTree":{"frame":{"id":"root"}}}),
        ),
        ("Page.createIsolatedWorld", json!({"executionContextId":42})),
    ] {
        let call = receive(socket).await;
        assert_eq!(call["method"], method);
        reply(socket, &call, result).await;
    }
    let call = receive(socket).await;
    assert_eq!(call["method"], "Runtime.evaluate");
    call
}

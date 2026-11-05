use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::time::Duration;
use teams_mcp::{
    config::{AuthSource, Config, Transport},
    server::TeamsServer,
};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot},
    task::JoinHandle,
    time::timeout,
};
use tokio_tungstenite::{WebSocketStream, accept_async, tungstenite::Message};

#[tokio::test]
async fn page_session_recheck_keeps_a_hidden_target_used_by_another_read() {
    let mut fixture = HiddenFixture::new(Transport::Page).await;
    let read = fixture.read();
    timeout(Duration::from_secs(3), &mut fixture.held)
        .await
        .unwrap()
        .unwrap();
    let status = fixture.status();
    let result = timeout(Duration::from_secs(3), status)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(result["authenticated"], true);
    assert!(
        fixture.closed.try_recv().is_err(),
        "session recheck closed a target with an active read"
    );
    fixture.release.send(()).unwrap();
    assert_eq!(
        read.await.unwrap().unwrap()["profile"]["id"],
        "fixture-user"
    );
    drop(fixture.server);
    fixture.peer.await.unwrap();
}

#[tokio::test]
async fn lease_session_recheck_waits_for_an_active_hidden_target_mint() {
    let mut fixture = HiddenFixture::new(Transport::Lease).await;
    let read = fixture.read();
    timeout(Duration::from_secs(3), &mut fixture.held)
        .await
        .unwrap()
        .unwrap();
    let status = fixture.status();
    assert!(
        timeout(Duration::from_millis(250), fixture.closed.recv())
            .await
            .is_err(),
        "session recheck closed a target while another call was minting its lease"
    );
    fixture.release.send(()).unwrap();
    assert_eq!(
        timeout(Duration::from_secs(3), read)
            .await
            .unwrap()
            .unwrap()
            .unwrap()["profile"]["id"],
        "fixture-user"
    );
    assert_eq!(
        timeout(Duration::from_secs(3), status)
            .await
            .unwrap()
            .unwrap()
            .unwrap()["authenticated"],
        true
    );
    drop(fixture.server);
    fixture.peer.await.unwrap();
}

struct HiddenFixture {
    server: TeamsServer,
    held: oneshot::Receiver<()>,
    release: oneshot::Sender<()>,
    closed: mpsc::UnboundedReceiver<()>,
    peer: JoinHandle<()>,
}

impl HiddenFixture {
    async fn new(transport: Transport) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!(
            "ws://{}/devtools/browser/hidden-recovery",
            listener.local_addr().unwrap()
        );
        let (held_send, held) = oneshot::channel();
        let (release, release_receive) = oneshot::channel();
        let (closed_send, closed) = mpsc::unbounded_channel();
        let peer = tokio::spawn(hidden_peer(
            listener,
            held_send,
            release_receive,
            closed_send,
        ));
        let server = TeamsServer::new(Config {
            auth_source: AuthSource::Browser,
            cdp_url: Some(endpoint),
            hidden_tab: true,
            transport,
            request_timeout: Duration::from_secs(5),
            ..Config::default()
        });
        Self {
            server,
            held,
            release,
            closed,
            peer,
        }
    }

    fn read(&self) -> JoinHandle<Result<Value, teams_mcp::error::Failure>> {
        let server = self.server.clone();
        tokio::spawn(async move { server.execute("teams_get_me", json!({})).await })
    }

    fn status(&self) -> JoinHandle<Result<Value, teams_mcp::error::Failure>> {
        let server = self.server.clone();
        tokio::spawn(async move { server.execute("teams_status", json!({})).await })
    }
}

async fn hidden_peer(
    listener: TcpListener,
    held_send: oneshot::Sender<()>,
    mut release: oneshot::Receiver<()>,
    closed: mpsc::UnboundedSender<()>,
) {
    let (stream, _) = listener.accept().await.unwrap();
    let mut socket = accept_async(stream).await.unwrap();
    let mut held_send = Some(held_send);
    let mut pending = None;
    loop {
        tokio::select! {
            _ = &mut release, if pending.is_some() => {
                let call = pending.take().unwrap();
                reply(&mut socket, &call, json!({"result":{"value":snapshot()}})).await;
            }
            message = socket.next() => {
                let Some(Ok(Message::Text(text))) = message else { break; };
                let call: Value = serde_json::from_str(&text).unwrap();
                if call["method"] == "Runtime.callFunctionOn" && held_send.is_some() {
                    pending = Some(call);
                    held_send.take().unwrap().send(()).unwrap();
                    continue;
                }
                if call["method"] == "Target.closeTarget" && pending.is_some() {
                    closed.send(()).unwrap();
                }
                let result = standard_result(&call);
                reply(&mut socket, &call, result).await;
            }
        }
    }
}

fn standard_result(call: &Value) -> Value {
    match call["method"].as_str().unwrap() {
        "Target.createTarget" => json!({"targetId":"hidden"}),
        "Target.getTargetInfo" => {
            json!({"targetInfo":{"targetId":"hidden","type":"other","url":"https://teams.microsoft.com/v2/"}})
        }
        "Target.attachToTarget" => json!({"sessionId":"hidden-session"}),
        "Page.getFrameTree" => json!({"frameTree":{"frame":{"id":"root"}}}),
        "Page.createIsolatedWorld" => json!({"executionContextId":42}),
        "Runtime.evaluate" => json!({"result":{"objectId":"bridge"}}),
        "Runtime.callFunctionOn" => json!({"result":{"value":snapshot()}}),
        "Target.closeTarget" => json!({"success":true}),
        "Target.detachFromTarget" => json!({}),
        other => panic!("unexpected synthetic CDP method: {other}"),
    }
}

fn snapshot() -> Value {
    json!({"ok":true,"profile":{"id":"fixture-user","email":"fixture@example.test","tenantId":"11111111-2222-4333-8444-555555555555"},"capabilities":{"chat":true},"lease":{"spaces":{"token":"synthetic-spaces","expiresAt":4102444800u64}}})
}

async fn reply(socket: &mut WebSocketStream<TcpStream>, call: &Value, result: Value) {
    socket
        .send(Message::Text(
            json!({"id":call["id"],"result":result}).to_string().into(),
        ))
        .await
        .unwrap();
}

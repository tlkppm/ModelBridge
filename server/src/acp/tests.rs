use std::future::Future;

use serde_json::json;
use tokio::io::{
    duplex, split, AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines, ReadHalf,
    WriteHalf,
};
use tokio::time::timeout;

use super::*;

struct Peer {
    incoming: Lines<BufReader<ReadHalf<DuplexStream>>>,
    outgoing: WriteHalf<DuplexStream>,
}

impl Peer {
    fn streams() -> (Self, ReadHalf<DuplexStream>, WriteHalf<DuplexStream>) {
        let (client, agent) = duplex(4096);
        let (reader, writer) = split(client);
        let (incoming, outgoing) = split(agent);
        (
            Self {
                incoming: BufReader::new(incoming).lines(),
                outgoing,
            },
            reader,
            writer,
        )
    }

    async fn receive(&mut self) -> Value {
        serde_json::from_str(&self.incoming.next_line().await.unwrap().unwrap()).unwrap()
    }

    async fn send(&mut self, message: Value) {
        self.send_batch(&[message]).await;
    }

    async fn send_batch(&mut self, messages: &[Value]) {
        let mut bytes = Vec::new();
        for message in messages {
            serde_json::to_writer(&mut bytes, message).unwrap();
            bytes.push(b'\n');
        }
        self.outgoing.write_all(&bytes).await.unwrap();
    }

    async fn setup(&mut self, cwd: &Path, session_id: &str) {
        assert_eq!(
            self.receive().await,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": 1, "clientCapabilities": {} } })
        );
        self.send(response(1, json!({ "protocolVersion": 1 })))
            .await;
        assert_eq!(
            self.receive().await,
            json!({ "jsonrpc": "2.0", "id": 2, "method": "session/new", "params": { "cwd": cwd.to_str().unwrap(), "mcpServers": [] } })
        );
        self.send(response(2, json!({ "sessionId": session_id })))
            .await;
    }

    async fn expect_prompt(&mut self, id: u64, session_id: &str, text: &str) {
        assert_eq!(
            self.receive().await,
            json!({
                "jsonrpc": "2.0", "id": id, "method": "session/prompt",
                "params": { "sessionId": session_id, "prompt": [{ "type": "text", "text": text }] }
            })
        );
    }
}

async fn run(client: impl Future<Output = ()>, peer: impl Future<Output = ()>) {
    timeout(Duration::from_secs(5), async {
        tokio::join!(client, peer);
    })
    .await
    .unwrap();
}

fn response(id: u64, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn stop(id: u64, reason: &str) -> Value {
    response(id, json!({ "stopReason": reason }))
}

fn update(session_id: &str, kind: &str, text: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": session_id,
            "update": { "sessionUpdate": kind, "content": { "type": "text", "text": text } }
        }
    })
}

fn text(session_id: &str, chunk: &str) -> Value {
    update(session_id, "agent_message_chunk", chunk)
}

#[tokio::test]
async fn reuses_session_and_filters_updates_between_turns() {
    let cwd = std::env::current_dir().unwrap();
    let (mut peer, reader, writer) = Peer::streams();
    let cancellation = CancellationToken::new();
    run(
        async {
            let mut client = AcpClient::connect(reader, writer, &cwd).await.unwrap();
            let oversized = "\n".repeat(MAX_FRAME_BYTES / 2);
            assert!(matches!(
                client.prompt(&oversized, &cancellation, |_| {}).await,
                Err(AcpError::FrameTooLarge)
            ));
            let mut chunks = Vec::new();
            assert_eq!(
                client
                    .prompt("第一轮\nline two", &cancellation, |text| chunks
                        .push(text.to_owned()))
                    .await
                    .unwrap(),
                StopReason::EndTurn
            );
            assert_eq!(chunks, ["hello", " world"]);
            chunks.clear();
            assert_eq!(
                client
                    .prompt("second", &cancellation, |text| chunks.push(text.to_owned()))
                    .await
                    .unwrap(),
                StopReason::MaxTokens
            );
            assert_eq!(chunks, ["fresh"]);
        },
        async {
            peer.setup(&cwd, "session").await;
            peer.expect_prompt(3, "session", "第一轮\nline two").await;
            peer.send_batch(&[
                stop(100, "refusal"),
                text("unrelated", "foreign"),
                update("session", "agent_thought_chunk", "private thought"),
                text("session", "hello"),
                text("session", " world"),
                stop(3, "end_turn"),
                text("session", "late"),
                stop(4, "refusal"),
            ])
            .await;
            peer.expect_prompt(4, "session", "second").await;
            peer.send_batch(&[text("session", "fresh"), stop(4, "max_tokens")])
                .await;
        },
    )
    .await;
}

#[tokio::test]
async fn independent_sessions_run_concurrently() {
    let cwd = std::env::current_dir().unwrap();
    let barrier = tokio::sync::Barrier::new(2);
    let exchange = |session_id, other_session_id| {
        let cwd = &cwd;
        let barrier = &barrier;
        async move {
            let (mut peer, reader, writer) = Peer::streams();
            run(
                async {
                    let mut client = AcpClient::connect(reader, writer, cwd).await.unwrap();
                    let mut output = String::new();
                    assert_eq!(
                        client
                            .prompt("prompt", &CancellationToken::new(), |text| output
                                .push_str(text))
                            .await
                            .unwrap(),
                        StopReason::MaxTurnRequests
                    );
                    assert_eq!(output, session_id);
                },
                async {
                    peer.setup(cwd, session_id).await;
                    peer.expect_prompt(3, session_id, "prompt").await;
                    barrier.wait().await;
                    peer.send_batch(&[
                        text(other_session_id, "foreign"),
                        text(session_id, session_id),
                        stop(3, "max_turn_requests"),
                    ])
                    .await;
                },
            )
            .await;
        }
    };
    tokio::join!(
        exchange("session-a", "session-b"),
        exchange("session-b", "session-a")
    );
}

#[tokio::test]
async fn denies_permissions_and_cancels_only_the_active_turn() {
    let cwd = std::env::current_dir().unwrap();
    let (mut peer, reader, writer) = Peer::streams();
    let cancellation = CancellationToken::new();
    let deny = |id: Value, session_id| {
        let request = json!({
            "jsonrpc": "2.0", "id": id, "method": "session/request_permission",
            "params": {
                "sessionId": session_id,
                "toolCall": { "toolCallId": "tool" },
                "options": [{ "optionId": "allow", "kind": "allow_always", "name": "Allow" }]
            }
        });
        let reply = json!({ "jsonrpc": "2.0", "id": id, "result": { "outcome": { "outcome": "cancelled" } } });
        (request, reply)
    };
    run(
        async {
            let mut client = AcpClient::connect(reader, writer, &cwd).await.unwrap();
            let cancelled = CancellationToken::new();
            cancelled.cancel();
            assert_eq!(client.prompt("skipped", &cancelled, |_| panic!()).await.unwrap(), StopReason::Cancelled);
            assert_eq!(client.prompt("cancel me", &cancellation, |_| panic!()).await.unwrap(), StopReason::Cancelled);
            assert_eq!(client.prompt("retry", &CancellationToken::new(), |_| panic!()).await.unwrap(), StopReason::Refusal);
        },
        async {
            peer.setup(&cwd, "session").await;
            peer.expect_prompt(3, "session", "cancel me").await;
            let (permission, expected) = deny(json!(3), "session");
            peer.send(permission).await;
            assert_eq!(peer.receive().await, expected);
            for method in ["fs/read_text_file", "fs/write_text_file", "terminal/create"] {
                peer.send(json!({ "jsonrpc": "2.0", "id": "unsupported", "method": method, "params": {} })).await;
                assert_eq!(peer.receive().await, json!({ "jsonrpc": "2.0", "id": "unsupported", "error": { "code": -32601, "message": "Method not found" } }));
            }
            cancellation.cancel();
            assert_eq!(peer.receive().await, json!({ "jsonrpc": "2.0", "method": "session/cancel", "params": { "sessionId": "session" } }));
            let (permission, expected) = deny(json!("pending"), "other-session");
            peer.send_batch(&[text("session", "after cancel"), permission]).await;
            assert_eq!(peer.receive().await, expected);
            peer.send(stop(3, "cancelled")).await;
            peer.expect_prompt(4, "session", "retry").await;
            peer.send(stop(4, "refusal")).await;
        },
    ).await;
}

#[tokio::test]
async fn rejects_incompatible_protocol_before_creating_session() {
    let cwd = std::env::current_dir().unwrap();
    let (mut peer, reader, writer) = Peer::streams();
    run(
        async {
            assert!(matches!(
                AcpClient::connect(reader, writer, &cwd).await,
                Err(AcpError::UnsupportedVersion(2))
            ));
        },
        async {
            assert_eq!(peer.receive().await["method"], "initialize");
            peer.send(response(1, json!({ "protocolVersion": 2 })))
                .await;
            assert!(peer.incoming.next_line().await.unwrap().is_none());
        },
    )
    .await;
}

#[tokio::test]
async fn disconnects_after_eof_and_rejects_truncated_frames() {
    for tail in [&b""[..], &b"{"[..]] {
        let cwd = std::env::current_dir().unwrap();
        let (mut peer, reader, writer) = Peer::streams();
        let cancellation = CancellationToken::new();
        run(
            async {
                let mut client = AcpClient::connect(reader, writer, &cwd).await.unwrap();
                let error = client
                    .prompt("first", &cancellation, |_| panic!())
                    .await
                    .unwrap_err();
                assert!(if tail.is_empty() {
                    matches!(error, AcpError::Eof)
                } else {
                    matches!(error, AcpError::Protocol("truncated frame"))
                });
                assert!(matches!(
                    client.prompt("second", &cancellation, |_| panic!()).await,
                    Err(AcpError::Disconnected)
                ));
            },
            async {
                peer.setup(&cwd, "session").await;
                peer.expect_prompt(3, "session", "first").await;
                peer.outgoing.write_all(tail).await.unwrap();
                drop(peer);
            },
        )
        .await;
    }
}

#[tokio::test]
async fn rejects_oversized_incoming_frame_without_waiting_for_newline() {
    let cwd = std::env::current_dir().unwrap();
    let (mut peer, reader, writer) = Peer::streams();
    run(
        async {
            assert!(matches!(
                AcpClient::connect(reader, writer, &cwd).await,
                Err(AcpError::FrameTooLarge)
            ));
        },
        async {
            assert_eq!(peer.receive().await["method"], "initialize");
            peer.outgoing
                .write_all(&vec![b' '; MAX_FRAME_BYTES + 1])
                .await
                .unwrap();
        },
    )
    .await;
}

#[tokio::test]
async fn uncompleted_idle_frame_respects_deadline() {
    let (mut peer, reader, writer) = Peer::streams();
    let mut transport = Transport::new(reader, writer);
    peer.outgoing.write_all(b"{").await.unwrap();
    assert!(matches!(
        transport
            .drain_idle(Instant::now() + Duration::from_millis(20))
            .await,
        Err(AcpError::Timeout)
    ));
}

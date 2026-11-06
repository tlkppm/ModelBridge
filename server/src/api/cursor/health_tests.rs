use std::time::Duration;

use futures_util::StreamExt;
use tower::ServiceExt;

use super::*;

async fn setup_health() -> (TransportRegistry, TestUpstream, tempfile::TempDir, Router) {
    let (registry, upstream) = setup().await;
    let directory = tempfile::tempdir().unwrap();
    let gateway = router_with_proxy(
        registry.clone(),
        upstream.proxy.clone(),
        knowledge::KnowledgeService::with_root(directory.path().join("rules")).unwrap(),
    );
    (registry, upstream, directory, gateway)
}

async fn wait_health_closed(registry: &TransportRegistry, request_id: &str) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while registry.health(request_id).await {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

fn health_append(request_id: &str, payload: Vec<u8>, seqno: i64, binary: bool) -> Request<Body> {
    let wire = if binary {
        binary_append_wire(request_id, payload, seqno)
    } else {
        append_wire(request_id, payload, seqno)
    };
    append_request(wire)
}

#[tokio::test]
async fn health_diagnostic_does_not_depend_on_official_upstream() {
    let (registry, mut upstream, _directory, gateway) = setup_health().await;
    let response = gateway
        .oneshot(health_stream_request("local-health"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(upstream.requests.try_recv().is_err());
    drop(response);
    wait_health_closed(&registry, "local-health").await;
    registry.shutdown().await;
}

#[tokio::test]
async fn health_diagnostic_echoes_five_live_round_trips_over_http() {
    let (registry, mut upstream, _directory, gateway) = setup_health().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut tasks = tokio::task::JoinSet::new();
    tasks.spawn(async move { axum::serve(listener, gateway).await.unwrap() });
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    for binary in [false, true] {
        let request_id = format!("health-live-{binary}");
        let response = client
            .post(format!(
                "http://{address}/aiserver.v1.HealthService/StreamBidiSSE"
            ))
            .header(header::CONTENT_TYPE, "application/proto")
            .body(
                ai::BidiRequestId {
                    request_id: request_id.clone(),
                }
                .encode_to_vec(),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/event-stream"
        );
        let mut stream = response.bytes_stream();
        assert!(
            tokio::time::timeout(Duration::from_millis(30), stream.next())
                .await
                .is_err()
        );
        for seqno in 0..5 {
            let message = HealthMessage {
                payload: format!("echo-{seqno}"),
            };
            let request = health_append(&request_id, message.encode_to_vec(), seqno, binary);
            let wire = to_bytes(request.into_body(), 4096).await.unwrap();
            let response = client
                .post(format!(
                    "http://{address}/aiserver.v1.BidiService/BidiAppend"
                ))
                .header(header::CONTENT_TYPE, "application/proto")
                .body(wire)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let expected = connect::encode_message(&message).unwrap();
            let mut actual = Vec::new();
            while actual.len() < expected.len() {
                let chunk = tokio::time::timeout(Duration::from_secs(2), stream.next())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
                actual.extend_from_slice(&chunk);
            }
            assert_eq!(actual.as_slice(), expected.as_ref());
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(30), stream.next())
                .await
                .is_err()
        );
        drop(stream);
        wait_health_closed(&registry, &request_id).await;
        assert!(registry.local(&request_id).await.is_none());
        assert!(!registry.upstream(&request_id).await);
    }
    assert!(upstream.requests.try_recv().is_err());
    registry.shutdown().await;
}

#[tokio::test]
async fn health_diagnostic_orders_appends_and_ignores_retries() {
    let (registry, mut upstream, _directory, gateway) = setup_health().await;
    let response = gateway
        .clone()
        .oneshot(health_stream_request("ordered-health"))
        .await
        .unwrap();
    let mut stream = response.into_body().into_data_stream();
    for seqno in [2, 0, 1, 1] {
        let response = gateway
            .clone()
            .oneshot(health_append(
                "ordered-health",
                HealthMessage {
                    payload: format!("echo-{seqno}"),
                }
                .encode_to_vec(),
                seqno,
                true,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        if seqno == 2 {
            assert!(
                tokio::time::timeout(Duration::from_millis(30), stream.next())
                    .await
                    .is_err()
            );
        }
    }
    for seqno in 0..3 {
        let frame = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            frame,
            connect::encode_message(&HealthMessage {
                payload: format!("echo-{seqno}")
            })
            .unwrap()
        );
    }
    for (payload, seqno) in [
        (vec![0x0e], 3),
        (
            HealthMessage {
                payload: "invalid-seqno".into(),
            }
            .encode_to_vec(),
            -1,
        ),
    ] {
        let response = gateway
            .clone()
            .oneshot(health_append("ordered-health", payload, seqno, true))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(30), stream.next())
            .await
            .is_err()
    );
    assert!(upstream.requests.try_recv().is_err());
    drop(stream);
    wait_health_closed(&registry, "ordered-health").await;
    registry.shutdown().await;
}

#[derive(Clone, PartialEq, Message)]
struct SimulatedHealthRequest {
    #[prost(string, tag = "1")]
    payload: String,
    #[prost(bool, optional, tag = "2")]
    simulate_error: Option<bool>,
}

#[tokio::test]
async fn health_diagnostic_preserves_simulated_errors_instead_of_reporting_success() {
    let (registry, mut upstream, _directory, gateway) = setup_health().await;
    let response = gateway
        .clone()
        .oneshot(health_stream_request("error-health"))
        .await
        .unwrap();
    let mut stream = response.into_body().into_data_stream();
    let response = gateway
        .oneshot(health_append(
            "error-health",
            SimulatedHealthRequest {
                payload: "not echoed".into(),
                simulate_error: Some(true),
            }
            .encode_to_vec(),
            0,
            true,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let frame = stream.next().await.unwrap().unwrap();
    let (flags, payload) = connect::decode_frames(&frame).unwrap().pop().unwrap();
    assert_eq!(flags, connect::END_STREAM_FLAG);
    let error: serde_json::Value = serde_json::from_slice(&payload).unwrap();
    assert_eq!(error["error"]["code"], "internal");
    assert!(stream.next().await.is_none());
    wait_health_closed(&registry, "error-health").await;
    assert!(upstream.requests.try_recv().is_err());
    registry.shutdown().await;
}

#[tokio::test]
async fn health_diagnostic_does_not_take_over_agent_or_inference_transports() {
    let (registry, mut upstream, _directory, gateway) = setup_health().await;
    registry.get_or_create("agent-owned").await.unwrap();
    registry.mark_upstream("inference-owned").await;
    for request_id in ["", "agent-owned", "inference-owned"] {
        let response = gateway
            .clone()
            .oneshot(health_stream_request(request_id))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    let response = gateway
        .clone()
        .oneshot(health_stream_request("health-owned"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let duplicate = gateway
        .clone()
        .oneshot(health_stream_request("health-owned"))
        .await
        .unwrap();
    assert_eq!(duplicate.status(), StatusCode::BAD_REQUEST);
    let inference = gateway
        .clone()
        .oneshot(inference_stream_request("health-owned"))
        .await
        .unwrap();
    assert_eq!(inference.status(), StatusCode::BAD_REQUEST);
    let agent = gateway
        .oneshot(
            Request::post("/agent.v1.AgentService/RunSSE")
                .header(header::CONTENT_TYPE, "application/proto")
                .body(Body::from(
                    ai::BidiRequestId {
                        request_id: "health-owned".into(),
                    }
                    .encode_to_vec(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(agent.status(), StatusCode::BAD_REQUEST);
    assert!(registry.health("health-owned").await);
    assert!(upstream.requests.try_recv().is_err());
    drop(response);
    wait_health_closed(&registry, "health-owned").await;
    registry.shutdown().await;
}

#[tokio::test]
async fn health_diagnostic_times_out_without_inventing_a_reply() {
    let (registry, mut upstream, _directory, gateway) = setup_health().await;
    let response = gateway
        .oneshot(health_stream_request("idle-health"))
        .await
        .unwrap();
    let mut stream = response.into_body().into_data_stream();
    let frame = tokio::time::timeout(Duration::from_secs(35), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let (flags, payload) = connect::decode_frames(&frame).unwrap().pop().unwrap();
    assert_eq!(flags, connect::END_STREAM_FLAG);
    let error: serde_json::Value = serde_json::from_slice(&payload).unwrap();
    assert_eq!(error["error"]["code"], "unavailable");
    assert!(stream.next().await.is_none());
    wait_health_closed(&registry, "idle-health").await;
    assert!(upstream.requests.try_recv().is_err());
    registry.shutdown().await;
}

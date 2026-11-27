#[path = "health_tests.rs"]
mod health;

use std::sync::Arc;

use axum::extract::OriginalUri;
use prost::Message;
use tokio::sync::mpsc;

use super::*;
use crate::{
    cursor::prompting::{PromptAssets, PromptCompiler},
    model::ModelInvocation,
    network::NetworkClients,
    provider::{Provider, ProviderStream},
    store::Store,
};

struct UnusedProvider;

impl Provider for UnusedProvider {
    fn stream(
        &self,
        _invocation: ModelInvocation,
        _cancellation: tokio_util::sync::CancellationToken,
    ) -> ProviderStream {
        panic!("routing tests must not call a model provider")
    }
}

struct TestUpstream {
    proxy: CursorProxy,
    requests: mpsc::UnboundedReceiver<(String, Bytes)>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for TestUpstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn setup() -> (TransportRegistry, TestUpstream) {
    let store = Store::connect("sqlite::memory:").await.unwrap();
    let clients = NetworkClients::new(store.clone());
    let registry = TransportRegistry::new(
        store,
        Arc::new(UnusedProvider),
        PromptCompiler::new(PromptAssets::embedded().unwrap()),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (sender, requests) = mpsc::unbounded_channel();
    let router = Router::new().fallback(move |OriginalUri(uri): OriginalUri, body: Bytes| {
        let sender = sender.clone();
        async move {
            sender.send((uri.to_string(), body)).unwrap();
            (
                StatusCode::ACCEPTED,
                [(header::CONTENT_TYPE, "application/proto")],
                Bytes::from_static(b"opaque-response"),
            )
        }
    });
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (
        registry,
        TestUpstream {
            proxy: CursorProxy::with_upstream(clients, format!("http://{address}")),
            requests,
            task,
        },
    )
}

fn append_wire(request_id: &str, payload: Vec<u8>, seqno: i64) -> Vec<u8> {
    ai::BidiAppendRequest {
        request_id: Some(ai::BidiRequestId {
            request_id: request_id.into(),
        }),
        append_seqno: seqno,
        data: hex::encode(payload),
        ..Default::default()
    }
    .encode_to_vec()
}

fn binary_append_wire(request_id: &str, payload: Vec<u8>, seqno: i64) -> Vec<u8> {
    ai::BidiAppendRequest {
        request_id: Some(ai::BidiRequestId {
            request_id: request_id.into(),
        }),
        append_seqno: seqno,
        data_binary: payload,
        ..Default::default()
    }
    .encode_to_vec()
}

fn append_request(wire: Vec<u8>) -> Request<Body> {
    Request::post("/aiserver.v1.BidiService/BidiAppend")
        .header(header::CONTENT_TYPE, "application/proto")
        .body(Body::from(wire))
        .unwrap()
}

#[derive(Clone, PartialEq, Message)]
struct InferenceRunRequest {
    #[prost(string, tag = "1")]
    conversation_id: String,
}

#[derive(Clone, PartialEq, Message)]
struct InferenceClientMessage {
    #[prost(message, optional, tag = "1")]
    run_request: Option<InferenceRunRequest>,
}

fn inference_payload(conversation_id: &str) -> Vec<u8> {
    InferenceClientMessage {
        run_request: Some(InferenceRunRequest {
            conversation_id: conversation_id.into(),
        }),
    }
    .encode_to_vec()
}

#[tokio::test]
async fn inference_first_append_is_forwarded_without_agent_decoding() {
    let (registry, mut upstream) = setup().await;
    for (conversation_id, binary) in [
        ("00000000-0000-0000-0000-000000000000", false),
        ("f0000000-0000-0000-0000-000000000000", false),
        ("a0000000-0000-0000-0000-000000000000", false),
        ("a0000000-0000-0000-0000-000000000000", true),
    ] {
        let payload = inference_payload(conversation_id);
        let wire = if binary {
            binary_append_wire("inference-request", payload, 0)
        } else {
            append_wire("inference-request", payload, 0)
        };
        let response = bidi_handler(
            State(registry.clone()),
            Extension(upstream.proxy.clone()),
            append_request(wire.clone()),
        )
        .await
        .expect("inference packets must reach their own upstream protocol");
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "application/proto"
        );
        assert_eq!(
            to_bytes(response.into_body(), 1024).await.unwrap(),
            Bytes::from_static(b"opaque-response")
        );
        let (path, body) = upstream.requests.recv().await.unwrap();
        assert_eq!(path, "/aiserver.v1.BidiService/BidiAppend");
        assert_eq!(body.as_ref(), wire);
        assert!(registry.local("inference-request").await.is_none());
        assert!(registry.upstream("inference-request").await);
    }
    registry.shutdown().await;
}

#[tokio::test]
async fn registered_upstream_append_preserves_opaque_and_binary_payloads() {
    let (registry, mut upstream) = setup().await;
    registry
        .mark_upstream("upstream-request", UpstreamKind::Inference)
        .await
        .unwrap();
    let mut messages = vec![append_wire("upstream-request", vec![0x0e], 1)];
    messages.push(
        ai::BidiAppendRequest {
            request_id: Some(ai::BidiRequestId {
                request_id: "upstream-request".into(),
            }),
            append_seqno: 2,
            data_binary: vec![0x0e, 0xff],
            ..Default::default()
        }
        .encode_to_vec(),
    );
    for wire in messages {
        let response = bidi_handler(
            State(registry.clone()),
            Extension(upstream.proxy.clone()),
            append_request(wire.clone()),
        )
        .await
        .expect("a registered upstream owns its packet format");
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let (_, body) = upstream.requests.recv().await.unwrap();
        assert_eq!(body.as_ref(), wire);
    }
    registry.shutdown().await;
}

#[tokio::test]
async fn local_transport_decode_failure_never_reaches_upstream() {
    let (registry, mut upstream) = setup().await;
    registry.get_or_create("local-request").await.unwrap();
    for payload in [
        vec![0x0e],
        inference_payload("a0000000-0000-0000-0000-000000000000"),
    ] {
        let result = bidi_handler(
            State(registry.clone()),
            Extension(upstream.proxy.clone()),
            append_request(append_wire("local-request", payload, 1)),
        )
        .await;
        assert!(matches!(result, Err(crate::Error::Decode(_))));
        assert!(!registry.upstream("local-request").await);
        assert!(upstream.requests.try_recv().is_err());
    }
    registry.shutdown().await;
}

fn inference_stream_request(request_id: &str) -> Request<Body> {
    Request::post("/aiserver.v1.InferenceService/RunInferenceSSE")
        .header(header::CONTENT_TYPE, "application/proto")
        .body(Body::from(
            agent::BidiRequestId {
                request_id: request_id.into(),
            }
            .encode_to_vec(),
        ))
        .unwrap()
}

async fn wait_upstream_closed(registry: &TransportRegistry, request_id: &str) {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while registry.upstream(request_id).await {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn inference_stream_first_registers_opaque_appends_and_cleans_up_at_end() {
    let (registry, mut upstream) = setup().await;
    let response = upstream_bidi_sse_handler(
        State(registry.clone()),
        Extension(upstream.proxy.clone()),
        inference_stream_request("stream-first"),
    )
    .await
    .unwrap();
    let (path, body) = upstream.requests.recv().await.unwrap();
    assert_eq!(path, "/aiserver.v1.InferenceService/RunInferenceSSE");
    assert_eq!(
        connect::decode_unary::<agent::BidiRequestId>(&body)
            .unwrap()
            .request_id,
        "stream-first"
    );
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert!(registry.upstream("stream-first").await);
    let wire = append_wire("stream-first", vec![0x0e], 1);
    bidi_handler(
        State(registry.clone()),
        Extension(upstream.proxy.clone()),
        append_request(wire.clone()),
    )
    .await
    .unwrap();
    assert_eq!(upstream.requests.recv().await.unwrap().1.as_ref(), wire);
    assert_eq!(
        to_bytes(response.into_body(), 1024).await.unwrap(),
        Bytes::from_static(b"opaque-response")
    );
    wait_upstream_closed(&registry, "stream-first").await;
    registry.shutdown().await;
}

#[tokio::test]
async fn inference_append_first_can_attach_stream_and_drop_it_before_polling() {
    let (registry, mut upstream) = setup().await;
    bidi_handler(
        State(registry.clone()),
        Extension(upstream.proxy.clone()),
        append_request(append_wire(
            "append-first",
            inference_payload("a0000000-0000-0000-0000-000000000000"),
            0,
        )),
    )
    .await
    .unwrap();
    upstream.requests.recv().await.unwrap();
    let response = upstream_bidi_sse_handler(
        State(registry.clone()),
        Extension(upstream.proxy.clone()),
        inference_stream_request("append-first"),
    )
    .await
    .unwrap();
    upstream.requests.recv().await.unwrap();
    assert!(registry.upstream("append-first").await);
    drop(response);
    wait_upstream_closed(&registry, "append-first").await;
    registry.shutdown().await;
}

#[tokio::test]
async fn inference_stream_cannot_take_over_a_local_transport() {
    let (registry, mut upstream) = setup().await;
    registry.get_or_create("local-request").await.unwrap();
    let result = upstream_bidi_sse_handler(
        State(registry.clone()),
        Extension(upstream.proxy.clone()),
        inference_stream_request("local-request"),
    )
    .await;
    assert!(matches!(result, Err(crate::Error::Protocol(_))));
    assert!(upstream.requests.try_recv().is_err());
    assert!(!registry.upstream("local-request").await);
    registry.shutdown().await;
}

#[tokio::test]
async fn stt_stream_and_opaque_appends_are_forwarded_in_either_arrival_order() {
    use tower::ServiceExt;

    let (registry, mut upstream) = setup().await;
    let directory = tempfile::tempdir().unwrap();
    let gateway = router_with_proxy(
        registry.clone(),
        upstream.proxy.clone(),
        knowledge::KnowledgeService::with_root(directory.path().join("rules")).unwrap(),
    );
    for (request_id, append_first) in [("stt-stream-first", false), ("stt-append-first", true)] {
        let wire = binary_append_wire(request_id, vec![0x0a, 4, 0x0a, 2, b'e', b'n'], 0);
        let append = gateway.clone().oneshot(append_request(wire.clone()));
        tokio::pin!(append);
        if append_first {
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(30), &mut append)
                    .await
                    .is_err()
            );
        }
        let mut request = inference_stream_request(request_id);
        *request.uri_mut() = "/aiserver.v1.AiService/StreamSttSSE".parse().unwrap();
        let response = gateway.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(
            registry.upstream_route(request_id).await.unwrap().kind,
            UpstreamKind::SpeechToText
        );
        let (path, request_body) = upstream.requests.recv().await.unwrap();
        assert_eq!(path, "/aiserver.v1.AiService/StreamSttSSE");
        assert_eq!(
            connect::decode_unary::<ai::BidiRequestId>(&request_body)
                .unwrap()
                .request_id,
            request_id
        );
        assert_eq!(append.await.unwrap().status(), StatusCode::ACCEPTED);
        assert_eq!(upstream.requests.recv().await.unwrap().1.as_ref(), wire);
        let conflict = gateway
            .clone()
            .oneshot(inference_stream_request(request_id))
            .await
            .unwrap();
        assert_eq!(conflict.status(), StatusCode::BAD_REQUEST);
        assert!(registry.get_or_create(request_id).await.is_err());
        assert!(upstream.requests.try_recv().is_err());
        if append_first {
            drop(response);
        } else {
            assert_eq!(
                to_bytes(response.into_body(), 1024).await.unwrap(),
                "opaque-response"
            );
        }
        wait_upstream_closed(&registry, request_id).await;
    }
    registry.shutdown().await;
}

#[tokio::test]
async fn stt_poll_keeps_opaque_appends_routed_between_polls() {
    use std::time::Duration;
    use tower::ServiceExt;

    let (registry, mut upstream) = setup().await;
    let directory = tempfile::tempdir().unwrap();
    let gateway = router_with_proxy(
        registry.clone(),
        upstream.proxy.clone(),
        knowledge::KnowledgeService::with_root(directory.path().join("rules")).unwrap(),
    );
    let request_id = "stt-poll";
    for seqno in 0..2 {
        let mut wire = SttPollRequest {
            request_id: Some(ai::BidiRequestId {
                request_id: request_id.into(),
            }),
        }
        .encode_to_vec();
        if seqno == 0 {
            wire.extend_from_slice(&[0x10, 1]);
        }
        let path = "/aiserver.v1.AiService/StreamSttPoll";
        let request = Request::post(path)
            .header(header::CONTENT_TYPE, "application/proto")
            .body(Body::from(wire.clone()))
            .unwrap();
        let response = gateway.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(
            to_bytes(response.into_body(), 1024).await.unwrap(),
            "opaque-response"
        );
        assert_eq!(
            upstream.requests.recv().await.unwrap(),
            (path.into(), Bytes::from(wire))
        );
        assert_eq!(
            registry.upstream_route(request_id).await.unwrap().kind,
            UpstreamKind::SpeechToText
        );

        let wire = append_wire(request_id, vec![0x12, 4, 0x0a, 2, 0, 0], seqno);
        let response = gateway
            .clone()
            .oneshot(append_request(wire.clone()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(upstream.requests.recv().await.unwrap().1.as_ref(), wire);
        assert!(registry.local(request_id).await.is_none());
    }
    tokio::time::timeout(Duration::from_secs(7), async {
        while registry.upstream(request_id).await {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    registry.shutdown().await;
}

#[tokio::test]
async fn failed_official_stream_connection_releases_its_request_id() {
    let (registry, _upstream) = setup().await;
    let proxy = CursorProxy::with_upstream(
        NetworkClients::new(registry.store().clone()),
        "invalid://unreachable".into(),
    );
    let request_id = "unreachable-agent";
    registry
        .mark_upstream(request_id, UpstreamKind::Agent)
        .await
        .unwrap();
    let mut request = inference_stream_request(request_id);
    *request.uri_mut() = "/agent.v1.AgentService/RunSSE".parse().unwrap();
    assert!(
        run_sse_handler(State(registry.clone()), Extension(proxy.clone()), request)
            .await
            .is_err()
    );
    wait_upstream_closed(&registry, request_id).await;

    let request_id = "unreachable-stt";
    let mut request = inference_stream_request(request_id);
    *request.uri_mut() = "/aiserver.v1.AiService/StreamSttSSE".parse().unwrap();
    assert!(
        upstream_bidi_sse_handler(State(registry.clone()), Extension(proxy), request)
            .await
            .is_err()
    );
    wait_upstream_closed(&registry, request_id).await;
    registry.shutdown().await;
}

#[derive(Clone, PartialEq, Message)]
struct HealthMessage {
    #[prost(string, tag = "1")]
    payload: String,
}

fn health_stream_request(request_id: &str) -> Request<Body> {
    let mut request = inference_stream_request(request_id);
    *request.uri_mut() = "/aiserver.v1.HealthService/StreamBidiSSE".parse().unwrap();
    request
}

#[tokio::test]
async fn health_first_append_waits_for_its_stream_instead_of_decoding_as_agent() {
    let (registry, mut upstream) = setup().await;
    let wire = binary_append_wire(
        "health-append-first",
        HealthMessage {
            payload: "foo".into(),
        }
        .encode_to_vec(),
        0,
    );
    let append_registry = registry.clone();
    let append_proxy = upstream.proxy.clone();
    let append_wire_copy = wire.clone();
    let mut pending = tokio::spawn(async move {
        bidi_handler(
            State(append_registry),
            Extension(append_proxy),
            append_request(append_wire_copy),
        )
        .await
    });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), &mut pending)
            .await
            .is_err(),
        "the diagnostic's HealthRequest was rejected before its stream could register"
    );
    let response = health_bidi_sse_handler(
        State(registry.clone()),
        health_stream_request("health-append-first"),
    )
    .await
    .unwrap();
    assert_eq!(pending.await.unwrap().unwrap().status(), StatusCode::OK);
    let mut stream = response.into_body().into_data_stream();
    let frame = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        futures_util::StreamExt::next(&mut stream),
    )
    .await
    .unwrap()
    .unwrap()
    .unwrap();
    assert_eq!(
        frame,
        connect::encode_message(&HealthMessage {
            payload: "foo".into()
        })
        .unwrap()
    );
    assert!(upstream.requests.try_recv().is_err());
    assert!(registry.local("health-append-first").await.is_none());
    assert!(!registry.upstream("health-append-first").await);
    drop(stream);
    registry.shutdown().await;
}

#[tokio::test]
async fn unknown_malformed_packet_is_rejected_without_upstream_fallback() {
    let (registry, mut upstream) = setup().await;
    let result = bidi_handler(
        State(registry.clone()),
        Extension(upstream.proxy.clone()),
        append_request(append_wire("unknown-request", vec![0x0e], 0)),
    )
    .await;
    assert!(matches!(result, Err(crate::Error::Decode(_))));
    assert!(upstream.requests.try_recv().is_err());
    assert!(!registry.upstream("unknown-request").await);
    registry.shutdown().await;
}

#[test]
fn incomplete_parent_headers_are_ignored_without_rejecting_the_request() {
    let mut headers = HeaderMap::new();
    headers.insert("x-parent-request-id", HeaderValue::from_static("parent"));
    assert_eq!(parent_headers(&headers).unwrap(), None);

    headers.clear();
    headers.insert(
        "x-parent-agent-tool-call-id",
        HeaderValue::from_static("tool"),
    );
    assert_eq!(parent_headers(&headers).unwrap(), None);

    headers.clear();
    headers.insert("x-parent-request-id", HeaderValue::from_static(""));
    headers.insert(
        "x-parent-agent-tool-call-id",
        HeaderValue::from_static("tool"),
    );
    assert_eq!(parent_headers(&headers).unwrap(), None);
}

#[test]
fn complete_parent_headers_are_preserved() {
    let mut headers = HeaderMap::new();
    headers.insert("x-parent-request-id", HeaderValue::from_static("parent"));
    headers.insert(
        "x-parent-agent-tool-call-id",
        HeaderValue::from_static("tool"),
    );
    assert_eq!(
        parent_headers(&headers).unwrap(),
        Some(TransportParent {
            request_id: "parent".into(),
            tool_call_id: "tool".into(),
        })
    );
}

#[tokio::test]
async fn run_sse_registers_complete_parent_headers_on_local_transport() {
    let (registry, upstream) = setup().await;
    let handle = registry.get_or_create("run-sse-parent").await.unwrap();
    let mut request = inference_stream_request("run-sse-parent");
    request.headers_mut().insert(
        "x-parent-request-id",
        HeaderValue::from_static("parent-request"),
    );
    request.headers_mut().insert(
        "x-parent-agent-tool-call-id",
        HeaderValue::from_static("parent-tool"),
    );
    let response = run_sse_handler(State(registry.clone()), Extension(upstream.proxy), request)
        .await
        .unwrap();
    assert_eq!(
        handle.parent(),
        Some(&TransportParent {
            request_id: "parent-request".into(),
            tool_call_id: "parent-tool".into(),
        })
    );
    drop(response);
    registry.shutdown().await;
}

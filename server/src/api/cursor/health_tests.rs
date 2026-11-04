use std::{
    convert::Infallible,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use futures_util::StreamExt;
use tower::ServiceExt;

use super::*;

#[tokio::test]
async fn health_diagnostic_forwards_five_live_round_trips_to_the_upstream() {
    let (registry, mut upstream) = setup().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (frames, receiver) = mpsc::unbounded_channel::<Bytes>();
    let receiver = Arc::new(tokio::sync::Mutex::new(Some(receiver)));
    let received = Arc::new(AtomicUsize::new(0));
    let backend = Router::new()
        .route(
            "/aiserver.v1.HealthService/StreamBidiSSE",
            post(move |body: Bytes| {
                let receiver = receiver.clone();
                async move {
                    let request: agent::BidiRequestId = connect::decode_unary(&body).unwrap();
                    assert_eq!(request.request_id, "health-live-echo");
                    let mut receiver = receiver.lock().await.take().unwrap();
                    let stream = async_stream::stream! {
                        for _ in 0..5 {
                            let frame = receiver.recv().await.unwrap();
                            yield Ok::<Bytes, Infallible>(frame);
                        }
                    };
                    let mut response = Response::new(Body::from_stream(stream));
                    response.headers_mut().insert(
                        header::CONTENT_TYPE,
                        HeaderValue::from_static("text/event-stream"),
                    );
                    response
                }
            }),
        )
        .route(
            "/aiserver.v1.BidiService/BidiAppend",
            post({
                let received = received.clone();
                move |body: Bytes| {
                    let frames = frames.clone();
                    let received = received.clone();
                    async move {
                        let append: ai::BidiAppendRequest = connect::decode_unary(&body).unwrap();
                        assert_eq!(bidi::request_id(&append).unwrap(), "health-live-echo");
                        assert_eq!(
                            append.append_seqno,
                            received.fetch_add(1, Ordering::SeqCst) as i64
                        );
                        let payload = hex::decode(append.data).unwrap();
                        let message = HealthMessage::decode(payload.as_slice()).unwrap();
                        assert_eq!(message.payload, "foo");
                        frames
                            .send(connect::encode_message(&message).unwrap())
                            .unwrap();
                        ([(header::CONTENT_TYPE, "application/proto")], Bytes::new())
                    }
                }
            }),
        );
    upstream.task.abort();
    upstream.task = tokio::spawn(async move {
        axum::serve(listener, backend).await.unwrap();
    });
    upstream.proxy = CursorProxy::with_upstream(
        NetworkClients::new(registry.store().clone()),
        format!("http://{address}"),
    );
    let directory = tempfile::tempdir().unwrap();
    let gateway = router_with_proxy(
        registry.clone(),
        upstream.proxy.clone(),
        knowledge::KnowledgeService::with_root(directory.path().join("rules")).unwrap(),
    );
    let response = tokio::time::timeout(
        Duration::from_secs(2),
        gateway
            .clone()
            .oneshot(health_stream_request("health-live-echo")),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/event-stream"
    );
    let mut stream = response.into_body().into_data_stream();
    assert!(
        tokio::time::timeout(Duration::from_millis(30), stream.next())
            .await
            .is_err()
    );
    for seqno in 0..5 {
        let message = HealthMessage {
            payload: "foo".into(),
        };
        let response = gateway
            .clone()
            .oneshot(append_request(append_wire(
                "health-live-echo",
                message.encode_to_vec(),
                seqno,
            )))
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
    assert!(tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .unwrap()
        .is_none());
    assert_eq!(received.load(Ordering::SeqCst), 5);
    wait_upstream_closed(&registry, "health-live-echo").await;
    assert!(registry.local("health-live-echo").await.is_none());
    registry.shutdown().await;
}

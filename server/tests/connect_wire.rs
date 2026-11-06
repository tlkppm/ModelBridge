//! Verifies captured Cursor Connect framing and protobuf compatibility.
#[path = "support/fake_cursor.rs"]
mod fake_cursor;
#[path = "support/fake_provider.rs"]
mod fake_provider;
#[path = "support/fixtures.rs"]
mod fixtures;

use std::{io::Write, sync::Arc};

use axum::{
    body::{to_bytes, Body},
    http::{header, Request, StatusCode},
};
use base64::{engine::general_purpose::STANDARD_NO_PAD, Engine};
use cursor_server::{
    api::cursor,
    cursor::prompting::{PromptAssets, PromptCompiler},
    cursor::protocol::{
        connect,
        proto::{agent::v1 as pb, aiserver::v1 as ai},
    },
    cursor::transport::TransportRegistry,
    network::NetworkClients,
};
use flate2::{write::GzEncoder, Compression};
use prost::Message;
use tower::ServiceExt;

#[test]
fn connect_envelope_is_flag_plus_big_endian_length_plus_protobuf() {
    let message = pb::BidiRequestId {
        request_id: "abc".into(),
    };
    let frame = connect::encode_message(&message).unwrap();
    assert_eq!(&frame[..5], &[0, 0, 0, 0, 5]);
    let decoded: pb::BidiRequestId = fake_cursor::decode_single(&frame).unwrap();
    assert_eq!(decoded.request_id, "abc");
}

#[test]
fn end_stream_matches_captured_connect_shape() {
    assert_eq!(
        connect::encode_end_stream().as_ref(),
        &[2, 0, 0, 0, 2, b'{', b'}']
    );
}

#[test]
fn error_end_stream_is_flagged_json_not_protobuf() {
    let frame = connect::encode_error_end_stream(&connect::ConnectStreamError {
        code: connect::ConnectCode::Unavailable,
        message: "overloaded".into(),
        details: vec![connect::ConnectErrorDetail {
            type_name: "aiserver.v1.ErrorDetails".into(),
            value: "AQ".into(),
        }],
    })
    .unwrap();
    let (flags, payload) = connect::decode_frames(&frame).unwrap().pop().unwrap();
    assert_eq!(flags, connect::END_STREAM_FLAG);
    let json: serde_json::Value = serde_json::from_slice(&payload).unwrap();
    assert_eq!(json["error"]["code"], "unavailable");
    assert_eq!(json["error"]["message"], "overloaded");
    assert_eq!(
        json["error"]["details"][0]["type"],
        "aiserver.v1.ErrorDetails"
    );
}

#[test]
fn cursor_error_details_subset_decodes_captured_wire_value() {
    let captured = "CAISVQoUQXV0aGVudGljYXRpb24gZXJyb3ISMklmIHlvdSBhcmUgbG9nZ2VkIGluLCB0cnkgbG9nZ2luZyBvdXQgYW5kIGJhY2sgaW4uIABSBwoFbG9naW4YAQ";
    let bytes = STANDARD_NO_PAD.decode(captured).unwrap();
    let details = ai::ErrorDetails::decode(bytes.as_slice()).unwrap();
    assert_eq!(details.error, 2, "ERROR_NOT_LOGGED_IN");
    assert_eq!(details.is_expected, Some(true));
    let custom = details.details.unwrap();
    assert_eq!(custom.title, "Authentication error");
    assert_eq!(custom.is_retryable, Some(false));
}

#[test]
fn captured_kv_ack_hex_decodes_as_agent_client_message() {
    let bytes = hex::decode("1a0408011a00").unwrap();
    let message = pb::AgentClientMessage::decode(bytes.as_slice()).unwrap();
    let Some(pb::agent_client_message::Message::KvClientMessage(kv)) = message.message else {
        panic!("expected KV client message")
    };
    assert_eq!(kv.id, 1);
    assert!(matches!(
        kv.message,
        Some(pb::kv_client_message::Message::SetBlobResult(_))
    ));
}

#[tokio::test]
async fn bidi_append_gzip_body_is_decompressed_before_protobuf_decode() {
    let (_directory, store) = fixtures::temp_store().await;
    let assets = PromptAssets::load(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("prompt/cursor")
            .as_path(),
    )
    .unwrap();
    let clients = NetworkClients::new(store.clone());
    let registry = TransportRegistry::new(
        store,
        Arc::new(fake_provider::FakeProvider::default()),
        PromptCompiler::new(assets),
    );
    let wire = ai::BidiAppendRequest {
        request_id: Some(ai::BidiRequestId {
            request_id: "gzip-request".into(),
        }),
        ..Default::default()
    }
    .encode_to_vec();
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&wire).unwrap();
    let compressed = encoder.finish().unwrap();

    let response = cursor::router(registry, clients)
        .unwrap()
        .oneshot(
            Request::post("/aiserver.v1.BidiService/BidiAppend")
                .header(header::CONTENT_TYPE, "application/proto")
                .header(header::CONTENT_ENCODING, "gzip")
                .body(Body::from(compressed))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = to_bytes(response.into_body(), 4096).await.unwrap();
    let text = std::str::from_utf8(&body).unwrap();
    assert!(text.contains("BidiAppend contains no AgentClientMessage"));
    assert!(!text.contains("protobuf decode error"));
}

async fn send_binary_agent_append(
    gateway: axum::Router,
    request_id: &str,
    seqno: i64,
    message: pb::AgentClientMessage,
    gzip: bool,
) {
    let mut wire = ai::BidiAppendRequest {
        request_id: Some(ai::BidiRequestId {
            request_id: request_id.into(),
        }),
        append_seqno: seqno,
        data_binary: message.encode_to_vec(),
        ..Default::default()
    }
    .encode_to_vec();
    let mut request = Request::post("/aiserver.v1.BidiService/BidiAppend")
        .header(header::CONTENT_TYPE, "application/proto");
    if gzip {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&wire).unwrap();
        wire = encoder.finish().unwrap();
        request = request.header(header::CONTENT_ENCODING, "gzip");
    }
    let response = gateway
        .oneshot(request.body(Body::from(wire)).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn binary_agent_run_and_kv_ack_complete_through_http_routes() {
    use std::time::Duration;

    use cursor_server::{
        model::ModelConfigInput,
        provider::{FinishReason, ModelEvent},
    };
    use futures_util::StreamExt;

    for gzip in [false, true] {
        let (_directory, store) = fixtures::temp_store().await;
        let input: ModelConfigInput = serde_json::from_value(serde_json::json!({
            "display_name": "Binary fixture",
            "type": "openai",
            "base_url": "https://example.invalid/v1",
            "api_key": "fixture-key",
            "tooltip_data": "test",
            "model_id": "fixture-model",
            "openai_endpoint": "/v1/chat/completions"
        }))
        .unwrap();
        let model = store.create_model(&input).await.unwrap();
        let provider = fake_provider::FakeProvider::default();
        provider.push(vec![
            ModelEvent::Start {
                model_call_id: "binary-model-call".into(),
            },
            ModelEvent::TextStart,
            ModelEvent::TextDelta("binary response".into()),
            ModelEvent::TextEnd,
            ModelEvent::Done(FinishReason::Stop),
        ]);
        let clients = NetworkClients::new(store.clone());
        let registry = TransportRegistry::new(
            store,
            Arc::new(provider.clone()),
            PromptCompiler::new(PromptAssets::embedded().unwrap()),
        );
        let gateway = cursor::router(registry.clone(), clients).unwrap();
        let mut pending = tokio::spawn(
            gateway.clone().oneshot(
                Request::post("/agent.v1.AgentService/RunSSE")
                    .header(header::CONTENT_TYPE, "application/proto")
                    .body(Body::from(
                        pb::BidiRequestId {
                            request_id: "binary-request".into(),
                        }
                        .encode_to_vec(),
                    ))
                    .unwrap(),
            ),
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(30), &mut pending)
                .await
                .is_err()
        );
        send_binary_agent_append(
            gateway.clone(),
            "binary-request",
            0,
            pb::AgentClientMessage {
                message: Some(pb::agent_client_message::Message::RunRequest(
                    pb::AgentRunRequest {
                        requested_model: Some(pb::RequestedModel {
                            model_id: model.model_hash,
                            ..Default::default()
                        }),
                        conversation_id: Some("binary-conversation".into()),
                        run_id: Some("binary-request".into()),
                        action: Some(pb::ConversationAction {
                            action: Some(pb::conversation_action::Action::UserMessageAction(
                                pb::UserMessageAction {
                                    user_message: Some(pb::UserMessage {
                                        text: "hello".into(),
                                        message_id: "binary-user".into(),
                                        mode: pb::AgentMode::Agent as i32,
                                        ..Default::default()
                                    }),
                                    ..Default::default()
                                },
                            )),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                )),
            },
            gzip,
        )
        .await;
        let response = tokio::time::timeout(Duration::from_secs(2), pending)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(registry.local("binary-request").await.is_some());
        assert!(!registry.upstream("binary-request").await);
        let mut stream = response.into_body().into_data_stream();
        let mut seqno = 1;
        let mut text = String::new();
        loop {
            let frame = tokio::time::timeout(Duration::from_secs(10), stream.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let (flags, payload) = connect::decode_frames(&frame).unwrap().pop().unwrap();
            if flags & connect::END_STREAM_FLAG != 0 {
                assert_eq!(
                    serde_json::from_slice::<serde_json::Value>(&payload).unwrap(),
                    serde_json::json!({})
                );
                break;
            }
            let message = pb::AgentServerMessage::decode(payload).unwrap();
            match message.message {
                Some(pb::agent_server_message::Message::KvServerMessage(kv)) => {
                    send_binary_agent_append(
                        gateway.clone(),
                        "binary-request",
                        seqno,
                        pb::AgentClientMessage {
                            message: Some(pb::agent_client_message::Message::KvClientMessage(
                                pb::KvClientMessage {
                                    id: kv.id,
                                    message: Some(pb::kv_client_message::Message::SetBlobResult(
                                        pb::SetBlobResult { error: None },
                                    )),
                                },
                            )),
                        },
                        gzip,
                    )
                    .await;
                    seqno += 1;
                }
                Some(pb::agent_server_message::Message::InteractionUpdate(update)) => {
                    if let Some(pb::interaction_update::Message::TextDelta(delta)) = update.message
                    {
                        text.push_str(&delta.text);
                    }
                }
                _ => {}
            }
        }
        assert_eq!(text, "binary response");
        assert_eq!(provider.requests().len(), 1);
        registry.shutdown().await;
    }
}

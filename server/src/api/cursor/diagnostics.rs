use std::time::Instant;

use axum::{
    body::Body,
    http::{header, HeaderMap, Request, Response},
    middleware::Next,
};
use tracing::Instrument;

pub(super) async fn log_request(request: Request<Body>, next: Next) -> Response<Body> {
    let span = tracing::info_span!(
        "cursor_diagnostic",
        diagnostic_id = %uuid::Uuid::new_v4(),
        request_id = tracing::field::Empty,
        method = %request.method(),
        path = request.uri().path(),
        authentication = authentication(request.headers()),
        http_version = ?request.version(),
        compressed = request.headers().contains_key(header::CONTENT_ENCODING),
    );
    async move {
        let started = Instant::now();
        tracing::info!("Cursor diagnostic: HTTP request received");
        let response = next.run(request).await;
        tracing::info!(
            status = response.status().as_u16(),
            elapsed_ms = started.elapsed().as_millis(),
            streaming = response
                .headers()
                .get(header::CONTENT_TYPE)
                .is_some_and(|value| value == "text/event-stream"),
            "Cursor diagnostic: HTTP response headers ready"
        );
        response
    }
    .instrument(span)
    .await
}

pub(super) fn log_append(append: &super::bidi::DecodedAppend) {
    use crate::cursor::protocol::proto::agent::v1::{
        agent_client_message::Message, exec_client_control_message::Message as Control,
        exec_client_message::Message as Exec,
    };

    match append.message.message.as_ref() {
        Some(Message::RunRequest(request)) => {
            tracing::info!(
                append_seqno = append.seqno,
                direct_mcp_tools = request
                    .mcp_tools
                    .as_ref()
                    .map_or(0, |tools| tools.mcp_tools.len()),
                has_action = request.action.is_some(),
                has_checkpoint = request.conversation_state.is_some(),
                subagent = request.subagent_type_name.is_some(),
                "Cursor diagnostic: Run request decoded"
            );
        }
        Some(Message::ExecClientMessage(message)) => {
            use crate::cursor::protocol::proto::agent::v1::shell_stream::Event;

            let kind = match message.message.as_ref() {
                Some(Exec::ShellStream(stream)) => match stream.event.as_ref() {
                    Some(Event::Start(_)) => "shell_start",
                    Some(Event::Exit(_)) => "shell_exit",
                    Some(Event::Backgrounded(_)) => "shell_backgrounded",
                    Some(Event::Rejected(_)) => "shell_rejected",
                    Some(Event::PermissionDenied(_)) => "shell_permission_denied",
                    Some(Event::SandboxUnsupported(_)) => "shell_sandbox_unsupported",
                    _ => return,
                },
                Some(Exec::RequestContextResult(_)) => "request_context",
                Some(Exec::ReadResult(_)) | Some(Exec::RedactedReadResult(_)) => "read",
                Some(Exec::WriteResult(_)) => "write",
                Some(Exec::McpResult(_)) => "mcp",
                Some(Exec::McpStateExecResult(_)) => "mcp_state",
                Some(_) => "other",
                None => "empty",
            };
            tracing::info!(
                append_seqno = append.seqno,
                id = message.id,
                result_kind = kind,
                "Cursor diagnostic: client execution event received"
            );
        }
        Some(Message::ExecClientControlMessage(message)) => match message.message.as_ref() {
            Some(Control::Throw(error)) => tracing::warn!(
                append_seqno = append.seqno,
                id = error.id,
                has_error_code = error.error_code.is_some(),
                error_bytes = error.error.len(),
                "Cursor diagnostic: client execution failed"
            ),
            Some(Control::StreamClose(close)) => tracing::info!(
                append_seqno = append.seqno,
                id = close.id,
                "Cursor diagnostic: client execution stream closed"
            ),
            _ => {}
        },
        Some(Message::InteractionResponse(response)) => tracing::info!(
            append_seqno = append.seqno,
            id = response.id,
            has_result = response.result.is_some(),
            "Cursor diagnostic: client interaction response received"
        ),
        None => tracing::warn!(
            append_seqno = append.seqno,
            "Cursor diagnostic: decoded client message has no supported message variant"
        ),
        _ => {}
    }
}

fn authentication(headers: &HeaderMap) -> &'static str {
    if crate::local_app::request_uses_local_cursor_token(headers) {
        "local"
    } else if headers.contains_key(header::AUTHORIZATION) {
        "external"
    } else {
        "missing"
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::{body::to_bytes, http::StatusCode, middleware, routing::post, Router};
    use tower::ServiceExt;
    use tracing::instrument::WithSubscriber;

    use super::*;

    struct LogWriter(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for LogWriter {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn authentication_only_exposes_a_static_category() {
        let mut headers = HeaderMap::new();
        assert_eq!(authentication(&headers), "missing");
        headers.insert(
            header::AUTHORIZATION,
            "Bearer diagnostic-test-placeholder".parse().unwrap(),
        );
        assert_eq!(authentication(&headers), "external");
        headers.insert(
            header::AUTHORIZATION,
            crate::local_app::local_cursor_authorization()
                .parse()
                .unwrap(),
        );
        assert_eq!(authentication(&headers), "local");
    }

    #[test]
    fn client_failure_logging_omits_error_text_and_stack() {
        use crate::cursor::protocol::proto::agent::v1 as pb;

        let logs = Arc::new(Mutex::new(Vec::new()));
        let writer = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || LogWriter(writer.clone()))
            .finish();
        let append = super::super::bidi::DecodedAppend {
            request_id: "diagnostic-request".into(),
            seqno: 3,
            message: pb::AgentClientMessage {
                message: Some(pb::agent_client_message::Message::ExecClientControlMessage(
                    pb::ExecClientControlMessage {
                        message: Some(pb::exec_client_control_message::Message::Throw(
                            pb::ExecClientThrow {
                                id: 7,
                                error: "private-error-placeholder".into(),
                                stack_trace: Some("private-stack-placeholder".into()),
                                error_code: Some("private-code-placeholder".into()),
                            },
                        )),
                    },
                )),
            },
        };
        tracing::subscriber::with_default(subscriber, || log_append(&append));
        let logs = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
        assert!(logs.contains("client execution failed"));
        assert!(logs.contains("id=7"));
        assert!(logs.contains("has_error_code=true"));
        for private in [
            "private-error-placeholder",
            "private-stack-placeholder",
            "private-code-placeholder",
        ] {
            assert!(!logs.contains(private));
        }
    }

    #[tokio::test]
    async fn request_logging_preserves_the_response_and_omits_sensitive_data() {
        let logs = Arc::new(Mutex::new(Vec::new()));
        let writer = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || LogWriter(writer.clone()))
            .finish();
        let router = Router::new()
            .route(
                "/probe",
                post(|request: Request<Body>| async move {
                    let body = to_bytes(request.into_body(), 4096).await.unwrap();
                    let mut response = Response::new(Body::from(body));
                    *response.status_mut() = StatusCode::BAD_REQUEST;
                    response
                }),
            )
            .layer(middleware::from_fn(log_request));
        let response = router
            .oneshot(
                Request::post("/probe?private=private-query-placeholder")
                    .header(header::AUTHORIZATION, "Bearer diagnostic-test-placeholder")
                    .header(header::COOKIE, "private-cookie-placeholder")
                    .body(Body::from("private-body-placeholder"))
                    .unwrap(),
            )
            .with_subscriber(subscriber)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            to_bytes(response.into_body(), 4096).await.unwrap().as_ref(),
            b"private-body-placeholder"
        );
        let logs = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
        assert!(logs.contains("authentication=\"external\""));
        assert!(logs.contains("status=400"));
        assert!(logs.contains("HTTP request received"));
        assert!(logs.contains("HTTP response headers ready"));
        for private in [
            "diagnostic-test-placeholder",
            "private-query-placeholder",
            "private-cookie-placeholder",
            "private-body-placeholder",
        ] {
            assert!(!logs.contains(private));
        }
    }
}

use std::time::Duration;

use axum::{
    body::Body,
    http::{header, HeaderValue, Response},
};
use prost::Message;

use crate::{
    cursor::{
        protocol::{connect, proto::aiserver::v1 as ai},
        transport::TransportRegistry,
    },
    Result,
};

use super::bidi;

#[derive(Clone, PartialEq, Message)]
struct HealthRequest {
    #[prost(string, tag = "1")]
    payload: String,
    #[prost(bool, optional, tag = "2")]
    simulate_error: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
struct HealthResponse {
    #[prost(string, tag = "1")]
    payload: String,
}

pub async fn append(registry: &TransportRegistry, request: &ai::BidiAppendRequest) -> Result<()> {
    let payload = bidi::payload(request)?;
    let message = HealthRequest::decode(payload.as_ref())?;
    let frame = if message.simulate_error == Some(true) {
        connect::encode_error_end_stream(&connect::ConnectStreamError {
            code: connect::ConnectCode::Internal,
            message: "Simulated health diagnostic error".into(),
            details: Vec::new(),
        })?
    } else {
        connect::encode_message(&HealthResponse {
            payload: message.payload,
        })?
    };
    registry
        .append_health(bidi::request_id(request)?, request.append_seqno, frame)
        .await
}

pub async fn stream(registry: TransportRegistry, request_id: String) -> Result<Response<Body>> {
    let (generation, mut receiver) = registry.open_health(&request_id).await?;
    let guard = HealthStreamGuard {
        registry,
        request_id,
        generation,
    };
    let stream = async_stream::stream! {
        let _guard = guard;
        loop {
            match tokio::time::timeout(Duration::from_secs(30), receiver.recv()).await {
                Ok(Some(frame)) => {
                    let terminal = frame[0] & connect::END_STREAM_FLAG != 0;
                    yield Ok::<_, std::convert::Infallible>(frame);
                    if terminal {
                        return;
                    }
                }
                Ok(None) => return,
                Err(_) => {
                    let frame = connect::encode_error_end_stream(&connect::ConnectStreamError {
                        code: connect::ConnectCode::Unavailable,
                        message: "Health diagnostic timed out waiting for an append".into(),
                        details: Vec::new(),
                    }).expect("health diagnostic error is serializable");
                    yield Ok(frame);
                    return;
                }
            }
        }
    };
    let mut response = Response::new(Body::from_stream(stream));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
        .headers_mut()
        .insert("connect-protocol-version", HeaderValue::from_static("1"));
    Ok(response)
}

struct HealthStreamGuard {
    registry: TransportRegistry,
    request_id: String,
    generation: u64,
}

impl Drop for HealthStreamGuard {
    fn drop(&mut self) {
        self.registry
            .finish_health(self.request_id.clone(), self.generation);
    }
}

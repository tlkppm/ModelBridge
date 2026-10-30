//! Implements Cursor analytics endpoints and event handling.
use axum::{
    body::{Body, Bytes},
    extract::Extension,
    http::{header, HeaderValue, Request, Response, StatusCode},
};
use base64::{engine::general_purpose::STANDARD, Engine};
use bytes::{BufMut, BytesMut};
use prost::Message;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::{api::cursor::proxy, Error, Result};

pub const BOOTSTRAP_STATSIG_PATH: &str = "/aiserver.v1.AnalyticsService/BootstrapStatsig";
const AGENT_RETRIES_GATE: &str = "nal_agent_retries";
const LOCAL_AGENT_LOOP_GATE: &str = "agent_host_local_loop";
const LOCAL_RULE: &str = "local_override";

#[derive(Clone, PartialEq, Message)]
struct BootstrapStatsigResponse {
    #[prost(string, tag = "1")]
    config: String,
    #[prost(uint64, tag = "2")]
    generated_at_ms: u64,
}

pub async fn bootstrap_statsig(
    Extension(upstream): Extension<proxy::CursorProxy>,
    request: Request<Body>,
) -> Result<Response<Body>> {
    match proxy::forward_buffered(&upstream, request).await {
        Ok(response) if response.status.is_success() => match patch_upstream(response) {
            Ok(response) => Ok(response),
            Err(error) => {
                tracing::warn!(%error, "Cursor Statsig bootstrap was invalid; using local bootstrap");
                local_response()
            }
        },
        Ok(response) => {
            tracing::warn!(status = %response.status, "Cursor Statsig bootstrap was rejected; using local bootstrap");
            local_response()
        }
        Err(error) => {
            tracing::warn!(%error, "Cursor Statsig bootstrap was unavailable; using local bootstrap");
            local_response()
        }
    }
}

fn patch_upstream(response: proxy::BufferedResponse) -> Result<Response<Body>> {
    let (framed, payload) = unary_payload(&response.body)?;
    let mut message = BootstrapStatsigResponse::decode(payload)?;
    let mut config = serde_json::from_str::<Value>(&message.config)?;
    let retries_before = config
        .get("feature_gates")
        .and_then(|gates| gates.get(statsig_key(&config, AGENT_RETRIES_GATE)))
        .and_then(|gate| gate.get("value"))
        .and_then(Value::as_bool);
    let local_loop_before = config
        .get("feature_gates")
        .and_then(|gates| gates.get(statsig_key(&config, LOCAL_AGENT_LOOP_GATE)))
        .and_then(|gate| gate.get("value"))
        .and_then(Value::as_bool);
    configure_agent_gates(&mut config)?;
    tracing::info!(
        source = "upstream",
        framed,
        agent_retries_before = ?retries_before,
        agent_retries_after = true,
        agent_host_local_loop_before = ?local_loop_before,
        agent_host_local_loop_after = false,
        "Cursor diagnostic: Statsig bootstrap prepared"
    );
    message.config = serde_json::to_string(&config)?;
    Ok(response.with_body(encode_unary(&message, framed)))
}

fn local_response() -> Result<Response<Body>> {
    let generated_at_ms = chrono::Utc::now().timestamp_millis() as u64;
    let mut config = json!({
        "feature_gates": {},
        "dynamic_configs": {},
        "layer_configs": {},
        "user": {
            "userID": "local_ultra",
            "customIDs": { "localUserID": "local_ultra" }
        },
        "has_updates": true,
        "hash_used": "none",
        "sdkParams": {
            "stableID": "local_ultra",
            "disableDiagnosticsLogging": true
        },
        "time": generated_at_ms
    });
    configure_agent_gates(&mut config)?;
    tracing::info!(
        source = "local",
        agent_retries_after = true,
        agent_host_local_loop_after = false,
        "Cursor diagnostic: Statsig bootstrap prepared"
    );
    let message = BootstrapStatsigResponse {
        config: serde_json::to_string(&config)?,
        generated_at_ms,
    };
    let body = message.encode_to_vec();
    let mut response = Response::new(Body::from(body.clone()));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/proto"),
    );
    response.headers_mut().insert(
        header::CONTENT_LENGTH,
        body.len()
            .to_string()
            .parse()
            .expect("body length is a valid header value"),
    );
    Ok(response)
}

fn configure_agent_gates(config: &mut Value) -> Result<()> {
    let gate_values = [
        (statsig_key(config, AGENT_RETRIES_GATE), true),
        (statsig_key(config, LOCAL_AGENT_LOOP_GATE), false),
    ];
    let root = config
        .as_object_mut()
        .ok_or_else(|| Error::Protocol("Statsig bootstrap config must be an object".into()))?;
    let gates = root
        .entry("feature_gates")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| Error::Protocol("Statsig feature_gates must be an object".into()))?;
    for (name, enabled) in gate_values {
        gates.insert(name.clone(), local_gate(&name, enabled));
    }
    root.insert("has_updates".into(), Value::Bool(true));
    Ok(())
}

fn statsig_key(config: &Value, name: &str) -> String {
    match config.get("hash_used").and_then(Value::as_str) {
        Some("djb2") => djb2(name),
        Some("sha256") => STANDARD.encode(Sha256::digest(name.as_bytes())),
        _ => name.to_owned(),
    }
}

fn djb2(value: &str) -> String {
    value
        .encode_utf16()
        .fold(0_u32, |hash, character| {
            hash.wrapping_mul(31).wrapping_add(u32::from(character))
        })
        .to_string()
}

fn local_gate(name: &str, enabled: bool) -> Value {
    json!({
        "name": name,
        "value": enabled,
        "rule_id": LOCAL_RULE,
        "ruleID": LOCAL_RULE,
        "group_name": LOCAL_RULE,
        "groupName": LOCAL_RULE,
        "secondary_exposures": [],
        "secondaryExposures": [],
        "undelegated_secondary_exposures": [],
        "undelegatedSecondaryExposures": [],
        "is_device_based": false,
        "isDeviceBased": false,
        "id_type": "userID",
        "idType": "userID"
    })
}

fn unary_payload(body: &Bytes) -> Result<(bool, &[u8])> {
    if body.len() < 5 {
        return Ok((false, body));
    }
    let flags = body[0];
    let length = u32::from_be_bytes([body[1], body[2], body[3], body[4]]) as usize;
    if length != body.len() - 5 {
        return Ok((false, body));
    }
    if flags != 0 {
        return Err(Error::Protocol(format!(
            "cannot patch compressed or terminal Statsig frame: flags={flags}"
        )));
    }
    Ok((true, &body[5..]))
}

fn encode_unary(message: &impl Message, framed: bool) -> Bytes {
    let payload = message.encode_to_vec();
    if !framed {
        return Bytes::from(payload);
    }
    let mut output = BytesMut::with_capacity(5 + payload.len());
    output.put_u8(0);
    output.put_u32(payload.len() as u32);
    output.extend_from_slice(&payload);
    output.freeze()
}

#[cfg(test)]
mod tests {
    use axum::body::to_bytes;

    use super::*;

    #[test]
    fn account_bootstrap_keeps_the_server_agent_protocol_for_every_hash_mode() {
        for (hash, retries_key, loop_key) in [
            ("none", "nal_agent_retries", "agent_host_local_loop"),
            ("djb2", "101914982", "1967931573"),
            (
                "sha256",
                "ifvjhhnfrCEApPQP1Pgv02EJdaWKslFkfOhSOAezhxk=",
                "pL49GCl+NNaNm8gwzyKmb8/Gz7VEiiUMVG56Nk3v+d0=",
            ),
        ] {
            let mut config = json!({
                "hash_used": hash,
                "has_updates": false,
                "feature_gates": { "unrelated": { "value": true } },
                "dynamic_configs": { "kept": { "value": 7 } },
                "layer_configs": { "kept": { "value": false } },
                "user": { "userID": "synthetic-user" },
                "time": 123
            });
            config["feature_gates"][retries_key] = json!({ "value": false });
            config["feature_gates"][loop_key] = json!({ "value": true });
            let before = config.clone();

            configure_agent_gates(&mut config).unwrap();

            assert_eq!(config["feature_gates"][loop_key]["value"], false, "{hash}");
            assert_eq!(config["feature_gates"][retries_key]["value"], true);
            assert_eq!(config["has_updates"], true);
            assert_eq!(config["feature_gates"].as_object().unwrap().len(), 3);
            for key in [
                "hash_used",
                "dynamic_configs",
                "layer_configs",
                "user",
                "time",
            ] {
                assert_eq!(config[key], before[key]);
            }
            assert_eq!(
                config["feature_gates"]["unrelated"],
                before["feature_gates"]["unrelated"]
            );
        }
    }

    #[tokio::test]
    async fn patched_bootstrap_preserves_framing_and_upstream_metadata() {
        for framed in [false, true] {
            let config = json!({
                "hash_used": "none",
                "has_updates": false,
                "feature_gates": { "agent_host_local_loop": { "value": true } },
                "dynamic_configs": { "kept": { "value": 7 } },
                "user": { "userID": "synthetic-user" }
            });
            let message = BootstrapStatsigResponse {
                config: config.to_string(),
                generated_at_ms: 123,
            };
            let mut headers = axum::http::HeaderMap::new();
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/proto"),
            );
            headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("private"));
            let response = patch_upstream(proxy::BufferedResponse {
                status: StatusCode::OK,
                headers,
                body: encode_unary(&message, framed),
            })
            .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()[header::CACHE_CONTROL], "private");
            let length = response.headers()[header::CONTENT_LENGTH]
                .to_str()
                .unwrap()
                .parse::<usize>()
                .unwrap();
            let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
            assert_eq!(body.len(), length);
            let (actual_framed, payload) = unary_payload(&body).unwrap();
            assert_eq!(actual_framed, framed);
            let decoded = BootstrapStatsigResponse::decode(payload).unwrap();
            assert_eq!(decoded.generated_at_ms, message.generated_at_ms);
            let patched: Value = serde_json::from_str(&decoded.config).unwrap();
            assert_eq!(
                patched["feature_gates"]["agent_host_local_loop"]["value"],
                false
            );
            assert_eq!(patched["has_updates"], true);
            assert_eq!(patched["dynamic_configs"], config["dynamic_configs"]);
            assert_eq!(patched["user"], config["user"]);
        }
    }

    #[tokio::test]
    async fn local_bootstrap_uses_the_same_server_agent_protocol() {
        let response = local_response().unwrap();
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let message = BootstrapStatsigResponse::decode(body).unwrap();
        let config: Value = serde_json::from_str(&message.config).unwrap();
        assert_eq!(
            config["feature_gates"]["agent_host_local_loop"]["value"],
            false
        );
        assert_eq!(config["feature_gates"]["nal_agent_retries"]["value"], true);
        assert_eq!(config["has_updates"], true);
    }

    #[test]
    fn malformed_bootstrap_shapes_are_rejected() {
        for mut config in [json!(null), json!([]), json!({ "feature_gates": [] })] {
            assert!(configure_agent_gates(&mut config).is_err());
        }
    }
}

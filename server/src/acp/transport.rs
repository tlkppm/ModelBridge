use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::time::{timeout, timeout_at, Instant};

use super::AcpError;

pub(super) const MAX_FRAME_BYTES: usize = 1024 * 1024;
pub(super) const SETUP_TIMEOUT: Duration = Duration::from_secs(10);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) struct Transport<Reader, Writer> {
    reader: BufReader<Reader>,
    writer: Writer,
    line: Vec<u8>,
}

impl<Reader: AsyncRead + Unpin, Writer: AsyncWrite + Unpin> Transport<Reader, Writer> {
    pub(super) fn new(reader: Reader, writer: Writer) -> Self {
        Self {
            reader: BufReader::new(reader),
            writer,
            line: Vec::new(),
        }
    }

    pub(super) async fn call(
        &mut self,
        id: u64,
        method: &str,
        params: Value,
    ) -> Result<Value, AcpError> {
        let frame = encode(&request(id, method, params))?;
        let deadline = Instant::now() + SETUP_TIMEOUT;
        timeout_at(deadline, self.send(&frame)).await??;
        loop {
            let message = self.receive_before(deadline).await?;
            if let Some(result) =
                timeout_at(deadline, self.dispatch(message, Some(id), |_| {})).await??
            {
                return Ok(result);
            }
        }
    }

    pub(super) async fn drain_idle(&mut self, deadline: Instant) -> Result<(), AcpError> {
        loop {
            if Instant::now() >= deadline {
                return Err(AcpError::Timeout);
            }
            let available = tokio::select! {
                biased;
                message = self.receive() => Some(message),
                _ = std::future::ready(()) => None,
            };
            let message = match available {
                Some(message) => message?,
                None if self.line.is_empty() => return Ok(()),
                None => self.receive_before(deadline).await?,
            };
            timeout_at(deadline, self.dispatch(message, None, |_| {})).await??;
        }
    }

    async fn receive_before(&mut self, deadline: Instant) -> Result<Incoming, AcpError> {
        if Instant::now() >= deadline {
            return Err(AcpError::Timeout);
        }
        timeout_at(deadline, self.receive()).await?
    }

    pub(super) async fn send(&mut self, frame: &[u8]) -> Result<(), AcpError> {
        timeout(WRITE_TIMEOUT, async {
            self.writer.write_all(frame).await?;
            self.writer.flush().await
        })
        .await??;
        Ok(())
    }

    pub(super) async fn receive(&mut self) -> Result<Incoming, AcpError> {
        loop {
            let chunk = self.reader.fill_buf().await?;
            if chunk.is_empty() {
                return Err(if self.line.is_empty() {
                    AcpError::Eof
                } else {
                    AcpError::Protocol("truncated frame")
                });
            }
            let newline = chunk.iter().position(|byte| *byte == b'\n');
            let length = newline.unwrap_or(chunk.len());
            if length > MAX_FRAME_BYTES - self.line.len() {
                return Err(AcpError::FrameTooLarge);
            }
            self.line.extend_from_slice(&chunk[..length]);
            self.reader.consume(length + usize::from(newline.is_some()));
            if newline.is_some() {
                let value = serde_json::from_slice(&self.line)
                    .map_err(|_| AcpError::Protocol("invalid JSON frame"));
                self.line.clear();
                return Incoming::parse(value?);
            }
        }
    }

    pub(super) async fn dispatch(
        &mut self,
        message: Incoming,
        expected_id: Option<u64>,
        mut on_update: impl FnMut(&Value),
    ) -> Result<Option<Value>, AcpError> {
        match message {
            Incoming::Request { id, method } => {
                let reply = if method == "session/request_permission" {
                    json!({ "jsonrpc": "2.0", "id": id, "result": { "outcome": { "outcome": "cancelled" } } })
                } else {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": "Method not found" } })
                };
                self.send(&encode(&reply)?).await?;
            }
            Incoming::Notification { method, params } if method == "session/update" => {
                on_update(&params);
            }
            Incoming::Response { id, result }
                if expected_id.is_some() && expected_id == id.as_u64() =>
            {
                return result.map(Some).map_err(AcpError::Rpc);
            }
            _ => {}
        }
        Ok(None)
    }
}

pub(super) fn request(id: u64, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

pub(super) fn encode(message: &Value) -> Result<Vec<u8>, AcpError> {
    let mut frame =
        serde_json::to_vec(message).map_err(|_| AcpError::Protocol("invalid outgoing JSON"))?;
    if frame.len() > MAX_FRAME_BYTES {
        return Err(AcpError::FrameTooLarge);
    }
    frame.push(b'\n');
    Ok(frame)
}

pub(super) enum Incoming {
    Request {
        id: Value,
        method: String,
    },
    Notification {
        method: String,
        params: Value,
    },
    Response {
        id: Value,
        result: Result<Value, i64>,
    },
}

impl Incoming {
    fn parse(value: Value) -> Result<Self, AcpError> {
        let Value::Object(mut fields) = value else {
            return Err(AcpError::Protocol("expected a JSON-RPC object"));
        };
        if fields.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return Err(AcpError::Protocol("expected JSON-RPC 2.0"));
        }
        let id = fields.remove("id");
        if id
            .as_ref()
            .is_some_and(|id| !(id.is_string() || id.is_i64() || id.is_u64()))
        {
            return Err(AcpError::Protocol("invalid JSON-RPC ID"));
        }
        if let Some(method) = fields.remove("method") {
            let Value::String(method) = method else {
                return Err(AcpError::Protocol("invalid JSON-RPC method"));
            };
            if fields.contains_key("result") || fields.contains_key("error") {
                return Err(AcpError::Protocol("invalid JSON-RPC method envelope"));
            }
            return Ok(match id {
                Some(id) => Self::Request { id, method },
                None => Self::Notification {
                    method,
                    params: fields.remove("params").unwrap_or(Value::Null),
                },
            });
        }
        let id = id.ok_or(AcpError::Protocol("missing JSON-RPC response ID"))?;
        let result = match (fields.remove("result"), fields.remove("error")) {
            (Some(result), None) => Ok(result),
            (None, Some(error)) if error["message"].is_string() => Err(error["code"]
                .as_i64()
                .ok_or(AcpError::Protocol("invalid JSON-RPC error code"))?),
            _ => return Err(AcpError::Protocol("invalid JSON-RPC response")),
        };
        Ok(Self::Response { id, result })
    }
}

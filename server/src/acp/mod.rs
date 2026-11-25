mod transport;

#[cfg(test)]
mod tests;

use std::io;
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::time::{sleep_until, timeout_at, Instant};
use tokio_util::sync::CancellationToken;
use transport::{encode, request, Transport, MAX_FRAME_BYTES, SETUP_TIMEOUT};

const PROTOCOL_VERSION: u64 = 1;
const TURN_TIMEOUT: Duration = Duration::from_secs(120);
const CANCEL_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum AcpError {
    #[error("ACP requires an absolute UTF-8 working directory")]
    InvalidWorkingDirectory,
    #[error("unsupported ACP protocol version {0}")]
    UnsupportedVersion(u64),
    #[error("ACP frame exceeds the size limit")]
    FrameTooLarge,
    #[error("ACP transport reached EOF")]
    Eof,
    #[error("ACP transport is disconnected")]
    Disconnected,
    #[error("ACP operation timed out")]
    Timeout,
    #[error("invalid ACP message: {0}")]
    Protocol(&'static str),
    #[error("ACP JSON-RPC error {0}")]
    Rpc(i64),
    #[error("ACP transport I/O failed: {0}")]
    Io(#[from] io::Error),
}

impl From<tokio::time::error::Elapsed> for AcpError {
    fn from(_: tokio::time::error::Elapsed) -> Self {
        Self::Timeout
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    MaxTurnRequests,
    Refusal,
    Cancelled,
}

pub struct AcpClient<Reader, Writer> {
    transport: Option<Transport<Reader, Writer>>,
    session_id: String,
    next_id: u64,
}

impl<Reader: AsyncRead + Unpin, Writer: AsyncWrite + Unpin> AcpClient<Reader, Writer> {
    pub async fn connect(reader: Reader, writer: Writer, cwd: &Path) -> Result<Self, AcpError> {
        let cwd = cwd
            .to_str()
            .filter(|_| cwd.is_absolute())
            .ok_or(AcpError::InvalidWorkingDirectory)?;
        if cwd.len() > MAX_FRAME_BYTES {
            return Err(AcpError::FrameTooLarge);
        }

        let mut transport = Transport::new(reader, writer);
        let initialize = transport
            .call(
                1,
                "initialize",
                json!({ "protocolVersion": PROTOCOL_VERSION, "clientCapabilities": {} }),
            )
            .await?;
        let version = initialize["protocolVersion"]
            .as_u64()
            .ok_or(AcpError::Protocol("missing protocol version"))?;
        if version != PROTOCOL_VERSION {
            return Err(AcpError::UnsupportedVersion(version));
        }

        let session = transport
            .call(2, "session/new", json!({ "cwd": cwd, "mcpServers": [] }))
            .await?;
        let session_id = session["sessionId"]
            .as_str()
            .ok_or(AcpError::Protocol("missing session ID"))?;
        Ok(Self {
            transport: Some(transport),
            session_id: session_id.to_owned(),
            next_id: 3,
        })
    }

    pub async fn prompt(
        &mut self,
        text: &str,
        cancellation: &CancellationToken,
        on_text: impl FnMut(&str),
    ) -> Result<StopReason, AcpError> {
        if self.transport.is_none() {
            return Err(AcpError::Disconnected);
        }
        if cancellation.is_cancelled() {
            return Ok(StopReason::Cancelled);
        }
        if text.len() > MAX_FRAME_BYTES {
            return Err(AcpError::FrameTooLarge);
        }

        let id = self.next_id;
        let frame = encode(&request(
            id,
            "session/prompt",
            json!({ "sessionId": self.session_id, "prompt": [{ "type": "text", "text": text }] }),
        ))?;
        self.next_id = id
            .checked_add(1)
            .ok_or(AcpError::Protocol("request IDs exhausted"))?;
        let mut transport = self.transport.take().ok_or(AcpError::Disconnected)?;
        let result = Self::run_prompt(
            &mut transport,
            id,
            &frame,
            &self.session_id,
            cancellation,
            on_text,
        )
        .await;
        if matches!(result, Ok(_) | Err(AcpError::Rpc(_))) {
            self.transport = Some(transport);
        }
        result
    }

    async fn run_prompt(
        transport: &mut Transport<Reader, Writer>,
        id: u64,
        frame: &[u8],
        session_id: &str,
        cancellation: &CancellationToken,
        mut on_text: impl FnMut(&str),
    ) -> Result<StopReason, AcpError> {
        transport.drain_idle(Instant::now() + SETUP_TIMEOUT).await?;
        if cancellation.is_cancelled() {
            return Ok(StopReason::Cancelled);
        }
        transport.send(frame).await?;

        let mut deadline = Instant::now() + TURN_TIMEOUT;
        let mut cancelled = false;
        loop {
            let message = tokio::select! {
                biased;
                _ = sleep_until(deadline) => return Err(AcpError::Timeout),
                _ = cancellation.cancelled(), if !cancelled => {
                    cancelled = true;
                    deadline = deadline.min(Instant::now() + CANCEL_TIMEOUT);
                    let cancel = encode(&json!({
                        "jsonrpc": "2.0",
                        "method": "session/cancel",
                        "params": { "sessionId": session_id }
                    }))?;
                    timeout_at(deadline, transport.send(&cancel)).await??;
                    continue;
                }
                message = transport.receive() => message?,
            };
            let result = timeout_at(
                deadline,
                transport.dispatch(message, Some(id), |params| {
                    if cancelled {
                        return;
                    }
                    if let Some(text) = text_update(params, session_id) {
                        on_text(text);
                    }
                }),
            )
            .await??;
            if let Some(result) = result {
                return StopReason::deserialize(&result["stopReason"])
                    .map_err(|_| AcpError::Protocol("invalid stop reason"));
            }
        }
    }
}

fn text_update<'a>(params: &'a Value, session_id: &str) -> Option<&'a str> {
    let update = &params["update"];
    let content = &update["content"];
    if params["sessionId"].as_str() == Some(session_id)
        && update["sessionUpdate"].as_str() == Some("agent_message_chunk")
        && content["type"].as_str() == Some("text")
    {
        return content["text"].as_str();
    }
    None
}

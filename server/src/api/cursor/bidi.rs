//! Accepts ordered Cursor Bidi append requests and routes them by request_id.
use std::borrow::Cow;

use prost::Message;

use crate::{
    cursor::{
        conversation::TransportCommand,
        protocol::{
            events,
            proto::{agent::v1 as agent, aiserver::v1 as ai},
        },
        transport::{TransportParent, TransportRegistry},
    },
    Error, Result,
};

pub struct DecodedAppend {
    pub request_id: String,
    pub seqno: i64,
    pub message: agent::AgentClientMessage,
}

impl DecodedAppend {
    pub fn model_id(&self) -> Option<&str> {
        let agent::agent_client_message::Message::RunRequest(request) =
            self.message.message.as_ref()?
        else {
            return None;
        };
        request
            .requested_model
            .as_ref()
            .map(|model| model.model_id.as_str())
            .filter(|model| !model.is_empty())
            .or_else(|| {
                request
                    .model_details
                    .as_ref()
                    .map(|model| model.model_id.as_str())
                    .filter(|model| !model.is_empty())
            })
    }

    pub fn conversation_id(&self) -> Option<&str> {
        let agent::agent_client_message::Message::RunRequest(request) =
            self.message.message.as_ref()?
        else {
            return None;
        };
        request.conversation_id.as_deref()
    }

    pub fn is_background_task_completion(&self) -> bool {
        let Some(agent::agent_client_message::Message::RunRequest(request)) =
            self.message.message.as_ref()
        else {
            return false;
        };
        matches!(
            request
                .action
                .as_ref()
                .and_then(|action| action.action.as_ref()),
            Some(agent::conversation_action::Action::BackgroundTaskCompletionAction(_))
        )
    }

    pub fn trace_metadata(&self) -> serde_json::Value {
        let Some(message) = self.message.message.as_ref() else {
            return serde_json::json!({
                "append_seqno": self.seqno,
                "message_type": "empty",
            });
        };
        let agent::agent_client_message::Message::RunRequest(request) = message else {
            return serde_json::json!({
                "append_seqno": self.seqno,
                "message_type": client_message_type(message),
            });
        };
        let (action_type, history_messages, history_images) = request
            .action
            .as_ref()
            .and_then(|action| action.action.as_ref())
            .map(|action| match action {
                agent::conversation_action::Action::UserMessageAction(action) => {
                    let history = action.conversation_history.as_ref();
                    (
                        "user_message",
                        history.map_or(0, |history| history.messages.len()),
                        history.map_or(0, history_image_count),
                    )
                }
                agent::conversation_action::Action::BackgroundTaskCompletionAction(_) => {
                    ("background_task_completion", 0, 0)
                }
                agent::conversation_action::Action::ExecutePlanAction(_) => ("execute_plan", 0, 0),
                agent::conversation_action::Action::SummarizeAction(_) => ("summarize", 0, 0),
                _ => ("other", 0, 0),
            })
            .unwrap_or(("none", 0, 0));
        let state = request.conversation_state.as_ref();
        serde_json::json!({
            "append_seqno": self.seqno,
            "message_type": "run_request",
            "conversation_id": request.conversation_id,
            "model_id": self.model_id(),
            "action_type": action_type,
            "conversation_history_messages": history_messages,
            "conversation_history_images": history_images,
            "root_message_count": state.map_or(0, |state| state.root_prompt_messages_json.len()),
            "turn_count": state.map_or(0, |state| state.turns.len()),
            "prefetched_blob_count": request.pre_fetched_blobs.len(),
        })
    }
}

fn client_message_type(message: &agent::agent_client_message::Message) -> &'static str {
    use agent::agent_client_message::Message;
    match message {
        Message::RunRequest(_) => "run_request",
        Message::ExecClientMessage(_) => "exec_client_message",
        Message::ExecClientControlMessage(_) => "exec_client_control_message",
        Message::KvClientMessage(_) => "kv_client_message",
        Message::ConversationAction(_) => "conversation_action",
        Message::InteractionResponse(_) => "interaction_response",
        Message::ClientHeartbeat(_) => "client_heartbeat",
        Message::PrewarmRequest(_) => "prewarm_request",
    }
}

fn history_image_count(history: &agent::ConversationHistory) -> usize {
    use agent::{
        conversation_history_message::Message,
        conversation_history_tool_result_content::Content as ToolContent,
        conversation_history_user_content::Content as UserContent,
    };
    history
        .messages
        .iter()
        .map(|message| match message.message.as_ref() {
            Some(Message::User(user)) => user
                .content
                .iter()
                .filter(|content| matches!(content.content, Some(UserContent::Image(_))))
                .count(),
            Some(Message::Tool(tool)) => tool
                .content
                .iter()
                .filter(|content| matches!(content.content, Some(ToolContent::Image(_))))
                .count(),
            _ => 0,
        })
        .sum()
}

pub fn request_id(request: &ai::BidiAppendRequest) -> Result<&str> {
    request
        .request_id
        .as_ref()
        .map(|id| id.request_id.as_str())
        .filter(|id| !id.is_empty())
        .ok_or_else(|| Error::Protocol("BidiAppend request_id is required".into()))
}

#[derive(Clone, PartialEq, Message)]
struct InferenceClientMessage {
    #[prost(message, optional, tag = "1")]
    run_request: Option<InferenceRunRequest>,
}

#[derive(Clone, PartialEq, Message)]
struct InferenceRunRequest {
    #[prost(string, tag = "1")]
    conversation_id: String,
}

pub fn is_inference_run(request: &ai::BidiAppendRequest) -> bool {
    if request.append_seqno != 0 {
        return false;
    }
    let Ok(payload) = payload(request) else {
        return false;
    };
    let Ok(message) = InferenceClientMessage::decode(payload.as_ref()) else {
        return false;
    };
    message
        .run_request
        .is_some_and(|request| uuid::Uuid::parse_str(&request.conversation_id).is_ok())
}

pub(super) fn payload(request: &ai::BidiAppendRequest) -> Result<Cow<'_, [u8]>> {
    if !request.data_binary.is_empty() {
        if !request.data.is_empty() {
            return Err(Error::Protocol(
                "BidiAppend cannot contain both data and data_binary".into(),
            ));
        }
        return Ok(Cow::Borrowed(&request.data_binary));
    }
    hex::decode(&request.data)
        .map(Cow::Owned)
        .map_err(|error| Error::Protocol(format!("invalid BidiAppend hex: {error}")))
}

pub fn decode(request: &ai::BidiAppendRequest) -> Result<DecodedAppend> {
    let request_id = request_id(request)?;
    let payload = payload(request)?;
    if payload.is_empty() {
        return Err(Error::Protocol(
            "BidiAppend contains no AgentClientMessage".into(),
        ));
    }
    Ok(DecodedAppend {
        request_id: request_id.into(),
        seqno: request.append_seqno,
        message: agent::AgentClientMessage::decode(payload.as_ref())?,
    })
}

pub async fn append(
    registry: &TransportRegistry,
    request: DecodedAppend,
    parent: Option<TransportParent>,
) -> Result<ai::BidiAppendResponse> {
    let replace_closing = request.model_id().is_some();
    let handle = registry
        .get_or_create_for_append(&request.request_id, replace_closing)
        .await?;
    let _admission = handle.admit()?;
    if let Some(conversation_id) = request.conversation_id() {
        handle.set_conversation_id(conversation_id)?;
    }
    if let Some(parent) = parent {
        handle.set_parent(parent)?;
    }
    if matches!(
        request.message.message.as_ref(),
        Some(agent::agent_client_message::Message::ClientHeartbeat(_))
    ) {
        handle.emit(&events::heartbeat())?;
    }
    handle
        .command(TransportCommand::Append {
            seqno: request.seqno,
            message: Box::new(request.message),
        })
        .await?;
    Ok(ai::BidiAppendResponse {})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, PartialEq, Message)]
    struct RunInferenceRunRequest {
        #[prost(string, tag = "1")]
        conversation_id: String,
    }

    #[derive(Clone, PartialEq, Message)]
    struct RunInferenceClientMessage {
        #[prost(message, optional, tag = "1")]
        run_request: Option<RunInferenceRunRequest>,
    }

    #[test]
    fn inference_classification_requires_an_initial_inference_run() {
        let agent_message = agent::AgentClientMessage {
            message: Some(agent::agent_client_message::Message::RunRequest(
                agent::AgentRunRequest {
                    conversation_state: Some(agent::ConversationStateStructure::default()),
                    conversation_id: Some("a0000000-0000-0000-0000-000000000000".into()),
                    ..Default::default()
                },
            )),
        };
        let mut request = ai::BidiAppendRequest {
            data: hex::encode(agent_message.encode_to_vec()),
            ..Default::default()
        };
        assert!(!is_inference_run(&request));
        request.data = hex::encode(
            RunInferenceClientMessage {
                run_request: Some(RunInferenceRunRequest {
                    conversation_id: "a0000000-0000-0000-0000-000000000000".into(),
                }),
            }
            .encode_to_vec(),
        );
        assert!(is_inference_run(&request));
        request.append_seqno = 1;
        assert!(!is_inference_run(&request));
        request.append_seqno = 0;
        request.data_binary = vec![1];
        assert!(!is_inference_run(&request));
        request.data_binary.clear();
        request.data = "0e".into();
        assert!(!is_inference_run(&request));
    }

    #[test]
    fn binary_agent_payload_decodes_like_hex() {
        let message = agent::AgentClientMessage {
            message: Some(agent::agent_client_message::Message::ClientHeartbeat(
                Default::default(),
            )),
        };
        let request = ai::BidiAppendRequest {
            request_id: Some(ai::BidiRequestId {
                request_id: "binary-agent".into(),
            }),
            append_seqno: 3,
            data_binary: message.encode_to_vec(),
            ..Default::default()
        };
        let decoded = decode(&request).expect("Cursor binary Agent messages must be accepted");
        assert_eq!(decoded.request_id, "binary-agent");
        assert_eq!(decoded.seqno, 3);
        assert_eq!(decoded.message, message);
    }

    #[test]
    fn binary_inference_run_is_not_decoded_as_agent() {
        let request = ai::BidiAppendRequest {
            data_binary: RunInferenceClientMessage {
                run_request: Some(RunInferenceRunRequest {
                    conversation_id: "a0000000-0000-0000-0000-000000000000".into(),
                }),
            }
            .encode_to_vec(),
            ..Default::default()
        };
        assert!(is_inference_run(&request));
    }

    #[test]
    fn conflicting_payload_encodings_are_rejected() {
        let request = ai::BidiAppendRequest {
            request_id: Some(ai::BidiRequestId {
                request_id: "ambiguous-agent".into(),
            }),
            data: "1a0408011a00".into(),
            data_binary: vec![0x32, 0],
            ..Default::default()
        };
        assert!(
            matches!(decode(&request), Err(Error::Protocol(error)) if error.contains("both data and data_binary"))
        );
        assert!(!is_inference_run(&request));
    }

    #[test]
    fn local_inference_protocol_reproduces_reported_agent_decode_errors() {
        for (conversation_id, expected) in [
            (
                "00000000-0000-0000-0000-000000000000",
                "ConversationStateStructure.summary",
            ),
            (
                "f0000000-0000-0000-0000-000000000000",
                "invalid wire type value: 6",
            ),
        ] {
            let payload = RunInferenceClientMessage {
                run_request: Some(RunInferenceRunRequest {
                    conversation_id: conversation_id.into(),
                }),
            }
            .encode_to_vec();
            let request = ai::BidiAppendRequest {
                request_id: Some(ai::BidiRequestId {
                    request_id: "synthetic-request".into(),
                }),
                data: hex::encode(payload),
                ..Default::default()
            };
            let Err(Error::Decode(error)) = decode(&request) else {
                panic!("a local inference message must not decode as an Agent message");
            };
            assert!(error.to_string().contains(expected), "{error}");
        }
    }
}

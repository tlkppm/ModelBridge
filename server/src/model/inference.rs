//! Defines provider-independent model requests and streaming responses.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{ModelSpec, ProjectedContent, ProjectedMessage, ProviderReplayState, ToolDefinition};

const PROVIDER_TOOL_CALL_ID_MAX_CHARS: usize = 64;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PromptSpec {
    pub instructions: String,
    pub tools: Vec<ToolDefinition>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ModelRequest {
    pub prompt: PromptSpec,
    pub model: ModelSpec,
    pub history: Vec<ProjectedMessage>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ModelInvocation {
    pub call_id: String,
    pub run_id: String,
    pub conversation_id: String,
    pub provider_call_index: u64,
    pub request: ModelRequest,
}

pub(crate) struct ResponsesReplayScope(String);

impl ResponsesReplayScope {
    const KIND: &str = "openai_responses";
    const PREFIX: &str = "openai_responses:";

    pub(crate) fn new(parts: impl IntoIterator<Item = impl AsRef<[u8]>>) -> Self {
        let mut digest = Sha256::new();
        for part in parts {
            let part = part.as_ref();
            digest.update((part.len() as u64).to_be_bytes());
            digest.update(part);
        }
        Self(format!("{}{:x}", Self::PREFIX, digest.finalize()))
    }

    pub(crate) fn restore(&self, history: &mut [ProjectedMessage]) {
        for message in history {
            let ProjectedContent::Assistant { replay_state, .. } = &mut message.content else {
                continue;
            };
            let Some(state) = replay_state else {
                continue;
            };
            if state.provider_kind == self.0 {
                state.provider_kind = Self::KIND.into();
            } else if state.provider_kind == Self::KIND
                || state.provider_kind.starts_with(Self::PREFIX)
            {
                *replay_state = None;
            }
        }
    }

    pub(crate) fn bind(&self, state: &mut ProviderReplayState) {
        if state.provider_kind == Self::KIND {
            state.provider_kind.clone_from(&self.0);
        }
    }
}

pub(crate) fn normalize_provider_tool_call_ids(history: &mut [ProjectedMessage]) {
    for message in history {
        match &mut message.content {
            ProjectedContent::Assistant { calls, .. } => {
                for call in calls {
                    truncate_tool_call_id(&mut call.call_id);
                }
            }
            ProjectedContent::ToolResult(result) => {
                truncate_tool_call_id(&mut result.call_id);
            }
            ProjectedContent::Parts(_) => {}
        }
    }
}

fn truncate_tool_call_id(call_id: &mut String) {
    if let Some((end, _)) = call_id.char_indices().nth(PROVIDER_TOOL_CALL_ID_MAX_CHARS) {
        call_id.truncate(end);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Role, ToolCallContent, ToolResultContent};
    use serde_json::json;

    #[test]
    fn responses_replay_is_restored_only_for_its_origin() {
        let current = ResponsesReplayScope::new(["plugin", "codex", "model", "account-one"]);
        let same = ResponsesReplayScope::new(["plugin", "codex", "model", "account-one"]);
        let other_account = ResponsesReplayScope::new(["plugin", "codex", "model", "account-two"]);
        let http = ResponsesReplayScope::new(["http", "http://localhost", "key", "model"]);
        let assistant = |replay_state| ProjectedMessage {
            message_id: "assistant".into(),
            role: Role::Assistant,
            content: ProjectedContent::Assistant {
                text: "answer".into(),
                thinking: "thought".into(),
                replay_state,
                calls: vec![ToolCallContent {
                    index: 0,
                    call_id: "call_1".into(),
                    name: "lookup".into(),
                    arguments: json!({"key": "value"}),
                }],
            },
        };
        let tool = ProjectedMessage {
            message_id: "result".into(),
            role: Role::Tool,
            content: ProjectedContent::ToolResult(ToolResultContent {
                call_id: "call_1".into(),
                name: "lookup".into(),
                content: "result".into(),
                is_error: false,
                image: None,
                provider_parts: Vec::new(),
            }),
        };
        let original = ProviderReplayState {
            provider_kind: ResponsesReplayScope::KIND.into(),
            value: json!([{"type":"reasoning","encrypted_content":"opaque"}]),
        };
        let mut bound = original.clone();
        current.bind(&mut bound);
        let mut history = vec![assistant(Some(bound.clone())), tool.clone()];
        same.restore(&mut history);
        assert_eq!(history, [assistant(Some(original.clone())), tool.clone()]);

        let mut http_bound = original.clone();
        http.bind(&mut http_bound);
        for (scope, state) in [
            (&other_account, bound.clone()),
            (&http, bound),
            (&same, http_bound),
            (&same, original),
        ] {
            let mut history = vec![assistant(Some(state)), tool.clone()];
            scope.restore(&mut history);
            assert_eq!(history, [assistant(None), tool.clone()]);
        }

        let anthropic = assistant(Some(ProviderReplayState {
            provider_kind: "anthropic".into(),
            value: json!({"signature":"opaque"}),
        }));
        let mut history = vec![anthropic.clone()];
        same.restore(&mut history);
        assert_eq!(history, [anthropic]);
    }
}

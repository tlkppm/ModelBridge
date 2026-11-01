use super::*;

fn request(text: &str, message_id: &str, subagent: Option<&str>) -> pb::AgentRunRequest {
    pb::AgentRunRequest {
        conversation_id: Some("child-conversation".into()),
        subagent_type_name: subagent.map(str::to_owned),
        action: Some(pb::ConversationAction {
            action: Some(pb::conversation_action::Action::UserMessageAction(
                pb::UserMessageAction {
                    user_message: Some(pb::UserMessage {
                        text: text.into(),
                        message_id: message_id.into(),
                        mode: pb::AgentMode::Agent as i32,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            )),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn project(request: &pb::AgentRunRequest) -> Result<ActionProjection> {
    let conversation_id =
        ConversationId::new(request.conversation_id.as_deref().unwrap_or("test-request"));
    action(request, &conversation_id)
}

#[test]
fn subagent_without_message_id_receives_a_stable_user_identity() {
    let request = request("Inspect the project", "", Some("generalPurpose"));
    let projection = project(&request).unwrap();
    let user = projection.turn_user.unwrap();
    assert!(projection.starts_turn);
    assert!(user.message_id.starts_with("subagent-user:"));
    assert_eq!(user.text, "Inspect the project");
    assert_eq!(
        projection.input_id,
        Some(format!("cursor:user:{}", user.message_id))
    );
    let Some(pb::conversation_action::Action::UserMessageAction(original)) =
        request.action.unwrap().action
    else {
        panic!("expected a user message action")
    };
    assert!(original.user_message.unwrap().message_id.is_empty());
}

#[test]
fn main_conversation_still_requires_a_user_message_id() {
    assert!(matches!(
        project(&request("Inspect the project", "", None)),
        Err(Error::Protocol(error)) if error == "Cursor user message action has no message_id"
    ));
}

#[test]
fn existing_user_message_ids_are_preserved() {
    for subagent in [None, Some("generalPurpose")] {
        let projection =
            project(&request("Inspect the project", "client-message", subagent)).unwrap();
        assert_eq!(
            projection.input_id.as_deref(),
            Some("cursor:user:client-message")
        );
        assert_eq!(projection.turn_user.unwrap().message_id, "client-message");
    }
}

#[test]
fn subagent_retry_identity_does_not_depend_on_execution_metadata() {
    let mut first = request("Inspect the project", "", Some("generalPurpose"));
    first.run_id = Some("first-run".into());
    first.agent_session_id = Some("first-session".into());
    let mut retry = first.clone();
    retry.run_id = Some("retry-run".into());
    retry.agent_session_id = Some("retry-session".into());
    assert_eq!(
        project(&first).unwrap().input_id,
        project(&retry).unwrap().input_id
    );
}

#[test]
fn subagent_conversations_and_prompts_have_distinct_input_identities() {
    let first = request("Inspect the project", "", Some("generalPurpose"));
    let mut other_conversation = first.clone();
    other_conversation.conversation_id = Some("another-child-conversation".into());
    let other_prompt = request("Inspect the tests", "", Some("generalPurpose"));
    let identity = project(&first).unwrap().input_id;
    assert_ne!(identity, project(&other_conversation).unwrap().input_id);
    assert_ne!(identity, project(&other_prompt).unwrap().input_id);
}

#[test]
fn empty_checkpoint_and_absent_checkpoint_have_the_same_subagent_identity() {
    let first = request("Inspect the project", "", Some("generalPurpose"));
    let mut retry = first.clone();
    retry.conversation_state = Some(pb::ConversationStateStructure::default());
    assert_eq!(
        project(&first).unwrap().input_id,
        project(&retry).unwrap().input_id
    );
}

#[test]
fn subagent_follow_up_identity_changes_with_checkpoint_history() {
    let first = request("continue", "", Some("generalPurpose"));
    let identity = project(&first).unwrap().input_id;
    for state in [
        pb::ConversationStateStructure {
            root_prompt_messages_json: vec![vec![1]],
            ..Default::default()
        },
        pb::ConversationStateStructure {
            turns: vec![vec![2]],
            ..Default::default()
        },
        pb::ConversationStateStructure {
            summary: Some(vec![3]),
            ..Default::default()
        },
    ] {
        let mut follow_up = first.clone();
        follow_up.conversation_state = Some(state);
        assert_ne!(identity, project(&follow_up).unwrap().input_id);
    }
}

#[test]
fn mutable_checkpoint_maps_do_not_change_subagent_input_identity() {
    let mut first = request("Inspect the project", "", Some("generalPurpose"));
    let mut state = pb::ConversationStateStructure::default();
    state.file_states.insert("first.rs".into(), vec![1]);
    state.file_states.insert("second.rs".into(), vec![2]);
    first.conversation_state = Some(state);
    let mut retry = first.clone();
    let state = retry.conversation_state.as_mut().unwrap();
    state.file_states.clear();
    state.file_states.insert("second.rs".into(), vec![3]);
    state.file_states.insert("first.rs".into(), vec![4]);
    assert_eq!(
        project(&first).unwrap().input_id,
        project(&retry).unwrap().input_id
    );
}

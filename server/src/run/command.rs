//! Defines Run commands and their explicit delivery results.

use tokio::sync::oneshot;

use crate::{
    cursor::protocol::proto::agent::v1::BackgroundTaskCompletion,
    model::{CanonicalMessage, ToolResult},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandResult {
    Applied,
    Duplicate,
    RunClosing,
    RunEnded,
    StaleTarget,
}

#[derive(Debug)]
pub struct MessageBatch {
    pub event_id: String,
    pub messages: Vec<CanonicalMessage>,
    pub background_completions: Vec<BackgroundTaskCompletion>,
    pub result: oneshot::Sender<CommandResult>,
}

impl MessageBatch {
    pub fn complete(self, result: CommandResult) {
        let _ = self.result.send(result);
    }
}

#[derive(Debug)]
pub enum RunCommand {
    ToolResult(ToolResult),
    InsertMessages(MessageBatch),
    BreakMessages(MessageBatch),
    Cancel,
}

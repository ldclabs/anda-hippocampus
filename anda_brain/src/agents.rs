mod formation;
mod maintenance;
pub mod prompts;
mod recall;

use anda_core::{BoxError, ContentPart, Document, Message, Principal};
use anda_db::schema::DocumentId;
use anda_engine::{
    context::CompletionRunner,
    memory::{Conversation, ConversationStatus},
};
use parking_lot::RwLock;
use std::collections::VecDeque;

pub use formation::*;
pub use maintenance::*;
pub use recall::*;

#[async_trait::async_trait]
pub trait BrainHook: Send + Sync {
    fn is_maintenance_processing(&self) -> bool;
    async fn on_conversation_end(&self, agent_name: &str, conversation: &Conversation);
    async fn try_start_formation(&self);
    async fn try_start_maintenance(&self, formation_id: DocumentId) -> Option<DocumentId>;
}

/// Principal ID: uuc56-gyb
pub static SELF_USER_ID: Principal = Principal::from_slice(&[1]);
pub static SYSTEM_PROMPT_DYNAMIC_BOUNDARY: &str = "__SYSTEM_PROMPT_DYNAMIC_BOUNDARY__";
const COMPACTION_CONTINUE_PROMPT: &str = "Continue the active memory-agent work from the compaction handoff. The handoff contains the conversation state immediately before compaction.";

fn queued_runner_tokens(runner: &CompletionRunner) -> u64 {
    runner
        .steering_message_iter()
        .chain(runner.follow_up_message_iter())
        .map(|part| part.estimated_tokens() as u64)
        .sum()
}

pub(super) async fn compact_runner_if_needed(
    runner: &mut CompletionRunner,
    extra_pending_tokens: u64,
    continue_after_compaction: bool,
) -> Result<bool, BoxError> {
    if !runner
        .needs_compaction_with(|| queued_runner_tokens(runner).saturating_add(extra_pending_tokens))
    {
        return Ok(false);
    }

    let (mut compacted, output) = runner.handoff(None).await?;
    compacted.accumulate(&output.usage);
    compacted.accumulate_tools_usage(&output.tools_usage);
    if continue_after_compaction {
        compacted.follow_up(ContentPart::from(COMPACTION_CONTINUE_PROMPT.to_string()));
    }
    *runner = compacted;
    Ok(true)
}

pub(super) fn push_completed_history(
    history: &RwLock<VecDeque<Document>>,
    conversation: &Conversation,
    max_len: usize,
) {
    if conversation.status != ConversationStatus::Completed || max_len == 0 {
        return;
    }

    let doc: Document = conversation.clone().into();
    let mut history = history.write();
    history.push_back(doc);
    let len = history.len();
    if len > max_len {
        history.drain(0..(len - max_len));
    }
}

pub(super) fn append_runner_history(
    conversation: &mut Conversation,
    chat_history: &[Message],
    persisted_runner_history_len: &mut usize,
    replace_existing: &mut bool,
) {
    if chat_history.is_empty() {
        return;
    }

    if *replace_existing {
        conversation.messages.clear();
        *replace_existing = false;
    }

    // Runner output is cumulative only within the current runner. After compaction,
    // the new runner starts from the handoff summary rather than the old full history.
    let incoming_len = chat_history.len();
    let new_messages = if incoming_len >= *persisted_runner_history_len {
        chat_history[*persisted_runner_history_len..].to_vec()
    } else {
        chat_history.to_vec()
    };
    conversation.append_messages(new_messages);
    *persisted_runner_history_len = incoming_len;
}

#[cfg(test)]
mod tests {
    use super::{append_runner_history, push_completed_history};
    use anda_core::Message;
    use anda_engine::memory::{Conversation, ConversationStatus};
    use parking_lot::RwLock;
    use std::collections::VecDeque;

    #[test]
    fn push_completed_history_ignores_working_conversations_and_caps_length() {
        let history = RwLock::new(VecDeque::new());
        let mut conversation = Conversation {
            _id: 1,
            status: ConversationStatus::Working,
            ..Default::default()
        };

        push_completed_history(&history, &conversation, 2);
        assert!(history.read().is_empty());

        conversation.status = ConversationStatus::Completed;
        push_completed_history(&history, &conversation, 2);

        conversation._id = 2;
        push_completed_history(&history, &conversation, 2);

        conversation._id = 3;
        push_completed_history(&history, &conversation, 2);

        assert_eq!(history.read().len(), 2);
    }

    #[test]
    fn append_runner_history_appends_after_runner_reset_without_clearing() {
        let mut conversation = Conversation::default();
        let mut persisted_runner_history_len = 0;
        let mut replace_existing = true;
        conversation.append_messages(vec![Message {
            role: "user".to_string(),
            content: vec!["original input".to_string().into()],
            ..Default::default()
        }]);

        append_runner_history(
            &mut conversation,
            &[Message {
                role: "assistant".to_string(),
                content: vec!["first runner draft".to_string().into()],
                ..Default::default()
            }],
            &mut persisted_runner_history_len,
            &mut replace_existing,
        );
        assert_eq!(conversation.messages.len(), 1);

        persisted_runner_history_len = 0;
        replace_existing = false;
        append_runner_history(
            &mut conversation,
            &[Message {
                role: "assistant".to_string(),
                content: vec!["compacted runner summary".to_string().into()],
                ..Default::default()
            }],
            &mut persisted_runner_history_len,
            &mut replace_existing,
        );

        let messages = serde_json::to_string(&conversation.messages).unwrap();
        assert!(messages.contains("first runner draft"));
        assert!(messages.contains("compacted runner summary"));
        assert!(!messages.contains("original input"));
    }
}

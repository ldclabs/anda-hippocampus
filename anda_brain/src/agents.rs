mod formation;
mod maintenance;
pub mod prompts;
mod recall;

use anda_core::{BoxError, ContentPart, Document, Message, Principal, Usage};
use anda_db::schema::DocumentId;
use anda_engine::{
    context::CompletionRunner,
    memory::{Conversation, ConversationStatus},
    unix_ms,
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
const COMPACTION_CONTINUE_PROMPT: &str = "Continue the active memory-agent work from the compaction handoff. The handoff contains the conversation state immediately before compaction.";

/// Persist in-flight (non-terminal) runner turns to the conversation store
/// only every N turns.
///
/// `Conversation::to_changes` re-encodes the full message array as CBOR and
/// rewrites the whole document, so persisting every turn costs O(turns^2)
/// over a long session. `Conversation::to_delta` cannot replace it: it is a
/// read-side view for the incremental fetch API (`GetConversationDelta`) and
/// there is no delta-write API in AndaDB. Terminal statuses are always
/// persisted immediately; a crash between throttled snapshots loses at most
/// N-1 turns of observability history, while crash recovery is unaffected
/// because reprocessing always restarts from the first (input) message of the
/// persisted conversation.
pub(super) const PERSIST_EVERY_N_TURNS: usize = 5;

/// Hard guardrails for the formation/maintenance runner loops (review P2-3).
///
/// A model that keeps emitting tool calls without converging would otherwise
/// run forever: the runner compacts every ~81 turns and simply continues, so
/// the agent's processing flag stays set and the space's memory system stalls
/// (formation's queue freezes; maintenance blocks formation entirely). Both
/// agents legitimately need far more turns than recall
/// (`RECALL_MAX_MODEL_TURNS` = 7 + 180s), so these caps are intentionally
/// loose. Exceeding either budget marks the conversation Failed through the
/// host's `mark_failed`, which reuses each agent's existing failure path
/// (formation: the Failed retry path; maintenance: releases the processing
/// slot through the guard/hook flow). Budgets are checked between turns so an
/// in-flight KIP write turn is never cancelled halfway.
pub(super) const RUNNER_MAX_MODEL_TURNS: usize = 200;
pub(super) const RUNNER_MAX_WALL_CLOCK_MS: u64 = 30 * 60 * 1000;

/// Control flow returned by [`RunnerHost::after_turn`].
pub(super) enum RunnerFlow {
    /// Keep looping (the host may have queued a follow-up).
    Continue,
    /// The conversation converged; leave the runner loop cleanly.
    Break,
}

/// Per-agent seams of the shared formation/maintenance runner loop
/// ([`drive_runner_loop`]). Everything else — budget guards, the compaction
/// arm, turn accounting, `append_runner_history`, the status three-state, the
/// persistence throttle, the single failure exit with usage backfill and the
/// tail Working flush — is identical between the two agents and lives in the
/// driver. recall deliberately does not use this skeleton: its timeout
/// wrapper and failed_output semantics do not fit these seams.
pub(super) trait RunnerHost {
    /// Agent label used in budget-exceeded failure reasons.
    fn label(&self) -> &'static str;

    /// The agent's completed-conversation ring served as model context.
    fn history(&self) -> &RwLock<VecDeque<Document>>;

    /// Persists the current full conversation snapshot; failures are logged
    /// by the host and must not interrupt the processing loop.
    async fn persist_snapshot(&self, conversation: &Conversation);

    /// Marks the conversation Failed with `reason` and persists it.
    async fn mark_failed(&self, conversation: &mut Conversation, reason: String);

    /// Host-specific convergence predicate for the current turn (formation
    /// also treats an idle runner as done once the review pass has run).
    fn turn_is_done(&self, runner: &CompletionRunner) -> bool;

    /// Runs on every successful (non-failed) turn, before the completed
    /// history push (formation clears `failed_reason` so the Failed-retry
    /// path converges to a clean snapshot; maintenance leaves it untouched).
    fn on_turn_success(&self, conversation: &mut Conversation);

    /// Runs at the end of every non-terminal turn, after persistence. The
    /// host decides whether the loop ends now (formation injects the pending
    /// review follow-up when the runner idles and otherwise breaks as soon as
    /// it is done) or defers to the runner's own `Ok(None)` exit on the next
    /// iteration (maintenance).
    fn after_turn(&mut self, runner: &mut CompletionRunner, is_done: bool) -> RunnerFlow;
}

/// Shared runner loop driving one formation/maintenance conversation to a
/// terminal state. Every failure exits through the labeled break so usage
/// backfill and `mark_failed` live at exactly one place below the loop.
pub(super) async fn drive_runner_loop<H: RunnerHost>(
    host: &mut H,
    runner: &mut CompletionRunner,
    conversation: &mut Conversation,
) {
    let started_at_ms = unix_ms();
    let mut replace_initial_input = true;
    let mut persisted_runner_history_len = 0;
    let mut total_model_turns = 0usize;
    let mut accounted_runner_turns = 0usize;
    let mut unpersisted_turns = 0usize;
    let failure: Option<String> = 'run: {
        loop {
            // Guardrails against a non-converging tool loop; see
            // RUNNER_MAX_MODEL_TURNS. Exceeding a budget takes the host's
            // existing mark_failed path.
            if total_model_turns >= RUNNER_MAX_MODEL_TURNS {
                break 'run Some(format!(
                    "{} exceeded model turn limit of {}",
                    host.label(),
                    RUNNER_MAX_MODEL_TURNS
                ));
            }
            if unix_ms().saturating_sub(started_at_ms) >= RUNNER_MAX_WALL_CLOCK_MS {
                break 'run Some(format!(
                    "{} exceeded wall-clock budget of {} seconds",
                    host.label(),
                    RUNNER_MAX_WALL_CLOCK_MS / 1000
                ));
            }

            match compact_runner_if_needed(runner).await {
                Ok(true) => {
                    // The compaction handoff consumed one model turn, and the
                    // replacement runner restarts its own turn counter.
                    total_model_turns = total_model_turns.saturating_add(1);
                    accounted_runner_turns = runner.turns();
                    persisted_runner_history_len = 0;
                    replace_initial_input = false;
                }
                Ok(false) => {}
                Err(err) => break 'run Some(format!("CompletionRunner error: {err:?}")),
            }

            match runner.next().await {
                Ok(None) => break 'run None,
                Ok(Some(res)) => {
                    let runner_turns = runner.turns();
                    total_model_turns = total_model_turns
                        .saturating_add(runner_turns.saturating_sub(accounted_runner_turns));
                    accounted_runner_turns = runner_turns;

                    let now_ms = unix_ms();
                    let is_done = host.turn_is_done(runner);

                    append_runner_history(
                        conversation,
                        &res.chat_history,
                        &mut persisted_runner_history_len,
                        &mut replace_initial_input,
                    );

                    conversation.status = if res.failed_reason.is_some() {
                        ConversationStatus::Failed
                    } else if is_done {
                        ConversationStatus::Completed
                    } else {
                        ConversationStatus::Working
                    };
                    conversation.usage = res.usage;
                    conversation.updated_at = now_ms;

                    if let Some(failed_reason) = res.failed_reason {
                        conversation.failed_reason = Some(failed_reason);
                    } else {
                        host.on_turn_success(conversation);
                        push_completed_history(host.history(), conversation, 2);
                    }

                    // Persisting rewrites the full message array (O(turns^2)
                    // over a session), so intermediate Working turns are
                    // throttled; terminal statuses always persist. See
                    // PERSIST_EVERY_N_TURNS.
                    unpersisted_turns = unpersisted_turns.saturating_add(1);
                    if conversation.status != ConversationStatus::Working
                        || unpersisted_turns >= PERSIST_EVERY_N_TURNS
                    {
                        host.persist_snapshot(conversation).await;
                        unpersisted_turns = 0;
                    }

                    if conversation.status == ConversationStatus::Cancelled
                        || conversation.status == ConversationStatus::Failed
                    {
                        break 'run None;
                    }

                    if let RunnerFlow::Break = host.after_turn(runner, is_done) {
                        break 'run None;
                    }
                }
                Err(err) => break 'run Some(format!("CompletionRunner error: {err:?}")),
            }
        }
    };

    // Single failure exit. The usage snapshot happens after the error
    // occurred: failure can strike after usage was accumulated but before it
    // was copied from a runner output (e.g. a compaction handoff consumed ~a
    // full context window of input tokens and the very next call errors), so
    // the runner's running total is backfilled here, like recall does, or
    // those tokens vanish from the agent's usage ledger. Success exits must
    // NOT backfill: the runner's final_output/final_idle_output mem::take
    // total_usage, so the last `res.usage` already carries the full total.
    if let Some(reason) = failure {
        conversation.usage = runner.total_usage().clone();
        host.mark_failed(conversation, reason).await;
    }

    // Terminal and failure exits above always persist (any non-Working
    // status forces a write, and mark_failed writes its own snapshot), so
    // only a Working exit — e.g. the runner returning `Ok(None)` — can still
    // hold turns skipped by the throttle.
    if unpersisted_turns > 0 && conversation.status == ConversationStatus::Working {
        host.persist_snapshot(conversation).await;
    }
}

fn queued_runner_tokens(runner: &CompletionRunner) -> u64 {
    runner
        .steering_message_iter()
        .chain(runner.follow_up_message_iter())
        .map(|part| part.estimated_tokens() as u64)
        .sum()
}

pub(super) async fn compact_runner_if_needed(
    runner: &mut CompletionRunner,
) -> Result<bool, BoxError> {
    // A finished runner must never be handed off: finalize() rejects it with
    // "completion already finalized", and surfacing that as a loop failure
    // would flip an already-persisted Completed conversation to Failed with
    // zeroed usage. Bound-mode hosts (maintenance) reach this arm on the
    // iteration right after convergence, before `runner.next()` returns
    // `Ok(None)`.
    if runner.is_done() {
        return Ok(false);
    }
    if !runner.needs_compaction_with(|| queued_runner_tokens(runner)) {
        return Ok(false);
    }

    // handoff()'s internal finalize mem::takes total_usage/tools_usage into an
    // output that handoff drops when the summarization turn itself fails
    // (refusal, cancellation, empty summary), which would make the caller's
    // failure-exit backfill record zero. Early handoff errors leave the totals
    // untouched, so restore only when they were actually drained.
    let pre_usage = runner.total_usage().clone();
    let pre_tools_usage = runner.tools_usage().clone();
    match runner.handoff(None).await {
        Ok((mut compacted, output)) => {
            compacted.accumulate(&output.usage);
            compacted.accumulate_tools_usage(&output.tools_usage);
            compacted.follow_up(ContentPart::from(COMPACTION_CONTINUE_PROMPT.to_string()));
            *runner = compacted;
            Ok(true)
        }
        Err(err) => {
            if usage_is_empty(runner.total_usage()) {
                runner.accumulate(&pre_usage);
            }
            if runner.tools_usage().is_empty() {
                runner.accumulate_tools_usage(&pre_tools_usage);
            }
            Err(err)
        }
    }
}

fn usage_is_empty(usage: &Usage) -> bool {
    usage.input_tokens == 0
        && usage.output_tokens == 0
        && usage.cached_tokens == 0
        && usage.requests == 0
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

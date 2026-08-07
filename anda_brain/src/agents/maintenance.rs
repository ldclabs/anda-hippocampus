use anda_core::{
    Agent, AgentContext, AgentOutput, BoxError, CompletionRequest, Document, Documents, Message,
    Resource, StateFeatures,
};
use anda_db::{collection::Collection, schema::DocumentId};
use anda_engine::{
    context::{AgentCtx, CompletionRunner},
    extension::note::{NoteTool, load_notes, load_notes_from_legacy},
    local_date_hour,
    memory::{Conversation, ConversationRef, ConversationStatus, Conversations, MemoryManagement},
    unix_ms,
};
use parking_lot::RwLock;
use serde_json::json;
use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use super::{BrainHook, RunnerFlow, RunnerHost, SELF_USER_ID, drive_runner_loop};
use crate::types::{MaintenanceAt, MaintenanceInput, MaintenanceScope};

/// Resets the AtomicBool to false on drop (panic guard for processing flag).
struct ProcessingGuard(Arc<AtomicBool>);
impl Drop for ProcessingGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// An externally-held claim on the maintenance processing slot (see
/// [`MaintenanceAgent::try_claim_processing`]). If the claim is never
/// consumed by [`MaintenanceAgent::run`] — e.g. the settlement or agent
/// dispatch errored first — dropping it releases the slot.
pub(crate) struct MaintenanceClaim {
    processing: Arc<AtomicBool>,
    external_claim: Arc<AtomicBool>,
}

impl Drop for MaintenanceClaim {
    fn drop(&mut self) {
        if self.external_claim.swap(false, Ordering::SeqCst) {
            self.processing.store(false, Ordering::SeqCst);
        }
    }
}

#[derive(Clone)]
pub struct MaintenanceAgent {
    pub conversations: Conversations,
    /// The collection backing `conversations`. `Conversations` wraps document
    /// access only, so the maintenance watermarks — collection extensions —
    /// are read and written through this handle.
    pub conversations_collection: Arc<Collection>,
    memory: Arc<MemoryManagement>,
    processing: Arc<AtomicBool>,
    /// True while a [`MaintenanceClaim`] holds `processing` on behalf of
    /// `Space::maintenance` and the claim has not been consumed by `run`.
    external_claim: Arc<AtomicBool>,
    hook: Arc<dyn BrainHook>,
    history: Arc<RwLock<VecDeque<Document>>>,
}

impl MaintenanceAgent {
    pub const NAME: &'static str = "maintenance_memory";
    pub fn new(
        memory: Arc<MemoryManagement>,
        conversations: Conversations,
        conversations_collection: Arc<Collection>,
        hook: Arc<dyn BrainHook>,
    ) -> Self {
        Self {
            memory,
            conversations,
            conversations_collection,
            processing: Arc::new(AtomicBool::new(false)),
            external_claim: Arc::new(AtomicBool::new(false)),
            hook,
            history: Arc::new(RwLock::new(VecDeque::new())),
        }
    }

    /// Claims the processing slot on behalf of `Space::maintenance` so the
    /// deterministic settlement that precedes the LLM cycle runs under the
    /// same formation/maintenance mutual exclusion as the cycle itself
    /// (review P1-3: without this, formation could start inside the
    /// multi-second settlement window and write the graph concurrently).
    /// The claim is inherited by the next [`Agent::run`] call; if `run` is
    /// never reached, dropping the claim releases the slot.
    pub(crate) fn try_claim_processing(&self) -> Option<MaintenanceClaim> {
        if self
            .processing
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return None;
        }
        self.external_claim.store(true, Ordering::SeqCst);
        Some(MaintenanceClaim {
            processing: self.processing.clone(),
            external_claim: self.external_claim.clone(),
        })
    }

    pub async fn init(&self) -> Result<(), BoxError> {
        let (conversations, _) = self
            .conversations
            .list_conversations_by_user(&SELF_USER_ID, None, Some(2))
            .await?;
        // Only completed conversations belong in the model context, matching
        // the runtime push_completed_history behavior. The list is newest
        // first while the runtime queue runs oldest -> newest, so reverse it;
        // otherwise the next push_back would evict the newest entry first.
        *self.history.write() = conversations
            .into_iter()
            .filter(|c| c.status == ConversationStatus::Completed)
            .rev()
            .map(Document::from)
            .collect();
        Ok(())
    }

    pub fn is_processing(&self) -> bool {
        self.processing.load(Ordering::SeqCst)
    }

    pub fn get_processed(&self) -> Option<DocumentId> {
        match self.conversations_collection.max_document_id() {
            0 => None,
            id => Some(id),
        }
    }

    pub fn get_processed_at(&self) -> MaintenanceAt {
        let mut rt = MaintenanceAt::default();
        self.conversations_collection.extensions_with(|kv| {
            if let Some(v) = kv.get("full")
                && let Ok(id) = v.try_into()
            {
                rt.full = id;
            }
            if let Some(v) = kv.get("quick")
                && let Ok(id) = v.try_into()
            {
                rt.quick = id;
            }
            if let Some(v) = kv.get("daydream")
                && let Ok(id) = v.try_into()
            {
                rt.daydream = id;
            }
            if let Some(v) = kv.get("start_at")
                && let Ok(ms) = v.try_into()
            {
                rt.start_at = ms;
            }
        });
        rt
    }

    /// Persists the start time of the latest maintenance task.
    pub async fn set_start_at(&self, now_ms: u64) -> Result<(), BoxError> {
        self.conversations_collection
            .save_extension_from("start_at".to_string(), &now_ms)
            .await?;
        Ok(())
    }

    pub async fn set_processed_at(
        &self,
        scope: MaintenanceScope,
        formation_id: DocumentId,
    ) -> Result<(), BoxError> {
        self.conversations_collection
            .save_extension_from(scope.to_string(), &formation_id)
            .await?;
        Ok(())
    }
}

impl Agent<AgentCtx> for MaintenanceAgent {
    fn name(&self) -> String {
        Self::NAME.to_string()
    }

    fn description(&self) -> String {
        "The Brain Maintenance agent operates in Sleep Mode — performing memory metabolism including consolidation, organization, pruning, and health optimization of the Cognitive Nexus during scheduled maintenance cycles.".to_string()
    }

    fn tool_dependencies(&self) -> Vec<String> {
        vec!["execute_kip".to_string(), NoteTool::NAME.to_string()]
    }

    /// Receives a trigger envelope (MaintenanceInput JSON), creates a conversation to track the
    /// maintenance cycle, and runs the sleep cycle workflow.
    async fn run(
        &self,
        ctx: AgentCtx,
        prompt: String, // MaintenanceInput serialized as JSON string
        _resources: Vec<Resource>,
    ) -> Result<AgentOutput, BoxError> {
        // Reject malformed input before claiming the processing slot; a bad
        // prompt would otherwise burn a full LLM maintenance cycle that can
        // never record its processed marker.
        let maintenance_input = serde_json::from_str::<MaintenanceInput>(&prompt)
            .map_err(|err| format!("invalid MaintenanceInput: {err}"))?;

        // Prevent concurrent maintenance runs. A claim taken by
        // `Space::maintenance` before settlement is inherited here instead of
        // re-acquired, so the slot is held continuously across settlement.
        if !self.external_claim.swap(false, Ordering::SeqCst)
            && self
                .processing
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
        {
            return Ok(AgentOutput {
                content: "Maintenance cycle is already in progress.".to_string(),
                ..Default::default()
            });
        }
        let guard = ProcessingGuard(self.processing.clone());

        let caller = ctx.caller();
        let now_ms = unix_ms();
        // Persistence failure must not block the maintenance cycle itself.
        if let Err(err) = self.set_start_at(now_ms).await {
            log::warn!(
                target: "brain",
                "failed to persist maintenance start_at: {err:?}"
            );
        }

        let mut conversation = Conversation {
            user: *caller,
            messages: vec![json!(Message {
                role: "user".into(),
                content: vec![prompt.into()],
                ..Default::default()
            })],
            status: ConversationStatus::Working,
            period: now_ms / 3600 / 1000,
            created_at: now_ms,
            updated_at: now_ms,
            label: Some("maintenance".to_string()),
            ..Default::default()
        };

        let id = self
            .conversations
            .add_conversation(ConversationRef::from(&conversation))
            .await?;
        conversation._id = id;

        let agent = self.clone();
        let ctx_clone = ctx.clone();
        tokio::spawn(async move {
            {
                // Guard resets processing to false when the task completes or panics.
                let _guard = guard;
                agent.process_one(&ctx_clone, &mut conversation).await;
                if conversation.status == ConversationStatus::Completed
                    && let Err(err) = agent
                        .set_processed_at(maintenance_input.scope, maintenance_input.formation_id)
                        .await
                {
                    log::error!(
                        target: "brain",
                        conversation = conversation._id,
                        formation_id = maintenance_input.formation_id;
                        "failed to persist maintenance processed marker: {err:?}"
                    );
                }
                agent
                    .hook
                    .on_conversation_end(MaintenanceAgent::NAME, &conversation)
                    .await;
            }
            // Trigger formation after the processing flag has been released.
            agent.hook.try_start_formation().await;
        });

        Ok(AgentOutput {
            conversation: Some(id),
            ..Default::default()
        })
    }
}

// The runner guardrails are shared with formation; see
// `RUNNER_MAX_MODEL_TURNS` in agents.rs. Tests keep the historical name.
#[cfg(test)]
use super::RUNNER_MAX_MODEL_TURNS as MAINTENANCE_MAX_MODEL_TURNS;

impl MaintenanceAgent {
    async fn mark_conversation_failed(&self, conversation: &mut Conversation, reason: String) {
        log::error!(
            target: "brain",
            "Maintenance conversation {} failed: {}",
            conversation._id,
            reason
        );
        conversation.failed_reason = Some(reason);
        conversation.status = ConversationStatus::Failed;
        conversation.updated_at = unix_ms();
        if let Ok(changes) = conversation.to_changes() {
            let _ = self
                .conversations
                .update_conversation(conversation._id, changes)
                .await;
        }
    }

    /// Persists the current full conversation snapshot; `to_changes` failures
    /// are logged and must not interrupt the processing loop.
    async fn persist_conversation_snapshot(&self, conversation: &Conversation) {
        match conversation.to_changes() {
            Ok(changes) => {
                let _ = self
                    .conversations
                    .update_conversation(conversation._id, changes)
                    .await;
            }
            Err(err) => {
                log::error!(
                    target: "brain",
                    "Failed to serialize maintenance conversation {} changes: {:?}",
                    conversation._id,
                    err
                );
            }
        }
    }

    async fn process_one(&self, ctx: &AgentCtx, conversation: &mut Conversation) {
        let prompt = match conversation
            .messages
            .first()
            .and_then(|v| serde_json::from_value::<Message>(v.clone()).ok())
            .and_then(|v| v.text())
        {
            Some(p) => p,
            None => {
                self.mark_conversation_failed(conversation, "No prompt found".to_string())
                    .await;
                return;
            }
        };

        let now_ms = unix_ms();
        // The context sources are independent; fetch them concurrently (same
        // pattern as recall's context assembly).
        let (primer, notes) = tokio::join!(
            async { self.memory.describe_primer().await.unwrap_or_default() },
            async {
                match load_notes(ctx).await {
                    Some(n) => n,
                    None => load_notes_from_legacy(ctx).await.unwrap_or_default(),
                }
            },
        );
        let chat_history: Vec<Document> = { self.history.read().iter().cloned().collect() };

        let chat_history = if chat_history.is_empty() {
            vec![]
        } else {
            vec![Message {
                role: "user".into(),
                content: vec![
                    Documents::new("history_maintenance".to_string(), chat_history)
                        .to_string()
                        .into(),
                ],
                name: Some("$system".into()),
                timestamp: Some(now_ms),
                ..Default::default()
            }]
        };
        let mut runner = ctx.clone().completion_iter(
            CompletionRequest {
                instructions: format!(
                    "{}\n\n---\n\n# `DESCRIBE PRIMER` Result:\n{}\n\n---\n\n# Your Notes:\n{}\n\n# Current Datetime: {}",
                    super::prompts::active_prompt(super::prompts::PromptTarget::Maintenance),
                    primer,
                    serde_json::to_string(&notes.items).unwrap_or_default(),
                    local_date_hour(now_ms).unwrap_or_default()
                ),
                prompt,
                chat_history,
                tools: ctx.tool_definitions(Some(&self.tool_dependencies())),
                tool_choice_required: true,
                ..Default::default()
            },
            vec![],
        );

        let mut host = MaintenanceRunnerHost { agent: self };
        drive_runner_loop(&mut host, &mut runner, conversation).await;
    }
}

/// Maintenance's seams of the shared runner loop (`drive_runner_loop`).
/// Unlike formation there is no review pass and no immediate break on a done
/// turn: the loop terminates through the runner's own `Ok(None)` exit on the
/// next iteration, and a successful turn never touches `failed_reason`
/// (maintenance conversations are created fresh per cycle and never retried).
struct MaintenanceRunnerHost<'a> {
    agent: &'a MaintenanceAgent,
}

impl RunnerHost for MaintenanceRunnerHost<'_> {
    fn label(&self) -> &'static str {
        "maintenance"
    }

    fn history(&self) -> &RwLock<VecDeque<Document>> {
        &self.agent.history
    }

    async fn persist_snapshot(&self, conversation: &Conversation) {
        self.agent.persist_conversation_snapshot(conversation).await;
    }

    async fn mark_failed(&self, conversation: &mut Conversation, reason: String) {
        self.agent
            .mark_conversation_failed(conversation, reason)
            .await;
    }

    fn turn_is_done(&self, runner: &CompletionRunner) -> bool {
        runner.is_done()
    }

    fn on_turn_success(&self, _conversation: &mut Conversation) {}

    fn after_turn(&mut self, _runner: &mut CompletionRunner, _is_done: bool) -> RunnerFlow {
        RunnerFlow::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::{MaintenanceAgent, ProcessingGuard};
    use crate::{
        agents::SELF_USER_ID,
        space::AppState,
        testkit::{app_state_core, create_loaded_space, models_with_completer},
        types::{MaintenanceInput, MaintenanceScope},
    };
    use anda_core::{
        Agent, AgentOutput, BoxError, BoxPinFut, CompletionRequest, Message, ToolCall, Usage,
    };
    use anda_engine::{
        context::AgentCtx,
        memory::{Conversation, ConversationRef, ConversationStatus},
        model::CompletionFeaturesDyn,
        unix_ms,
    };
    use serde_json::json;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    #[derive(Debug)]
    struct FinalCompleter;

    impl CompletionFeaturesDyn for FinalCompleter {
        fn model_name(&self) -> String {
            "maintenance-final-test-model".to_string()
        }

        fn completion(&self, req: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
            Box::pin(async move {
                Ok(AgentOutput {
                    content: "maintained".to_string(),
                    chat_history: vec![Message {
                        role: "assistant".to_string(),
                        content: vec![format!("maintained: {}", req.prompt).into()],
                        ..Default::default()
                    }],
                    ..Default::default()
                })
            })
        }
    }

    #[derive(Debug)]
    struct FailedReasonCompleter;

    impl CompletionFeaturesDyn for FailedReasonCompleter {
        fn model_name(&self) -> String {
            "maintenance-failed-reason-test-model".to_string()
        }

        fn completion(&self, _req: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
            Box::pin(async move {
                Ok(AgentOutput {
                    failed_reason: Some("maintenance failed".to_string()),
                    chat_history: vec![Message {
                        role: "assistant".to_string(),
                        content: vec!["maintenance failure".to_string().into()],
                        ..Default::default()
                    }],
                    ..Default::default()
                })
            })
        }
    }

    #[derive(Debug)]
    struct ErrorCompleter;

    impl CompletionFeaturesDyn for ErrorCompleter {
        fn model_name(&self) -> String {
            "maintenance-error-test-model".to_string()
        }

        fn completion(&self, _req: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
            Box::pin(async move { Err("model error".into()) })
        }
    }

    /// Converges on the first turn (final answer, no tool calls) while
    /// reporting input-token usage far above the compaction threshold, so the
    /// loop iteration right after convergence sees `needs_compaction`.
    #[derive(Debug)]
    struct HugeUsageFinalCompleter;

    impl CompletionFeaturesDyn for HugeUsageFinalCompleter {
        fn model_name(&self) -> String {
            "maintenance-huge-usage-test-model".to_string()
        }

        fn completion(&self, _req: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
            Box::pin(async move {
                Ok(AgentOutput {
                    content: "maintained".to_string(),
                    usage: Usage {
                        input_tokens: 200_000,
                        output_tokens: 10,
                        requests: 1,
                        ..Default::default()
                    },
                    chat_history: vec![Message {
                        role: "assistant".to_string(),
                        content: vec!["maintained".to_string().into()],
                        ..Default::default()
                    }],
                    ..Default::default()
                })
            })
        }
    }

    /// Emits one over-threshold tool-call turn, then an empty summary for the
    /// compaction handoff turn (tools cleared), making `handoff()` fail after
    /// its internal finalize already drained the runner's total usage.
    #[derive(Debug)]
    struct CompactionFailingCompleter;

    impl CompletionFeaturesDyn for CompactionFailingCompleter {
        fn model_name(&self) -> String {
            "maintenance-compaction-failing-test-model".to_string()
        }

        fn completion(&self, req: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
            Box::pin(async move {
                if req.tools.is_empty() {
                    // Compaction handoff turn: an empty summary makes
                    // handoff() return Err after finalize drained usage.
                    return Ok(AgentOutput {
                        content: "  ".to_string(),
                        usage: Usage {
                            input_tokens: 5_000,
                            output_tokens: 1,
                            requests: 1,
                            ..Default::default()
                        },
                        ..Default::default()
                    });
                }
                Ok(AgentOutput {
                    tool_calls: vec![ToolCall {
                        name: "execute_kip".to_string(),
                        args: serde_json::json!({"commands": []}),
                        result: None,
                        call_id: Some("over-threshold".to_string()),
                        remote_id: None,
                    }],
                    usage: Usage {
                        input_tokens: 200_000,
                        output_tokens: 10,
                        requests: 1,
                        ..Default::default()
                    },
                    ..Default::default()
                })
            })
        }
    }

    /// Emits an endless stream of tool calls (a non-converging model), and a
    /// summary for compaction handoff requests so the runner keeps looping.
    #[derive(Debug)]
    struct ToolLoopCompleter;

    impl CompletionFeaturesDyn for ToolLoopCompleter {
        fn model_name(&self) -> String {
            "maintenance-tool-loop-test-model".to_string()
        }

        fn completion(&self, req: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
            Box::pin(async move {
                let usage = Usage {
                    input_tokens: 10,
                    output_tokens: 1,
                    requests: 1,
                    ..Default::default()
                };
                if req.tools.is_empty() {
                    // Compaction handoff turn: tools are cleared for the
                    // summarization request.
                    return Ok(AgentOutput {
                        content: "handoff summary".to_string(),
                        usage,
                        ..Default::default()
                    });
                }
                Ok(AgentOutput {
                    tool_calls: vec![ToolCall {
                        name: "execute_kip".to_string(),
                        args: serde_json::json!({"commands": []}),
                        result: None,
                        call_id: Some("loop".to_string()),
                        remote_id: None,
                    }],
                    usage,
                    ..Default::default()
                })
            })
        }
    }

    fn test_app_state_with_completer<C>(name: &str, completer: C) -> AppState
    where
        C: CompletionFeaturesDyn,
    {
        app_state_core(name, models_with_completer(completer), vec![], "test", 0)
    }

    fn maintenance_prompt(scope: MaintenanceScope) -> String {
        serde_json::to_string(&MaintenanceInput {
            scope,
            formation_id: 99,
            ..Default::default()
        })
        .unwrap()
    }

    async fn stored_conversation(
        agent: &MaintenanceAgent,
        messages: Vec<serde_json::Value>,
    ) -> Conversation {
        let now = unix_ms();
        let mut conversation = Conversation {
            user: SELF_USER_ID,
            status: ConversationStatus::Submitted,
            messages,
            label: Some("maintenance".to_string()),
            created_at: now,
            updated_at: now,
            ..Default::default()
        };
        let id = agent
            .conversations
            .add_conversation(ConversationRef::from(&conversation))
            .await
            .unwrap();
        conversation._id = id;
        conversation
    }

    #[test]
    fn processing_guard_resets_processing_flag_on_drop() {
        let processing = Arc::new(AtomicBool::new(true));

        {
            let _guard = ProcessingGuard(processing.clone());
            assert!(processing.load(Ordering::SeqCst));
        }

        assert!(!processing.load(Ordering::SeqCst));
    }

    #[test]
    fn maintenance_agent_name_matches_registered_agent_name() {
        assert_eq!(MaintenanceAgent::NAME, "maintenance_memory");
    }

    #[tokio::test]
    async fn maintenance_agent_trait_metadata_and_processed_markers() {
        let app = test_app_state_with_completer("maintenance_trait", FinalCompleter);
        let space = create_loaded_space(&app, "maintenance_trait").await;
        let maintenance = space.maintenance_for_test();

        assert_eq!(
            Agent::<AgentCtx>::name(maintenance.as_ref()),
            MaintenanceAgent::NAME
        );
        assert!(Agent::<AgentCtx>::description(maintenance.as_ref()).contains("Sleep Mode"));
        let tools = Agent::<AgentCtx>::tool_dependencies(maintenance.as_ref());
        assert!(tools.iter().any(|name| name == "execute_kip"));
        assert!(tools.iter().any(|name| name == "note"));
        assert_eq!(maintenance.get_processed(), None);

        maintenance
            .set_processed_at(MaintenanceScope::Quick, 7)
            .await
            .unwrap();
        assert_eq!(maintenance.get_processed_at().quick, 7);

        assert_eq!(maintenance.get_processed_at().start_at, 0);
        maintenance.set_start_at(12345).await.unwrap();
        assert_eq!(maintenance.get_processed_at().start_at, 12345);
    }

    #[tokio::test]
    async fn init_restores_history_in_oldest_first_order() {
        let app = test_app_state_with_completer("maintenance_init_order", FinalCompleter);
        let space = create_loaded_space(&app, "maintenance_init_order").await;
        let maintenance = space.maintenance_for_test();

        for _ in 0..3 {
            let conversation = Conversation {
                user: SELF_USER_ID,
                status: ConversationStatus::Completed,
                label: Some("maintenance".to_string()),
                ..Default::default()
            };
            maintenance
                .conversations
                .add_conversation(ConversationRef::from(&conversation))
                .await
                .unwrap();
        }

        maintenance.init().await.unwrap();

        let ids: Vec<u64> = maintenance
            .history
            .read()
            .iter()
            .map(|doc| doc.metadata.get("_id").and_then(|v| v.as_u64()).unwrap())
            .collect();
        // The two newest completed conversations, ordered oldest -> newest to
        // match the runtime push_completed_history queue.
        assert_eq!(ids, vec![2, 3]);
    }

    #[tokio::test]
    async fn run_rejects_invalid_maintenance_input_before_processing() {
        let app = test_app_state_with_completer("maintenance_invalid_input", FinalCompleter);
        let space = create_loaded_space(&app, "maintenance_invalid_input").await;
        let maintenance = space.maintenance_for_test();
        let ctx = space
            .ctx_for_test(SELF_USER_ID, MaintenanceAgent::NAME)
            .unwrap();

        let err =
            Agent::<AgentCtx>::run(maintenance.as_ref(), ctx, "not a json".to_string(), vec![])
                .await
                .unwrap_err();

        assert!(err.to_string().contains("invalid MaintenanceInput"));
        assert!(!maintenance.is_processing());
        assert_eq!(maintenance.conversations_collection.len(), 0);
    }

    #[tokio::test]
    async fn process_one_convergence_survives_pending_compaction_check() {
        // Regression: maintenance runs the runner in bound mode and
        // `after_turn` keeps looping after convergence, so the next loop
        // iteration runs the compaction check against a finished runner.
        // Handing it off would flip the persisted Completed conversation to
        // Failed ("completion already finalized") and zero its usage.
        let app =
            test_app_state_with_completer("maintenance_compact_done", HugeUsageFinalCompleter);
        let space = create_loaded_space(&app, "maintenance_compact_done").await;
        let maintenance = space.maintenance_for_test();
        let ctx = space
            .ctx_for_test(SELF_USER_ID, MaintenanceAgent::NAME)
            .unwrap();
        let mut conversation = stored_conversation(
            &maintenance,
            vec![json!(Message {
                role: "user".to_string(),
                content: vec![maintenance_prompt(MaintenanceScope::Quick).into()],
                ..Default::default()
            })],
        )
        .await;

        maintenance.process_one(&ctx, &mut conversation).await;

        assert_eq!(conversation.status, ConversationStatus::Completed);
        assert!(conversation.failed_reason.is_none());
        assert_eq!(conversation.usage.input_tokens, 200_000);
        let stored = maintenance
            .conversations
            .get_conversation(conversation._id)
            .await
            .unwrap();
        assert_eq!(stored.status, ConversationStatus::Completed);
    }

    #[tokio::test]
    async fn failed_compaction_handoff_keeps_accumulated_usage() {
        // Regression: handoff()'s internal finalize mem::takes the runner's
        // total usage into an output that handoff drops when the
        // summarization turn produces an empty summary. Without the restore
        // in compact_runner_if_needed the failure exit would backfill zero
        // usage over the tokens already paid for.
        let app =
            test_app_state_with_completer("maintenance_compact_failed", CompactionFailingCompleter);
        let space = create_loaded_space(&app, "maintenance_compact_failed").await;
        let maintenance = space.maintenance_for_test();
        let ctx = space
            .ctx_for_test(SELF_USER_ID, MaintenanceAgent::NAME)
            .unwrap();
        let mut conversation = stored_conversation(
            &maintenance,
            vec![json!(Message {
                role: "user".to_string(),
                content: vec![maintenance_prompt(MaintenanceScope::Quick).into()],
                ..Default::default()
            })],
        )
        .await;

        maintenance.process_one(&ctx, &mut conversation).await;

        assert_eq!(conversation.status, ConversationStatus::Failed);
        assert!(
            conversation
                .failed_reason
                .as_deref()
                .unwrap_or_default()
                .contains("empty summary")
        );
        // The pre-handoff total survives; only the failed summarization
        // turn's own usage is unknowable and lost.
        assert_eq!(conversation.usage.input_tokens, 200_000);
    }

    #[tokio::test]
    async fn mark_conversation_failed_persists_status_and_reason() {
        let app = test_app_state_with_completer("maintenance_mark_failed", FinalCompleter);
        let space = create_loaded_space(&app, "maintenance_mark_failed").await;
        let maintenance = space.maintenance_for_test();
        let mut conversation = stored_conversation(&maintenance, vec![]).await;

        maintenance
            .mark_conversation_failed(&mut conversation, "boom".to_string())
            .await;

        assert_eq!(conversation.status, ConversationStatus::Failed);
        assert_eq!(conversation.failed_reason.as_deref(), Some("boom"));
        let stored = maintenance
            .conversations
            .get_conversation(conversation._id)
            .await
            .unwrap();
        assert_eq!(stored.status, ConversationStatus::Failed);
        assert_eq!(stored.failed_reason.as_deref(), Some("boom"));
    }

    #[tokio::test]
    async fn process_one_marks_missing_prompt_and_completion_errors() {
        let app = test_app_state_with_completer("maintenance_no_prompt", FinalCompleter);
        let space = create_loaded_space(&app, "maintenance_no_prompt").await;
        let maintenance = space.maintenance_for_test();
        let ctx = space
            .ctx_for_test(SELF_USER_ID, MaintenanceAgent::NAME)
            .unwrap();
        let mut no_prompt = stored_conversation(&maintenance, vec![]).await;

        maintenance.process_one(&ctx, &mut no_prompt).await;

        assert_eq!(no_prompt.status, ConversationStatus::Failed);
        assert_eq!(no_prompt.failed_reason.as_deref(), Some("No prompt found"));

        let app = test_app_state_with_completer("maintenance_model_error", ErrorCompleter);
        let space = create_loaded_space(&app, "maintenance_model_error").await;
        let maintenance = space.maintenance_for_test();
        let ctx = space
            .ctx_for_test(SELF_USER_ID, MaintenanceAgent::NAME)
            .unwrap();
        let mut conversation = stored_conversation(
            &maintenance,
            vec![json!(Message {
                role: "user".to_string(),
                content: vec![maintenance_prompt(MaintenanceScope::Quick).into()],
                ..Default::default()
            })],
        )
        .await;

        maintenance.process_one(&ctx, &mut conversation).await;

        assert_eq!(conversation.status, ConversationStatus::Failed);
        assert!(
            conversation
                .failed_reason
                .as_deref()
                .unwrap_or_default()
                .contains("CompletionRunner error")
        );
    }

    #[tokio::test]
    async fn process_one_fails_tool_loop_at_model_turn_limit_and_throttles_persistence() {
        let app = test_app_state_with_completer("maintenance_turn_limit", ToolLoopCompleter);
        let space = create_loaded_space(&app, "maintenance_turn_limit").await;
        let maintenance = space.maintenance_for_test();
        let ctx = space
            .ctx_for_test(SELF_USER_ID, MaintenanceAgent::NAME)
            .unwrap();
        let mut conversation = stored_conversation(
            &maintenance,
            vec![json!(Message {
                role: "user".to_string(),
                content: vec![maintenance_prompt(MaintenanceScope::Quick).into()],
                ..Default::default()
            })],
        )
        .await;

        let updates_before = maintenance.conversations_collection.stats().update_count;
        maintenance.process_one(&ctx, &mut conversation).await;
        let updates_after = maintenance.conversations_collection.stats().update_count;

        assert_eq!(conversation.status, ConversationStatus::Failed);
        assert!(
            conversation
                .failed_reason
                .as_deref()
                .unwrap_or_default()
                .contains(&format!(
                    "exceeded model turn limit of {}",
                    super::MAINTENANCE_MAX_MODEL_TURNS
                )),
            "failed_reason: {:?}",
            conversation.failed_reason
        );
        let stored = maintenance
            .conversations
            .get_conversation(conversation._id)
            .await
            .unwrap();
        assert_eq!(stored.status, ConversationStatus::Failed);

        // ~200 Working turns persisted every PERSIST_EVERY_N_TURNS plus the
        // final failure write — far fewer than one write per turn.
        let update_delta = updates_after - updates_before;
        assert!(
            (2..100).contains(&update_delta),
            "update_delta: {update_delta}"
        );
    }

    #[tokio::test]
    async fn process_one_uses_history_and_persists_failed_reason() {
        let app = test_app_state_with_completer("maintenance_history", FinalCompleter);
        let space = create_loaded_space(&app, "maintenance_history").await;
        let maintenance = space.maintenance_for_test();
        let ctx = space
            .ctx_for_test(SELF_USER_ID, MaintenanceAgent::NAME)
            .unwrap();

        let mut first = stored_conversation(
            &maintenance,
            vec![json!(Message {
                role: "user".to_string(),
                content: vec![maintenance_prompt(MaintenanceScope::Quick).into()],
                ..Default::default()
            })],
        )
        .await;
        maintenance.process_one(&ctx, &mut first).await;
        assert_eq!(first.status, ConversationStatus::Completed);

        let mut second = stored_conversation(
            &maintenance,
            vec![json!(Message {
                role: "user".to_string(),
                content: vec![maintenance_prompt(MaintenanceScope::Full).into()],
                ..Default::default()
            })],
        )
        .await;
        maintenance.process_one(&ctx, &mut second).await;
        assert_eq!(second.status, ConversationStatus::Completed);

        let app = test_app_state_with_completer("maintenance_failed_reason", FailedReasonCompleter);
        let space = create_loaded_space(&app, "maintenance_failed_reason").await;
        let maintenance = space.maintenance_for_test();
        let ctx = space
            .ctx_for_test(SELF_USER_ID, MaintenanceAgent::NAME)
            .unwrap();
        let mut failed = stored_conversation(
            &maintenance,
            vec![json!(Message {
                role: "user".to_string(),
                content: vec![maintenance_prompt(MaintenanceScope::Daydream).into()],
                ..Default::default()
            })],
        )
        .await;

        maintenance.process_one(&ctx, &mut failed).await;

        assert_eq!(failed.status, ConversationStatus::Failed);
        assert_eq!(failed.failed_reason.as_deref(), Some("maintenance failed"));
    }
}

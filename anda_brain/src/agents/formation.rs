use anda_core::{
    Agent, AgentContext, AgentOutput, BoxError, CompletionRequest, Document, Documents, Message,
    Resource, StateFeatures, Tool, estimate_tokens,
};
use anda_db::{
    query::Fv,
    schema::{DocumentId, Json, Map},
};
use anda_engine::{
    context::{AgentCtx, CompletionRunner},
    extension::note::{NoteTool, load_notes, load_notes_from_legacy},
    local_date_hour,
    memory::{Conversation, ConversationRef, ConversationStatus, MemoryManagement},
    unix_ms,
};
use parking_lot::RwLock;
use serde_json::json;
use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use super::{BrainHook, RunnerFlow, RunnerHost, drive_runner_loop};
use crate::types::FormationInput;

// Kept for reference; the runtime prompt comes from `prompts::active_prompt`
// so the eval optimizer can install candidate prompts.
#[allow(dead_code)]
const SELF_INSTRUCTIONS: &str = include_str!("../../assets/BrainFormation.md");
const REVIEW_INSTRUCTIONS: &str = include_str!("../../assets/BrainFormationReview.md");

// The runner guardrails are shared with maintenance; see
// `RUNNER_MAX_MODEL_TURNS` in agents.rs. Tests keep the historical name.
#[cfg(test)]
use super::RUNNER_MAX_MODEL_TURNS as FORMATION_MAX_MODEL_TURNS;

/// Resets the AtomicU64 to 0 on drop (panic guard for processing_conversation).
struct ProcessingGuard(Arc<AtomicU64>);
impl Drop for ProcessingGuard {
    fn drop(&mut self) {
        self.0.store(0, Ordering::SeqCst);
    }
}

#[derive(Clone)]
pub struct FormationAgent {
    memory: Arc<MemoryManagement>,
    processing_conversation: Arc<AtomicU64>,
    hook: Arc<dyn BrainHook>,
    history: Arc<RwLock<VecDeque<Document>>>,
    max_input_tokens: usize,
}

impl FormationAgent {
    pub const NAME: &'static str = "formation_memory";
    pub fn new(
        memory: Arc<MemoryManagement>,
        hook: Arc<dyn BrainHook>,
        max_input_tokens: usize,
    ) -> Self {
        Self {
            max_input_tokens,
            memory,
            processing_conversation: Arc::new(AtomicU64::new(0)),
            history: Arc::new(RwLock::new(VecDeque::new())),
            hook,
        }
    }

    /// Backfills the completed-conversation ring after a restart, mirroring
    /// recall/maintenance `init` — without it the `history_formation` context
    /// block stays empty until the next conversation completes. Formation
    /// conversations live in the shared memory store under their ingesting
    /// user, so there is no single-user list to query; walk backwards from
    /// the newest conversation and keep the latest completed formation ones.
    /// The scan is bounded: this is best-effort context, not recovery state.
    pub async fn init(&self) -> Result<(), BoxError> {
        // Matches the `push_completed_history` cap in `drive_runner_loop`.
        const HISTORY_LEN: usize = 2;
        const SCAN_LIMIT: u64 = 32;

        // Collected newest-first; the runtime ring runs oldest -> newest.
        let mut newest: Vec<Document> = Vec::with_capacity(HISTORY_LEN);
        let mut id = self.memory.max_conversation_id();
        let mut scanned = 0u64;
        while id > 0 && scanned < SCAN_LIMIT && newest.len() < HISTORY_LEN {
            scanned += 1;
            if let Ok(conv) = self.memory.get_conversation(id).await
                && conv.status == ConversationStatus::Completed
                && conv
                    .label
                    .as_deref()
                    .is_none_or(|label| label == "formation")
            {
                newest.push(Document::from(conv));
            }
            id -= 1;
        }
        newest.reverse();
        *self.history.write() = newest.into();
        Ok(())
    }

    pub fn is_processing(&self) -> bool {
        self.processing_conversation.load(Ordering::SeqCst) != 0
    }

    pub fn get_processed(&self) -> Option<DocumentId> {
        self.memory
            .conversations
            .get_extension_as::<DocumentId>("brain_processed")
    }

    pub async fn get_or_init_counterparty(
        &self,
        counterparty: String,
        name: Option<String>,
    ) -> Result<Json, BoxError> {
        let mut attributes = Map::new();
        let mut metadata = Map::new();
        attributes.insert("id".to_string(), counterparty.clone().into());
        attributes.insert("person_class".to_string(), "Human".into());
        if let Some(name) = name {
            attributes.insert("name".to_string(), name.into());
        }
        metadata.insert("author".to_string(), "$system".into());
        metadata.insert("status".to_string(), "active".into());
        let user = self
            .memory
            .nexus
            .get_or_init_concept("Person".to_string(), counterparty, attributes, metadata)
            .await?;

        Ok(user.to_concept_node())
    }

    pub async fn start_process(
        &self,
        ctx: AgentCtx,
        conversation: DocumentId,
    ) -> Result<(), BoxError> {
        let current = self.processing_conversation.load(Ordering::SeqCst);
        if current != 0 {
            return Err(format!(
                "FormationAgent is already processing conversation {}",
                current
            )
            .into());
        }
        if self.hook.is_maintenance_processing() {
            return Err(
                "MaintenanceAgent is processing, formation will resume when maintenance completes"
                    .into(),
            );
        }

        // Find the next valid pending conversation starting from the given ID (inclusive)
        let conv = self
            .find_next_submitted(conversation.saturating_sub(1))
            .await
            .ok_or_else(|| {
                format!(
                    "No pending formation conversation found starting from {}",
                    conversation
                )
            })?;

        self.try_process(ctx, conv);
        Ok(())
    }

    pub fn try_process(&self, ctx: AgentCtx, conversation: Conversation) {
        if self
            .processing_conversation
            .compare_exchange(0, conversation._id, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            log::info!(
                target: "brain",
                "FormationAgent is already processing conversation {}, cannot process conversation {}",
                self.processing_conversation.load(Ordering::SeqCst),
                conversation._id
            );
            return;
        }

        let agent = self.clone();
        let pc = self.processing_conversation.clone();
        tokio::spawn(async move {
            // Guard resets processing_conversation to 0 if the task panics.
            let guard = ProcessingGuard(pc);
            agent.process_loop(ctx, conversation).await;
            // Normal exit: process_loop already manages the atomic properly,
            // so defuse the guard to avoid clobbering a valid value.
            std::mem::forget(guard);
        });
    }

    async fn process_loop(&self, ctx: AgentCtx, mut conversation: Conversation) {
        loop {
            let conv_id = conversation._id;

            self.process_one(&ctx, &mut conversation).await;
            self.hook
                .on_conversation_end(Self::NAME, &conversation)
                .await;
            if conversation.status == ConversationStatus::Failed {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await; // 避免快速失败循环
                // 重试一次
                self.process_one(&ctx, &mut conversation).await;
                self.hook
                    .on_conversation_end(Self::NAME, &conversation)
                    .await;
            }

            if conversation.status != ConversationStatus::Completed {
                log::error!(
                    target: "brain",
                    "Conversation {} ended with status {:?}, not marking as processed",
                    conv_id,
                    conversation.status
                );
                // 上游异常，重置 processing 状态以允许外部干预或后续请求自动触发
                self.processing_conversation.store(0, Ordering::SeqCst);
                break;
            }

            // The marker is a high-water mark: reprocessing an older
            // conversation (e.g. via restart_formation) must not rewind it.
            let processed = self.get_processed().unwrap_or_default().max(conv_id);
            self.memory
                .conversations
                .save_extension("brain_processed".to_string(), processed.into())
                .await
                .ok();

            if let Some(id) = self.hook.try_start_maintenance(conv_id).await {
                log::info!(
                    target: "brain",
                    "Triggered maintenance for conversation {}, new maintenance conversation {}",
                    conv_id,
                    id
                );

                // 重置 processing 状态，以便 maintenance 完成后 try_start_formation 能重新启动
                self.processing_conversation.store(0, Ordering::SeqCst);
                break; // 交由 maintenance agent 处理后续流程，退出循环
            }

            // 查找下一个待处理的 conversation
            match self.find_next_submitted(conv_id).await {
                Some(next_conv) => {
                    if self
                        .processing_conversation
                        .compare_exchange(
                            conv_id,
                            next_conv._id,
                            Ordering::SeqCst,
                            Ordering::SeqCst,
                        )
                        .is_ok()
                    {
                        conversation = next_conv;
                        continue;
                    }
                    // CAS 失败说明其他线程已接管，退出
                    break;
                }
                None => {
                    self.processing_conversation.store(0, Ordering::SeqCst);
                    // 双重检查：store(0) 前可能有新 conversation 到达但 try_process CAS 失败
                    if let Some(next_conv) = self.find_next_submitted(conv_id).await
                        && self
                            .processing_conversation
                            .compare_exchange(0, next_conv._id, Ordering::SeqCst, Ordering::SeqCst)
                            .is_ok()
                    {
                        conversation = next_conv;
                        continue;
                    }
                    break;
                }
            }
        }
    }

    async fn find_next_submitted(&self, after_id: u64) -> Option<Conversation> {
        let mut id = after_id;
        while id < self.memory.max_conversation_id() {
            id += 1;
            match self.memory.get_conversation(id).await {
                Ok(conv) => {
                    if conv.status == ConversationStatus::Completed
                        || conv.status == ConversationStatus::Cancelled
                    {
                        continue;
                    }
                    if let Some(label) = &conv.label
                        && label != "formation"
                    {
                        continue; // 只处理 label 为 "formation" 的 conversation，跳过其他类型
                    }
                    return Some(conv);
                }
                _ => continue,
            }
        }
        None
    }

    async fn mark_conversation_failed(&self, conversation: &mut Conversation, reason: String) {
        log::error!(target: "brain", "Conversation {} failed: {}", conversation._id, reason);
        conversation.failed_reason = Some(reason);
        conversation.status = ConversationStatus::Failed;
        conversation.updated_at = unix_ms();

        if let Ok(changes) = conversation.to_changes() {
            let _ = self
                .memory
                .update_conversation(conversation._id, changes)
                .await;
        }
    }

    /// Persists the current full conversation snapshot; `to_changes` failures
    /// are logged and must not interrupt the processing loop.
    async fn persist_conversation_snapshot(&self, conversation: &Conversation) {
        match conversation.to_changes() {
            Ok(mut changes) => {
                if conversation.failed_reason.is_none() {
                    changes.insert("failed_reason".to_string(), Fv::Null);
                }
                let _ = self
                    .memory
                    .update_conversation(conversation._id, changes)
                    .await;
            }
            Err(err) => {
                log::error!(
                    target: "brain",
                    "Failed to serialize formation conversation {} changes: {:?}",
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

        let counterparty = serde_json::from_str::<FormationInput>(&prompt)
            .ok()
            .and_then(|input| input.context)
            .and_then(|input_ctx| input_ctx.counterparty);

        let now_ms = unix_ms();
        // The context sources are independent; fetch them concurrently (same
        // pattern as recall's context assembly).
        let (counterparty_info, primer, notes) = tokio::join!(
            async {
                match counterparty {
                    Some(counterparty) => {
                        self.get_or_init_counterparty(counterparty, None).await.ok()
                    }
                    None => None,
                }
            },
            async { self.memory.describe_primer().await.unwrap_or_default() },
            async {
                match load_notes(ctx).await {
                    Some(n) => n,
                    None => load_notes_from_legacy(ctx).await.unwrap_or_default(),
                }
            },
        );

        // add history conversations to provide more context for recall
        let chat_history: Vec<Document> = { self.history.read().iter().cloned().collect() };

        let chat_history = if chat_history.is_empty() {
            vec![]
        } else {
            vec![Message {
                role: "user".into(),
                content: vec![
                    Documents::new("history_formation".to_string(), chat_history)
                        .to_string()
                        .into(),
                ],
                name: Some("$system".into()),
                timestamp: Some(now_ms),
                ..Default::default()
            }]
        };
        let should_review = estimate_tokens(&prompt) >= 10000;
        let mut runner = ctx.clone().completion_iter(
            CompletionRequest {
                instructions: format!(
                    "{}\n\n---\n\n# `DESCRIBE PRIMER` Result:\n{}\n\n---\n\n# Your Notes:\n{}\n\n# Counterparty Profile:\n{}\n\n# Current Datetime: {}",
                    super::prompts::active_prompt(super::prompts::PromptTarget::Formation),
                    primer,
                    serde_json::to_string(&notes.items).unwrap_or_default(),
                    serde_json::to_string(&counterparty_info).unwrap_or_default(),
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
        runner.set_unbound(true);

        let mut host = FormationRunnerHost {
            agent: self,
            review_pending: should_review,
        };
        drive_runner_loop(&mut host, &mut runner, conversation).await;
    }
}

/// Formation's seams of the shared runner loop (`drive_runner_loop`): the
/// review pass for large inputs and the Failed-retry `failed_reason` reset.
struct FormationRunnerHost<'a> {
    agent: &'a FormationAgent,
    /// Large inputs get a mandatory review pass (REVIEW_INSTRUCTIONS) before
    /// an idle runner may count as done.
    review_pending: bool,
}

impl RunnerHost for FormationRunnerHost<'_> {
    fn label(&self) -> &'static str {
        "formation"
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
        runner.is_done() || runner.is_idle() && !self.review_pending
    }

    fn on_turn_success(&self, conversation: &mut Conversation) {
        // Clears a previous attempt's failure so the process_loop Failed
        // retry converges to a clean Completed snapshot.
        conversation.failed_reason = None;
    }

    fn after_turn(&mut self, runner: &mut CompletionRunner, is_done: bool) -> RunnerFlow {
        if self.review_pending && runner.is_idle() {
            runner.prune_req_raw_history();
            runner.follow_up(REVIEW_INSTRUCTIONS.to_string());
            self.review_pending = false;
            return RunnerFlow::Continue;
        }

        if is_done {
            RunnerFlow::Break
        } else {
            RunnerFlow::Continue
        }
    }
}

impl Agent<AgentCtx> for FormationAgent {
    fn name(&self) -> String {
        Self::NAME.to_string()
    }

    fn description(&self) -> String {
        "Receives conversation messages and encodes them into structured memory within the Cognitive Nexus via KIP.".to_string()
    }

    fn tool_dependencies(&self) -> Vec<String> {
        vec![self.memory.name(), NoteTool::NAME.to_string()]
    }

    // 接收来自外部的 FormationInput，创建一个新的 Conversation，并启动处理流程。
    async fn run(
        &self,
        ctx: AgentCtx,
        prompt: String, // FormationInput serialized as JSON string
        _resources: Vec<Resource>,
    ) -> Result<AgentOutput, BoxError> {
        let caller = ctx.caller();
        let now_ms = unix_ms();
        let token_count = estimate_tokens(&prompt);
        if token_count > self.max_input_tokens {
            return Err(format!(
                "Input too large: {} tokens (estimated), max allowed is {} tokens",
                token_count, self.max_input_tokens
            )
            .into());
        }

        let mut conversation = Conversation {
            user: *caller,
            messages: vec![json!(Message {
                role: "user".into(),
                content: vec![prompt.into()],
                ..Default::default()
            })],
            period: now_ms / 3600 / 1000,
            created_at: now_ms,
            updated_at: now_ms,
            label: Some("formation".to_string()),
            ..Default::default()
        };

        let id = self
            .memory
            .add_conversation(ConversationRef::from(&conversation))
            .await?;
        conversation._id = id;
        let res = AgentOutput {
            conversation: Some(id),
            ..Default::default()
        };

        let is_idle = self.processing_conversation.load(Ordering::SeqCst) == 0;
        if is_idle {
            if self.hook.is_maintenance_processing() {
                log::info!(
                    target: "brain",
                    conversation = id;
                    "Formation queued while maintenance is processing"
                );
            } else {
                // A missing marker means nothing was processed yet; resume from
                // the beginning to catch conversations queued before this one.
                let prev_id = self.get_processed().unwrap_or_default();
                if prev_id + 1 < id {
                    // Resume from the last processed conversation to catch any missed ones
                    if let Some(conv) = self.find_next_submitted(prev_id).await {
                        self.try_process(ctx, conv);
                    }
                } else {
                    self.try_process(ctx, conversation);
                }
            }
        }

        Ok(res)
    }
}

#[cfg(test)]
mod tests {
    use super::{FormationAgent, ProcessingGuard};
    use crate::{
        agents::SELF_USER_ID,
        space::AppState,
        types::{FormationInput, InputContext, MaintenanceInput, MaintenanceScope},
    };
    use anda_core::{
        Agent, AgentOutput, BoxError, BoxPinFut, CompletionRequest, ContentPart, Message,
        Principal, ToolCall, Usage,
    };
    use anda_db::{database::DBConfig, storage::StorageConfig};
    use anda_engine::{
        context::{AgentCtx, COMPACTION_PROMPT},
        management::{BaseManagement, Visibility},
        memory::{Conversation, ConversationRef, ConversationStatus},
        model::{CompletionFeaturesDyn, Model, Models, reqwest},
        unix_ms,
    };
    use object_store::memory::InMemory;
    use serde_json::json;
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };
    use std::{collections::BTreeSet, sync::Mutex};
    use tokio::time::{Duration, sleep};

    #[derive(Debug)]
    struct SuccessCompleter;

    impl CompletionFeaturesDyn for SuccessCompleter {
        fn model_name(&self) -> String {
            "success-test-model".to_string()
        }

        fn completion(&self, req: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
            Box::pin(async move {
                Ok(AgentOutput {
                    content: "formation done".to_string(),
                    chat_history: vec![Message {
                        role: "assistant".to_string(),
                        content: vec![format!("processed: {}", req.prompt).into()],
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
            "failed-reason-test-model".to_string()
        }

        fn completion(&self, _req: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
            Box::pin(async move {
                Ok(AgentOutput {
                    failed_reason: Some("formation failed".to_string()),
                    chat_history: vec![Message {
                        role: "assistant".to_string(),
                        content: vec!["formation failure".to_string().into()],
                        ..Default::default()
                    }],
                    ..Default::default()
                })
            })
        }
    }

    #[derive(Debug)]
    struct RetryCompleter {
        calls: Arc<AtomicU64>,
    }

    impl CompletionFeaturesDyn for RetryCompleter {
        fn model_name(&self) -> String {
            "retry-test-model".to_string()
        }

        fn completion(&self, req: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
            let calls = self.calls.clone();
            Box::pin(async move {
                let call = calls.fetch_add(1, Ordering::SeqCst);
                if call == 0 {
                    Ok(AgentOutput {
                        failed_reason: Some("transient formation failure".to_string()),
                        chat_history: vec![Message {
                            role: "assistant".to_string(),
                            content: vec!["retry later".to_string().into()],
                            ..Default::default()
                        }],
                        ..Default::default()
                    })
                } else {
                    Ok(AgentOutput {
                        content: "formation retried".to_string(),
                        chat_history: vec![Message {
                            role: "assistant".to_string(),
                            content: vec![format!("recovered: {}", req.prompt).into()],
                            ..Default::default()
                        }],
                        ..Default::default()
                    })
                }
            })
        }
    }

    #[derive(Debug)]
    struct ErrorCompleter;

    impl CompletionFeaturesDyn for ErrorCompleter {
        fn model_name(&self) -> String {
            "error-test-model".to_string()
        }

        fn completion(&self, _req: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
            Box::pin(async move { Err("model error".into()) })
        }
    }

    /// Emits an endless stream of tool calls (a non-converging model), and a
    /// summary for compaction handoff requests so the runner keeps looping.
    #[derive(Debug)]
    struct ToolLoopCompleter;

    impl CompletionFeaturesDyn for ToolLoopCompleter {
        fn model_name(&self) -> String {
            "formation-tool-loop-test-model".to_string()
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

    /// Emits `tool_turns` tool-call turns, then a final content turn.
    #[derive(Debug)]
    struct CountedToolThenDoneCompleter {
        calls: Arc<AtomicU64>,
        tool_turns: u64,
    }

    impl CompletionFeaturesDyn for CountedToolThenDoneCompleter {
        fn model_name(&self) -> String {
            "formation-counted-tool-test-model".to_string()
        }

        fn completion(&self, _req: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
            let calls = self.calls.clone();
            let tool_turns = self.tool_turns;
            Box::pin(async move {
                let call = calls.fetch_add(1, Ordering::SeqCst);
                let usage = Usage {
                    input_tokens: 10,
                    output_tokens: 1,
                    requests: 1,
                    ..Default::default()
                };
                if call < tool_turns {
                    return Ok(AgentOutput {
                        tool_calls: vec![ToolCall {
                            name: "execute_kip".to_string(),
                            args: serde_json::json!({"commands": []}),
                            result: None,
                            call_id: Some(format!("call-{call}")),
                            remote_id: None,
                        }],
                        usage,
                        ..Default::default()
                    });
                }
                Ok(AgentOutput {
                    content: "formation done".to_string(),
                    chat_history: vec![Message {
                        role: "assistant".to_string(),
                        content: vec!["formation done".to_string().into()],
                        ..Default::default()
                    }],
                    usage,
                    ..Default::default()
                })
            })
        }
    }

    #[derive(Debug)]
    struct SlowCompleter;

    impl CompletionFeaturesDyn for SlowCompleter {
        fn model_name(&self) -> String {
            "slow-test-model".to_string()
        }

        fn completion(&self, req: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
            Box::pin(async move {
                sleep(Duration::from_millis(150)).await;
                Ok(AgentOutput {
                    content: "done".to_string(),
                    chat_history: vec![Message {
                        role: "assistant".to_string(),
                        content: vec![format!("processed: {}", req.prompt).into()],
                        ..Default::default()
                    }],
                    ..Default::default()
                })
            })
        }
    }

    #[derive(Debug)]
    struct CompactionCompleter {
        requests: Arc<Mutex<Vec<String>>>,
    }

    impl CompletionFeaturesDyn for CompactionCompleter {
        fn model_name(&self) -> String {
            "formation-compaction-test-model".to_string()
        }

        fn completion(&self, req: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
            let requests = self.requests.clone();
            Box::pin(async move {
                let text = request_text(&req);
                requests.lock().unwrap().push(text.clone());
                if text.contains(COMPACTION_PROMPT.trim()) {
                    return Ok(AgentOutput {
                        content: "handoff summary".to_string(),
                        chat_history: vec![Message {
                            role: "assistant".to_string(),
                            content: vec!["summary turn".to_string().into()],
                            ..Default::default()
                        }],
                        usage: Usage {
                            input_tokens: 8,
                            output_tokens: 2,
                            requests: 1,
                            ..Default::default()
                        },
                        ..Default::default()
                    });
                }

                if text.contains("handoff summary") {
                    return Ok(AgentOutput {
                        content: "review complete".to_string(),
                        chat_history: vec![Message {
                            role: "assistant".to_string(),
                            content: vec!["reviewed after compaction".to_string().into()],
                            ..Default::default()
                        }],
                        usage: Usage {
                            input_tokens: 16,
                            output_tokens: 4,
                            requests: 1,
                            ..Default::default()
                        },
                        ..Default::default()
                    });
                }

                Ok(AgentOutput {
                    content: "formation draft".to_string(),
                    chat_history: vec![Message {
                        role: "assistant".to_string(),
                        content: vec!["draft before compaction".to_string().into()],
                        ..Default::default()
                    }],
                    usage: Usage {
                        input_tokens: 900,
                        output_tokens: 4,
                        requests: 1,
                        ..Default::default()
                    },
                    ..Default::default()
                })
            })
        }
    }

    fn part_text(part: &ContentPart) -> Option<String> {
        match part {
            ContentPart::Text { text } | ContentPart::Reasoning { text } => Some(text.clone()),
            ContentPart::ToolOutput { output, .. } | ContentPart::ToolCall { args: output, .. } => {
                Some(output.to_string())
            }
            _ => None,
        }
    }

    fn request_text(req: &CompletionRequest) -> String {
        let mut text = Vec::new();
        if !req.prompt.is_empty() {
            text.push(req.prompt.clone());
        }
        text.extend(req.content.iter().filter_map(part_text));
        for message in &req.chat_history {
            text.extend(message.content.iter().filter_map(part_text));
        }
        text.join("\n")
    }

    fn test_db_config(name: &str) -> DBConfig {
        DBConfig {
            name: name.to_string(),
            description: "test database".to_string(),
            storage: StorageConfig::default(),
            lock: None,
        }
    }

    fn test_app_state(name: &str) -> AppState {
        test_app_state_with_models(name, Arc::new(Models::default()))
    }

    fn test_app_state_with_completer<C>(name: &str, completer: C) -> AppState
    where
        C: CompletionFeaturesDyn,
    {
        let models = Models::default();
        models.set_model(Model::with_completer(Arc::new(completer)));
        test_app_state_with_models(name, Arc::new(models))
    }

    fn test_app_state_with_models(name: &str, models: Arc<Models>) -> AppState {
        let management = Arc::new(BaseManagement {
            controller: SELF_USER_ID,
            managers: BTreeSet::new(),
            visibility: Visibility::Public,
        });
        let http_client = reqwest::Client::builder().build().unwrap();

        AppState::new(
            Arc::new(InMemory::new()),
            Arc::new(test_db_config(name)),
            management,
            http_client,
            models,
            Arc::new(vec![]),
            "anda_brain".to_string(),
            "test".to_string(),
            0,
        )
    }

    fn formation_prompt(counterparty: Option<&str>) -> String {
        formation_prompt_with_text("remember this preference", counterparty)
    }

    fn formation_prompt_with_text(text: &str, counterparty: Option<&str>) -> String {
        serde_json::to_string(&FormationInput {
            messages: vec![Message {
                role: "user".to_string(),
                content: vec![text.to_string().into()],
                ..Default::default()
            }],
            context: counterparty.map(|counterparty| InputContext {
                counterparty: Some(counterparty.to_string()),
                ..Default::default()
            }),
            timestamp: None,
        })
        .unwrap()
    }

    async fn stored_conversation(
        space: &crate::space::Space,
        messages: Vec<serde_json::Value>,
    ) -> Conversation {
        let now = unix_ms();
        let mut conversation = Conversation {
            user: SELF_USER_ID,
            status: ConversationStatus::Submitted,
            messages,
            label: Some("formation".to_string()),
            created_at: now,
            updated_at: now,
            ..Default::default()
        };
        let id = space
            .memory
            .add_conversation(ConversationRef::from(&conversation))
            .await
            .unwrap();
        conversation._id = id;
        conversation
    }

    async fn create_loaded_space(app: &AppState, id: &str) -> Arc<crate::space::Space> {
        app.admin_create_space(
            Principal::from_slice(&[1]),
            Principal::from_slice(&[2]),
            id.to_string(),
            1,
            unix_ms(),
        )
        .await
        .unwrap();

        app.load_space(id, false).await.unwrap()
    }

    #[test]
    fn processing_guard_resets_conversation_id_on_drop() {
        let processing = Arc::new(AtomicU64::new(42));

        {
            let _guard = ProcessingGuard(processing.clone());
            assert_eq!(processing.load(Ordering::SeqCst), 42);
        }

        assert_eq!(processing.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn formation_agent_name_matches_registered_agent_name() {
        assert_eq!(FormationAgent::NAME, "formation_memory");
    }

    #[tokio::test]
    async fn formation_agent_trait_metadata_matches_runtime_registration() {
        let app = test_app_state("formation_trait_metadata");
        let space = create_loaded_space(&app, "formation_trait_metadata").await;

        assert_eq!(
            Agent::<AgentCtx>::name(space.formation.as_ref()),
            FormationAgent::NAME
        );
        assert!(
            Agent::<AgentCtx>::description(space.formation.as_ref()).contains("structured memory")
        );
        let tools = Agent::<AgentCtx>::tool_dependencies(space.formation.as_ref());
        assert!(tools.iter().any(|name| name == "execute_kip"));
        assert!(tools.iter().any(|name| name == "note"));
    }

    #[tokio::test]
    async fn find_next_submitted_skips_terminal_and_non_formation_conversations() {
        let app = test_app_state("formation_find_next");
        let space = create_loaded_space(&app, "formation_find_next").await;
        let now = unix_ms();

        for conversation in [
            Conversation {
                user: SELF_USER_ID,
                status: ConversationStatus::Completed,
                label: Some("formation".to_string()),
                created_at: now,
                updated_at: now,
                ..Default::default()
            },
            Conversation {
                user: SELF_USER_ID,
                status: ConversationStatus::Submitted,
                label: Some("recall".to_string()),
                created_at: now + 1,
                updated_at: now + 1,
                ..Default::default()
            },
            Conversation {
                user: SELF_USER_ID,
                status: ConversationStatus::Cancelled,
                label: Some("formation".to_string()),
                created_at: now + 2,
                updated_at: now + 2,
                ..Default::default()
            },
        ] {
            space
                .memory
                .add_conversation(ConversationRef::from(&conversation))
                .await
                .unwrap();
        }

        let pending = Conversation {
            user: SELF_USER_ID,
            status: ConversationStatus::Submitted,
            label: Some("formation".to_string()),
            created_at: now + 3,
            updated_at: now + 3,
            ..Default::default()
        };
        let pending_id = space
            .memory
            .add_conversation(ConversationRef::from(&pending))
            .await
            .unwrap();

        let found = space.formation.find_next_submitted(0).await.unwrap();
        assert_eq!(found._id, pending_id);
        assert!(
            space
                .formation
                .find_next_submitted(pending_id)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn mark_conversation_failed_persists_status_and_reason() {
        let app = test_app_state("formation_mark_failed");
        let space = create_loaded_space(&app, "formation_mark_failed").await;
        let now = unix_ms();
        let mut conversation = Conversation {
            user: SELF_USER_ID,
            status: ConversationStatus::Submitted,
            label: Some("formation".to_string()),
            created_at: now,
            updated_at: now,
            ..Default::default()
        };
        let id = space
            .memory
            .add_conversation(ConversationRef::from(&conversation))
            .await
            .unwrap();
        conversation._id = id;

        space
            .formation
            .mark_conversation_failed(&mut conversation, "boom".to_string())
            .await;

        assert_eq!(conversation.status, ConversationStatus::Failed);
        assert_eq!(conversation.failed_reason.as_deref(), Some("boom"));
        let stored = space.memory.get_conversation(id).await.unwrap();
        assert_eq!(stored.status, ConversationStatus::Failed);
        assert_eq!(stored.failed_reason.as_deref(), Some("boom"));
    }

    #[tokio::test]
    async fn start_process_rejects_busy_and_maintenance_states() {
        let app = test_app_state("formation_start_guards");
        let space = create_loaded_space(&app, "formation_start_guards").await;
        let ctx = space
            .ctx_for_test(SELF_USER_ID, FormationAgent::NAME)
            .unwrap();

        space
            .formation
            .processing_conversation
            .store(42, Ordering::SeqCst);
        let err = space
            .formation
            .start_process(ctx.clone(), 1)
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("already processing conversation 42")
        );
        space
            .formation
            .processing_conversation
            .store(0, Ordering::SeqCst);

        let app = test_app_state_with_completer("formation_maintenance_guard", SlowCompleter);
        let space = create_loaded_space(&app, "formation_maintenance_guard").await;
        let maintenance = space
            .maintenance(
                SELF_USER_ID,
                MaintenanceInput {
                    scope: MaintenanceScope::Quick,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(maintenance.conversation.is_some());

        let ctx = space
            .ctx_for_test(SELF_USER_ID, FormationAgent::NAME)
            .unwrap();
        let err = space.formation.start_process(ctx, 1).await.unwrap_err();
        assert!(err.to_string().contains("MaintenanceAgent is processing"));

        for _ in 0..100 {
            if !space.is_processing() {
                return;
            }
            sleep(Duration::from_millis(10)).await;
        }
        panic!("maintenance did not finish");
    }

    #[tokio::test]
    async fn start_process_finds_pending_conversation_and_dispatches_worker() {
        let app = test_app_state_with_completer("formation_start_success", SuccessCompleter);
        let space = create_loaded_space(&app, "formation_start_success").await;
        let ctx = space
            .ctx_for_test(SELF_USER_ID, FormationAgent::NAME)
            .unwrap();
        let pending = stored_conversation(
            &space,
            vec![json!(Message {
                role: "user".to_string(),
                content: vec![formation_prompt(None).into()],
                ..Default::default()
            })],
        )
        .await;

        space
            .formation
            .start_process(ctx, pending._id)
            .await
            .unwrap();
        for _ in 0..100 {
            if !space.formation.is_processing() {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }

        assert_eq!(space.formation.get_processed(), Some(pending._id));
        assert_eq!(
            space
                .memory
                .get_conversation(pending._id)
                .await
                .unwrap()
                .status,
            ConversationStatus::Completed
        );
    }

    #[tokio::test]
    async fn run_queues_while_maintenance_runs_and_resumes_from_processed_gap() {
        let app = test_app_state_with_completer("formation_run_resume_gap", SuccessCompleter);
        let space = create_loaded_space(&app, "formation_run_resume_gap").await;
        let ctx = space
            .ctx_for_test(SELF_USER_ID, FormationAgent::NAME)
            .unwrap();
        space
            .memory
            .conversations
            .save_extension("brain_processed".to_string(), 0_u64.into())
            .await
            .unwrap();
        let missed = stored_conversation(
            &space,
            vec![json!(Message {
                role: "user".to_string(),
                content: vec![formation_prompt(None).into()],
                ..Default::default()
            })],
        )
        .await;

        let output = Agent::<AgentCtx>::run(
            space.formation.as_ref(),
            ctx,
            formation_prompt(Some("resume-gap-user")),
            vec![],
        )
        .await
        .unwrap();
        let queued_id = output.conversation.unwrap();

        for _ in 0..100 {
            if !space.formation.is_processing() {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(space.formation.get_processed(), Some(queued_id));
        assert_eq!(
            space
                .memory
                .get_conversation(missed._id)
                .await
                .unwrap()
                .status,
            ConversationStatus::Completed
        );

        let app = test_app_state_with_completer("formation_run_maintenance_queue", SlowCompleter);
        let space = create_loaded_space(&app, "formation_run_maintenance_queue").await;
        let maintenance = space
            .maintenance(
                SELF_USER_ID,
                MaintenanceInput {
                    scope: MaintenanceScope::Quick,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(maintenance.conversation.is_some());
        let ctx = space
            .ctx_for_test(SELF_USER_ID, FormationAgent::NAME)
            .unwrap();
        let output = Agent::<AgentCtx>::run(
            space.formation.as_ref(),
            ctx,
            formation_prompt(None),
            vec![],
        )
        .await
        .unwrap();
        assert!(output.conversation.is_some());
        assert!(!space.formation.is_processing());

        for _ in 0..100 {
            if !space.is_processing() {
                return;
            }
            sleep(Duration::from_millis(10)).await;
        }
        panic!("maintenance did not finish");
    }

    #[tokio::test]
    async fn try_process_returns_when_another_conversation_owns_the_guard() {
        let app = test_app_state("formation_try_process_guard");
        let space = create_loaded_space(&app, "formation_try_process_guard").await;
        let ctx = space
            .ctx_for_test(SELF_USER_ID, FormationAgent::NAME)
            .unwrap();
        let conversation = stored_conversation(
            &space,
            vec![json!(Message {
                role: "user".to_string(),
                content: vec![formation_prompt(None).into()],
                ..Default::default()
            })],
        )
        .await;

        space
            .formation
            .processing_conversation
            .store(conversation._id + 10, Ordering::SeqCst);
        space.formation.try_process(ctx, conversation.clone());

        assert_eq!(
            space
                .formation
                .processing_conversation
                .load(Ordering::SeqCst),
            conversation._id + 10
        );
    }

    #[tokio::test]
    async fn process_one_marks_missing_prompt_and_completion_errors() {
        let app = test_app_state("formation_no_prompt");
        let space = create_loaded_space(&app, "formation_no_prompt").await;
        let ctx = space
            .ctx_for_test(SELF_USER_ID, FormationAgent::NAME)
            .unwrap();
        let mut no_prompt = stored_conversation(&space, vec![]).await;

        space.formation.process_one(&ctx, &mut no_prompt).await;

        assert_eq!(no_prompt.status, ConversationStatus::Failed);
        assert_eq!(no_prompt.failed_reason.as_deref(), Some("No prompt found"));

        let app = test_app_state_with_completer("formation_model_error", ErrorCompleter);
        let space = create_loaded_space(&app, "formation_model_error").await;
        let ctx = space
            .ctx_for_test(SELF_USER_ID, FormationAgent::NAME)
            .unwrap();
        let mut conversation = stored_conversation(
            &space,
            vec![json!(Message {
                role: "user".to_string(),
                content: vec![formation_prompt(Some("counterparty-error")).into()],
                ..Default::default()
            })],
        )
        .await;

        space.formation.process_one(&ctx, &mut conversation).await;

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
    async fn process_one_persists_model_failed_reason() {
        let app = test_app_state_with_completer("formation_failed_reason", FailedReasonCompleter);
        let space = create_loaded_space(&app, "formation_failed_reason").await;
        let ctx = space
            .ctx_for_test(SELF_USER_ID, FormationAgent::NAME)
            .unwrap();
        let mut conversation = stored_conversation(
            &space,
            vec![json!(Message {
                role: "user".to_string(),
                content: vec![formation_prompt(None).into()],
                ..Default::default()
            })],
        )
        .await;

        space.formation.process_one(&ctx, &mut conversation).await;

        assert_eq!(conversation.status, ConversationStatus::Failed);
        assert_eq!(
            conversation.failed_reason.as_deref(),
            Some("formation failed")
        );
        let stored = space
            .memory
            .get_conversation(conversation._id)
            .await
            .unwrap();
        assert_eq!(stored.status, ConversationStatus::Failed);
        assert_eq!(stored.failed_reason.as_deref(), Some("formation failed"));
    }

    #[tokio::test]
    async fn process_one_fails_tool_loop_at_model_turn_limit() {
        let app = test_app_state_with_completer("formation_turn_limit", ToolLoopCompleter);
        let space = create_loaded_space(&app, "formation_turn_limit").await;
        let ctx = space
            .ctx_for_test(SELF_USER_ID, FormationAgent::NAME)
            .unwrap();
        let mut conversation = stored_conversation(
            &space,
            vec![json!(Message {
                role: "user".to_string(),
                content: vec![formation_prompt(None).into()],
                ..Default::default()
            })],
        )
        .await;

        space.formation.process_one(&ctx, &mut conversation).await;

        assert_eq!(conversation.status, ConversationStatus::Failed);
        assert!(
            conversation
                .failed_reason
                .as_deref()
                .unwrap_or_default()
                .contains(&format!(
                    "exceeded model turn limit of {}",
                    super::FORMATION_MAX_MODEL_TURNS
                )),
            "failed_reason: {:?}",
            conversation.failed_reason
        );
        let stored = space
            .memory
            .get_conversation(conversation._id)
            .await
            .unwrap();
        assert_eq!(stored.status, ConversationStatus::Failed);
    }

    #[tokio::test]
    async fn process_one_throttles_intermediate_persistence_until_terminal() {
        let app = test_app_state_with_completer(
            "formation_persist_throttle",
            CountedToolThenDoneCompleter {
                calls: Arc::new(AtomicU64::new(0)),
                // Fewer working turns than PERSIST_EVERY_N_TURNS, so only the
                // terminal turn triggers a write.
                tool_turns: 3,
            },
        );
        let space = create_loaded_space(&app, "formation_persist_throttle").await;
        let ctx = space
            .ctx_for_test(SELF_USER_ID, FormationAgent::NAME)
            .unwrap();
        let mut conversation = stored_conversation(
            &space,
            vec![json!(Message {
                role: "user".to_string(),
                content: vec![formation_prompt(None).into()],
                ..Default::default()
            })],
        )
        .await;

        let updates_before = space.memory.conversations.stats().update_count;
        space.formation.process_one(&ctx, &mut conversation).await;
        let updates_after = space.memory.conversations.stats().update_count;

        assert_eq!(conversation.status, ConversationStatus::Completed);
        // 4 model turns total: 3 intermediate Working turns are throttled and
        // only the terminal Completed turn is written.
        assert_eq!(updates_after - updates_before, 1);
        let stored = space
            .memory
            .get_conversation(conversation._id)
            .await
            .unwrap();
        assert_eq!(stored.status, ConversationStatus::Completed);
        // The terminal write carries the full accumulated usage (4 model
        // turns x 10 input tokens; `requests` also counts tool executions),
        // so throttled turns are not lost from the stored snapshot.
        assert_eq!(stored.usage.input_tokens, 40);
    }

    #[tokio::test]
    async fn run_rejects_oversized_formation_input_before_persisting() {
        let app = test_app_state("formation_input_too_large");
        let space = create_loaded_space(&app, "formation_input_too_large").await;
        let ctx = space
            .ctx_for_test(SELF_USER_ID, FormationAgent::NAME)
            .unwrap();
        let prompt = "x ".repeat(1_000_000);

        let err = Agent::<AgentCtx>::run(space.formation.as_ref(), ctx, prompt, vec![])
            .await
            .unwrap_err();

        assert!(err.to_string().contains("Input too large"));
        assert_eq!(space.memory.conversations.len(), 0);
    }

    #[tokio::test]
    async fn process_loop_processes_submitted_formation_queue_sequentially() {
        let app = test_app_state_with_completer("formation_process_loop_queue", SuccessCompleter);
        let space = create_loaded_space(&app, "formation_process_loop_queue").await;
        let ctx = space
            .ctx_for_test(SELF_USER_ID, FormationAgent::NAME)
            .unwrap();
        let first = stored_conversation(
            &space,
            vec![json!(Message {
                role: "user".to_string(),
                content: vec![formation_prompt(None).into()],
                ..Default::default()
            })],
        )
        .await;
        let second = stored_conversation(
            &space,
            vec![json!(Message {
                role: "user".to_string(),
                content: vec![formation_prompt(Some("queue-user")).into()],
                ..Default::default()
            })],
        )
        .await;

        space
            .formation
            .processing_conversation
            .store(first._id, Ordering::SeqCst);
        space.formation.process_loop(ctx, first).await;

        assert_eq!(
            space
                .formation
                .processing_conversation
                .load(Ordering::SeqCst),
            0
        );
        assert_eq!(space.formation.get_processed(), Some(second._id));
        assert_eq!(
            space
                .memory
                .get_conversation(second._id)
                .await
                .unwrap()
                .status,
            ConversationStatus::Completed
        );
    }

    #[tokio::test(start_paused = true)]
    async fn process_loop_retries_failed_conversation_once_and_clears_failure_reason() {
        let calls = Arc::new(AtomicU64::new(0));
        let app = test_app_state_with_completer(
            "formation_process_loop_retry",
            RetryCompleter {
                calls: calls.clone(),
            },
        );
        let space = create_loaded_space(&app, "formation_process_loop_retry").await;
        let ctx = space
            .ctx_for_test(SELF_USER_ID, FormationAgent::NAME)
            .unwrap();
        let pending = stored_conversation(
            &space,
            vec![json!(Message {
                role: "user".to_string(),
                content: vec![formation_prompt(None).into()],
                ..Default::default()
            })],
        )
        .await;

        space
            .formation
            .processing_conversation
            .store(pending._id, Ordering::SeqCst);
        space.formation.process_loop(ctx, pending.clone()).await;

        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            space
                .formation
                .processing_conversation
                .load(Ordering::SeqCst),
            0
        );
        assert_eq!(space.formation.get_processed(), Some(pending._id));
        let stored = space.memory.get_conversation(pending._id).await.unwrap();
        assert_eq!(stored.status, ConversationStatus::Completed);
        assert_eq!(stored.failed_reason, None);
    }

    #[tokio::test]
    async fn process_loop_keeps_processed_marker_as_high_water_mark() {
        let app = test_app_state_with_completer("formation_marker_high_water", SuccessCompleter);
        let space = create_loaded_space(&app, "formation_marker_high_water").await;
        let ctx = space
            .ctx_for_test(SELF_USER_ID, FormationAgent::NAME)
            .unwrap();
        let pending = stored_conversation(
            &space,
            vec![json!(Message {
                role: "user".to_string(),
                content: vec![formation_prompt(None).into()],
                ..Default::default()
            })],
        )
        .await;
        space
            .memory
            .conversations
            .save_extension("brain_processed".to_string(), 9_u64.into())
            .await
            .unwrap();

        space
            .formation
            .processing_conversation
            .store(pending._id, Ordering::SeqCst);
        space.formation.process_loop(ctx, pending.clone()).await;

        // Reprocessing an older conversation must not rewind the marker.
        assert_eq!(space.formation.get_processed(), Some(9));
        assert_eq!(
            space
                .memory
                .get_conversation(pending._id)
                .await
                .unwrap()
                .status,
            ConversationStatus::Completed
        );
    }

    #[tokio::test]
    async fn process_loop_triggers_scheduled_maintenance_at_threshold() {
        let app =
            test_app_state_with_completer("formation_process_loop_maintenance", SuccessCompleter);
        let space = create_loaded_space(&app, "formation_process_loop_maintenance").await;
        let ctx = space
            .ctx_for_test(SELF_USER_ID, FormationAgent::NAME)
            .unwrap();

        for _ in 0..20 {
            let completed = Conversation {
                user: SELF_USER_ID,
                status: ConversationStatus::Completed,
                label: Some("formation".to_string()),
                created_at: unix_ms(),
                updated_at: unix_ms(),
                ..Default::default()
            };
            space
                .memory
                .add_conversation(ConversationRef::from(&completed))
                .await
                .unwrap();
        }
        let pending = stored_conversation(
            &space,
            vec![json!(Message {
                role: "user".to_string(),
                content: vec![formation_prompt(None).into()],
                ..Default::default()
            })],
        )
        .await;
        assert_eq!(pending._id, 21);

        space
            .formation
            .processing_conversation
            .store(pending._id, Ordering::SeqCst);
        space.formation.process_loop(ctx, pending).await;

        assert_eq!(
            space
                .formation
                .processing_conversation
                .load(Ordering::SeqCst),
            0
        );
        for _ in 0..100 {
            if !space.is_processing() {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(space.maintenance_for_test().get_processed_at().daydream, 21);
    }

    #[tokio::test]
    async fn process_one_reviews_large_prompts_and_appends_follow_up_history() {
        let app = test_app_state_with_completer("formation_review_large_prompt", SuccessCompleter);
        let space = create_loaded_space(&app, "formation_review_large_prompt").await;
        let ctx = space
            .ctx_for_test(SELF_USER_ID, FormationAgent::NAME)
            .unwrap();
        // ~44k chars ≈ 11k estimated tokens, above the 10k-token review threshold
        let large_text = "x".repeat(44_000);
        let mut conversation = stored_conversation(
            &space,
            vec![json!(Message {
                role: "user".to_string(),
                content: vec![formation_prompt_with_text(&large_text, None).into()],
                ..Default::default()
            })],
        )
        .await;

        space.formation.process_one(&ctx, &mut conversation).await;

        assert_eq!(conversation.status, ConversationStatus::Completed);
        assert!(conversation.messages.len() >= 2);
    }

    #[tokio::test]
    async fn process_one_compacts_before_large_prompt_review_handoff() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let models = Models::default();
        let mut model = Model::with_completer(Arc::new(CompactionCompleter {
            requests: requests.clone(),
        }));
        model.context_window = 1000;
        models.set_model(model);
        let app = test_app_state_with_models("formation_compacts_before_review", Arc::new(models));
        let space = create_loaded_space(&app, "formation_compacts_before_review").await;
        let ctx = space
            .ctx_for_test(SELF_USER_ID, FormationAgent::NAME)
            .unwrap();
        // ~44k chars ≈ 11k estimated tokens, above the 10k-token review threshold
        let large_text = "x".repeat(44_000);
        let mut conversation = stored_conversation(
            &space,
            vec![json!(Message {
                role: "user".to_string(),
                content: vec![formation_prompt_with_text(&large_text, None).into()],
                ..Default::default()
            })],
        )
        .await;

        space.formation.process_one(&ctx, &mut conversation).await;

        assert_eq!(conversation.status, ConversationStatus::Completed);
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 3, "{requests:#?}");
        assert!(requests[1].contains(COMPACTION_PROMPT.trim()));
        assert!(requests[2].contains("handoff summary"));

        let messages = serde_json::to_string(&conversation.messages).unwrap();
        assert!(messages.contains("draft before compaction"));
        assert!(messages.contains("handoff summary"));
        assert!(messages.contains("reviewed after compaction"));
    }
}

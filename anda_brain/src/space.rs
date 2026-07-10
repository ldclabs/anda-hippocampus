use anda_cognitive_nexus::{CognitiveNexus, ConceptPK};
use anda_core::{
    AgentInput, AgentOutput, BoxError, FunctionDefinition, Message, Principal, Resource, Usage,
};
use anda_db::{
    collection::{Collection, CollectionConfig},
    database::{AndaDB, DBConfig},
    error::DBError,
    index::BTree,
    query::Fv,
    schema::DocumentId,
};
use anda_db_tfs::jieba_tokenizer;
use anda_engine::{
    engine::Engine,
    extension::note::NoteTool,
    management::Management,
    memory::{Conversation, ConversationStatus, Conversations, MemoryManagement, MemoryTool},
    model::{Model, ModelConfig as EngineModelConfig, Models, reqwest},
    rfc3339_datetime, rfc3339_datetime_now, unix_ms,
};
use anda_kip::{
    KipError, KipErrorCode, META_SELF_NAME, PERSON_SELF_KIP, PERSON_SYSTEM_KIP, PERSON_TYPE,
    parse_kml,
};
use ic_auth_types::ByteBufB64;
use ic_cose_types::cose::{
    SIGN1_TAG, cwt::cwt_from, ed25519::VerifyingKey, sign1::cose_sign1_from, skip_prefix,
};
use object_store::{ObjectStore, memory::InMemory};
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    str::FromStr,
    sync::{
        Arc, LazyLock, OnceLock, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{OnceCell, RwLock},
    time::timeout,
};
use tokio_util::sync::CancellationToken;

use crate::{
    agents::{
        BrainHook, FormationAgent, MaintenanceAgent, READONLY_KIP_TIMEOUT, RecallAgent,
        SELF_USER_ID, TimedMemoryReadonly,
    },
    assess,
    ledger::{MissCache, UsageLedger},
    payload::StringOr,
    types::{
        AddSpaceTokenInput, CWToken, FormationInput, FormationStatus, MaintenanceInput,
        MaintenanceScope, MemoryForgetEntity, MemoryForgetInput, MemoryForgetReport,
        MemoryGraphCounters, MemoryMetrics, MemoryPolicy, MemorySettlementReport, MemoryStatus,
        ModelConfig, ProbeOutput, RecallInput, RecallOutput, SchemaAudit, SelfTestReport,
        ShadowEvalInput, ShadowReport, ShadowSample, SourceReliability, SpaceInfo, SpaceTier,
        SpaceToken, TokenScope, UpdateSpaceInput,
    },
    wiki::{
        WikiCommitTool, WikiDigest, WikiDigestReport, WikiReadTool, WikiSearchTool, WikiService,
    },
};

pub static FUNCTION_DEFINITION: LazyLock<FunctionDefinition> = LazyLock::new(|| {
    serde_json::from_value(json!({
        "name": "execute_kip",
        "description": "Executes one or more KIP (Knowledge Interaction Protocol) commands against the Cognitive Nexus to interact with your persistent memory.",
        "parameters": {
            "type": "object",
            "properties": {
                "commands": {
                    "type": "array",
                    "description": "An array of KIP commands for batch execution (reduces round-trips). Commands are executed sequentially; execution stops on first KML error.",
                    "items": {
                        "type": "string"
                    }
                },
                "parameters": {
                    "type": "object",
                    "description": "An optional JSON object of key-value pairs used for safe substitution of placeholders in the command string(s). Placeholders should start with ':' (e.g., :name, :limit). IMPORTANT: A placeholder must represent a complete JSON value token (e.g., name: :name). Do not embed placeholders inside quoted strings (e.g., \"Hello :name\"), because substitution uses JSON serialization."
                },
            },
            "required": ["commands"]
        },
        "strict": true
    })).unwrap()
});

pub struct SpaceEntry {
    cell: OnceCell<Arc<Space>>,
    last_access_ms: AtomicU64,
}

impl SpaceEntry {
    fn new() -> Self {
        Self {
            cell: OnceCell::new(),
            last_access_ms: AtomicU64::new(unix_ms()),
        }
    }

    fn touch(&self) {
        self.last_access_ms.store(unix_ms(), Ordering::Relaxed);
    }

    fn last_access_ms(&self) -> u64 {
        self.last_access_ms.load(Ordering::Relaxed)
    }
}

#[derive(Clone)]
pub struct AppState {
    spaces: Arc<RwLock<BTreeMap<String, Arc<SpaceEntry>>>>,
    object_store: Arc<dyn ObjectStore>,
    db_config: Arc<DBConfig>,
    http_client: reqwest::Client,
    models: Arc<Models>,
    ed25519_pubkeys: Arc<Vec<VerifyingKey>>,
    management: Arc<dyn Management>,
    /// Independent judge model (plan M9), installed on every space this
    /// state loads — service mode included, so shadow-eval verdicts stop
    /// falling back to the evaluated space's own model.
    judge_model: Arc<Option<ModelConfig>>,

    pub app_name: String,
    pub app_version: String,
    pub sharding: u32,
}

impl AppState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        object_store: Arc<dyn ObjectStore>,
        db_config: Arc<DBConfig>,
        management: Arc<dyn Management>,
        http_client: reqwest::Client,
        models: Arc<Models>,
        ed25519_pubkeys: Arc<Vec<VerifyingKey>>,
        app_name: String,
        app_version: String,
        sharding: u32,
    ) -> Self {
        Self {
            spaces: Arc::new(RwLock::new(BTreeMap::new())),
            object_store,
            db_config,
            management,
            http_client,
            models,
            ed25519_pubkeys,
            judge_model: Arc::new(None),
            app_name,
            app_version,
            sharding,
        }
    }

    /// Configures the independent judge model this state installs on every
    /// space it loads (consuming builder; call before the state is cloned).
    pub fn with_judge_model(mut self, config: Option<ModelConfig>) -> Self {
        self.judge_model = Arc::new(config);
        self
    }

    pub fn cwt_auth_enabled(&self) -> bool {
        !self.ed25519_pubkeys.is_empty()
    }

    /// The object store backing this state's spaces.
    pub fn object_store(&self) -> Arc<dyn ObjectStore> {
        self.object_store.clone()
    }

    /// Removes a space entry from the in-process cache. Used by the eval
    /// harness after a run-scoped space is closed and its objects deleted,
    /// so nothing can reopen a half-removed space through the cache.
    pub async fn evict_space(&self, space_id: &str) {
        self.spaces.write().await.remove(space_id);
    }

    /// Forks a space into its own in-memory store, optionally installing a
    /// candidate memory policy. Forks are fully isolated: nothing they do
    /// can reach the source space's graph, ledger, or metrics.
    async fn fork_space_for_shadow(
        &self,
        space_id: &str,
        policy: Option<MemoryPolicy>,
    ) -> Result<Arc<Space>, BoxError> {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        copy_space_objects(&self.object_store(), &store, space_id).await?;
        let state = self.fork_with_store(store);
        // autostart=false: the fork inherits the live space's formation
        // cursor and wiki-digest backlog, and must NOT resume them — that
        // would burn real LLM tokens twice and mutate both forks mid-replay,
        // making the A/B comparison non-reproducible.
        let fork = state.load_space_with(space_id, false, false).await?;
        if let Some(policy) = policy {
            fork.db
                .set_extension_from(MemoryPolicy::EXTENSION_KEY.to_string(), policy);
        }
        Ok(fork)
    }

    /// On-demand shadow evaluation (plan M11): forks the space twice —
    /// current policy vs candidate policy — settles both forks, replays
    /// recent real recall queries on each, and lets the judge blind-compare
    /// the answers (deterministically alternating A/B order to cancel
    /// position bias). The live space is only read: replays run on forks,
    /// so they can never pollute its conversations, usage ledger, or
    /// metrics (plan guardrail 4). Promotion stays human: read the report,
    /// then `update_space` with the candidate policy if it won.
    pub async fn run_shadow_eval(
        &self,
        space_id: &str,
        input: ShadowEvalInput,
    ) -> Result<ShadowReport, BoxError> {
        input.policy.validate()?;
        // Unpinned: shadow evaluation must not exempt a cold space from idle
        // eviction forever.
        let space = self.load_space(space_id, false).await?;
        // One shadow evaluation per space at a time: each run holds two full
        // in-memory copies of the space, so concurrent retries would stack
        // copies until the process OOMs (and race the `shadow_report` write).
        let Ok(_shadow_guard) = space.shadow_lock.try_lock() else {
            return Err("a shadow evaluation is already running for this space".into());
        };
        let now_ms = unix_ms();
        let sample = input
            .replay_sample
            .unwrap_or_else(|| space.memory_policy().shadow_replay_sample as usize)
            .clamp(1, 16);

        let queries = space.recent_recall_queries(sample).await?;
        if queries.is_empty() {
            return Err("no completed recall conversations to replay".into());
        }

        // Flush the live space so the forks see its latest persisted state,
        // then fork twice: baseline keeps the current policy, candidate gets
        // the proposed one. Settling both makes the comparison fair — same
        // metabolism pass, different knobs.
        space.db.flush_metadata(now_ms).await.ok();
        let baseline = self.fork_space_for_shadow(space_id, None).await?;
        let candidate = self
            .fork_space_for_shadow(space_id, Some(input.policy.clone()))
            .await?;
        // Interval 0 bypasses the weekly decay gate: the forks inherit the
        // live space's `decay_applied_at` stamps, and under the gate both
        // sides would settle identically whenever the live space decayed
        // recently — turning every decay-knob comparison into a tie. Forks
        // are throwaway copies, so over-decaying them has no consequence.
        let _ = baseline
            .settle_memory_metabolism_with(MaintenanceScope::Full, now_ms, 0)
            .await;
        let _ = candidate
            .settle_memory_metabolism_with(MaintenanceScope::Full, now_ms, 0)
            .await;

        let mut report = ShadowReport {
            compared_at: now_ms,
            candidate_policy: input.policy,
            ..Default::default()
        };
        for (index, query) in queries.iter().enumerate() {
            let recall_input = || {
                StringOr::Value(RecallInput {
                    query: query.clone(),
                    context: None,
                })
            };
            let baseline_out = baseline.query(SELF_USER_ID, recall_input()).await;
            let candidate_out = candidate.query(SELF_USER_ID, recall_input()).await;
            let (baseline_answer, candidate_answer) = match (baseline_out, candidate_out) {
                (Ok(baseline_out), Ok(candidate_out)) => {
                    report.usage.accumulate(&baseline_out.usage);
                    report.usage.accumulate(&candidate_out.usage);
                    (baseline_out.content, candidate_out.content)
                }
                _ => {
                    report.judge_errors += 1;
                    report.samples.push(ShadowSample {
                        query: crate::assess::truncate_chars(query, 200),
                        winner: "error".to_string(),
                        reason: "replay failed on one side".to_string(),
                    });
                    continue;
                }
            };
            report.replayed += 1;

            // Deterministic order alternation cancels position bias without
            // sacrificing reproducibility.
            let swap = index % 2 == 1;
            let (answer_a, answer_b) = if swap {
                (&candidate_answer, &baseline_answer)
            } else {
                (&baseline_answer, &candidate_answer)
            };
            let prompt = format!(
                "# User query\n{query}\n\n# Answer A\n{answer_a}\n\n# Answer B\n{answer_b}"
            );
            let verdict = crate::assess::AssessContext::judge_complete(
                space.as_ref(),
                anda_core::CompletionRequest {
                    instructions: SHADOW_JUDGE_INSTRUCTIONS.to_string(),
                    prompt,
                    effort: Some(anda_core::ModelEffort::Low),
                    ..Default::default()
                },
            )
            .await
            .and_then(|output| {
                report.usage.accumulate(&output.usage);
                crate::assess::parse_json_payload::<ShadowVerdict>(&output.content)
            });

            let (winner, reason) = match verdict {
                Ok(verdict) => {
                    let winner = match (verdict.winner.trim().to_lowercase().as_str(), swap) {
                        ("a", false) | ("b", true) => {
                            report.baseline_wins += 1;
                            "baseline"
                        }
                        ("b", false) | ("a", true) => {
                            report.candidate_wins += 1;
                            "candidate"
                        }
                        _ => {
                            report.ties += 1;
                            "tie"
                        }
                    };
                    (winner.to_string(), verdict.reason)
                }
                Err(err) => {
                    report.judge_errors += 1;
                    ("error".to_string(), err.to_string())
                }
            };
            report.samples.push(ShadowSample {
                query: crate::assess::truncate_chars(query, 200),
                winner,
                reason,
            });
        }

        // Forks live in memory and vanish on drop; closing is best-effort.
        let _ = baseline.db.close().await;
        let _ = candidate.db.close().await;

        space
            .db
            .set_extension_from("shadow_report".to_string(), report.clone());
        space.db.flush_metadata(unix_ms()).await.ok();
        Ok(report)
    }

    /// A sibling `AppState` over a different object store, sharing model and
    /// management configuration but with an empty space cache. Used by the
    /// eval harness to open forked space copies in isolation.
    pub fn fork_with_store(&self, object_store: Arc<dyn ObjectStore>) -> AppState {
        AppState {
            spaces: Arc::new(RwLock::new(BTreeMap::new())),
            object_store,
            db_config: self.db_config.clone(),
            http_client: self.http_client.clone(),
            models: self.models.clone(),
            judge_model: self.judge_model.clone(),
            ed25519_pubkeys: self.ed25519_pubkeys.clone(),
            management: self.management.clone(),
            app_name: self.app_name.clone(),
            app_version: self.app_version.clone(),
            sharding: self.sharding,
        }
    }

    // 平台管理员权限
    pub fn check_admin(
        &self,
        token: &str,
        audience: &str,
        scope: TokenScope,
        now_ms: u64,
    ) -> Result<CWToken, BoxError> {
        if self.ed25519_pubkeys.is_empty() {
            return Ok(CWToken {
                user: Principal::management_canister(),
                audience: audience.to_string(),
                scope,
            });
        }

        let token = self.check_auth(token, audience, scope, now_ms)?;
        if !self.management.is_manager(&token.user) {
            return Err("admin access required".into());
        }

        Ok(token)
    }

    // 用户权限
    pub fn check_auth_if(
        &self,
        token: &str,
        audience: &str,
        scope: TokenScope,
        now_ms: u64,
    ) -> Result<Option<CWToken>, BoxError> {
        if self.ed25519_pubkeys.is_empty() {
            return Ok(Some(CWToken {
                user: SELF_USER_ID,
                audience: audience.to_string(),
                scope,
            }));
        }

        if token.len() < 60 {
            return Ok(None);
        }

        let token = self.check_auth(token, audience, scope, now_ms)?;
        Ok(Some(token))
    }

    pub fn check_auth(
        &self,
        token: &str,
        audience: &str,
        scope: TokenScope,
        now_ms: u64,
    ) -> Result<CWToken, BoxError> {
        if self.ed25519_pubkeys.is_empty() {
            return Ok(CWToken {
                user: SELF_USER_ID,
                audience: audience.to_string(),
                scope,
            });
        }

        let data = ByteBufB64::from_str(token)?;
        let data = skip_prefix(&SIGN1_TAG, &data);
        let cs1 = cose_sign1_from(data, &[], &[], &self.ed25519_pubkeys)?;
        let claims = cwt_from(&cs1.payload.unwrap_or_default(), (now_ms / 1000) as i64)?;
        let token = CWToken::from_claims(claims)?;
        if token.audience != audience && token.audience != "*" {
            return Err("invalid audience".into());
        }

        if !token.scope.allows(scope) {
            return Err("insufficient scope".into());
        }
        Ok(token)
    }

    pub async fn admin_create_space(
        &self,
        creator: Principal,
        owner: Principal,
        id: String,
        tier: u32,
        now_ms: u64,
    ) -> Result<SpaceInfo, BoxError> {
        {
            let spaces = self.spaces.read().await;
            if spaces
                .get(&id)
                .is_some_and(|entry| entry.cell.initialized())
            {
                return Err(format!("space {} already exists", &id).into());
            }
        }

        let mut db_config = (*self.db_config).clone();
        db_config.name = id;
        Space::create(
            self.object_store.clone(),
            db_config,
            creator,
            owner,
            tier,
            now_ms,
        )
        .await
    }

    pub async fn load_space(&self, space_id: &str, pinned: bool) -> Result<Arc<Space>, BoxError> {
        self.load_space_with(space_id, pinned, true).await
    }

    /// `load_space` with control over background autostart. `autostart:
    /// false` opens the space without resuming its formation backlog or wiki
    /// digest — required for shadow forks, which are throwaway copies whose
    /// backlog must not burn LLM tokens or mutate the fork mid-replay.
    pub async fn load_space_with(
        &self,
        space_id: &str,
        pinned: bool,
        autostart: bool,
    ) -> Result<Arc<Space>, BoxError> {
        let entry = {
            let spaces = self.spaces.read().await;
            spaces.get(space_id).cloned()
        };

        let entry = match entry {
            Some(entry) => entry,
            None => {
                let mut spaces = self.spaces.write().await;
                spaces
                    .entry(space_id.to_string())
                    .or_insert_with(|| Arc::new(SpaceEntry::new()))
                    .clone()
            }
        };

        let space = entry
            .cell
            .get_or_try_init(|| async {
                let mut db_config = (*self.db_config).clone();
                db_config.name = space_id.to_string();
                let space = Space::connect(
                    self.object_store.clone(),
                    db_config,
                    self.management.clone(),
                    self.http_client.clone(),
                    self.models.clone(),
                    pinned,
                    autostart,
                )
                .await?;
                if let Some(judge) = self.judge_model.as_ref()
                    && let Err(err) = space.set_judge_model(judge.clone())
                {
                    log::warn!(
                        target: "brain",
                        space_id = space.id;
                        "installing the independent judge model failed: {err:?}"
                    );
                }
                Ok::<_, BoxError>(space)
            })
            .await
            .cloned()?;

        entry.touch();
        Ok(space)
    }

    /// Starts background maintenance tasks:
    /// - Flushes active space databases every 5 minutes.
    /// - Evicts spaces idle for over 9 minutes.
    pub async fn start_background_tasks(&self, cancel_token: CancellationToken) {
        let flush_interval = Duration::from_secs(5 * 60);
        let idle_timeout_ms: u64 = 9 * 60 * 1000;

        loop {
            tokio::select! {
                _ = cancel_token.cancelled() => {
                    // Close all spaces concurrently so shutdown stays fast even
                    // with many loaded spaces.
                    let entries: Vec<(String, Arc<SpaceEntry>)> = {
                        let spaces = self.spaces.read().await;
                        spaces.iter().map(|(id, entry)| (id.clone(), entry.clone())).collect()
                    };
                    let mut tasks = tokio::task::JoinSet::new();
                    for (id, entry) in entries {
                        if let Some(space) = entry.cell.get().cloned() {
                            tasks.spawn(async move {
                                if let Err(err) = space.db.close().await {
                                    log::error!(target: "brain", space_id = id; "close on shutdown failed: {err:?}");
                                }
                            });
                        }
                    }
                    while tasks.join_next().await.is_some() {}
                    return;
                }
                _ = tokio::time::sleep(flush_interval) => {}
            }

            self.flush_and_evict_once(unix_ms(), idle_timeout_ms).await;
        }
    }

    async fn flush_and_evict_once(&self, now: u64, idle_timeout_ms: u64) {
        // Collect entries snapshot under read lock
        let entries: Vec<(String, Arc<SpaceEntry>)> = {
            let spaces = self.spaces.read().await;
            spaces.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
        };

        for (id, entry) in &entries {
            if self
                .try_evict_idle_space(id, entry, now, idle_timeout_ms)
                .await
            {
                log::warn!(target: "brain", space_id = id; "space evicted due to inactivity");
                continue;
            }

            // Periodic flush for active spaces
            if let Some(space) = entry.cell.get()
                && let Err(err) = space.flush().await
            {
                log::error!(target: "brain", space_id = id; "periodic flush failed: {err:?}");
            }
        }
    }

    /// Evicts an idle space entry, closing its database *before* removing it
    /// from the map. The close happens while holding the map write lock so a
    /// concurrent `load_space` cannot connect a second AndaDB instance to the
    /// same storage while the old one is still flushing. Idle spaces were
    /// already flushed by the periodic pass, so this close is cheap.
    async fn try_evict_idle_space(
        &self,
        id: &str,
        entry: &Arc<SpaceEntry>,
        now_ms: u64,
        idle_timeout_ms: u64,
    ) -> bool {
        let mut spaces = self.spaces.write().await;
        let Some(current_entry) = spaces.get(id) else {
            return false;
        };
        if !Arc::ptr_eq(current_entry, entry) {
            return false;
        }

        let is_idle = now_ms.saturating_sub(entry.last_access_ms()) > idle_timeout_ms;
        if !is_idle {
            return false;
        }

        match entry.cell.get() {
            Some(space) => {
                if space.pinned || space.is_processing() {
                    return false;
                }
                // Map + background snapshot are the only expected SpaceEntry refs here;
                // OnceCell is the only expected Space ref. Anything more means a request
                // has recently loaded or is still using this space, so eviction waits.
                if Arc::strong_count(entry) > 2 || Arc::strong_count(space) > 1 {
                    return false;
                }
                if let Err(err) = space.close().await {
                    log::error!(target: "brain", space_id = id; "close before eviction failed: {err:?}");
                }
            }
            None => {
                // Initialization never succeeded (e.g. probes for unknown space
                // IDs). Drop the unused placeholder so such probes cannot grow
                // the map unboundedly.
                if Arc::strong_count(entry) > 2 {
                    return false;
                }
            }
        }

        spaces.remove(id).is_some()
    }
}

pub struct Space {
    id: String,
    engine: Engine,
    http_client: reqwest::Client,
    models: Arc<Models>,
    maintenance: Arc<MaintenanceAgent>,
    pinned: bool,
    /// Memory usage ledger (plan M1): off-graph recall/correction counters.
    ledger: Arc<UsageLedger>,
    /// Negative-knowledge cache (plan M5): probe queries the graph had
    /// nothing for; cleared whenever formation completes.
    miss_cache: Arc<MissCache>,
    /// Serializes memory-metabolism settlements (plan M2); the settlement
    /// itself is idempotent, the lock just avoids wasted duplicate passes.
    settlement_lock: tokio::sync::Mutex<()>,
    /// At most one dream self-test (plan M7) runs at a time; overlapping
    /// kicks are skipped, not queued.
    self_test_lock: tokio::sync::Mutex<()>,
    /// At most one shadow evaluation (plan M11) per space: each run holds
    /// two full in-memory copies, so stacking runs is an OOM vector.
    shadow_lock: tokio::sync::Mutex<()>,
    /// Independent judge model for eval runs (plan M9); unset means judge
    /// completions share the space's default model (documented caveat).
    judge_model: std::sync::RwLock<Option<Arc<Model>>>,
    pub formation: Arc<FormationAgent>,
    pub recall: Arc<RecallAgent>,
    pub db: Arc<AndaDB>,
    pub memory: Arc<MemoryManagement>,
    pub wiki: Arc<WikiService>,
    pub wiki_digest: Arc<WikiDigest>,
}

impl Space {
    pub fn is_processing(&self) -> bool {
        self.formation.is_processing() || self.maintenance.is_processing()
    }

    pub fn get_tier(&self) -> SpaceTier {
        self.db.get_extension_as("tier").unwrap_or_default()
    }

    pub async fn admin_update_tier(&self, tier: u32, now_ms: u64) -> Result<SpaceTier, BoxError> {
        let tier = SpaceTier {
            tier,
            updated_at: now_ms,
        };
        self.db
            .save_extension_from("tier".to_string(), &tier.to_ref())
            .await?;
        Ok(tier)
    }

    pub async fn add_space_token(
        &self,
        token: String,
        input: AddSpaceTokenInput,
        now_ms: u64,
    ) -> Result<SpaceToken, BoxError> {
        let count = self
            .db
            .extensions_with(|kv| kv.keys().filter(|k| k.starts_with("ST")).count());
        if count >= 100 {
            return Err("space token limit reached".into());
        }

        let sp = SpaceToken {
            token: token.clone(),
            scope: input.scope,
            name: input.name,
            expires_at: input.expires_at,
            labels: input.labels,
            created_at: now_ms,
            updated_at: now_ms,
            ..Default::default()
        };

        self.db.save_extension_from(token, &sp.to_ref()).await?;
        Ok(sp)
    }

    pub fn verify_space_token(
        &self,
        token: String,
        scope: TokenScope,
        now_ms: u64,
    ) -> Result<SpaceToken, BoxError> {
        // Space tokens always carry the "ST" prefix. Rejecting other keys here
        // keeps non-token extensions (e.g. "byok", "tier") out of the
        // credential lookup below.
        if !token.starts_with("ST") {
            return Err("invalid space token".into());
        }
        let token = self
            .db
            .set_extension_from_with::<_, SpaceToken>(token, |v| {
                if let Some(mut st) = v
                    && st.expires_at.map(|exp| exp > now_ms).unwrap_or(true)
                    && st.scope.allows(scope)
                {
                    st.usage = st.usage.saturating_add(1);
                    st.updated_at = now_ms;
                    return Some(st);
                }
                None
            });

        token.ok_or_else(|| "invalid space token".into())
    }

    pub async fn revoke_space_token(&self, token: &str) -> Result<bool, BoxError> {
        // Same guard as verify_space_token: the token is caller-supplied, so
        // restricting it to the "ST" prefix keeps non-token extensions
        // (e.g. "byok", "tier", "owner") safe from deletion through this API.
        if !token.starts_with("ST") {
            return Err("invalid space token".into());
        }
        let rt = self.db.remove_extension(token).await?;
        Ok(rt.is_some())
    }

    pub fn list_space_tokens(&self) -> Result<Vec<SpaceToken>, BoxError> {
        let tokens: Vec<SpaceToken> = self.db.extensions_with(|kvs| {
            kvs.iter()
                .filter_map(|(k, v)| {
                    if k.starts_with("ST")
                        && let Ok(mut st) = v.clone().deserialized::<SpaceToken>()
                    {
                        st.token = k.clone();
                        Some(st)
                    } else {
                        None
                    }
                })
                .collect()
        });

        Ok(tokens)
    }

    pub async fn update(&self, input: UpdateSpaceInput, now_ms: u64) -> Result<(), BoxError> {
        // Validate up front: a bad policy must reject the request before any
        // other field of this update mutates in-memory extension state.
        if let Some(policy) = &input.memory_policy {
            policy.validate()?;
        }

        let mut changed = false;
        if let Some(name) = input.name {
            changed = true;
            self.db.set_extension_from("name".to_string(), name);
        }
        if let Some(description) = input.description {
            changed = true;
            self.db
                .set_extension_from("description".to_string(), description);
        }
        if let Some(public) = input.public {
            changed = true;
            self.db.set_extension_from("public".to_string(), public);
        }
        if let Some(wiki_digest) = input.wiki_digest {
            changed = true;
            self.db
                .set_extension_from("wiki_digest".to_string(), wiki_digest);
        }
        if let Some(audit_reads) = input.wiki_audit_reads {
            changed = true;
            self.db
                .set_extension_from("wiki_audit_reads".to_string(), audit_reads);
            self.wiki.set_audit_reads(audit_reads);
        }
        if let Some(defaults) = input.wiki_acl_defaults {
            changed = true;
            self.wiki.set_acl_defaults(defaults).await?;
        }
        if let Some(policy) = input.memory_policy {
            changed = true;
            self.db
                .set_extension_from(MemoryPolicy::EXTENSION_KEY.to_string(), policy);
        }
        if changed {
            self.db.flush_metadata(now_ms).await?;
        }
        Ok(())
    }

    /// The space's memory policy; absent means the process-wide eval
    /// override (optimizer runs, plan M10) or [`MemoryPolicy::default`],
    /// which reproduces the compiled-in behavior (plan module M-P).
    pub fn memory_policy(&self) -> MemoryPolicy {
        self.db
            .get_extension_as(MemoryPolicy::EXTENSION_KEY)
            .or_else(MemoryPolicy::eval_override)
            .unwrap_or_default()
    }

    pub fn get_byok(&self) -> Option<ModelConfig> {
        self.db.get_extension_as("byok")
    }

    pub async fn update_byok(&self, model_config: ModelConfig) -> Result<(), BoxError> {
        let engine_config: EngineModelConfig = model_config.clone().into();
        let model = engine_config.model(self.http_client.clone())?;
        self.db
            .save_extension_from("byok".to_string(), &model_config.to_ref())
            .await?;
        self.models.set_model(model);
        Ok(())
    }

    pub fn is_public(&self) -> bool {
        self.db.get_extension_as("public").unwrap_or(false)
    }

    pub fn get_info(&self) -> SpaceInfo {
        let mut info = SpaceInfo {
            id: self.id.clone(),
            db_stats: self.db.stats(),
            concepts: self.memory.nexus.concepts.len(),
            propositions: self.memory.nexus.propositions.len(),
            conversations: self.memory.conversations.len(),
            formation_processed_id: self.formation.get_processed().unwrap_or_default(),
            maintenance_processed_id: self.maintenance.get_processed().unwrap_or_default(),
            maintenance_at: self.maintenance.get_processed_at(),
            wiki_docs: self.wiki.docs_count(),
            wiki_chunks: self.wiki.chunks_count(),
            wiki_versions: self.wiki.versions_count(),
            wiki_queries: self.wiki.queries_count(),
            wiki_digested: self.wiki_digest.cursor(),
            wiki_stale_docs: self.wiki.stale_report_cached().stale_docs,
            ..Default::default()
        };

        self.db.extensions_with(|kv| {
            info.name = kv
                .get("name")
                .and_then(|v| String::try_from(v.clone()).ok());
            info.description = kv
                .get("description")
                .and_then(|v| String::try_from(v.clone()).ok());
            info.owner = kv
                .get("owner")
                .and_then(|v| String::try_from(v.clone()).ok())
                .unwrap_or_default();
            info.public = kv
                .get("public")
                .and_then(|v| bool::try_from(v.clone()).ok())
                .unwrap_or(false);
            info.tier = kv
                .get("tier")
                .and_then(|v| v.clone().deserialized::<SpaceTier>().ok())
                .unwrap_or_default();
            info.formation_usage = kv
                .get("formation_usage")
                .and_then(|v| v.clone().deserialized::<Usage>().ok())
                .unwrap_or_default();
            info.recall_usage = kv
                .get("recall_usage")
                .and_then(|v| v.clone().deserialized::<Usage>().ok())
                .unwrap_or_default();
            info.maintenance_usage = kv
                .get("maintenance_usage")
                .and_then(|v| v.clone().deserialized::<Usage>().ok())
                .unwrap_or_default();
        });
        info
    }

    pub fn formation_status(&self) -> FormationStatus {
        FormationStatus {
            id: self.id.clone(),
            concepts: self.memory.nexus.concepts.len(),
            propositions: self.memory.nexus.propositions.len(),
            conversations: self.memory.conversations.len(),
            formation_processing: self.formation.is_processing(),
            maintenance_processing: self.maintenance.is_processing(),
            formation_processed_id: self.formation.get_processed().unwrap_or_default(),
            maintenance_processed_id: self.maintenance.get_processed().unwrap_or_default(),
            maintenance_at: self.maintenance.get_processed_at(),
        }
    }

    pub async fn ingest(
        &self,
        user: Principal,
        input: StringOr<FormationInput>,
    ) -> Result<AgentOutput, BoxError> {
        let nodes = self
            .memory
            .nexus
            .concepts
            .len()
            .max(self.memory.conversations.len()) as u64;
        let tier = self.get_tier();
        if tier.allow_nodes() < nodes {
            return Err(format!(
                "node limit exceeded: {} nodes vs tier limit {}",
                nodes,
                tier.allow_nodes()
            )
            .into());
        }

        self.engine
            .agent_run(
                user,
                AgentInput {
                    name: FormationAgent::NAME.to_string(),
                    prompt: input.to_string(),
                    resources: vec![],
                    ..Default::default()
                },
            )
            .await
    }

    async fn run_recall(
        &self,
        user: Principal,
        input: StringOr<RecallInput>,
    ) -> Result<AgentOutput, BoxError> {
        self.engine
            .agent_run(
                user,
                AgentInput {
                    name: RecallAgent::NAME.to_string(),
                    prompt: input.to_string(),
                    resources: vec![],
                    ..Default::default()
                },
            )
            .await
    }

    pub async fn query(
        &self,
        user: Principal,
        input: StringOr<RecallInput>,
    ) -> Result<AgentOutput, BoxError> {
        let mut output = self.run_recall(user, input).await?;
        // The self-report footer (plan M4) is machine metadata; plain-text
        // callers must never see it. `query_structured` surfaces it instead.
        let (answer, meta) = assess::split_recall_meta(&output.content);
        output.content = answer;
        // The final assistant message inside `chat_history` carries the raw
        // model output — strip the footer there too, or clients reading the
        // history see the markup that `content` hides. Same for the failure
        // diagnostic, which embeds the rendered conversation.
        strip_recall_meta_from_history(&mut output.chat_history);
        if let Some(reason) = output.failed_reason.take() {
            output.failed_reason = Some(assess::split_recall_meta(&reason).0);
        }
        // Plain recalls feed the calibration counters (plan M12) exactly
        // like structured ones — else most production traffic is a blind
        // spot — but only successful runs: a failed recall's self-report is
        // not a calibration sample.
        if output.failed_reason.is_none()
            && let Some(uncertainty) = meta.and_then(|meta| meta.uncertainty)
        {
            self.bump_metrics(|metrics| {
                metrics.uncertainty_reports += 1;
                metrics.uncertainty_sum += uncertainty;
            });
        }
        Ok(output)
    }

    /// Recall with machine-readable provenance (plan M4): the answer plus
    /// trace-derived memory citations and the model's self-reported
    /// `found`/`uncertainty`, so a business agent can decide whether to
    /// assert, hedge, or ask.
    pub async fn query_structured(
        &self,
        user: Principal,
        input: StringOr<RecallInput>,
    ) -> Result<RecallOutput, BoxError> {
        let output = self.run_recall(user, input).await?;
        let (answer, meta) = assess::split_recall_meta(&output.content);
        let memories = match output.conversation {
            Some(id) => match self.recall.conversations.get_conversation(id).await {
                Ok(conversation) => {
                    let messages: Vec<Message> = conversation
                        .messages
                        .into_iter()
                        .filter_map(|message| serde_json::from_value::<Message>(message).ok())
                        .collect();
                    assess::extract_memory_citations(&assess::RecallTrace::from_messages(&messages))
                }
                Err(_) => Vec::new(),
            },
            None => Vec::new(),
        };
        let meta = meta.unwrap_or_default();
        if output.failed_reason.is_none()
            && let Some(uncertainty) = meta.uncertainty
        {
            // Calibration raw material (plan M12): predicted uncertainty is
            // later audited against actual correction rates. Failed recalls
            // are excluded — their self-report is not a calibration sample.
            self.bump_metrics(|metrics| {
                metrics.uncertainty_reports += 1;
                metrics.uncertainty_sum += uncertainty;
            });
        }
        Ok(RecallOutput {
            answer,
            // The trace is the ground truth when the model does not report.
            found: meta.found.unwrap_or(!memories.is_empty()),
            uncertainty: meta.uncertainty,
            memories,
            conversation: output.conversation,
            usage: output.usage,
            failed_reason: output
                .failed_reason
                .map(|reason| assess::split_recall_meta(&reason).0),
        })
    }

    /// The user queries of the most recent completed recall conversations,
    /// newest first — the shadow evaluation's replay corpus (plan M11).
    pub(crate) async fn recent_recall_queries(
        &self,
        limit: usize,
    ) -> Result<Vec<String>, BoxError> {
        let (conversations, _) = self
            .recall
            .conversations
            .list_conversations_by_user(&SELF_USER_ID, None, Some(limit.saturating_mul(2)))
            .await?;
        let mut queries = Vec::new();
        for conversation in conversations {
            if conversation.status != ConversationStatus::Completed {
                continue;
            }
            let Some(first) = conversation.messages.first() else {
                continue;
            };
            let text: String = first
                .get("content")
                .and_then(serde_json::Value::as_array)
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(|part| part.get("text").and_then(serde_json::Value::as_str))
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            if text.trim().is_empty() {
                continue;
            }
            // The stored prompt is either a serialized `RecallInput` or the
            // raw query string.
            let query = serde_json::from_str::<RecallInput>(&text)
                .map(|input| input.query)
                .unwrap_or(text);
            queries.push(query);
            if queries.len() >= limit {
                break;
            }
        }
        Ok(queries)
    }

    /// Records which graph entities a completed recall surfaced
    /// (plan M1). Called from the conversation-end hook, off the hot path.
    pub(crate) async fn record_recall_usage(
        &self,
        messages: &[serde_json::Value],
    ) -> Result<u64, BoxError> {
        let messages: Vec<Message> = messages
            .iter()
            .filter_map(|message| serde_json::from_value::<Message>(message.clone()).ok())
            .collect();
        let entities = assess::RecallTrace::from_messages(&messages).entity_ids();
        let touched = if entities.is_empty() {
            0
        } else {
            self.ledger.record_recall(&entities, unix_ms()).await?
        };
        self.bump_metrics(|metrics| {
            metrics.recalls_completed += 1;
            metrics.entities_recalled += touched;
        });
        Ok(touched)
    }

    pub async fn maintenance(
        &self,
        user: Principal,
        mut input: MaintenanceInput,
    ) -> Result<AgentOutput, BoxError> {
        input.formation_id = self.formation.get_processed().unwrap_or_default();
        // Callers that pass explicit parameters keep them; everyone else runs
        // under the space's memory policy. Default policy values equal the
        // defaults documented in BrainMaintenance.md, so an unset policy is
        // not a behavior change (plan module M-P).
        if input.parameters.is_none() {
            input.parameters = Some(self.memory_policy().maintenance_parameters());
        }
        // Deterministic metabolism settles before the LLM cycle starts, so
        // the agent assesses an already-settled graph. Settlement failures
        // degrade the cycle, never abort it.
        match self.settle_memory_metabolism(input.scope, unix_ms()).await {
            Ok(report) => {
                log::info!(
                    target: "brain",
                    space_id = self.id,
                    report:serde = report;
                    "memory metabolism settled"
                );
            }
            Err(err) => {
                log::warn!(
                    target: "brain",
                    space_id = self.id;
                    "memory metabolism settlement failed: {err:?}"
                );
            }
        }
        let rt = self
            .engine
            .agent_run(
                user,
                AgentInput {
                    name: MaintenanceAgent::NAME.to_string(),
                    prompt: StringOr::Value(&input).to_string(),
                    resources: vec![],
                    ..Default::default()
                },
            )
            .await?;
        Ok(rt)
    }

    /// The last memory-metabolism settlement report, when one has run.
    pub fn memory_settlement(&self) -> Option<MemorySettlementReport> {
        self.db.get_extension_as("memory_settlement")
    }

    /// Bumps the incrementally-updated observability counters (plan M12).
    /// Writers pay one in-memory extension update; readers never pay a
    /// heavy query.
    fn bump_metrics(&self, update: impl FnOnce(&mut MemoryMetrics)) {
        let now_ms = unix_ms();
        let _ = self
            .db
            .set_extension_from_with("memory_metrics".to_string(), |value| {
                let mut metrics: MemoryMetrics = value.unwrap_or_default();
                update(&mut metrics);
                metrics.updated_at = now_ms;
                Some(metrics)
            });
    }

    /// Memory observability snapshot (plan M12): incrementally-maintained
    /// counters, derived rates, graph counts, and the latest module reports.
    pub async fn memory_status(&self) -> MemoryStatus {
        fn ratio(numerator: u64, denominator: u64) -> Option<f64> {
            (denominator > 0).then(|| numerator as f64 / denominator as f64)
        }

        let metrics: MemoryMetrics = self
            .db
            .get_extension_as("memory_metrics")
            .unwrap_or_default();
        // Graph counters come from the settlement-time census (M12: readers
        // never pay heavy queries — the orphan count is a near-full scan,
        // and this endpoint is reachable anonymously on public spaces). A
        // space that has never settled pays the census once and caches it.
        let graph = match self
            .db
            .get_extension_as::<MemoryGraphCounters>("memory_graph_counters")
        {
            Some(graph) => graph,
            None => {
                let graph = self.census_graph_counters(unix_ms()).await;
                self.db
                    .set_extension_from("memory_graph_counters".to_string(), graph.clone());
                graph
            }
        };
        let maintenance_usage: Usage = self
            .db
            .get_extension_as("maintenance_usage")
            .unwrap_or_default();
        let maintenance_tokens = maintenance_usage
            .input_tokens
            .saturating_add(maintenance_usage.output_tokens);

        MemoryStatus {
            groundability: ratio(metrics.self_test_grounded, metrics.self_test_tested),
            probe_hit_rate: ratio(
                metrics.probe_hits,
                metrics.probe_hits + metrics.probe_misses,
            ),
            correction_rate: ratio(metrics.corrections, metrics.recalls_completed),
            avg_uncertainty: (metrics.uncertainty_reports > 0)
                .then(|| metrics.uncertainty_sum / metrics.uncertainty_reports as f64),
            maintenance_tokens_per_recall: ratio(maintenance_tokens, metrics.recalls_completed),
            metrics,
            graph,
            last_settlement: self.memory_settlement(),
            last_self_test: self.db.get_extension_as("memory_self_test"),
            last_shadow: self.db.get_extension_as("shadow_report"),
        }
    }

    /// Counts the graph-health numbers `memory_status` reports. Heavy (the
    /// orphan query is a near-full scan), so it runs at settlement time and
    /// the result is cached in the `memory_graph_counters` extension.
    async fn census_graph_counters(&self, now_ms: u64) -> MemoryGraphCounters {
        let formation = self.formation_status();
        MemoryGraphCounters {
            concepts: formation.concepts as u64,
            propositions: formation.propositions as u64,
            unsorted: assess::kip_count(self, assess::UNSORTED_COUNT_KQL).await,
            orphans: assess::kip_count(self, assess::ORPHAN_COUNT_KQL).await,
            predicate_types: assess::kip_count(self, assess::PREDICATE_TYPES_COUNT_KQL).await,
            as_of: Some(now_ms),
        }
    }

    /// Per-predicate link census (plan M8), run by full-scope settlements.
    /// The counts feed the schema-sprawl metric and give the Maintenance
    /// prompt's merge guidance real numbers to look at.
    async fn audit_schema(&self, now_ms: u64) -> Result<(), BoxError> {
        let response = self
            .execute_kip_readonly(anda_kip::Request {
                command: "FIND(?t.name) WHERE { ?t {type: \"$PropositionType\"} } LIMIT 100"
                    .to_string(),
                readonly: true,
                ..Default::default()
            })
            .await?;
        let mut names = BTreeSet::new();
        if let anda_kip::Response::Ok { result, .. } = &response {
            collect_string_leaves(result, &mut names);
        }

        let mut predicates = BTreeMap::new();
        for name in names.into_iter().take(50) {
            // A failed count (typically the engine's full-scan cap on the
            // busiest predicates) must be *absent*, not zero: reporting the
            // most-used predicate as having zero links would point the
            // Phase-6 merge guidance at exactly the wrong target.
            match assess::kip_count(
                self,
                &format!(
                    "FIND(COUNT(?link)) WHERE {{ ?link (?s, {}, ?o) }}",
                    kip_string_literal(&name)
                ),
            )
            .await
            {
                Some(count) => {
                    predicates.insert(name, count);
                }
                None => {
                    log::warn!(
                        target: "brain",
                        space_id = self.id;
                        "schema census count failed for predicate `{name}`; omitted from audit"
                    );
                }
            }
        }
        self.db.set_extension_from(
            "schema_audit".to_string(),
            SchemaAudit {
                audited_at: now_ms,
                predicates,
            },
        );

        // Correction *rates* need a denominator (plan M3): for every source
        // already charged with corrections, census its total link count.
        // Sources storing `metadata.source` as an array are skipped by the
        // equality filter — the rate is then an upper bound, which is the
        // safe direction for encode-time discounting.
        let sources: BTreeMap<String, SourceReliability> = self
            .db
            .get_extension_as("source_reliability")
            .unwrap_or_default();
        if !sources.is_empty() {
            let mut totals: BTreeMap<String, Option<u64>> = BTreeMap::new();
            for source in sources.keys().take(20) {
                let total = assess::kip_count(
                    self,
                    &format!(
                        "FIND(COUNT(?link)) WHERE {{ ?link (?s, ?p, ?o) FILTER(?link.metadata.source == {}) }}",
                        kip_string_literal(source)
                    ),
                )
                .await;
                totals.insert(source.clone(), total);
            }
            let _ = self
                .db
                .set_extension_from_with("source_reliability".to_string(), |value| {
                    let mut map: BTreeMap<String, SourceReliability> = value.unwrap_or_default();
                    for (source, total) in totals {
                        if let (Some(entry), Some(total)) = (map.get_mut(&source), total) {
                            entry.total_links = Some(total);
                        }
                    }
                    Some(map)
                });
        }
        Ok(())
    }

    /// The last per-predicate schema census, when one has run.
    pub fn schema_audit(&self) -> Option<SchemaAudit> {
        self.db.get_extension_as("schema_audit")
    }

    /// Ledger rows corrected after `since_ms` — the scenario-mining signal
    /// (plan M9).
    pub async fn corrected_entities(
        &self,
        since_ms: u64,
        limit: usize,
    ) -> Result<Vec<String>, BoxError> {
        Ok(self
            .ledger
            .corrected_since(since_ms, limit)
            .await?
            .into_iter()
            .map(|row| row.entity)
            .collect())
    }

    /// Installs an independent judge model for eval runs (plan M9): judge
    /// completions stop sharing the evaluated system's model and blind spots.
    pub fn set_judge_model(&self, config: ModelConfig) -> Result<(), BoxError> {
        if config.disabled {
            return Err("judge model is disabled".into());
        }
        let engine_config: EngineModelConfig = config.into();
        let model = engine_config.model(self.http_client.clone())?;
        *self.judge_model.write().expect("judge model lock poisoned") = Some(Arc::new(model));
        Ok(())
    }

    /// The installed judge model, when one exists.
    pub(crate) fn judge_model(&self) -> Option<Arc<Model>> {
        self.judge_model
            .read()
            .expect("judge model lock poisoned")
            .clone()
    }

    /// Test-only judge model injection without a provider config.
    #[cfg(test)]
    pub(crate) fn set_judge_model_for_test(&self, model: Model) {
        *self.judge_model.write().expect("judge model lock poisoned") = Some(Arc::new(model));
    }

    /// Executes a settlement-built write KIP request. Only deterministic,
    /// code-generated commands go through here — never model output.
    async fn execute_kip_settlement(
        &self,
        request: anda_kip::Request,
    ) -> Result<anda_kip::Response, BoxError> {
        match timeout(
            SETTLEMENT_KIP_TIMEOUT,
            request.execute(self.memory.nexus.as_ref()),
        )
        .await
        {
            Ok((_, res)) => Ok(res),
            Err(_) => Err(format!(
                "settlement KIP execution timed out after {} seconds",
                SETTLEMENT_KIP_TIMEOUT.as_secs()
            )
            .into()),
        }
    }

    /// Deterministic memory metabolism (plan M2/M3), run before each
    /// maintenance cycle. Three idempotent passes:
    ///
    /// 1. **Reinforcement flush** (every scope): usage-ledger counters for
    ///    recalled propositions are written onto graph metadata
    ///    (`last_recalled_at`, `recall_count`), where the decay filter and
    ///    the Recall/Maintenance prompts can see them.
    /// 2. **Bulk confidence decay** (full scope only, self rate-limited via
    ///    `decay_applied_at`): the Phase-7 decay the Maintenance prompt used
    ///    to run by hand, now usage-modulated — recently recalled, pinned,
    ///    superseded, and system-truth links are exempt.
    /// 3. **Correction discovery** (every scope): newly superseded links are
    ///    recorded in the ledger and aggregated per `metadata.source` into
    ///    the `source_reliability` extension.
    pub async fn settle_memory_metabolism(
        &self,
        scope: MaintenanceScope,
        now_ms: u64,
    ) -> Result<MemorySettlementReport, BoxError> {
        self.settle_memory_metabolism_with(scope, now_ms, DECAY_MIN_INTERVAL_MS)
            .await
    }

    /// [`Self::settle_memory_metabolism`] with control over the decay rate
    /// limit. Shadow forks pass `0`: they inherit the live space's
    /// `decay_applied_at` stamps, and under the weekly gate both forks would
    /// settle identically whenever the live space decayed within the window
    /// — every comparison of decay knobs would be a systematic tie.
    pub(crate) async fn settle_memory_metabolism_with(
        &self,
        scope: MaintenanceScope,
        now_ms: u64,
        decay_min_interval_ms: u64,
    ) -> Result<MemorySettlementReport, BoxError> {
        let _guard = self.settlement_lock.lock().await;
        let policy = self.memory_policy();
        let mut report = MemorySettlementReport {
            settled_at: now_ms,
            ..Default::default()
        };

        // 1) Reinforcement flush: drain dirty ledger rows in batches. The
        // dirty flag — not a time-window watermark — is the cursor, so a row
        // whose KIP write fails (or that arrives past a batch limit) stays
        // dirty and is retried by every later settlement; usage counts can
        // no longer be lost. Rows already attempted this pass are skipped so
        // persistent failures cannot spin the loop.
        let mut attempted: BTreeSet<u64> = BTreeSet::new();
        for _ in 0..SETTLEMENT_MAX_BATCHES {
            let rows = self
                .ledger
                .unflushed_recalls(SETTLEMENT_BATCH_LIMIT)
                .await?;
            let mut progressed = false;
            for row in &rows {
                if !attempted.insert(row._id) {
                    continue;
                }
                progressed = true;
                if !crate::assess::is_proposition_entity_id(&row.entity) {
                    // Concept usage stays ledger-only: decay targets links, and
                    // marking the row flushed keeps it out of future scans.
                    self.ledger
                        .mark_flushed(row._id, row.recall_count, now_ms)
                        .await?;
                    continue;
                }
                let command = reinforcement_update_command(
                    &row.entity,
                    row.last_recalled_at,
                    row.recall_count,
                );
                match self
                    .execute_kip_settlement(anda_kip::Request {
                        command,
                        ..Default::default()
                    })
                    .await
                {
                    Ok(anda_kip::Response::Ok { .. }) => {
                        report.reinforced += 1;
                        self.ledger
                            .mark_flushed(row._id, row.recall_count, now_ms)
                            .await?;
                    }
                    // Stays dirty: retried on the next settlement.
                    Ok(anda_kip::Response::Err { .. }) | Err(_) => {
                        report.flush_retries += 1;
                    }
                }
            }
            if !progressed || rows.len() < SETTLEMENT_BATCH_LIMIT {
                break;
            }
        }

        // 2) Bulk decay, full scope only (mirrors the old Phase-7 cadence).
        // A failing decay pass degrades — corrections and the schema census
        // below still run. The loudest expected cause is the engine's
        // full-scan solution cap (KIP_4002 at 65,536 propositions): decay
        // stops working on graphs past that size, which must page an
        // operator, not vanish into a debug log.
        if scope == MaintenanceScope::Full {
            report.decay_ran = true;
            let command = decay_update_command(&policy, now_ms, decay_min_interval_ms);
            for _ in 0..SETTLEMENT_MAX_BATCHES {
                let response = self
                    .execute_kip_settlement(anda_kip::Request {
                        command: command.clone(),
                        ..Default::default()
                    })
                    .await?;
                let updated = match &response {
                    anda_kip::Response::Ok { result, .. } => result
                        .get("updated")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0),
                    anda_kip::Response::Err { .. } => {
                        log::error!(
                            target: "brain",
                            space_id = self.id;
                            "bulk decay pass failed — confidence decay is NOT running \
                             (graph past the full-scan engine cap?): {response:?}"
                        );
                        report.decay_error = Some(format!("{response:?}"));
                        break;
                    }
                };
                report.decayed += updated;
                if updated < SETTLEMENT_BATCH_LIMIT as u64 {
                    break;
                }
            }
        }

        // 3) Correction discovery: superseded links not yet marked settled.
        // The `correction_settled` graph marker (not a windowed scan) is the
        // cursor: processed links leave the result set, so a superseded
        // backlog larger than one batch drains across cycles instead of new
        // corrections starving forever behind the first LIMIT-full.
        let scan = self
            .execute_kip_readonly(anda_kip::Request {
                command: format!(
                    "FIND(?link) WHERE {{ ?link (?s, ?p, ?o) FILTER(?link.metadata.superseded == true) FILTER(IS_NULL(?link.metadata.correction_settled)) }} LIMIT {SETTLEMENT_BATCH_LIMIT}"
                ),
                readonly: true,
                ..Default::default()
            })
            .await;
        match scan {
            Ok(anda_kip::Response::Ok { result, .. }) => {
                let mut hits: Vec<(String, Vec<String>)> = Vec::new();
                crate::assess::collect_entity_objects(&result, &mut |id, object| {
                    if !crate::assess::is_proposition_entity_id(id) {
                        return;
                    }
                    let sources = match object.get("metadata").and_then(|meta| meta.get("source")) {
                        Some(serde_json::Value::String(source)) => vec![source.clone()],
                        Some(serde_json::Value::Array(items)) => items
                            .iter()
                            .filter_map(serde_json::Value::as_str)
                            .map(str::to_string)
                            .collect(),
                        _ => Vec::new(),
                    };
                    hits.push((id.to_string(), sources));
                });
                for (entity, sources) in hits {
                    if self.ledger.record_correction(&entity, now_ms).await? {
                        report.new_corrections += 1;
                        if !sources.is_empty() {
                            let _ = self.db.set_extension_from_with(
                                "source_reliability".to_string(),
                                |value| {
                                    let mut map: BTreeMap<String, SourceReliability> =
                                        value.unwrap_or_default();
                                    for source in &sources {
                                        let entry = map.entry(source.clone()).or_default();
                                        entry.corrections += 1;
                                        entry.last_corrected_at = now_ms;
                                    }
                                    Some(map)
                                },
                            );
                        }
                    }
                    // Mark the link settled whether or not it was new to the
                    // ledger, so it stops occupying the scan window. A failed
                    // mark is retried next cycle; `record_correction` dedupes,
                    // so re-processing never double-counts.
                    let mark = metadata_flag_command(&entity, "correction_settled", "true");
                    if !matches!(
                        self.execute_kip_settlement(anda_kip::Request {
                            command: mark,
                            ..Default::default()
                        })
                        .await,
                        Ok(anda_kip::Response::Ok { .. })
                    ) {
                        log::warn!(
                            target: "brain",
                            space_id = self.id;
                            "marking correction settled failed for {entity}; will re-scan"
                        );
                    }
                }
            }
            Ok(response) => {
                log::error!(
                    target: "brain",
                    space_id = self.id;
                    "correction discovery scan failed — new corrections are NOT being \
                     recorded (graph past the full-scan engine cap?): {response:?}"
                );
                report.correction_scan_error = Some(format!("{response:?}"));
            }
            Err(err) => {
                log::error!(
                    target: "brain",
                    space_id = self.id;
                    "correction discovery scan failed — new corrections are NOT being \
                     recorded: {err:?}"
                );
                report.correction_scan_error = Some(err.to_string());
            }
        }

        // Full cycles also refresh the per-predicate schema census (plan M8).
        if scope == MaintenanceScope::Full
            && let Err(err) = self.audit_schema(now_ms).await
        {
            log::warn!(
                target: "brain",
                space_id = self.id;
                "schema audit failed: {err:?}"
            );
        }

        self.bump_metrics(|metrics| {
            metrics.corrections += report.new_corrections;
            metrics.decayed += report.decayed;
            metrics.reinforced += report.reinforced;
        });
        // Refresh the cached graph counters `memory_status` serves (M12:
        // readers never pay heavy queries).
        let counters = self.census_graph_counters(now_ms).await;
        self.db
            .set_extension_from("memory_graph_counters".to_string(), counters);
        self.db
            .set_extension_from("memory_settlement_at".to_string(), now_ms);
        self.db
            .set_extension_from("memory_settlement".to_string(), report.clone());
        self.db.flush_metadata(now_ms).await.ok();
        Ok(report)
    }

    /// Metamemory probe (plan M5): a cheap, LLM-free existence check.
    /// Answers "does the brain know anything about this?" so callers can
    /// decide whether a full recall is worth its latency and tokens. Empty
    /// results are remembered in the negative-knowledge cache until new
    /// memory forms.
    pub async fn probe_memory(
        &self,
        query: &str,
        limit: Option<usize>,
    ) -> Result<ProbeOutput, BoxError> {
        let query = query.trim();
        if query.is_empty() {
            return Err("probe query must not be empty".into());
        }
        let now_ms = unix_ms();
        if self.miss_cache.is_fresh_miss(query, now_ms).await? {
            self.bump_metrics(|metrics| metrics.negative_cache_hits += 1);
            return Ok(ProbeOutput {
                found: false,
                negative_cached: true,
                hits: Vec::new(),
            });
        }

        let limit = limit.unwrap_or(8).clamp(1, 50);
        // MODE omitted: the engine picks hybrid when it has semantic
        // capability, keyword otherwise.
        let response = self
            .execute_kip_readonly(anda_kip::Request {
                command: format!("SEARCH CONCEPT {} LIMIT {limit}", kip_string_literal(query)),
                readonly: true,
                ..Default::default()
            })
            .await?;
        let mut hits = match &response {
            anda_kip::Response::Ok { result, .. } => assess::citations_from_json(result),
            anda_kip::Response::Err { .. } => {
                return Err(format!("probe search failed: {response:?}").into());
            }
        };
        // The engine's keyword fallback has no relevance threshold, so any
        // token can match graph plumbing (meta-schema, domains, sleep
        // tasks, `$system` identities). Those are not user memory: counting
        // them as `found` would tell callers to pay for a recall that has
        // nothing to say.
        hits.retain(|hit| {
            !matches!(
                hit.r#type.as_deref(),
                Some("$ConceptType")
                    | Some("$PropositionType")
                    | Some("Domain")
                    | Some("SleepTask")
            ) && !hit.name.as_deref().unwrap_or_default().starts_with('$')
        });
        if hits.is_empty() {
            self.miss_cache.record_miss(query, now_ms).await?;
        }
        self.bump_metrics(|metrics| {
            if hits.is_empty() {
                metrics.probe_misses += 1;
            } else {
                metrics.probe_hits += 1;
            }
        });
        Ok(ProbeOutput {
            found: !hits.is_empty(),
            negative_cached: false,
            hits,
        })
    }

    /// Pins (or unpins) a graph entity (plan M6). Pinned memories are exempt
    /// from confidence decay. Returns the number of updated entities (0 when
    /// the id does not exist).
    pub async fn pin_memory(&self, entity: &str, pinned: bool) -> Result<u64, BoxError> {
        let entity = entity.trim();
        let command = if assess::is_proposition_entity_id(entity) {
            format!(
                "UPDATE ?link\nSET METADATA {{ pinned: {pinned} }}\nWHERE {{ ?link (id: {}) }}",
                kip_string_literal(entity)
            )
        } else if assess::is_concept_entity_id(entity) {
            format!(
                "UPDATE ?c\nSET METADATA {{ pinned: {pinned} }}\nWHERE {{ ?c {{id: {}}} }}",
                kip_string_literal(entity)
            )
        } else {
            return Err(format!("`{entity}` is not a graph entity id (C:* or P:*)").into());
        };
        let response = self
            .execute_kip_settlement(anda_kip::Request {
                command,
                ..Default::default()
            })
            .await?;
        match &response {
            anda_kip::Response::Ok { result, .. } => Ok(result
                .get("updated")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0)),
            anda_kip::Response::Err { .. } => Err(format!("pin failed: {response:?}").into()),
        }
    }

    /// Privacy-grade deletion (plan M6): physically removes entities from
    /// the graph (concepts detach and take their propositions with them) and
    /// their usage-ledger rows. Archive does not satisfy forget. Run with
    /// `dry_run` first; per-entity errors (e.g. KIP_3004 protecting system
    /// nodes) do not abort the batch.
    pub async fn forget_memory(
        &self,
        input: MemoryForgetInput,
    ) -> Result<MemoryForgetReport, BoxError> {
        // A running maintenance cycle already holds graph content in its LLM
        // context and re-UPSERTs concepts while consolidating — it could
        // silently re-materialize what we are about to physically delete.
        // (A formation backlog can still re-form a fact from *queued
        // conversations*; that is new information arriving, and conversation
        // scrubbing is tracked separately in the plan's deferred list.)
        if !input.dry_run && self.maintenance.is_processing() {
            return Err(
                "a maintenance cycle is running and could re-materialize deleted memories; \
                 retry forget when maintenance is idle"
                    .into(),
            );
        }
        let mut report = MemoryForgetReport {
            dry_run: input.dry_run,
            ..Default::default()
        };
        let mut seen = BTreeSet::new();
        for entity in &input.entities {
            let entity = entity.trim().to_string();
            if !seen.insert(entity.clone()) {
                continue;
            }
            let mut entry = MemoryForgetEntity {
                entity: entity.clone(),
                ..Default::default()
            };
            let (exists_command, delete_command) = if assess::is_proposition_entity_id(&entity) {
                let id = kip_string_literal(&entity);
                (
                    format!("FIND(?link) WHERE {{ ?link (id: {id}) }} LIMIT 1"),
                    format!("DELETE PROPOSITIONS ?link WHERE {{ ?link (id: {id}) }}"),
                )
            } else if assess::is_concept_entity_id(&entity) {
                let id = kip_string_literal(&entity);
                (
                    format!("FIND(?c) WHERE {{ ?c {{id: {id}}} }} LIMIT 1"),
                    format!("DELETE CONCEPT ?c DETACH WHERE {{ ?c {{id: {id}}} }}"),
                )
            } else {
                entry.error = Some("not a graph entity id (C:* or P:*)".to_string());
                report.entities.push(entry);
                continue;
            };

            let response = self
                .execute_kip_readonly(anda_kip::Request {
                    command: exists_command,
                    readonly: true,
                    ..Default::default()
                })
                .await?;
            entry.existed = matches!(&response, anda_kip::Response::Ok { result, .. }
                if assess::citations_from_json(result).iter().any(|hit| hit.entity == entity));

            if input.dry_run || !entry.existed {
                report.entities.push(entry);
                continue;
            }

            // Deleting a concept DETACH-deletes all its propositions; their
            // ledger rows must cascade too (entity ids embed predicate names
            // like `P:7:has_allergy` — usage traces of a forgotten memory).
            // Enumerate them before the DELETE destroys the links.
            let mut cascade: Vec<String> = vec![entity.clone()];
            if assess::is_concept_entity_id(&entity) {
                cascade.extend(self.concept_proposition_ids(&entity).await);
            }

            match self
                .execute_kip_settlement(anda_kip::Request {
                    command: delete_command,
                    ..Default::default()
                })
                .await
            {
                Ok(anda_kip::Response::Ok { result, .. }) => {
                    report.deleted_concepts += result
                        .get("deleted_concepts")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0);
                    report.deleted_propositions += result
                        .get("deleted_propositions")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0);
                    for gone in &cascade {
                        let _ = self.ledger.forget_entity(gone).await;
                    }
                }
                Ok(response) => {
                    entry.error = Some(format!("{response:?}"));
                }
                Err(err) => {
                    entry.error = Some(err.to_string());
                }
            }
            report.entities.push(entry);
        }
        let removed = report.deleted_concepts + report.deleted_propositions;
        if removed > 0 {
            self.bump_metrics(|metrics| metrics.forgotten_entities += removed);
            // Plan M6 cascade: cached probe-miss rows carry raw query text
            // that may reference the forgotten content; dropping the whole
            // (bounded) cache is the conservative fulfillment.
            if let Err(err) = self.miss_cache.clear().await {
                log::warn!(
                    target: "brain",
                    space_id = self.id;
                    "negative-knowledge cache clear after forget failed: {err:?}"
                );
            }
        }
        Ok(report)
    }

    /// Ids of every proposition attached to a concept (either slot); used by
    /// forget to cascade ledger rows for DETACH-deleted links. Best-effort:
    /// an enumeration failure only leaves ledger rows behind, never blocks
    /// the deletion itself.
    async fn concept_proposition_ids(&self, concept_id: &str) -> Vec<String> {
        let id = kip_string_literal(concept_id);
        let mut ids = BTreeSet::new();
        for command in [
            format!("FIND(?link) WHERE {{ ?link ({{id: {id}}}, ?p, ?o) }} LIMIT 1000"),
            format!("FIND(?link) WHERE {{ ?link (?s, ?p, {{id: {id}}}) }} LIMIT 1000"),
        ] {
            match self
                .execute_kip_readonly(anda_kip::Request {
                    command,
                    readonly: true,
                    ..Default::default()
                })
                .await
            {
                Ok(anda_kip::Response::Ok { result, .. }) => {
                    assess::collect_entity_objects(&result, &mut |id, _| {
                        if assess::is_proposition_entity_id(id) {
                            ids.insert(id.to_string());
                        }
                    });
                }
                other => {
                    log::warn!(
                        target: "brain",
                        space_id = self.id;
                        "enumerating propositions of {concept_id} for forget cascade failed: {other:?}"
                    );
                }
            }
        }
        ids.into_iter().collect()
    }

    /// Fires the dream self-test in the background (plan M7); called after a
    /// maintenance cycle completes. Skipped when disabled by policy or when
    /// a pass is already running.
    pub fn kick_memory_self_test(self: &Arc<Self>) {
        if self.memory_policy().self_test_queries_per_cycle == 0 {
            return;
        }
        let space = self.clone();
        tokio::spawn(async move {
            match space.run_memory_self_test(unix_ms()).await {
                Ok(Some(report)) => {
                    log::info!(
                        target: "brain",
                        space_id = space.id,
                        report:serde = report;
                        "memory self-test completed"
                    );
                }
                Ok(None) => {}
                Err(err) => {
                    log::warn!(
                        target: "brain",
                        space_id = space.id;
                        "memory self-test failed: {err:?}"
                    );
                }
            }
        });
    }

    /// The dream self-test (plan M7): sample recent, never-recalled memories,
    /// generate one natural query each (single LLM call), and check whether
    /// search actually surfaces them. Ungroundable memories become `review`
    /// SleepTasks the next full maintenance re-encodes. Self-test retrievals
    /// count only into `self_test_count` — never into usage reinforcement.
    ///
    /// Returns `None` when disabled, already running, or nothing qualifies.
    pub async fn run_memory_self_test(
        &self,
        now_ms: u64,
    ) -> Result<Option<SelfTestReport>, BoxError> {
        let Ok(_guard) = self.self_test_lock.try_lock() else {
            return Ok(None);
        };
        let policy = self.memory_policy();
        let budget = policy.self_test_queries_per_cycle as usize;
        if budget == 0 {
            return Ok(None);
        }

        // 1) Sample candidate links: active encoded memories (< 1.0 excludes
        // schema/system truths), skipping graph plumbing predicates. Links
        // already self-tested (within the retest horizon) or with proven
        // recall usage are excluded *in the query*: the engine returns
        // untested rows in stable order, so the sample window slides across
        // the whole graph over successive passes instead of re-reading the
        // same fixed prefix until coverage stalls.
        let retest_before =
            kip_string_literal(&kip_timestamp(now_ms.saturating_sub(SELF_TEST_RETEST_MS)));
        let response = self
            .execute_kip_readonly(anda_kip::Request {
                command: format!(
                    r#"FIND(?link) WHERE {{
  ?link (?s, ?p, ?o)
  FILTER(?p != "belongs_to_domain")
  FILTER(?p != "assigned_to")
  FILTER(IS_NULL(?link.metadata.superseded) || ?link.metadata.superseded != true)
  FILTER(IS_NULL(?link.metadata.self_tested_at) || ?link.metadata.self_tested_at < {retest_before})
  FILTER(IS_NULL(?link.metadata.last_recalled_at))
  FILTER(?link.metadata.confidence < 1.0)
}} LIMIT {}"#,
                    budget * 4
                ),
                readonly: true,
                ..Default::default()
            })
            .await?;
        if let anda_kip::Response::Err { .. } = &response {
            log::error!(
                target: "brain",
                space_id = self.id;
                "self-test sampling scan failed — dream self-test is NOT running \
                 (graph past the full-scan engine cap?): {response:?}"
            );
            return Ok(None);
        }
        let mut sampled: Vec<SelfTestCandidate> = Vec::new();
        if let anda_kip::Response::Ok { result, .. } = &response {
            assess::collect_entity_objects(result, &mut |id, object| {
                if !assess::is_proposition_entity_id(id) || sampled.len() >= budget * 4 {
                    return;
                }
                let subject = object.get("subject").and_then(serde_json::Value::as_str);
                let object_id = object.get("object").and_then(serde_json::Value::as_str);
                if let (Some(subject), Some(object_id)) = (subject, object_id) {
                    sampled.push(SelfTestCandidate {
                        id: id.to_string(),
                        subject: subject.to_string(),
                        object: object_id.to_string(),
                        predicate: id.splitn(3, ':').nth(2).unwrap_or_default().to_string(),
                        subject_type: String::new(),
                        subject_name: String::new(),
                        object_name: String::new(),
                    });
                }
            });
        }

        // Prefer memories with no usage evidence at all: recalled ones are
        // proven groundable, already-tested ones had their chance.
        let mut candidates = Vec::new();
        for candidate in sampled {
            let usage = self.ledger.get(&candidate.id).await?;
            if usage
                .as_ref()
                .is_none_or(|row| row.recall_count == 0 && row.self_test_count == 0)
            {
                candidates.push(candidate);
            }
            if candidates.len() >= budget {
                break;
            }
        }
        if candidates.is_empty() {
            return Ok(None);
        }

        // 2) Resolve subject/object names for query generation.
        let mut concept_cache: BTreeMap<String, (String, String)> = BTreeMap::new();
        for candidate in &mut candidates {
            let (subject_type, subject_name) = self
                .self_test_concept(&mut concept_cache, &candidate.subject)
                .await;
            let (_, object_name) = self
                .self_test_concept(&mut concept_cache, &candidate.object)
                .await;
            candidate.subject_type = subject_type;
            candidate.subject_name = subject_name;
            candidate.object_name = object_name;
        }
        // Unresolvable candidates (their subject concept is gone) are marked
        // tested too, or they would occupy the sample window forever.
        let unresolved: Vec<String> = candidates
            .iter()
            .filter(|candidate| candidate.subject_name.is_empty())
            .map(|candidate| candidate.id.clone())
            .collect();
        candidates.retain(|candidate| !candidate.subject_name.is_empty());
        if candidates.is_empty() {
            self.mark_self_tested(unresolved.iter(), now_ms).await;
            return Ok(None);
        }

        // 3) One LLM call generates all probe queries. The token budget is
        // enforced *before* the call by shrinking the candidate batch to fit
        // (≈3 chars per token, conservative); the knob bounds real spend
        // instead of warning after the fact.
        let max_prompt_chars = (policy.self_test_token_budget as usize).saturating_mul(3);
        while candidates.len() > 1
            && serde_json::to_string(&candidates)
                .map(|prompt| prompt.len() > max_prompt_chars)
                .unwrap_or(false)
        {
            candidates.pop();
        }
        let output = assess::AssessContext::complete(
            self,
            anda_core::CompletionRequest {
                instructions: SELF_TEST_INSTRUCTIONS.to_string(),
                prompt: serde_json::to_string_pretty(&candidates).unwrap_or_default(),
                effort: Some(anda_core::ModelEffort::Low),
                ..Default::default()
            },
        )
        .await?;
        let queries: SelfTestQueries = assess::parse_json_payload(&output.content)?;
        let mut report = SelfTestReport {
            tested_at: now_ms,
            usage: output.usage,
            ..Default::default()
        };
        let budget_tokens = report
            .usage
            .input_tokens
            .saturating_add(report.usage.output_tokens);
        if budget_tokens > policy.self_test_token_budget {
            log::warn!(
                target: "brain",
                space_id = self.id;
                "memory self-test used {budget_tokens} tokens, over policy budget {}",
                policy.self_test_token_budget
            );
        }

        // 4) Deterministic grounding check: does search surface the memory's
        // subject or object concept for the generated query?
        let mut tested_entities = BTreeSet::new();
        for candidate in &candidates {
            let Some(query) = queries
                .queries
                .iter()
                .find(|query| query.id == candidate.id)
                .map(|query| query.query.trim())
                .filter(|query| !query.is_empty())
            else {
                continue;
            };
            let response = self
                .execute_kip_readonly(anda_kip::Request {
                    command: format!("SEARCH CONCEPT {} LIMIT 8", kip_string_literal(query)),
                    readonly: true,
                    ..Default::default()
                })
                .await?;
            let mut hit_ids = BTreeSet::new();
            if let anda_kip::Response::Ok { result, .. } = &response {
                assess::collect_entity_objects(result, &mut |id, _| {
                    hit_ids.insert(id.to_string());
                });
            }
            report.tested += 1;
            tested_entities.insert(candidate.id.clone());
            if hit_ids.contains(&candidate.subject) || hit_ids.contains(&candidate.object) {
                report.grounded += 1;
                continue;
            }

            // Ungroundable: enqueue a review SleepTask for the next cycle,
            // unless one is already pending for this concept.
            if self
                .has_pending_review_task(&candidate.subject_name)
                .await?
            {
                continue;
            }
            self.ensure_sleep_task_schema().await?;
            let command = self_test_task_command(candidate, query, now_ms);
            match self
                .execute_kip_settlement(anda_kip::Request {
                    command,
                    ..Default::default()
                })
                .await
            {
                Ok(anda_kip::Response::Ok { .. }) => report.reencode_tasks += 1,
                Ok(response) => {
                    log::warn!(
                        target: "brain",
                        space_id = self.id;
                        "self-test SleepTask creation failed: {response:?}"
                    );
                }
                Err(err) => {
                    log::warn!(
                        target: "brain",
                        space_id = self.id;
                        "self-test SleepTask creation failed: {err:?}"
                    );
                }
            }
        }

        self.ledger
            .record_self_test(&tested_entities, now_ms)
            .await?;
        // Stamp tested (and unresolvable) links on the graph so the next
        // sampling pass moves past them — this is what keeps self-test
        // coverage sliding across the whole graph.
        self.mark_self_tested(tested_entities.iter().chain(unresolved.iter()), now_ms)
            .await;
        self.bump_metrics(|metrics| {
            metrics.self_test_tested += report.tested;
            metrics.self_test_grounded += report.grounded;
            metrics.reencode_tasks += report.reencode_tasks;
        });
        self.db
            .set_extension_from("memory_self_test".to_string(), report.clone());
        self.db.flush_metadata(now_ms).await.ok();
        Ok(Some(report))
    }

    /// Stamps `self_tested_at` on the given links; best-effort (an unmarked
    /// link is simply re-sampled by a later pass).
    async fn mark_self_tested(&self, entities: impl Iterator<Item = &String>, now_ms: u64) -> u64 {
        let stamp = kip_string_literal(&kip_timestamp(now_ms));
        let mut marked = 0u64;
        for entity in entities {
            let command = metadata_flag_command(entity, "self_tested_at", &stamp);
            match self
                .execute_kip_settlement(anda_kip::Request {
                    command,
                    ..Default::default()
                })
                .await
            {
                Ok(anda_kip::Response::Ok { .. }) => marked += 1,
                _ => {
                    log::warn!(
                        target: "brain",
                        space_id = self.id;
                        "marking self-tested failed for {entity}; will re-sample"
                    );
                }
            }
        }
        marked
    }

    /// Resolves a concept id to `(type, name)`, memoized per pass.
    async fn self_test_concept(
        &self,
        cache: &mut BTreeMap<String, (String, String)>,
        concept_id: &str,
    ) -> (String, String) {
        if let Some(found) = cache.get(concept_id) {
            return found.clone();
        }
        let mut resolved = (String::new(), String::new());
        if let Ok(anda_kip::Response::Ok { result, .. }) = self
            .execute_kip_readonly(anda_kip::Request {
                command: format!(
                    "FIND(?c) WHERE {{ ?c {{id: {}}} }} LIMIT 1",
                    kip_string_literal(concept_id)
                ),
                readonly: true,
                ..Default::default()
            })
            .await
        {
            assess::collect_entity_objects(&result, &mut |id, object| {
                if id == concept_id {
                    resolved = (
                        object
                            .get("type")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        object
                            .get("name")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    );
                }
            });
        }
        cache.insert(concept_id.to_string(), resolved.clone());
        resolved
    }

    /// True when a pending review SleepTask already targets this concept.
    async fn has_pending_review_task(&self, target_name: &str) -> Result<bool, BoxError> {
        let response = self
            .execute_kip_readonly(anda_kip::Request {
                command: format!(
                    "FIND(?task) WHERE {{ ?task {{type: \"SleepTask\"}} FILTER(?task.attributes.target_name == {}) FILTER(?task.attributes.status == \"pending\") }} LIMIT 1",
                    kip_string_literal(target_name)
                ),
                readonly: true,
                ..Default::default()
            })
            .await?;
        Ok(match &response {
            anda_kip::Response::Ok { result, .. } => {
                let mut found = false;
                assess::collect_entity_objects(result, &mut |_, _| found = true);
                found
            }
            // An unknown SleepTask type (schema not yet installed) means no
            // pending tasks either way.
            anda_kip::Response::Err { .. } => false,
        })
    }

    /// Installs the SleepTask type capsule (with its `assigned_to`
    /// predicate) when the graph does not have it yet.
    async fn ensure_sleep_task_schema(&self) -> Result<(), BoxError> {
        if self
            .memory
            .nexus
            .has_concept(&ConceptPK::Object {
                r#type: "$ConceptType".to_string(),
                name: "SleepTask".to_string(),
            })
            .await
        {
            return Ok(());
        }
        let response = self
            .execute_kip_settlement(anda_kip::Request {
                command: anda_kip::SLEEP_TASK_KIP.to_string(),
                ..Default::default()
            })
            .await?;
        match response {
            anda_kip::Response::Ok { .. } => Ok(()),
            response => Err(format!("SleepTask capsule install failed: {response:?}").into()),
        }
    }

    /// WikiDigest is off by default (PRD §13): extraction writes to the
    /// graph, so spaces opt in explicitly via `update_space { wiki_digest }`.
    pub fn wiki_digest_enabled(&self) -> bool {
        self.db.get_extension_as("wiki_digest").unwrap_or(false)
    }

    /// Runs the wiki digest synchronously: distills pending wiki versions
    /// into the Cognitive Nexus with citation provenance, supersedes stale
    /// facts, and re-verifies a citation sample.
    pub async fn run_wiki_digest(&self, user: Principal) -> Result<WikiDigestReport, BoxError> {
        if !self.wiki_digest_enabled() {
            return Err(
                "wiki digest is disabled for this space; enable it via update_space { \"wiki_digest\": true }"
                    .into(),
            );
        }
        // Digest is not a registered engine agent; it borrows the formation
        // agent's context (same write-path trust) with its own label.
        let ctx = self.engine.ctx_with(
            user,
            FormationAgent::NAME,
            "wiki_digest",
            Default::default(),
        )?;
        let rt = self.wiki_digest.run_pending(ctx, unix_ms()).await?;
        let _ = self
            .db
            .set_extension_from_with("wiki_digest_usage".to_string(), |v| {
                let mut usage: Usage = v.unwrap_or_default();
                usage.accumulate(&rt.usage);
                Some(usage)
            });
        // The digest writes graph memory outside the formation hook, so it
        // must invalidate the negative-knowledge cache itself (plan M5): a
        // probe miss cached before this digest could now be answerable.
        if rt.digested > 0
            && let Err(err) = self.miss_cache.clear().await
        {
            log::warn!(
                target: "brain",
                space_id = self.id;
                "negative-knowledge cache clear after wiki digest failed: {err:?}"
            );
        }
        Ok(rt)
    }

    /// Non-LLM wiki housekeeping (PRD §7.4 Full tier): audit-log retention
    /// pruning and the stale-document report. Cheap enough to run alongside
    /// every digest kick.
    pub fn kick_wiki_housekeeping(self: &Arc<Self>) {
        let space = self.clone();
        tokio::spawn(async move {
            let now_ms = unix_ms();
            if let Err(err) = space
                .wiki
                .prune_events(crate::wiki::DEFAULT_EVENT_RETENTION, now_ms)
                .await
            {
                log::warn!(target: "brain", space_id = space.id; "wiki event prune failed: {err:?}");
            }
            if let Err(err) = space
                .wiki
                .stale_report(now_ms, crate::wiki::DEFAULT_STALE_AFTER_MS)
                .await
            {
                log::warn!(target: "brain", space_id = space.id; "wiki stale report failed: {err:?}");
            }
        });
    }

    /// Fire-and-forget digest kick used by startup and post-maintenance
    /// hooks; a no-op when disabled or already running.
    pub fn kick_wiki_digest(self: &Arc<Self>) {
        if !self.wiki_digest_enabled() || self.wiki_digest.is_processing() {
            return;
        }
        let space = self.clone();
        tokio::spawn(async move {
            match space.run_wiki_digest(SELF_USER_ID).await {
                Ok(report) if report.digested > 0 => {
                    log::info!(target: "brain", space_id = space.id, report:serde = report; "wiki digest completed");
                }
                Ok(_) => {}
                Err(err) => {
                    log::warn!(target: "brain", space_id = space.id; "wiki digest failed: {err:?}");
                }
            }
        });
    }

    pub async fn restart_formation(
        &self,
        user: Principal,
        conversation: u64,
    ) -> Result<(), BoxError> {
        let ctx = self.engine.ctx_with(
            user,
            "formation_memory",
            "formation_memory",
            Default::default(),
        )?;
        self.formation.start_process(ctx, conversation).await
    }

    pub async fn execute_kip_readonly(
        &self,
        mut req: anda_kip::Request,
    ) -> Result<anda_kip::Response, BoxError> {
        req.readonly = true;
        match timeout(
            READONLY_KIP_TIMEOUT,
            req.execute(self.memory.nexus.as_ref()),
        )
        .await
        {
            Ok((_, res)) => Ok(res),
            Err(_) => Ok(anda_kip::Response::err(KipError::new(
                KipErrorCode::ExecutionTimeout,
                format!(
                    "read-only KIP execution timed out after {} seconds; memory is busy, retry later",
                    READONLY_KIP_TIMEOUT.as_secs()
                ),
            ))),
        }
    }

    pub async fn get_conversation(
        &self,
        collection: Option<String>,
        id: u64,
    ) -> Result<Conversation, BoxError> {
        let rt = match collection {
            Some(name) if name == "recall" => {
                self.recall.conversations.get_conversation(id).await?
            }
            Some(name) if name == "maintenance" => {
                self.maintenance.conversations.get_conversation(id).await?
            }
            _ => self.memory.get_conversation(id).await?,
        };

        Ok(rt)
    }

    pub async fn list_conversations(
        &self,
        collection: Option<String>,
        cursor: Option<String>,
        limit: Option<usize>,
    ) -> Result<(Vec<Conversation>, Option<String>), BoxError> {
        use anda_db::query::{Filter, Query, RangeQuery};

        let collection = match collection {
            Some(name) if name == "recall" => self.recall.conversations.conversations.clone(),
            Some(name) if name == "maintenance" => {
                self.maintenance.conversations.conversations.clone()
            }
            _ => self.memory.conversations.clone(),
        };
        // 0 means "no limit" to the database (an unbounded scan), and an empty
        // page would panic on `rt.first().unwrap()` below; clamp instead.
        let limit = limit.unwrap_or(10).clamp(1, 100);
        let cursor = match BTree::from_cursor::<u64>(&cursor)? {
            Some(cursor) => cursor,
            None => collection.max_document_id() + 1,
        };

        let filter = Some(Filter::Field((
            "_id".to_string(),
            RangeQuery::Lt(Fv::U64(cursor)),
        )));

        let rt: Vec<Conversation> = collection
            .search_as(Query {
                search: None,
                filter,
                limit: Some(limit),
            })
            .await?;
        let cursor = if rt.len() >= limit {
            BTree::to_cursor(&rt.first().unwrap()._id)
        } else {
            None
        };
        Ok((rt, cursor))
    }

    async fn flush(&self) -> Result<(), BoxError> {
        self.db.flush().await?;
        Ok(())
    }

    async fn close(&self) -> Result<(), BoxError> {
        self.db.close().await?;
        Ok(())
    }

    async fn create(
        object_store: Arc<dyn ObjectStore>,
        db_config: DBConfig,
        creator: Principal,
        owner: Principal,
        tier: u32,
        now_ms: u64,
    ) -> Result<SpaceInfo, BoxError> {
        let id = db_config.name.clone();
        let db = AndaDB::create(object_store.clone(), db_config).await?;
        let tier = SpaceTier {
            tier,
            updated_at: now_ms,
        };

        db.set_extension_from("creator".to_string(), creator.to_string());
        db.set_extension_from("owner".to_string(), owner.to_string());
        db.set_extension_from("tier".to_string(), &tier);

        let db = Arc::new(db);
        let nexus =
            CognitiveNexus::connect(db.clone(), async |nexus| init_nexus_kip(nexus).await).await?;

        let nexus = Arc::new(nexus);
        let memory = MemoryManagement::connect(db.clone(), nexus.clone()).await?;
        let info = SpaceInfo {
            id: id.clone(),
            name: None,
            description: None,
            owner: owner.to_string(),
            db_stats: db.stats(),
            concepts: nexus.concepts.len(),
            propositions: nexus.propositions.len(),
            conversations: memory.conversations.len(),
            public: false,
            tier,
            ..Default::default()
        };
        db.close().await?;
        Ok(info)
    }

    async fn connect(
        object_store: Arc<dyn ObjectStore>,
        db_config: DBConfig,
        management: Arc<dyn Management>,
        http_client: reqwest::Client,
        models: Arc<Models>,
        pinned: bool,
        autostart: bool,
    ) -> Result<Arc<Self>, BoxError> {
        let id = db_config.name.clone();
        let db = Arc::new(AndaDB::open(object_store.clone(), db_config).await?);
        let nexus =
            CognitiveNexus::connect(db.clone(), async |nexus| init_nexus_kip(nexus).await).await?;
        let mut schema = Conversation::schema()?;
        schema.with_version(4);

        let conversations = db
            .open_or_create_collection(
                schema.clone(),
                CollectionConfig {
                    name: "conversations".to_string(),
                    description: "conversations collection".to_string(),
                },
                async |collection| init_conversation_collection(collection).await,
            )
            .await?;

        let recall_conversations = db
            .open_or_create_collection(
                schema.clone(),
                CollectionConfig {
                    name: "recall".to_string(),
                    description: "Recall conversations collection".to_string(),
                },
                async |collection| init_conversation_collection(collection).await,
            )
            .await?;

        let maintenance_conversations = db
            .open_or_create_collection(
                schema.clone(),
                CollectionConfig {
                    name: "maintenance".to_string(),
                    description: "Maintenance conversations collection".to_string(),
                },
                async |collection| init_conversation_collection(collection).await,
            )
            .await?;

        let resources = db
            .open_or_create_collection(
                Resource::schema()?,
                CollectionConfig {
                    name: "resources".to_string(),
                    description: "Resources collection".to_string(),
                },
                async |collection| init_resource_collection(collection).await,
            )
            .await?;

        let memory = MemoryManagement {
            nexus: Arc::new(nexus),
            conversations,
            resources,
            kip_function_definitions: FUNCTION_DEFINITION.clone(),
        };
        let wiki = Arc::new(WikiService::connect(id.clone(), db.clone()).await?);

        // create a new models instance for each space to allow per-space customization in the future (e.g., different model providers or credentials)
        let models = Arc::new(Models::from_clone(models.as_ref()));
        let memory = Arc::new(memory);
        let wiki_digest = Arc::new(WikiDigest::new(wiki.clone(), memory.clone()));
        wiki.set_audit_reads(db.get_extension_as("wiki_audit_reads").unwrap_or(false));
        let memory_r = TimedMemoryReadonly::new(memory.clone());
        let memory_tool = MemoryTool::new(memory.clone());
        let note_tool = NoteTool::new();

        let hooks = Arc::new(Hooks::new(db.clone()));
        let formation = Arc::new(FormationAgent::new(memory.clone(), hooks.clone(), 100000));
        let recall = Arc::new(RecallAgent::new(
            memory.clone(),
            Conversations {
                conversations: recall_conversations,
            },
            hooks.clone(),
            65535,
        ));
        let maintenance = Arc::new(MaintenanceAgent::new(
            memory.clone(),
            Conversations {
                conversations: maintenance_conversations,
            },
            hooks.clone(),
        ));
        // Build agent engine with all configured components
        let engine = Engine::builder()
            .with_management(management)
            .with_models(models.clone())
            .register_tool(memory.clone())?
            .register_tool(Arc::new(memory_r))?
            .register_tool(Arc::new(memory_tool))?
            .register_tool(Arc::new(note_tool))?
            .register_tool(Arc::new(WikiSearchTool::new(wiki.clone())))?
            .register_tool(Arc::new(WikiReadTool::new(wiki.clone())))?
            .register_tool(Arc::new(WikiCommitTool::new(wiki.clone())))?
            .register_agent(formation.clone(), None)?
            .register_agent(recall.clone(), None)?
            .register_agent(maintenance.clone(), None)?
            .export_tools(vec![
                MemoryTool::NAME.to_string(),
                WikiSearchTool::NAME.to_string(),
                WikiReadTool::NAME.to_string(),
                WikiCommitTool::NAME.to_string(),
            ])
            .export_agents(vec![
                RecallAgent::NAME.to_string(),
                FormationAgent::NAME.to_string(),
                MaintenanceAgent::NAME.to_string(),
            ]);

        // Initialize and start the server
        let engine = engine.build(RecallAgent::NAME.to_string()).await?;
        let ledger = Arc::new(UsageLedger::connect(&db).await?);
        let miss_cache = Arc::new(MissCache::connect(&db).await?);
        let this = Arc::new(Self {
            id,
            db: db.clone(),
            http_client,
            models,
            formation,
            recall,
            maintenance,
            ledger,
            miss_cache,
            settlement_lock: tokio::sync::Mutex::new(()),
            self_test_lock: tokio::sync::Mutex::new(()),
            shadow_lock: tokio::sync::Mutex::new(()),
            judge_model: std::sync::RwLock::new(None),
            memory,
            wiki,
            wiki_digest,
            engine,
            pinned,
        });
        hooks.bind_space(Arc::downgrade(&this));

        if let Some(cfg) = db.get_extension_as::<ModelConfig>("byok") {
            let cfg: EngineModelConfig = cfg.into();
            if let Ok(model) = cfg.model(this.http_client.clone()) {
                this.models.set_model(model);
            } else {
                log::error!(target: "brain", space_id = this.id; "failed to initialize BYOK model from config: {:?}", cfg);
            }
        }

        if autostart {
            let this_clone = this.clone();
            tokio::spawn(async move {
                if let Err(err) = this_clone.maintenance.init().await {
                    log::warn!(target: "brain", space_id = this_clone.id; "maintenance history init failed: {err:?}");
                }
                if let Err(err) = this_clone.recall.init().await {
                    log::warn!(target: "brain", space_id = this_clone.id; "recall history init failed: {err:?}");
                }
                // Startup repair: reclaim wiki commit-crash leftovers before the
                // space serves queries built on them.
                match this_clone.wiki.orphan_sweep(unix_ms()).await {
                    Ok(report) if !report.is_empty() => {
                        log::warn!(target: "brain", space_id = this_clone.id, report:serde = report; "wiki orphan sweep repaired state");
                    }
                    Ok(_) => {}
                    Err(err) => {
                        log::warn!(target: "brain", space_id = this_clone.id; "wiki orphan sweep failed: {err:?}");
                    }
                }
                // Resume any wiki digest backlog left from before the restart.
                this_clone.kick_wiki_digest();
                this_clone.kick_wiki_housekeeping();
                // Resume formation if it was interrupted before. A missing marker
                // means nothing was processed yet, so resume from the beginning.
                let conversation = this_clone.formation.get_processed().unwrap_or_default();
                let _ = this_clone
                    .restart_formation(SELF_USER_ID, conversation + 1)
                    .await;
            });
        } else {
            // No-autostart open (shadow forks): the agents still need their
            // history cursors, but the inherited formation backlog and wiki
            // digest must stay untouched.
            if let Err(err) = this.maintenance.init().await {
                log::warn!(target: "brain", space_id = this.id; "maintenance history init failed: {err:?}");
            }
            if let Err(err) = this.recall.init().await {
                log::warn!(target: "brain", space_id = this.id; "recall history init failed: {err:?}");
            }
        }

        Ok(this)
    }
}

struct Hooks {
    db: Arc<AndaDB>,
    space: OnceLock<Weak<Space>>,
}

impl Hooks {
    fn new(db: Arc<AndaDB>) -> Self {
        Self {
            db,
            space: OnceLock::new(),
        }
    }

    fn bind_space(&self, space: Weak<Space>) {
        let _ = self.space.set(space);
    }

    fn space(&self) -> Option<Arc<Space>> {
        self.space.get().and_then(Weak::upgrade)
    }
}

// grcov-excl-start: async_trait rewrites this impl into generated futures; behavior is covered by hook and agent scheduling tests.
#[async_trait::async_trait]
impl BrainHook for Hooks {
    fn is_maintenance_processing(&self) -> bool {
        self.space()
            .map(|space| space.maintenance.is_processing())
            .unwrap_or(false)
    }

    async fn on_conversation_end(&self, agent_name: &str, conversation: &Conversation) {
        match agent_name {
            "recall_memory" => {
                let _ = self
                    .db
                    .set_extension_from_with("recall_usage".to_string(), |v| {
                        let mut usage: Usage = v.unwrap_or_default();
                        usage.accumulate(&conversation.usage);
                        Some(usage)
                    });
                // Usage-ledger writeback (plan M1): record which memories
                // this completed recall surfaced. Local collection writes —
                // cheap enough to run inline, which also guarantees a
                // maintenance cycle right after a recall sees its usage.
                if conversation.status == ConversationStatus::Completed
                    && let Some(space) = self.space()
                    && let Err(err) = space.record_recall_usage(&conversation.messages).await
                {
                    log::warn!(
                        target: "brain",
                        space_id = space.id;
                        "recall usage ledger writeback failed: {err:?}"
                    );
                }
            }
            "maintenance_memory" => {
                let _ = self
                    .db
                    .set_extension_from_with("maintenance_usage".to_string(), |v| {
                        let mut usage: Usage = v.unwrap_or_default();
                        usage.accumulate(&conversation.usage);
                        Some(usage)
                    });
                // Dream self-test (plan M7): after the sleep cycle ends, probe
                // whether recent memories are actually findable; failures
                // become review SleepTasks for the next cycle.
                if conversation.status == ConversationStatus::Completed
                    && let Some(space) = self.space()
                {
                    // Maintenance re-encodes and merges graph memory, so a
                    // probe miss cached before the cycle could now be
                    // answerable (plan M5 invalidation).
                    if let Err(err) = space.miss_cache.clear().await {
                        log::warn!(
                            target: "brain",
                            space_id = space.id;
                            "negative-knowledge cache clear after maintenance failed: {err:?}"
                        );
                    }
                    space.kick_memory_self_test();
                }
            }
            "formation_memory" => {
                let _ = self
                    .db
                    .set_extension_from_with("formation_usage".to_string(), |v| {
                        let mut usage: Usage = v.unwrap_or_default();
                        usage.accumulate(&conversation.usage);
                        Some(usage)
                    });
                // New memory can answer any past miss: drop the whole
                // negative-knowledge cache (plan M5 invalidation).
                if conversation.status == ConversationStatus::Completed
                    && let Some(space) = self.space()
                    && let Err(err) = space.miss_cache.clear().await
                {
                    log::warn!(
                        target: "brain",
                        space_id = space.id;
                        "negative-knowledge cache clear failed: {err:?}"
                    );
                }
            }
            _ => {}
        }
    }

    async fn try_start_formation(&self) {
        let space = match self.space() {
            Some(space) => space,
            None => return,
        };

        // A missing marker means nothing was processed yet; resume from the
        // beginning so conversations queued during maintenance are not stuck.
        let id = space.formation.get_processed().unwrap_or_default();
        let _ = space.restart_formation(SELF_USER_ID, id + 1).await;
        // Post-sleep digest: fold freshly committed wiki knowledge into the
        // graph while formation is quiet (PRD §7.3, Daydream cadence).
        space.kick_wiki_digest();
        space.kick_wiki_housekeeping();
    }

    async fn try_start_maintenance(&self, formation_id: DocumentId) -> Option<DocumentId> {
        let space = match self.space() {
            Some(space) => space,
            None => return None,
        };

        let at = space.maintenance.get_processed_at();
        let scope = if formation_id >= at.full + 168 {
            MaintenanceScope::Full
        } else if formation_id >= at.quick.max(at.full) + 42 {
            MaintenanceScope::Quick
        } else if formation_id >= at.daydream.max(at.quick).max(at.full) + 21 {
            MaintenanceScope::Daydream
        } else {
            return None;
        };

        let input = MaintenanceInput {
            trigger: "scheduled".to_string(),
            scope,
            timestamp: Some(rfc3339_datetime_now()),
            parameters: None,
            formation_id,
        };
        match space.maintenance(SELF_USER_ID, input).await {
            Ok(rt) => rt.conversation,
            Err(err) => {
                log::error!(target: "brain", formation_id; "scheduled maintenance failed to start: {}", err);
                None
            }
        }
    }
}
// grcov-excl-stop

async fn init_conversation_collection(collection: &mut Collection) -> Result<(), DBError> {
    collection.set_tokenizer(jieba_tokenizer());
    collection.create_btree_index_nx(&["user"]).await?;
    collection.remove_btree_index(&["thread"]).await?;
    collection.remove_btree_index(&["period"]).await?;
    collection
        .remove_bm25_index(&["messages", "resources", "artifacts"])
        .await?;
    Ok(())
}

async fn init_resource_collection(collection: &mut Collection) -> Result<(), DBError> {
    collection.set_tokenizer(jieba_tokenizer());
    collection.create_btree_index_nx(&["tags"]).await?;
    collection.create_btree_index_nx(&["hash"]).await?;
    collection.create_btree_index_nx(&["mime_type"]).await?;
    collection
        .remove_bm25_index(&["name", "description", "metadata"])
        .await?;
    Ok(())
}

async fn init_nexus_kip(nexus: &CognitiveNexus) -> Result<(), KipError> {
    if !nexus
        .has_concept(&ConceptPK::Object {
            r#type: PERSON_TYPE.to_string(),
            name: META_SELF_NAME.to_string(),
        })
        .await
    {
        // uuc56-gyb: Principal::from_slice(&[1])
        let kml = &[PERSON_SELF_KIP, PERSON_SYSTEM_KIP].join("\n");

        let result = nexus.execute_kml(parse_kml(kml)?, false).await?;
        log::info!(target: "brain", result:serde = result; "Init $self and $system");
    }
    Ok(())
}

#[cfg(test)]
impl Space {
    pub(crate) fn ctx_for_test(
        &self,
        user: Principal,
        agent_name: &str,
    ) -> Result<anda_engine::context::AgentCtx, BoxError> {
        self.engine
            .ctx_with(user, agent_name, agent_name, Default::default())
    }

    pub(crate) fn maintenance_for_test(&self) -> Arc<MaintenanceAgent> {
        self.maintenance.clone()
    }
}

impl Space {
    /// One-shot completion on this space's model, used only by the eval
    /// harness (judge, user simulator, prompt optimizer). Not exposed over
    /// HTTP/MCP.
    pub(crate) async fn eval_complete(
        &self,
        req: anda_core::CompletionRequest,
    ) -> Result<AgentOutput, BoxError> {
        use anda_core::CompletionFeatures;

        let ctx = self.engine.ctx_with(
            SELF_USER_ID,
            RecallAgent::NAME,
            RecallAgent::NAME,
            Default::default(),
        )?;
        ctx.completion(req, Vec::new()).await
    }
}

/// Timeout for one settlement-built write KIP command.
const SETTLEMENT_KIP_TIMEOUT: Duration = Duration::from_secs(30);

/// Per-command row limit for bulk settlement passes.
const SETTLEMENT_BATCH_LIMIT: usize = 500;

/// Upper bound of decay batches per settlement (500 × 20 = 10k links).
const SETTLEMENT_MAX_BATCHES: usize = 20;

/// Bulk decay is a weekly-rate process (the factor is documented per week in
/// BrainMaintenance.md); links decayed more recently than this are skipped,
/// so daily maintenance cannot over-decay.
const DECAY_MIN_INTERVAL_MS: u64 = 7 * 24 * 3_600 * 1_000;

/// A link self-tested longer ago than this becomes eligible for re-sampling,
/// so re-encoded memories eventually get their grounding re-verified.
const SELF_TEST_RETEST_MS: u64 = 30 * 24 * 3_600 * 1_000;

/// Renders a KIP string literal with backslashes and quotes escaped.
fn kip_string_literal(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

const SHADOW_JUDGE_INSTRUCTIONS: &str = r#"You compare two answers an AI memory system gave to the same user query under two different internal configurations. Pick the answer that better serves the user: correct use of remembered facts, honoring later corrections, honest uncertainty. Ignore style differences.

Respond with ONLY a JSON object: {"winner": "a" | "b" | "tie", "reason": "..."}"#;

#[derive(Debug, serde::Deserialize)]
struct ShadowVerdict {
    winner: String,
    #[serde(default)]
    reason: String,
}

/// Collects every JSON string leaf; used to read KQL name projections.
fn collect_string_leaves(value: &serde_json::Value, out: &mut BTreeSet<String>) {
    match value {
        serde_json::Value::String(text) if !text.trim().is_empty() => {
            out.insert(text.clone());
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_string_leaves(item, out);
            }
        }
        serde_json::Value::Object(map) => {
            for item in map.values() {
                collect_string_leaves(item, out);
            }
        }
        _ => {}
    }
}

/// Graph metadata timestamps are RFC3339 strings (lexicographically
/// comparable in KQL filters).
fn kip_timestamp(now_ms: u64) -> String {
    rfc3339_datetime(now_ms).unwrap_or_else(rfc3339_datetime_now)
}

/// Strips the recall self-report footer from assistant text in a chat
/// history (plan M4): `content` is stripped by the callers, and the history
/// must not re-leak the markup to clients that read it.
fn strip_recall_meta_from_history(history: &mut [anda_core::Message]) {
    for message in history {
        if message.role != "assistant" {
            continue;
        }
        for part in &mut message.content {
            if let anda_core::ContentPart::Text { text } = part
                && text.contains(assess::RECALL_META_TAG_OPEN)
            {
                let (stripped, _) = assess::split_recall_meta(text);
                *text = stripped;
            }
        }
    }
}

/// Settlement command: set one metadata flag on one link by id. `value` is
/// raw KIP (pass `"true"` for booleans, a `kip_string_literal` for strings).
fn metadata_flag_command(entity: &str, key: &str, value: &str) -> String {
    format!(
        "UPDATE ?link\nSET METADATA {{ {key}: {value} }}\nWHERE {{ ?link (id: {entity}) }}",
        entity = kip_string_literal(entity),
    )
}

/// Settlement command: flush one recalled proposition's usage counters onto
/// its graph metadata (plan M2 step 1). Absolute values, so re-running is
/// idempotent.
fn reinforcement_update_command(entity: &str, last_recalled_ms: u64, recall_count: u64) -> String {
    format!(
        "UPDATE ?link\nSET METADATA {{ last_recalled_at: {recalled_at}, recall_count: {recall_count} }}\nWHERE {{ ?link (id: {entity}) }}",
        recalled_at = kip_string_literal(&kip_timestamp(last_recalled_ms)),
        entity = kip_string_literal(entity),
    )
}

/// One memory sampled for the dream self-test (plan M7); serialized as the
/// query-generation prompt.
#[derive(Debug, serde::Serialize)]
struct SelfTestCandidate {
    id: String,
    subject: String,
    object: String,
    predicate: String,
    subject_type: String,
    subject_name: String,
    object_name: String,
}

#[derive(Debug, Default, serde::Deserialize)]
struct SelfTestQueries {
    #[serde(default)]
    queries: Vec<SelfTestQuery>,
}

#[derive(Debug, serde::Deserialize)]
struct SelfTestQuery {
    id: String,
    query: String,
}

const SELF_TEST_INSTRUCTIONS: &str = r#"You test the searchability of an AI's memory graph. You will receive a JSON array of memories, each a proposition with a subject, predicate, and object.

For each memory, write ONE short natural-language query a real user would plausibly ask that this memory should answer. Use the everyday words of the subject/object names — never internal ids, never the predicate name verbatim unless a user would say it.

Respond with ONLY a JSON object:
{"queries": [{"id": "<memory id>", "query": "..."}]}"#;

/// Self-test command: enqueue a `review` SleepTask (capsule schema) for a
/// memory that search could not surface, targeting its subject concept —
/// re-encoding (aliases, richer description, domain links) happens at the
/// concept level.
fn self_test_task_command(candidate: &SelfTestCandidate, query: &str, now_ms: u64) -> String {
    let date = kip_timestamp(now_ms).chars().take(10).collect::<String>();
    let slug: String = candidate
        .subject_name
        .chars()
        .map(|ch| {
            let ch = ch.to_ascii_lowercase();
            if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
                ch
            } else {
                '_'
            }
        })
        .take(40)
        .collect();
    let reason = format!(
        "memory self-test: the query {} did not surface `{}` ({}) via search; re-encode the concept with aliases, a richer description, or domain links so it becomes findable",
        query, candidate.subject_name, candidate.id
    );
    format!(
        r#"UPSERT {{
  CONCEPT ?task {{
    {{type: "SleepTask", name: {name}}}
    SET ATTRIBUTES {{
      target_type: {target_type},
      target_name: {target_name},
      requested_action: "review",
      reason: {reason},
      status: "pending",
      priority: 2
    }}
    SET PROPOSITIONS {{
      ("assigned_to", {{type: "Person", name: "$system"}}),
      ("belongs_to_domain", {{type: "Domain", name: "System"}})
    }}
  }}
}}
WITH METADATA {{ source: "memory_self_test", author: "$system", confidence: 1.0, created_at: {created_at} }}"#,
        name = kip_string_literal(&format!("SleepTask:{date}:review:{slug}")),
        target_type = kip_string_literal(&candidate.subject_type),
        target_name = kip_string_literal(&candidate.subject_name),
        reason = kip_string_literal(&reason),
        created_at = kip_string_literal(&kip_timestamp(now_ms)),
    )
}

/// Settlement command: one usage-modulated bulk decay batch (plan M2 step 2).
/// This is the Maintenance prompt's former Phase-7 command with three new
/// exemptions the runtime can now enforce: recently recalled links (usage
/// reinforcement), pinned links, and links decayed within the weekly window.
fn decay_update_command(policy: &MemoryPolicy, now_ms: u64, decay_min_interval_ms: u64) -> String {
    let stale_window_ms = u64::from(policy.stale_event_threshold_days) * 86_400_000;
    let created_before = kip_string_literal(&kip_timestamp(now_ms.saturating_sub(stale_window_ms)));
    // The filter is also the intra-settlement batch cursor: rows stamped
    // `now` this pass no longer match `< decay_before`, so an interval of 0
    // still terminates — it just disables the *cross-cycle* rate limit.
    let decay_before =
        kip_string_literal(&kip_timestamp(now_ms.saturating_sub(decay_min_interval_ms)));
    let now_iso = kip_string_literal(&kip_timestamp(now_ms));
    format!(
        r#"UPDATE ?link
SET METADATA {{
  confidence: CLAMP(MUL(?link.metadata.confidence, {factor}), {floor}, 1.0),
  decay_applied_at: {now_iso}
}}
WHERE {{
  ?link (?s, ?p, ?o)
  FILTER(?p != "belongs_to_domain")
  FILTER(IS_NULL(?link.metadata.superseded) || ?link.metadata.superseded != true)
  FILTER(IS_NULL(?link.metadata.pinned) || ?link.metadata.pinned != true)
  FILTER(IS_NOT_NULL(?link.metadata.created_at))
  FILTER(?link.metadata.created_at < {created_before})
  FILTER(IS_NULL(?link.metadata.decay_applied_at) || ?link.metadata.decay_applied_at < {decay_before})
  FILTER(IS_NULL(?link.metadata.last_recalled_at) || ?link.metadata.last_recalled_at < {created_before})
  FILTER(?link.metadata.confidence > {floor} && ?link.metadata.confidence < 1.0)
}}
LIMIT {limit}"#,
        factor = policy.confidence_decay_factor,
        floor = policy.decay_floor,
        limit = SETTLEMENT_BATCH_LIMIT,
    )
}

/// Copies every object of a space (`{space_id}/**`) from one object store to
/// another, preserving paths. This is the eval fork primitive: AndaDB
/// metadata embeds its own base path, so a space must keep its id and be
/// forked into a *different* store — never renamed inside the same store.
pub async fn copy_space_objects(
    src: &Arc<dyn ObjectStore>,
    dst: &Arc<dyn ObjectStore>,
    space_id: &str,
) -> Result<u64, BoxError> {
    use futures::TryStreamExt;
    use object_store::ObjectStoreExt;

    let prefix = object_store::path::Path::from(space_id);
    let mut objects = src.list(Some(&prefix));
    let mut copied = 0u64;
    while let Some(meta) = objects.try_next().await? {
        let payload = src.get(&meta.location).await?.bytes().await?;
        dst.put(&meta.location, payload.into()).await?;
        copied += 1;
    }
    if copied == 0 {
        return Err(format!("space {space_id} has no objects to copy").into());
    }
    Ok(copied)
}

/// Deletes every object of a space (`{space_id}/**`) from the store. Used by
/// the eval harness to remove run-scoped spaces after a run; the space must
/// be closed first.
pub async fn delete_space_objects(
    store: &Arc<dyn ObjectStore>,
    space_id: &str,
) -> Result<u64, BoxError> {
    use futures::TryStreamExt;
    use object_store::ObjectStoreExt;

    let prefix = object_store::path::Path::from(space_id);
    // Collect before deleting: mutating while listing can invalidate the
    // stream on some backends.
    let locations: Vec<_> = store
        .list(Some(&prefix))
        .map_ok(|meta| meta.location)
        .try_collect()
        .await?;
    let deleted = locations.len() as u64;
    for location in locations {
        store.delete(&location).await?;
    }
    Ok(deleted)
}

#[cfg(test)]
mod tests {
    use super::{
        AppState, Hooks, Space, SpaceEntry, init_conversation_collection, init_resource_collection,
    };
    use crate::{
        agents::{BrainHook, SELF_USER_ID, TimedMemoryReadonly},
        payload::StringOr,
        types::{
            AddSpaceTokenInput, FormationInput, InputContext, MaintenanceInput,
            MaintenanceParameters, MaintenanceScope, MemoryPolicy, ModelConfig, RecallInput,
            SpaceTier, TokenScope, UpdateSpaceInput,
        },
    };
    use anda_core::{
        AgentOutput, BoxError, BoxPinFut, CompletionRequest, Message, Principal, Resource, Tool,
        Usage,
    };
    use anda_db::{collection::CollectionConfig, database::DBConfig, storage::StorageConfig};
    use anda_engine::{
        context::BaseCtx,
        management::{BaseManagement, Visibility},
        memory::{Conversation, ConversationRef, ConversationStatus, MemoryReadonly},
        model::{CompletionFeaturesDyn, Model, Models, reqwest},
        unix_ms,
    };
    use cose2::{CoseMap, Label, Sign1Message, Value, cwt::Claims, iana};
    use ic_auth_types::ByteBufB64;
    use ic_cose_types::cose::ed25519::{SigningKey, VerifyingKey, ed25519_sign};
    use object_store::memory::InMemory;
    use std::collections::BTreeSet;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use tokio::time::{Duration, sleep};
    use tokio_util::sync::CancellationToken;

    #[derive(Debug)]
    struct FinalCompleter;

    impl CompletionFeaturesDyn for FinalCompleter {
        fn model_name(&self) -> String {
            "final-test-model".to_string()
        }

        fn completion(&self, req: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
            Box::pin(async move {
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

    /// Answers the self-test query-generation call: the first candidate (by
    /// id order) gets a query matching its subject name, the second gets
    /// unfindable gibberish — one grounded, one not.
    #[derive(Debug)]
    struct SelfTestCompleter;

    impl CompletionFeaturesDyn for SelfTestCompleter {
        fn model_name(&self) -> String {
            "self-test-model".to_string()
        }

        fn completion(&self, req: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
            Box::pin(async move {
                let candidates: Vec<serde_json::Value> =
                    serde_json::from_str(&req.prompt).unwrap_or_default();
                let mut ids: Vec<(String, String)> = candidates
                    .iter()
                    .filter_map(|candidate| {
                        Some((
                            candidate.get("id")?.as_str()?.to_string(),
                            candidate.get("subject_name")?.as_str()?.to_string(),
                        ))
                    })
                    .collect();
                ids.sort();
                let queries: Vec<serde_json::Value> = ids
                    .iter()
                    .enumerate()
                    .map(|(index, (id, subject_name))| {
                        let query = if index == 0 {
                            subject_name.clone()
                        } else {
                            "qqqzzzxxx nonsense".to_string()
                        };
                        serde_json::json!({"id": id, "query": query})
                    })
                    .collect();
                Ok(AgentOutput {
                    content: serde_json::json!({ "queries": queries }).to_string(),
                    usage: Usage {
                        input_tokens: 20,
                        output_tokens: 10,
                        ..Default::default()
                    },
                    ..Default::default()
                })
            })
        }
    }

    fn test_app_state_with_self_test_model(name: &str) -> AppState {
        let models = Models::default();
        models.set_model(Model::with_completer(Arc::new(SelfTestCompleter)));
        test_app_state_with_models(name, Arc::new(models))
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
                    content: "slow done".to_string(),
                    chat_history: vec![Message {
                        role: "assistant".to_string(),
                        content: vec![format!("slow processed: {}", req.prompt).into()],
                        ..Default::default()
                    }],
                    ..Default::default()
                })
            })
        }
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

    fn test_app_state_with_final_model(name: &str) -> AppState {
        let models = Models::default();
        models.set_model(Model::with_completer(Arc::new(FinalCompleter)));
        test_app_state_with_models(name, Arc::new(models))
    }

    fn test_app_state_with_slow_model(name: &str) -> AppState {
        let models = Models::default();
        models.set_model(Model::with_completer(Arc::new(SlowCompleter)));
        test_app_state_with_models(name, Arc::new(models))
    }

    fn test_app_state_with_pubkeys(name: &str) -> AppState {
        let mut bytes = [0x66; 32];
        bytes[0] = 0x58;
        let key = VerifyingKey::from_bytes(&bytes).unwrap();
        let mut app = test_app_state_with_models(name, Arc::new(Models::default()));
        app.ed25519_pubkeys = Arc::new(vec![key]);
        app
    }

    fn test_signing_key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    fn test_app_state_with_signing_key(name: &str, signing_key: &SigningKey) -> AppState {
        let mut app = test_app_state_with_models(name, Arc::new(Models::default()));
        app.ed25519_pubkeys = Arc::new(vec![signing_key.verifying_key()]);
        app
    }

    fn signed_token(
        signing_key: &SigningKey,
        user: Principal,
        audience: &str,
        scope: &str,
    ) -> String {
        let claims = Claims {
            subject: Some(user.to_string()),
            audience: Some(audience.to_string()),
            extra: CoseMap::from_iter([(
                Label::Int(iana::CWTClaimScope),
                Value::Text(scope.to_string()),
            )]),
            ..Default::default()
        };
        let payload = claims.to_vec().unwrap();
        let mut sign1 = Sign1Message::new(Some(payload));
        let tbs_data = sign1
            .prepare_signature(Some(Label::Int(iana::AlgorithmEdDSA)), None, None)
            .unwrap();
        sign1
            .set_signature(ed25519_sign(signing_key.as_bytes(), &tbs_data).to_vec())
            .unwrap();
        ByteBufB64(sign1.to_vec().unwrap()).to_string()
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

    async fn wait_until_idle(space: &Space) {
        for _ in 0..100 {
            if !space.is_processing() {
                return;
            }
            sleep(Duration::from_millis(10)).await;
        }
        panic!("space did not become idle");
    }

    async fn create_loaded_space(app: &AppState, id: &str) -> Arc<Space> {
        app.admin_create_space(
            Principal::from_slice(&[1]),
            Principal::from_slice(&[2]),
            id.to_string(),
            1,
            123,
        )
        .await
        .unwrap();

        app.load_space(id, false).await.unwrap()
    }

    #[tokio::test]
    async fn copy_space_objects_forks_space_into_isolated_store() {
        let app = test_app_state("fork_src");
        let space = create_loaded_space(&app, "fork_space").await;
        space
            .update(
                UpdateSpaceInput {
                    name: Some("before fork".to_string()),
                    ..Default::default()
                },
                unix_ms(),
            )
            .await
            .unwrap();
        space.db.close().await.unwrap();

        let fork_store: Arc<dyn super::ObjectStore> = Arc::new(InMemory::new());
        let copied = super::copy_space_objects(&app.object_store(), &fork_store, "fork_space")
            .await
            .unwrap();
        assert!(copied > 0);

        // Copying a missing space fails loudly instead of forking nothing.
        let empty: Arc<dyn super::ObjectStore> = Arc::new(InMemory::new());
        assert!(
            super::copy_space_objects(&app.object_store(), &empty, "missing_space")
                .await
                .is_err()
        );

        // The fork opens under the same id in its own store, sees the same
        // state, and mutations do not leak back to the source store.
        let fork_state = app.fork_with_store(fork_store);
        let fork = fork_state.load_space("fork_space", true).await.unwrap();
        assert_eq!(fork.get_info().name.as_deref(), Some("before fork"));
        fork.update(
            UpdateSpaceInput {
                name: Some("after fork".to_string()),
                ..Default::default()
            },
            unix_ms(),
        )
        .await
        .unwrap();
        fork.db.close().await.unwrap();

        let fork_store2: Arc<dyn super::ObjectStore> = Arc::new(InMemory::new());
        super::copy_space_objects(&app.object_store(), &fork_store2, "fork_space")
            .await
            .unwrap();
        let fork_state2 = app.fork_with_store(fork_store2);
        let fork2 = fork_state2.load_space("fork_space", true).await.unwrap();
        assert_eq!(fork2.get_info().name.as_deref(), Some("before fork"));
        fork2.db.close().await.unwrap();
    }

    #[tokio::test]
    async fn delete_space_objects_removes_run_scoped_space() {
        let app = test_app_state("delete_src");
        let space = create_loaded_space(&app, "delete_space").await;
        space.db.close().await.unwrap();

        let deleted = super::delete_space_objects(&app.object_store(), "delete_space")
            .await
            .unwrap();
        assert!(deleted > 0);
        app.evict_space("delete_space").await;

        // The prefix is empty afterwards: forking the deleted space fails.
        let empty: Arc<dyn super::ObjectStore> = Arc::new(InMemory::new());
        assert!(
            super::copy_space_objects(&app.object_store(), &empty, "delete_space")
                .await
                .is_err()
        );

        // Deleting an already-empty prefix is a no-op, not an error.
        assert_eq!(
            super::delete_space_objects(&app.object_store(), "delete_space")
                .await
                .unwrap(),
            0
        );
    }

    #[test]
    fn space_entry_starts_uninitialized_with_recent_access_time() {
        let before = unix_ms();
        let entry = SpaceEntry::new();
        let after = unix_ms();

        assert!(!entry.cell.initialized());
        assert!(entry.last_access_ms() >= before);
        assert!(entry.last_access_ms() <= after);
    }

    #[test]
    fn space_entry_touch_refreshes_last_access_time() {
        let entry = SpaceEntry::new();
        entry.last_access_ms.store(0, Ordering::Relaxed);
        let before_touch = unix_ms();

        entry.touch();

        assert!(entry.last_access_ms() >= before_touch);
    }

    #[tokio::test]
    async fn create_space_persists_metadata_before_returning() {
        let object_store = Arc::new(InMemory::new());
        let db_config = test_db_config("create_space_persists_metadata");
        let creator = Principal::from_slice(&[1]);
        let owner = Principal::from_slice(&[2]);

        let info = Space::create(
            object_store.clone(),
            db_config.clone(),
            creator,
            owner,
            1,
            123,
        )
        .await
        .unwrap();

        assert_eq!(info.owner, owner.to_string());
        assert_eq!(info.tier.tier, 1);

        let db = anda_db::database::AndaDB::open(object_store, db_config)
            .await
            .unwrap();
        let persisted_owner: String = db.get_extension_as("owner").unwrap();
        let persisted_tier: SpaceTier = db.get_extension_as("tier").unwrap();

        assert_eq!(persisted_owner, owner.to_string());
        assert_eq!(persisted_tier.tier, 1);

        db.close().await.unwrap();
    }

    #[tokio::test]
    async fn collection_bootstrap_helpers_create_and_prune_indexes() {
        let object_store = Arc::new(InMemory::new());
        let db_config = test_db_config("collection_bootstrap_helpers");
        let db = anda_db::database::AndaDB::create(object_store, db_config)
            .await
            .unwrap();
        let mut conversation_schema = Conversation::schema().unwrap();
        conversation_schema.with_version(4);

        let conversations = db
            .open_or_create_collection(
                conversation_schema,
                CollectionConfig {
                    name: "conversations".to_string(),
                    description: "conversations collection".to_string(),
                },
                async |collection| {
                    collection.create_btree_index_nx(&["thread"]).await?;
                    collection.create_btree_index_nx(&["period"]).await?;
                    collection
                        .create_bm25_index_nx(&["messages", "resources", "artifacts"])
                        .await?;
                    init_conversation_collection(collection).await
                },
            )
            .await
            .unwrap();
        let meta = conversations.metadata();
        assert!(meta.btree_indexes.contains_key("user"));
        assert!(!meta.btree_indexes.contains_key("thread"));
        assert!(!meta.btree_indexes.contains_key("period"));
        assert!(
            !meta
                .bm25_indexes
                .contains_key("messages-resources-artifacts")
        );

        let resources = db
            .open_or_create_collection(
                Resource::schema().unwrap(),
                CollectionConfig {
                    name: "resources".to_string(),
                    description: "Resources collection".to_string(),
                },
                async |collection| {
                    collection
                        .create_bm25_index_nx(&["name", "description", "metadata"])
                        .await?;
                    init_resource_collection(collection).await
                },
            )
            .await
            .unwrap();
        let meta = resources.metadata();
        assert!(meta.btree_indexes.contains_key("tags"));
        assert!(meta.btree_indexes.contains_key("hash"));
        assert!(meta.btree_indexes.contains_key("mime_type"));
        assert!(!meta.bm25_indexes.contains_key("name-description-metadata"));

        db.close().await.unwrap();
    }

    #[test]
    fn app_state_allows_local_auth_when_no_pubkeys_are_configured() {
        let app = test_app_state("local_auth");
        let now_ms = 123;

        let admin = app
            .check_admin("", "space", TokenScope::Write, now_ms)
            .unwrap();
        assert_eq!(admin.user, Principal::management_canister());
        assert_eq!(admin.audience, "space");
        assert_eq!(admin.scope, TokenScope::Write);

        let user = app
            .check_auth("", "space", TokenScope::Read, now_ms)
            .unwrap();
        assert_eq!(user.user, SELF_USER_ID);

        let optional = app
            .check_auth_if("", "space", TokenScope::Read, now_ms)
            .unwrap()
            .unwrap();
        assert_eq!(optional.user, SELF_USER_ID);
    }

    #[test]
    fn app_state_rejects_invalid_tokens_when_pubkeys_are_configured() {
        let app = test_app_state_with_pubkeys("configured_auth");
        let now_ms = 123;

        assert!(
            app.check_auth_if("short", "space", TokenScope::Read, now_ms)
                .unwrap()
                .is_none()
        );
        assert!(
            app.check_auth("not-base64", "space", TokenScope::Read, now_ms)
                .is_err()
        );
        assert!(
            app.check_admin("not-base64", "space", TokenScope::Write, now_ms)
                .is_err()
        );
    }

    #[test]
    fn app_state_accepts_valid_signed_tokens_and_rejects_scope_mismatches() {
        let signing_key = test_signing_key();
        let app = test_app_state_with_signing_key("signed_auth", &signing_key);
        let now_ms = 1_725_000_000_000;

        let read_token = signed_token(&signing_key, SELF_USER_ID, "space-a", "read");
        let auth = app
            .check_auth(&read_token, "space-a", TokenScope::Read, now_ms)
            .unwrap();
        assert_eq!(auth.user, SELF_USER_ID);
        assert_eq!(auth.audience, "space-a");
        assert_eq!(auth.scope, TokenScope::Read);
        assert!(
            app.check_auth(&read_token, "space-a", TokenScope::Write, now_ms)
                .err()
                .unwrap()
                .to_string()
                .contains("insufficient scope")
        );
        assert!(
            app.check_auth(&read_token, "space-b", TokenScope::Read, now_ms)
                .err()
                .unwrap()
                .to_string()
                .contains("invalid audience")
        );

        let admin_token = signed_token(&signing_key, SELF_USER_ID, "*", "*");
        let admin = app
            .check_admin(&admin_token, "any-space", TokenScope::Write, now_ms)
            .unwrap();
        assert_eq!(admin.user, SELF_USER_ID);
        assert_eq!(admin.scope, TokenScope::All);

        let optional = app
            .check_auth_if(&admin_token, "any-space", TokenScope::Read, now_ms)
            .unwrap()
            .unwrap();
        assert_eq!(optional.audience, "*");

        let non_admin = signed_token(&signing_key, Principal::from_slice(&[99]), "*", "*");
        assert!(
            app.check_admin(&non_admin, "any-space", TokenScope::Read, now_ms)
                .err()
                .unwrap()
                .to_string()
                .contains("admin access required")
        );
    }

    #[tokio::test]
    async fn app_state_loads_spaces_once_and_rejects_duplicate_loaded_space() {
        let app = test_app_state("load_cache");
        let id = "load_cache_space";
        let owner = Principal::from_slice(&[3]);

        let info = app
            .admin_create_space(Principal::from_slice(&[1]), owner, id.to_string(), 2, 456)
            .await
            .unwrap();
        assert_eq!(info.id, id);
        assert_eq!(info.owner, owner.to_string());

        let loaded = app.load_space(id, false).await.unwrap();
        let loaded_again = app.load_space(id, false).await.unwrap();
        assert!(Arc::ptr_eq(&loaded, &loaded_again));

        let err = app
            .admin_create_space(Principal::from_slice(&[1]), owner, id.to_string(), 2, 456)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("already exists"));
    }

    #[tokio::test]
    async fn app_state_background_shutdown_and_idle_eviction_paths() {
        let app = test_app_state("background_eviction");
        let space_id = "background_eviction_space";
        let space = create_loaded_space(&app, space_id).await;

        let cancel = CancellationToken::new();
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(2), app.start_background_tasks(cancel))
            .await
            .unwrap();

        let entry = {
            let spaces = app.spaces.read().await;
            spaces.get(space_id).unwrap().clone()
        };
        app.flush_and_evict_once(unix_ms(), 10_000).await;
        assert!(app.spaces.read().await.contains_key(space_id));

        entry.last_access_ms.store(0, Ordering::Relaxed);
        assert!(!app.try_evict_idle_space(space_id, &entry, 10_000, 1).await);

        let wrong_entry = Arc::new(SpaceEntry::new());
        assert!(
            !app.try_evict_idle_space(space_id, &wrong_entry, 10_000, 1)
                .await
        );

        drop(space);
        for _ in 0..100 {
            let space_refs = entry.cell.get().map(Arc::strong_count).unwrap_or_default();
            if space_refs == 1 {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
        drop(entry);
        app.flush_and_evict_once(10_000, 1).await;
        assert!(!app.spaces.read().await.contains_key(space_id));

        let missing_entry = Arc::new(SpaceEntry::new());
        assert!(
            !app.try_evict_idle_space("missing_space", &missing_entry, 10_000, 1)
                .await
        );

        assert!(app.load_space("never_created_space", false).await.is_err());
        let uninitialized = {
            let spaces = app.spaces.read().await;
            spaces.get("never_created_space").unwrap().clone()
        };
        assert!(
            !app.try_evict_idle_space("never_created_space", &uninitialized, 10_000, 1)
                .await
        );
    }

    #[tokio::test]
    async fn flush_and_evict_removes_idle_uninitialized_placeholders() {
        let app = test_app_state("placeholder_eviction");
        assert!(app.load_space("placeholder_space", false).await.is_err());
        {
            let spaces = app.spaces.read().await;
            let entry = spaces.get("placeholder_space").unwrap();
            assert!(!entry.cell.initialized());
        }

        // Not idle yet: the placeholder entry is kept for retrying.
        app.flush_and_evict_once(unix_ms(), 10_000).await;
        assert!(app.spaces.read().await.contains_key("placeholder_space"));

        // Idle: the placeholder is dropped so probes for unknown space IDs
        // cannot grow the map unboundedly.
        app.flush_and_evict_once(unix_ms() + 20_000, 10_000).await;
        assert!(!app.spaces.read().await.contains_key("placeholder_space"));
    }

    #[tokio::test]
    async fn space_metadata_tier_byok_and_tokens_roundtrip() {
        let app = test_app_state("space_metadata");
        let space = create_loaded_space(&app, "space_metadata").await;

        let tier = space.admin_update_tier(3, 999).await.unwrap();
        assert_eq!(tier.tier, 3);
        assert_eq!(space.get_tier().tier, 3);

        space
            .update(
                UpdateSpaceInput {
                    name: Some("Research Brain".to_string()),
                    description: Some("memory space".to_string()),
                    public: Some(true),
                    ..Default::default()
                },
                1000,
            )
            .await
            .unwrap();
        assert!(space.is_public());

        let info = space.get_info();
        assert_eq!(info.name.as_deref(), Some("Research Brain"));
        assert_eq!(info.description.as_deref(), Some("memory space"));
        assert_eq!(info.tier.tier, 3);

        let byok = ModelConfig {
            family: "openai".to_string(),
            model: "gpt-test".to_string(),
            api_base: "https://api.example.test".to_string(),
            api_key: "test-key".to_string(),
            ..Default::default()
        };
        space.update_byok(byok.clone()).await.unwrap();
        assert_eq!(space.get_byok().unwrap().model, byok.model);

        let disabled_byok = ModelConfig {
            family: "openai".to_string(),
            model: "disabled-test".to_string(),
            api_base: "https://api.example.test".to_string(),
            api_key: "test-key".to_string(),
            disabled: true,
            ..Default::default()
        };
        let err = space.update_byok(disabled_byok).await.unwrap_err();
        assert!(err.to_string().contains("model is disabled"));
        assert_eq!(space.get_byok().unwrap().model, byok.model);

        let token = "STtest-token".to_string();
        let st = space
            .add_space_token(
                token.clone(),
                AddSpaceTokenInput {
                    scope: TokenScope::Read,
                    name: "reader".to_string(),
                    expires_at: Some(2000),
                    labels: None,
                },
                1100,
            )
            .await
            .unwrap();
        assert_eq!(st.scope, TokenScope::Read);
        assert_eq!(st.name, "reader");

        space
            .verify_space_token(token.clone(), TokenScope::Read, 1200)
            .unwrap();
        assert!(
            space
                .verify_space_token(token.clone(), TokenScope::Write, 1200)
                .is_err()
        );
        assert!(
            space
                .verify_space_token(token.clone(), TokenScope::Read, 2500)
                .is_err()
        );

        let tokens = space.list_space_tokens().unwrap();
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].token, token);
        assert_eq!(tokens[0].usage, 1);

        assert!(space.revoke_space_token("STtest-token").await.unwrap());
        assert!(!space.revoke_space_token("STtest-token").await.unwrap());

        // Platform-managed extensions must not be deletable through the
        // space-token revoke API.
        assert!(space.revoke_space_token("tier").await.is_err());
        assert_eq!(space.get_tier().tier, 3);
        assert!(space.revoke_space_token("byok").await.is_err());
        assert!(space.get_byok().is_some());

        space
            .update(
                UpdateSpaceInput {
                    ..Default::default()
                },
                3000,
            )
            .await
            .unwrap();
        assert!(space.get_byok().is_some());
    }

    #[tokio::test]
    async fn memory_policy_round_trips_and_rejects_invalid_values() {
        let app = test_app_state("memory_policy");
        let space = create_loaded_space(&app, "memory_policy").await;

        // Absent policy means defaults (compiled-in behavior).
        assert_eq!(space.memory_policy(), MemoryPolicy::default());

        let policy = MemoryPolicy {
            confidence_decay_factor: 0.9,
            orphan_max_count: 5,
            ..Default::default()
        };
        space
            .update(
                UpdateSpaceInput {
                    memory_policy: Some(policy.clone()),
                    ..Default::default()
                },
                1000,
            )
            .await
            .unwrap();
        assert_eq!(space.memory_policy(), policy);

        // Invalid values reject the update and leave the stored policy alone.
        let invalid = MemoryPolicy {
            confidence_decay_factor: 0.0,
            ..Default::default()
        };
        let err = space
            .update(
                UpdateSpaceInput {
                    memory_policy: Some(invalid),
                    ..Default::default()
                },
                1001,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("confidence_decay_factor"));
        assert_eq!(space.memory_policy(), policy);

        // Budget knobs are capped, not just floored: this object is settable
        // over HTTP, and an unbounded self-test budget is a cost bomb.
        let bomb = MemoryPolicy {
            self_test_queries_per_cycle: u32::MAX,
            ..Default::default()
        };
        let err = space
            .update(
                UpdateSpaceInput {
                    memory_policy: Some(bomb),
                    ..Default::default()
                },
                1002,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("self_test_queries_per_cycle"));
        assert_eq!(space.memory_policy(), policy);
    }

    #[tokio::test]
    async fn maintenance_fills_parameters_from_memory_policy() {
        let app = test_app_state_with_slow_model("maintenance_policy_params");
        let space = create_loaded_space(&app, "maintenance_policy_params").await;
        space
            .update(
                UpdateSpaceInput {
                    memory_policy: Some(MemoryPolicy {
                        unsorted_max_backlog: 42,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                1000,
            )
            .await
            .unwrap();

        let output = space
            .maintenance(SELF_USER_ID, MaintenanceInput::default())
            .await
            .unwrap();
        let conversation = space
            .get_conversation(
                Some("maintenance".to_string()),
                output.conversation.unwrap(),
            )
            .await
            .unwrap();
        let encoded = serde_json::to_string(&conversation.messages).unwrap();
        // The prompt is the pretty-printed MaintenanceInput JSON, escaped
        // inside the stored message text.
        assert!(encoded.contains("\\\"unsorted_max_backlog\\\": 42"));
        assert!(encoded.contains("\\\"confidence_decay_factor\\\": 0.95"));
    }

    #[tokio::test]
    async fn maintenance_keeps_explicit_parameters() {
        let app = test_app_state_with_slow_model("maintenance_explicit_params");
        let space = create_loaded_space(&app, "maintenance_explicit_params").await;

        let input = MaintenanceInput {
            parameters: Some(MaintenanceParameters {
                stale_event_threshold_days: Some(3),
                confidence_decay_factor: None,
                unsorted_max_backlog: None,
                orphan_max_count: None,
            }),
            ..Default::default()
        };
        let output = space.maintenance(SELF_USER_ID, input).await.unwrap();
        let conversation = space
            .get_conversation(
                Some("maintenance".to_string()),
                output.conversation.unwrap(),
            )
            .await
            .unwrap();
        let encoded = serde_json::to_string(&conversation.messages).unwrap();
        assert!(encoded.contains("\\\"stale_event_threshold_days\\\": 3"));
        // The policy must not overwrite explicit parameters.
        assert!(!encoded.contains("confidence_decay_factor"));
    }

    #[tokio::test]
    async fn usage_ledger_counts_corrections_and_flush_state() {
        let app = test_app_state("usage_ledger");
        let space = create_loaded_space(&app, "usage_ledger").await;

        let entities =
            std::collections::BTreeSet::from(["P:1:prefers".to_string(), "C:9".to_string()]);
        space.ledger.record_recall(&entities, 100).await.unwrap();
        space
            .ledger
            .record_recall(
                &std::collections::BTreeSet::from(["P:1:prefers".to_string()]),
                200,
            )
            .await
            .unwrap();

        let row = space.ledger.get("P:1:prefers").await.unwrap().unwrap();
        assert_eq!(row.recall_count, 2);
        assert_eq!(row.last_recalled_at, 200);
        assert_eq!(
            space.ledger.get("C:9").await.unwrap().unwrap().recall_count,
            1
        );

        let pending = space.ledger.unflushed_recalls(100).await.unwrap();
        assert_eq!(pending.len(), 2);
        assert!(pending.iter().all(|row| row.dirty == 1));

        // Corrections record once per entity.
        assert!(
            space
                .ledger
                .record_correction("P:1:prefers", 300)
                .await
                .unwrap()
        );
        assert!(
            !space
                .ledger
                .record_correction("P:1:prefers", 400)
                .await
                .unwrap()
        );
        let row = space.ledger.get("P:1:prefers").await.unwrap().unwrap();
        assert_eq!(row.correction_count, 1);
        assert_eq!(row.last_corrected_at, 300);

        // Flushed rows drop out of the pending scan until recalled again.
        space
            .ledger
            .mark_flushed(row._id, row.recall_count, 500)
            .await
            .unwrap();
        let pending = space.ledger.unflushed_recalls(100).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].entity, "C:9");

        // A recall recorded after the flush re-dirties the row: the flag —
        // not a time watermark — decides retry, so late writes are never
        // stranded outside a scan window.
        space
            .ledger
            .record_recall(
                &std::collections::BTreeSet::from(["P:1:prefers".to_string()]),
                50, // deliberately older than the flush timestamp
            )
            .await
            .unwrap();
        let pending = space.ledger.unflushed_recalls(100).await.unwrap();
        assert_eq!(pending.len(), 2);
    }

    async fn seed_kip(space: &Space, command: &str) {
        let response = space
            .execute_kip_settlement(anda_kip::Request {
                command: command.to_string(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            matches!(response, anda_kip::Response::Ok { .. }),
            "seed failed: {response:?}"
        );
    }

    async fn link_metadata(space: &Space, id: &str) -> serde_json::Value {
        let response = space
            .execute_kip_readonly(anda_kip::Request {
                command: format!("FIND(?link) WHERE {{ ?link (id: \"{id}\") }}"),
                readonly: true,
                ..Default::default()
            })
            .await
            .unwrap();
        let mut metadata = serde_json::Value::Null;
        if let anda_kip::Response::Ok { result, .. } = &response {
            crate::assess::collect_entity_objects(result, &mut |found, object| {
                if found == id {
                    metadata = object
                        .get("metadata")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null);
                }
            });
        }
        metadata
    }

    #[tokio::test]
    async fn settlement_decays_unused_and_spares_recalled_memories() {
        let app = test_app_state("settlement");
        let space = create_loaded_space(&app, "settlement").await;
        let now_ms = unix_ms();
        let old_iso = anda_engine::rfc3339_datetime(now_ms - 30 * 86_400_000).unwrap();

        // Seed a tiny graph: one schema type, one predicate, three concepts,
        // two month-old links at confidence 0.8.
        seed_kip(
            &space,
            r#"UPSERT { CONCEPT ?t { {type: "$ConceptType", name: "Topic"} } WITH METADATA { "source": "test", "confidence": 1.0 } }"#,
        )
        .await;
        seed_kip(
            &space,
            r#"UPSERT { CONCEPT ?p { {type: "$PropositionType", name: "linked_to"} } WITH METADATA { "source": "test", "confidence": 1.0 } }"#,
        )
        .await;
        for name in ["alpha", "beta", "gamma"] {
            seed_kip(
                &space,
                &format!(
                    r#"UPSERT {{ CONCEPT ?c {{ {{type: "Topic", name: "{name}"}} }} WITH METADATA {{ "source": "test", "confidence": 1.0 }} }}"#
                ),
            )
            .await;
        }
        for target in ["beta", "gamma"] {
            seed_kip(
                &space,
                &format!(
                    r#"UPSERT {{ CONCEPT ?c {{ {{type: "Topic", name: "alpha"}} SET PROPOSITIONS {{ ("linked_to", {{type: "Topic", name: "{target}"}}) }} }} WITH METADATA {{ "source": "test_source", "confidence": 0.8, "created_at": "{old_iso}" }} }}"#
                ),
            )
            .await;
        }

        let response = space
            .execute_kip_readonly(anda_kip::Request {
                command: r#"FIND(?link) WHERE { ?link (?s, "linked_to", ?o) }"#.to_string(),
                readonly: true,
                ..Default::default()
            })
            .await
            .unwrap();
        let mut ids: Vec<String> = Vec::new();
        if let anda_kip::Response::Ok { result, .. } = &response {
            crate::assess::collect_entity_objects(result, &mut |id, _| {
                if crate::assess::is_proposition_entity_id(id) {
                    ids.push(id.to_string());
                }
            });
        }
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 2, "expected two links: {response:?}");
        let (recalled, unused) = (ids[0].clone(), ids[1].clone());

        // One link was surfaced by a recall; the other never was.
        space
            .ledger
            .record_recall(
                &std::collections::BTreeSet::from([recalled.clone()]),
                now_ms,
            )
            .await
            .unwrap();

        let report = space
            .settle_memory_metabolism(MaintenanceScope::Full, now_ms)
            .await
            .unwrap();
        assert_eq!(report.reinforced, 1, "{report:?}");
        assert!(report.decay_ran);
        assert_eq!(report.decayed, 1, "{report:?}");
        assert_eq!(report.new_corrections, 0);

        // The recalled link kept its confidence and gained usage metadata.
        let metadata = link_metadata(&space, &recalled).await;
        assert_eq!(metadata["recall_count"], 1, "{metadata}");
        assert!(metadata["last_recalled_at"].is_string());
        let confidence = metadata["confidence"].as_f64().unwrap();
        assert!((confidence - 0.8).abs() < 1e-9, "{metadata}");

        // The unused link decayed by the policy factor (0.8 × 0.95).
        let metadata = link_metadata(&space, &unused).await;
        let confidence = metadata["confidence"].as_f64().unwrap();
        assert!((confidence - 0.76).abs() < 1e-9, "{metadata}");
        assert!(metadata["decay_applied_at"].is_string());

        // Idempotence: an immediate re-settlement neither re-decays (weekly
        // rate limit) nor re-flushes (ledger flush marker).
        let report = space
            .settle_memory_metabolism(MaintenanceScope::Full, now_ms + 1)
            .await
            .unwrap();
        assert_eq!(report.reinforced, 0, "{report:?}");
        assert_eq!(report.decayed, 0, "{report:?}");

        // Supersede the unused link: the next settlement records it as a
        // correction and charges its source.
        seed_kip(
            &space,
            &format!(
                "UPDATE ?link\nSET METADATA {{ superseded: true }}\nWHERE {{ ?link (id: \"{unused}\") }}"
            ),
        )
        .await;
        let report = space
            .settle_memory_metabolism(MaintenanceScope::Quick, now_ms + 2)
            .await
            .unwrap();
        assert!(!report.decay_ran);
        assert_eq!(report.new_corrections, 1, "{report:?}");
        let row = space.ledger.get(&unused).await.unwrap().unwrap();
        assert_eq!(row.correction_count, 1);
        let reliability: std::collections::BTreeMap<String, crate::types::SourceReliability> =
            space.db.get_extension_as("source_reliability").unwrap();
        assert_eq!(reliability["test_source"].corrections, 1);

        // The processed link is marked settled on the graph, so it leaves
        // the discovery window (the marker, not a LIMIT window, is the scan
        // cursor) and the next settlement finds nothing new.
        assert_eq!(
            link_metadata(&space, &unused).await["correction_settled"],
            true
        );
        let report = space
            .settle_memory_metabolism(MaintenanceScope::Quick, now_ms + 3)
            .await
            .unwrap();
        assert_eq!(report.new_corrections, 0, "{report:?}");

        // The settlement report is persisted for observability.
        assert!(space.memory_settlement().is_some());
    }

    /// Seeds Topic concepts alpha/beta/gamma plus two month-old `linked_to`
    /// links from alpha at confidence 0.8. Returns the sorted link ids.
    async fn seed_topic_links(space: &Space, created_iso: &str) -> Vec<String> {
        seed_kip(
            space,
            r#"UPSERT { CONCEPT ?t { {type: "$ConceptType", name: "Topic"} } WITH METADATA { "source": "test", "confidence": 1.0 } }"#,
        )
        .await;
        seed_kip(
            space,
            r#"UPSERT { CONCEPT ?p { {type: "$PropositionType", name: "linked_to"} } WITH METADATA { "source": "test", "confidence": 1.0 } }"#,
        )
        .await;
        for name in ["alpha", "beta", "gamma"] {
            seed_kip(
                space,
                &format!(
                    r#"UPSERT {{ CONCEPT ?c {{ {{type: "Topic", name: "{name}"}} }} WITH METADATA {{ "source": "test", "confidence": 1.0 }} }}"#
                ),
            )
            .await;
        }
        for target in ["beta", "gamma"] {
            seed_kip(
                space,
                &format!(
                    r#"UPSERT {{ CONCEPT ?c {{ {{type: "Topic", name: "alpha"}} SET PROPOSITIONS {{ ("linked_to", {{type: "Topic", name: "{target}"}}) }} }} WITH METADATA {{ "source": "test_source", "confidence": 0.8, "created_at": "{created_iso}" }} }}"#
                ),
            )
            .await;
        }

        let response = space
            .execute_kip_readonly(anda_kip::Request {
                command: r#"FIND(?link) WHERE { ?link (?s, "linked_to", ?o) }"#.to_string(),
                readonly: true,
                ..Default::default()
            })
            .await
            .unwrap();
        let mut ids: Vec<String> = Vec::new();
        if let anda_kip::Response::Ok { result, .. } = &response {
            crate::assess::collect_entity_objects(result, &mut |id, _| {
                if crate::assess::is_proposition_entity_id(id) {
                    ids.push(id.to_string());
                }
            });
        }
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 2);
        ids
    }

    #[tokio::test]
    async fn probe_memory_uses_negative_knowledge_cache() {
        let app = test_app_state("probe_memory");
        let space = create_loaded_space(&app, "probe_memory").await;
        let now_ms = unix_ms();
        let old_iso = anda_engine::rfc3339_datetime(now_ms - 86_400_000).unwrap();
        seed_topic_links(&space, &old_iso).await;

        let hit = space.probe_memory("alpha", None).await.unwrap();
        assert!(hit.found, "{hit:?}");
        assert!(!hit.negative_cached);
        assert!(
            hit.hits
                .iter()
                .any(|citation| citation.name.as_deref() == Some("alpha"))
        );

        let miss = space
            .probe_memory("qqqzzzxxx nonsense", None)
            .await
            .unwrap();
        assert!(!miss.found);
        assert!(!miss.negative_cached);

        // The second identical miss is answered from the cache.
        let cached = space
            .probe_memory("qqqzzzxxx nonsense", None)
            .await
            .unwrap();
        assert!(!cached.found);
        assert!(cached.negative_cached);

        // Formation completion clears negative knowledge (hook calls this).
        space.miss_cache.clear().await.unwrap();
        let fresh = space
            .probe_memory("qqqzzzxxx nonsense", None)
            .await
            .unwrap();
        assert!(!fresh.negative_cached);

        // Oversized queries are never cached (unauthenticated probes on
        // public spaces must not be a disk-write amplifier): the identical
        // repeat still misses without a cache hit.
        let long_query = format!("qqqzzzxxx {}", "x".repeat(600));
        let miss = space.probe_memory(&long_query, None).await.unwrap();
        assert!(!miss.found);
        let repeat = space.probe_memory(&long_query, None).await.unwrap();
        assert!(!repeat.negative_cached);
    }

    #[tokio::test]
    async fn pin_exempts_from_decay_and_forget_removes_for_real() {
        let app = test_app_state("pin_forget");
        let space = create_loaded_space(&app, "pin_forget").await;
        let now_ms = unix_ms();
        let old_iso = anda_engine::rfc3339_datetime(now_ms - 30 * 86_400_000).unwrap();
        let ids = seed_topic_links(&space, &old_iso).await;
        let (pinned, doomed) = (ids[0].clone(), ids[1].clone());

        // Pin one link: decay must skip it (plan M6 + M2 integration).
        assert_eq!(space.pin_memory(&pinned, true).await.unwrap(), 1);
        let report = space
            .settle_memory_metabolism(MaintenanceScope::Full, now_ms)
            .await
            .unwrap();
        assert_eq!(report.decayed, 1, "{report:?}");
        let metadata = link_metadata(&space, &pinned).await;
        assert_eq!(metadata["pinned"], true);
        assert!((metadata["confidence"].as_f64().unwrap() - 0.8).abs() < 1e-9);

        // Dry run reports without deleting.
        let report = space
            .forget_memory(crate::types::MemoryForgetInput {
                entities: vec![doomed.clone()],
                dry_run: true,
            })
            .await
            .unwrap();
        assert!(report.dry_run);
        assert!(report.entities[0].existed);
        assert_eq!(report.deleted_propositions, 0);
        assert!(!link_metadata(&space, &doomed).await.is_null());

        // Real forget removes the link, its ledger row, and reports bogus
        // ids per entity without aborting the batch.
        space
            .ledger
            .record_recall(&BTreeSet::from([doomed.clone()]), now_ms)
            .await
            .unwrap();
        let report = space
            .forget_memory(crate::types::MemoryForgetInput {
                entities: vec![doomed.clone(), "bogus".to_string()],
                dry_run: false,
            })
            .await
            .unwrap();
        assert_eq!(report.deleted_propositions, 1, "{report:?}");
        assert!(
            report
                .entities
                .iter()
                .any(|entry| entry.entity == "bogus" && entry.error.is_some())
        );
        assert!(link_metadata(&space, &doomed).await.is_null());
        assert!(space.ledger.get(&doomed).await.unwrap().is_none());

        // Forgetting the concept detaches and removes its remaining link —
        // and cascades the ledger rows of the DETACH-deleted propositions
        // (their ids embed predicate names; usage traces of a forgotten
        // memory must not survive).
        space
            .ledger
            .record_recall(&BTreeSet::from([pinned.clone()]), now_ms)
            .await
            .unwrap();
        let hit = space.probe_memory("alpha", None).await.unwrap();
        let concept = hit
            .hits
            .iter()
            .find(|citation| citation.name.as_deref() == Some("alpha"))
            .unwrap()
            .entity
            .clone();
        let report = space
            .forget_memory(crate::types::MemoryForgetInput {
                entities: vec![concept],
                dry_run: false,
            })
            .await
            .unwrap();
        assert_eq!(report.deleted_concepts, 1, "{report:?}");
        assert!(report.deleted_propositions >= 1, "{report:?}");
        assert!(link_metadata(&space, &pinned).await.is_null());
        assert!(
            space.ledger.get(&pinned).await.unwrap().is_none(),
            "cascaded proposition must lose its ledger row"
        );
    }

    #[tokio::test]
    async fn memory_self_test_flags_unfindable_memories() {
        let app = test_app_state_with_self_test_model("memory_self_test");
        let space = create_loaded_space(&app, "memory_self_test").await;
        let now_ms = unix_ms();
        let old_iso = anda_engine::rfc3339_datetime(now_ms - 86_400_000).unwrap();
        let ids = seed_topic_links(&space, &old_iso).await;

        let report = space
            .run_memory_self_test(now_ms)
            .await
            .unwrap()
            .expect("self-test must run");
        assert_eq!(report.tested, 2, "{report:?}");
        assert_eq!(report.grounded, 1, "{report:?}");
        assert_eq!(report.reencode_tasks, 1, "{report:?}");
        assert_eq!(report.groundability(), Some(0.5));

        // The ungroundable memory produced one pending review SleepTask
        // targeting its subject concept.
        let response = space
            .execute_kip_readonly(anda_kip::Request {
                command: "FIND(?task) WHERE { ?task {type: \"SleepTask\"} FILTER(?task.attributes.status == \"pending\") } LIMIT 10".to_string(),
                readonly: true,
                ..Default::default()
            })
            .await
            .unwrap();
        let tasks = match &response {
            anda_kip::Response::Ok { result, .. } => crate::assess::citations_from_json(result)
                .into_iter()
                .filter(|citation| citation.r#type.as_deref() == Some("SleepTask"))
                .count(),
            _ => 0,
        };
        assert_eq!(tasks, 1, "{response:?}");

        // Guardrail: self-tests count only into self_test_count — never into
        // usage reinforcement.
        for id in &ids {
            let row = space.ledger.get(id).await.unwrap().unwrap();
            assert_eq!(row.self_test_count, 1);
            assert_eq!(row.recall_count, 0);
            assert_eq!(row.last_recalled_at, 0);
        }

        // Every candidate was already tested: the next pass has nothing to do.
        assert!(
            space
                .run_memory_self_test(now_ms + 1)
                .await
                .unwrap()
                .is_none()
        );

        // The exclusion lives on the graph (`self_tested_at`), not just in
        // the ledger: even with ledger rows gone, tested links stay out of
        // the sample window. This is what makes coverage slide across the
        // graph instead of re-reading the same fixed prefix forever.
        for id in &ids {
            assert!(
                !link_metadata(&space, id).await["self_tested_at"].is_null(),
                "tested link must carry the self_tested_at stamp"
            );
            space.ledger.forget_entity(id).await.unwrap();
        }
        assert!(
            space
                .run_memory_self_test(now_ms + 2)
                .await
                .unwrap()
                .is_none()
        );

        // A newly formed memory enters the window on the next pass.
        let old_iso = anda_engine::rfc3339_datetime(now_ms - 86_400_000).unwrap();
        seed_kip(
            &space,
            &format!(
                r#"UPSERT {{ CONCEPT ?c {{ {{type: "Topic", name: "beta"}} SET PROPOSITIONS {{ ("linked_to", {{type: "Topic", name: "gamma"}}) }} }} WITH METADATA {{ "source": "test_source", "confidence": 0.8, "created_at": "{old_iso}" }} }}"#
            ),
        )
        .await;
        let report = space
            .run_memory_self_test(now_ms + 3)
            .await
            .unwrap()
            .expect("new memory must be sampled");
        assert_eq!(report.tested, 1, "{report:?}");

        // The report persists and surfaces as the groundability graph stat.
        let stored: crate::types::SelfTestReport = space
            .db
            .get_extension_as("memory_self_test")
            .expect("report stored");
        assert_eq!(stored.groundability(), Some(1.0));
    }

    #[derive(Debug)]
    struct JudgeCompleter;

    impl CompletionFeaturesDyn for JudgeCompleter {
        fn model_name(&self) -> String {
            "judge-test-model".to_string()
        }

        fn completion(&self, _req: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
            Box::pin(async move {
                Ok(AgentOutput {
                    content: "judge verdict".to_string(),
                    ..Default::default()
                })
            })
        }
    }

    #[tokio::test]
    async fn judge_complete_routes_to_independent_model() {
        use crate::assess::AssessContext;
        let app = test_app_state_with_final_model("judge_route");
        let space = create_loaded_space(&app, "judge_route").await;
        let request = || CompletionRequest {
            prompt: "judge this".to_string(),
            ..Default::default()
        };

        // Without a judge model, judge completions share the space model.
        let out = AssessContext::judge_complete(space.as_ref(), request())
            .await
            .unwrap();
        assert_eq!(out.content, "done");

        space.set_judge_model_for_test(Model::with_completer(Arc::new(JudgeCompleter)));
        let out = AssessContext::judge_complete(space.as_ref(), request())
            .await
            .unwrap();
        assert_eq!(out.content, "judge verdict");

        // Non-judge completions (simulator, optimizer) keep the space model.
        let out = AssessContext::complete(space.as_ref(), request())
            .await
            .unwrap();
        assert_eq!(out.content, "done");
    }

    /// Answers the scenario-mining call with a fixed valid scenario that
    /// deliberately contains PII the miner must scrub.
    #[derive(Debug)]
    struct MinerCompleter;

    impl CompletionFeaturesDyn for MinerCompleter {
        fn model_name(&self) -> String {
            "miner-test-model".to_string()
        }

        fn completion(&self, _req: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
            Box::pin(async move {
                let scenario = serde_json::json!({
                    "scenario": {
                        "id": "pref_fix",
                        "hidden_profile": {"contact": "work address"},
                        "timeline": [
                            {"turn": 1, "type": "normal",
                             "timestamp": "2026-06-01T10:00:00Z",
                             "user": "My email is bob@example.com and card 12345678901."},
                            {"turn": 2, "type": "normal",
                             "timestamp": "2026-06-05T10:00:00Z",
                             "user": "Correction: use my work address instead."},
                            {"turn": 3, "type": "maintenance",
                             "maintenance": {"trigger": "on_demand", "scope": "quick"}},
                            {"turn": 4, "type": "checkpoint_synthetic",
                             "timestamp": "2026-06-06T10:00:00Z",
                             "query": "Which contact should you use?",
                             "evaluation": {
                                 "scoring_rubric": "honor the correction",
                                 "required_answer_terms": ["work"],
                                 "forbidden_answer_terms": ["card"]
                             }}
                        ]
                    }
                });
                Ok(AgentOutput {
                    content: scenario.to_string(),
                    usage: Usage {
                        input_tokens: 30,
                        output_tokens: 15,
                        ..Default::default()
                    },
                    ..Default::default()
                })
            })
        }
    }

    #[tokio::test]
    async fn mine_scenarios_distills_corrections_and_scrubs_pii() {
        let models = Models::default();
        models.set_model(Model::with_completer(Arc::new(MinerCompleter)));
        let app = test_app_state_with_models("mine_corrections", Arc::new(models));
        let space = create_loaded_space(&app, "mine_corrections").await;
        let now_ms = unix_ms();
        let old_iso = anda_engine::rfc3339_datetime(now_ms - 86_400_000).unwrap();
        let ids = seed_topic_links(&space, &old_iso).await;

        // One corrected memory is the mining signal.
        space
            .ledger
            .record_correction(&ids[0], now_ms)
            .await
            .unwrap();

        let (mined, usage) = crate::eval::mine::mine_scenarios(
            space.as_ref(),
            &crate::eval::mine::MineConfig {
                since_ms: 0,
                max_scenarios: 4,
            },
        )
        .await
        .unwrap();

        assert_eq!(mined.len(), 1);
        assert_eq!(mined[0].signal, ids[0]);
        let scenario = &mined[0].scenario;
        assert_eq!(scenario.id, "mined_pref_fix");
        assert!(
            scenario
                .description
                .as_deref()
                .unwrap()
                .contains("review before adding")
        );
        // PII scrubbed from the produced scenario.
        let encoded = serde_json::to_string(scenario).unwrap();
        assert!(encoded.contains("[email]"), "{encoded}");
        assert!(encoded.contains("[number]"), "{encoded}");
        assert!(!encoded.contains("bob@example.com"));
        assert!(!encoded.contains("12345678901"));
        assert!(usage.input_tokens > 0);
    }

    #[tokio::test]
    async fn memory_status_aggregates_counters_and_schema_audit() {
        let app = test_app_state("memory_status");
        let space = create_loaded_space(&app, "memory_status").await;
        let now_ms = unix_ms();
        let old_iso = anda_engine::rfc3339_datetime(now_ms - 30 * 86_400_000).unwrap();
        let ids = seed_topic_links(&space, &old_iso).await;

        // Probe activity: one hit, one miss, one negative-cache hit.
        assert!(space.probe_memory("alpha", None).await.unwrap().found);
        assert!(!space.probe_memory("qqqzzz", None).await.unwrap().found);
        assert!(
            space
                .probe_memory("qqqzzz", None)
                .await
                .unwrap()
                .negative_cached
        );

        // One completed recall surfacing one entity.
        let message = serde_json::json!(Message {
            role: "assistant".to_string(),
            content: vec![
                anda_core::ContentPart::ToolCall {
                    name: "execute_kip_readonly".to_string(),
                    args: serde_json::json!({"command": "FIND"}),
                    call_id: Some("c1".to_string()),
                },
                anda_core::ContentPart::ToolOutput {
                    name: "execute_kip_readonly".to_string(),
                    output: serde_json::json!([{"id": ids[0]}]),
                    is_error: None,
                    call_id: Some("c1".to_string()),
                    remote_id: None,
                }
            ],
            ..Default::default()
        });
        space.record_recall_usage(&[message]).await.unwrap();

        // One correction + a full settlement (decay + schema audit).
        seed_kip(
            &space,
            &format!(
                "UPDATE ?link\nSET METADATA {{ superseded: true }}\nWHERE {{ ?link (id: \"{}\") }}",
                ids[1]
            ),
        )
        .await;
        space
            .settle_memory_metabolism(MaintenanceScope::Full, now_ms)
            .await
            .unwrap();

        let status = space.memory_status().await;
        assert_eq!(status.metrics.probe_hits, 1);
        assert_eq!(status.metrics.probe_misses, 1);
        assert_eq!(status.metrics.negative_cache_hits, 1);
        assert_eq!(status.metrics.recalls_completed, 1);
        assert_eq!(status.metrics.entities_recalled, 1);
        assert_eq!(status.metrics.corrections, 1);
        assert_eq!(status.metrics.reinforced, 1);
        assert_eq!(status.probe_hit_rate, Some(0.5));
        assert_eq!(status.correction_rate, Some(1.0));
        assert!(status.graph.concepts > 0);
        assert!(status.graph.predicate_types.unwrap_or(0) >= 1);
        assert!(status.last_settlement.is_some());

        // The full settlement also refreshed the per-predicate census.
        let audit = space.schema_audit().expect("schema audit stored");
        assert_eq!(audit.predicates.get("linked_to"), Some(&2));
    }

    /// Shadow judge: always votes for answer B — with deterministic A/B
    /// alternation this splits the wins 1:1, proving the swap works.
    #[derive(Debug)]
    struct ShadowJudgeCompleter;

    impl CompletionFeaturesDyn for ShadowJudgeCompleter {
        fn model_name(&self) -> String {
            "shadow-judge-model".to_string()
        }

        fn completion(&self, _req: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
            Box::pin(async move {
                Ok(AgentOutput {
                    content: serde_json::json!({"winner": "b", "reason": "richer"}).to_string(),
                    usage: Usage {
                        input_tokens: 5,
                        output_tokens: 2,
                        ..Default::default()
                    },
                    ..Default::default()
                })
            })
        }
    }

    #[tokio::test]
    async fn shadow_eval_compares_policies_without_touching_live_space() {
        let app = test_app_state_with_final_model("shadow_eval");
        let space = create_loaded_space(&app, "shadow_eval").await;
        space.set_judge_model_for_test(Model::with_completer(Arc::new(ShadowJudgeCompleter)));

        // Two completed recall conversations: one stores a serialized
        // RecallInput, one a raw query string.
        for (id_suffix, prompt) in [
            (1u64, r#"{"query": "What tea do I drink?"}"#),
            (2u64, "Where do I work?"),
        ] {
            let now = unix_ms() + id_suffix;
            let conversation = Conversation {
                user: SELF_USER_ID,
                status: ConversationStatus::Completed,
                messages: vec![serde_json::json!(Message {
                    role: "user".to_string(),
                    content: vec![prompt.to_string().into()],
                    ..Default::default()
                })],
                label: Some("recall".to_string()),
                created_at: now,
                updated_at: now,
                ..Default::default()
            };
            space
                .recall
                .conversations
                .add_conversation(ConversationRef::from(&conversation))
                .await
                .unwrap();
        }

        let candidate = crate::types::MemoryPolicy {
            confidence_decay_factor: 0.9,
            ..Default::default()
        };
        let report = app
            .run_shadow_eval(
                "shadow_eval",
                crate::types::ShadowEvalInput {
                    policy: candidate.clone(),
                    replay_sample: Some(2),
                },
            )
            .await
            .unwrap();

        assert_eq!(report.replayed, 2, "{report:?}");
        assert_eq!(report.judge_errors, 0, "{report:?}");
        // The judge always votes "B"; the deterministic order alternation
        // maps that to one win per side.
        assert_eq!(report.candidate_wins, 1, "{report:?}");
        assert_eq!(report.baseline_wins, 1, "{report:?}");
        assert_eq!(report.samples.len(), 2);
        assert_eq!(report.candidate_policy.confidence_decay_factor, 0.9);

        // The report persists on the live space...
        let stored: crate::types::ShadowReport = space
            .db
            .get_extension_as("shadow_report")
            .expect("report stored");
        assert_eq!(stored.replayed, 2);
        // ...while the live space itself stayed untouched: no policy change,
        // no usage recorded by the fork replays (plan guardrail 4).
        assert_eq!(space.memory_policy(), crate::types::MemoryPolicy::default());
        assert_eq!(space.memory_status().await.metrics.recalls_completed, 0);
    }

    #[tokio::test]
    async fn space_token_limit_and_tier_node_limit_are_enforced() {
        let app = test_app_state("space_limits");
        let space = create_loaded_space(&app, "space_limits").await;
        space.admin_update_tier(0, 1).await.unwrap();

        for idx in 0..100 {
            space
                .add_space_token(
                    format!("STlimit-{idx}"),
                    AddSpaceTokenInput {
                        scope: TokenScope::Read,
                        name: format!("reader-{idx}"),
                        expires_at: None,
                        labels: None,
                    },
                    idx,
                )
                .await
                .unwrap();
        }
        let err = space
            .add_space_token(
                "STlimit-overflow".to_string(),
                AddSpaceTokenInput {
                    scope: TokenScope::Read,
                    name: "overflow".to_string(),
                    expires_at: None,
                    labels: None,
                },
                101,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("space token limit reached"));

        for idx in 0..101 {
            let conversation = Conversation {
                user: SELF_USER_ID,
                status: ConversationStatus::Completed,
                created_at: idx,
                updated_at: idx,
                label: Some("formation".to_string()),
                ..Default::default()
            };
            space
                .memory
                .add_conversation(ConversationRef::from(&conversation))
                .await
                .unwrap();
        }
        let err = space
            .ingest(
                SELF_USER_ID,
                StringOr::Value(FormationInput {
                    messages: vec![],
                    context: None,
                    timestamp: None,
                }),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("node limit exceeded"));
    }

    #[tokio::test]
    async fn space_conversations_are_accessible_across_collections() {
        let app = test_app_state("space_conversations");
        let space = create_loaded_space(&app, "space_conversations").await;
        let now = unix_ms();

        let formation = Conversation {
            user: SELF_USER_ID,
            status: ConversationStatus::Completed,
            created_at: now,
            updated_at: now,
            label: Some("formation".to_string()),
            ..Default::default()
        };
        let recall = Conversation {
            user: SELF_USER_ID,
            status: ConversationStatus::Completed,
            created_at: now + 1,
            updated_at: now + 1,
            label: Some("recall".to_string()),
            ..Default::default()
        };
        let maintenance = Conversation {
            user: SELF_USER_ID,
            status: ConversationStatus::Completed,
            created_at: now + 2,
            updated_at: now + 2,
            label: Some("maintenance".to_string()),
            ..Default::default()
        };

        let formation_id = space
            .memory
            .add_conversation(ConversationRef::from(&formation))
            .await
            .unwrap();
        let recall_id = space
            .recall
            .conversations
            .add_conversation(ConversationRef::from(&recall))
            .await
            .unwrap();
        let maintenance_id = space
            .maintenance
            .conversations
            .add_conversation(ConversationRef::from(&maintenance))
            .await
            .unwrap();

        assert_eq!(
            space
                .get_conversation(None, formation_id)
                .await
                .unwrap()
                .label,
            Some("formation".to_string())
        );
        assert_eq!(
            space
                .get_conversation(Some("recall".to_string()), recall_id)
                .await
                .unwrap()
                .label,
            Some("recall".to_string())
        );
        assert_eq!(
            space
                .get_conversation(Some("maintenance".to_string()), maintenance_id)
                .await
                .unwrap()
                .label,
            Some("maintenance".to_string())
        );

        let (items, cursor) = space.list_conversations(None, None, Some(1)).await.unwrap();
        assert_eq!(items.len(), 1);
        assert!(cursor.is_some());

        let (recall_items, _) = space
            .list_conversations(Some("recall".to_string()), None, Some(10))
            .await
            .unwrap();
        assert_eq!(recall_items.len(), 1);

        let status = space.formation_status();
        assert_eq!(status.conversations, 1);
        assert!(!status.formation_processing);
        assert!(!status.maintenance_processing);

        assert!(
            space
                .list_conversations(None, Some("not-a-cursor".to_string()), Some(1))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn list_conversations_clamps_limit_to_safe_bounds() {
        let app = test_app_state("list_limit_clamp");
        let space = create_loaded_space(&app, "list_limit_clamp").await;

        // limit=0 on an empty collection must not panic on the cursor below.
        let (items, cursor) = space.list_conversations(None, None, Some(0)).await.unwrap();
        assert!(items.is_empty());
        assert!(cursor.is_none());

        for idx in 0..3 {
            let conversation = Conversation {
                user: SELF_USER_ID,
                status: ConversationStatus::Completed,
                created_at: idx,
                updated_at: idx,
                label: Some("formation".to_string()),
                ..Default::default()
            };
            space
                .memory
                .add_conversation(ConversationRef::from(&conversation))
                .await
                .unwrap();
        }

        // limit=0 is clamped to 1 instead of dumping the whole collection.
        let (items, cursor) = space.list_conversations(None, None, Some(0)).await.unwrap();
        assert_eq!(items.len(), 1);
        assert!(cursor.is_some());
    }

    #[tokio::test]
    async fn space_agent_entrypoints_use_memory_and_model_without_network() {
        let app = test_app_state_with_final_model("space_agent_entrypoints");
        let space = create_loaded_space(&app, "space_agent_entrypoints").await;

        let formation = FormationInput {
            messages: vec![Message {
                role: "user".to_string(),
                content: vec![
                    "remember that the preferred color is blue"
                        .to_string()
                        .into(),
                ],
                ..Default::default()
            }],
            context: Some(InputContext {
                counterparty: Some("external-user-formation".to_string()),
                agent: Some("agent-a".to_string()),
                source: Some("thread-1".to_string()),
                topic: Some("preferences".to_string()),
            }),
            timestamp: Some("2026-06-05T00:00:00Z".to_string()),
        };
        let formation_output = space
            .ingest(SELF_USER_ID, StringOr::Value(formation))
            .await
            .unwrap();
        let formation_id = formation_output.conversation.unwrap();
        wait_until_idle(&space).await;

        let formation_conversation = space.get_conversation(None, formation_id).await.unwrap();
        assert_eq!(formation_conversation.status, ConversationStatus::Completed);
        assert_eq!(space.formation.get_processed(), Some(formation_id));

        let counterparty = space
            .formation
            .get_or_init_counterparty(
                "external-user-formation".to_string(),
                Some("Formation User".to_string()),
            )
            .await
            .unwrap();
        assert_eq!(counterparty["type"], "Person");
        assert!(counterparty.to_string().contains("external-user-formation"));

        let recall = RecallInput {
            query: "What color is preferred?".to_string(),
            context: Some(InputContext {
                counterparty: Some("external-user-formation".to_string()),
                agent: None,
                source: None,
                topic: Some("preferences".to_string()),
            }),
        };
        let recall_output = space
            .query(SELF_USER_ID, StringOr::Value(recall))
            .await
            .unwrap();
        let recall_id = recall_output.conversation.unwrap();
        let recall_conversation = space
            .get_conversation(Some("recall".to_string()), recall_id)
            .await
            .unwrap();
        assert_eq!(recall_conversation.status, ConversationStatus::Completed);

        let maintenance_output = space
            .maintenance(
                SELF_USER_ID,
                MaintenanceInput {
                    scope: MaintenanceScope::Quick,
                    formation_id,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(maintenance_output.conversation.is_some());
        wait_until_idle(&space).await;
        assert_eq!(space.maintenance.get_processed_at().quick, formation_id);
        space
            .maintenance
            .set_processed_at(MaintenanceScope::Full, formation_id + 1)
            .await
            .unwrap();
        space
            .maintenance
            .set_processed_at(MaintenanceScope::Daydream, formation_id + 2)
            .await
            .unwrap();
        let maintenance_at = space.maintenance.get_processed_at();
        assert_eq!(maintenance_at.full, formation_id + 1);
        assert_eq!(maintenance_at.daydream, formation_id + 2);

        let kip = space
            .execute_kip_readonly(anda_kip::Request {
                command: "DESCRIBE PRIMER".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(!serde_json::to_value(kip).unwrap().is_null());

        let restart_err = space
            .restart_formation(SELF_USER_ID, formation_id + 1)
            .await
            .unwrap_err();
        assert!(
            restart_err
                .to_string()
                .contains("No pending formation conversation")
        );
    }

    #[tokio::test]
    async fn space_agent_guards_and_readonly_tool_paths() {
        let app = test_app_state_with_final_model("space_agent_guards");
        let space = create_loaded_space(&app, "space_agent_guards").await;

        let readonly = TimedMemoryReadonly::new(space.memory.clone());
        assert_eq!(Tool::<BaseCtx>::name(&readonly), MemoryReadonly::NAME);
        assert_eq!(Tool::<BaseCtx>::definition(&readonly).strict, Some(true));

        let ok_ctx = space
            .engine
            .base_ctx_with(
                SELF_USER_ID,
                "recall_memory",
                MemoryReadonly::NAME,
                Default::default(),
            )
            .unwrap();
        let ok = Tool::<BaseCtx>::call(
            &readonly,
            ok_ctx,
            anda_kip::Request {
                command: "DESCRIBE PRIMER".to_string(),
                ..Default::default()
            },
            vec![],
        )
        .await
        .unwrap();
        assert_eq!(ok.is_error, None);

        let err_ctx = space
            .engine
            .base_ctx_with(
                SELF_USER_ID,
                "recall_memory",
                MemoryReadonly::NAME,
                Default::default(),
            )
            .unwrap();
        let err = Tool::<BaseCtx>::call(
            &readonly,
            err_ctx,
            anda_kip::Request {
                command: "NOT A VALID KIP COMMAND".to_string(),
                ..Default::default()
            },
            vec![],
        )
        .await
        .unwrap();
        assert_eq!(err.is_error, Some(true));
    }

    #[tokio::test]
    async fn maintenance_rejects_concurrent_runs() {
        let app = test_app_state_with_slow_model("maintenance_concurrent");
        let space = create_loaded_space(&app, "maintenance_concurrent").await;

        let first = space
            .maintenance(
                SELF_USER_ID,
                MaintenanceInput {
                    scope: MaintenanceScope::Quick,
                    formation_id: 1,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(first.conversation.is_some());

        let second = space
            .maintenance(
                SELF_USER_ID,
                MaintenanceInput {
                    scope: MaintenanceScope::Quick,
                    formation_id: 2,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(second.content.contains("already in progress"));

        wait_until_idle(&space).await;
    }

    #[tokio::test]
    async fn hooks_handle_unbound_space_and_accumulate_usage() {
        let app = test_app_state_with_final_model("hooks_usage");
        let space = create_loaded_space(&app, "hooks_usage").await;
        let unbound = Hooks::new(space.db.clone());

        assert!(!BrainHook::is_maintenance_processing(&unbound));
        BrainHook::try_start_formation(&unbound).await;
        assert!(
            BrainHook::try_start_maintenance(&unbound, 168)
                .await
                .is_none()
        );

        let hooks = Hooks::new(space.db.clone());
        hooks.bind_space(Arc::downgrade(&space));
        assert!(!BrainHook::is_maintenance_processing(&hooks));
        space
            .memory
            .conversations
            .save_extension("brain_processed".to_string(), 7_u64.into())
            .await
            .unwrap();
        BrainHook::try_start_formation(&hooks).await;

        let conversation = Conversation {
            usage: Usage {
                input_tokens: 11,
                output_tokens: 7,
                cached_tokens: 3,
                requests: 2,
            },
            ..Default::default()
        };

        BrainHook::on_conversation_end(&hooks, "recall_memory", &conversation).await;
        BrainHook::on_conversation_end(&hooks, "formation_memory", &conversation).await;
        BrainHook::on_conversation_end(&hooks, "maintenance_memory", &conversation).await;
        BrainHook::on_conversation_end(&hooks, "unknown_agent", &conversation).await;

        let info = space.get_info();
        assert_eq!(info.recall_usage.requests, 2);
        assert_eq!(info.formation_usage.input_tokens, 11);
        assert_eq!(info.maintenance_usage.output_tokens, 7);
        assert_eq!(info.maintenance_usage.cached_tokens, 3);
    }

    #[tokio::test]
    async fn hooks_schedule_maintenance_at_thresholds() {
        let app = test_app_state_with_final_model("hooks_thresholds");
        let space = create_loaded_space(&app, "hooks_thresholds").await;
        let hooks = Hooks::new(space.db.clone());
        hooks.bind_space(Arc::downgrade(&space));

        assert!(BrainHook::try_start_maintenance(&hooks, 20).await.is_none());

        space
            .memory
            .conversations
            .save_extension("brain_processed".to_string(), 21_u64.into())
            .await
            .unwrap();
        let daydream = BrainHook::try_start_maintenance(&hooks, 21).await.unwrap();
        wait_until_idle(&space).await;
        assert_eq!(space.maintenance_for_test().get_processed_at().daydream, 21);

        space
            .memory
            .conversations
            .save_extension("brain_processed".to_string(), 42_u64.into())
            .await
            .unwrap();
        let quick = BrainHook::try_start_maintenance(&hooks, 42).await.unwrap();
        wait_until_idle(&space).await;
        assert!(quick > daydream);
        assert_eq!(space.maintenance_for_test().get_processed_at().quick, 42);

        space
            .memory
            .conversations
            .save_extension("brain_processed".to_string(), 168_u64.into())
            .await
            .unwrap();
        let full = BrainHook::try_start_maintenance(&hooks, 168).await.unwrap();
        wait_until_idle(&space).await;
        assert!(full > quick);
        assert_eq!(space.maintenance_for_test().get_processed_at().full, 168);
    }

    /// M4 acceptance: an exported OKF bundle plus its manifest replays into
    /// an empty space with every document checksum intact.
    #[tokio::test]
    async fn wiki_export_bundle_replays_into_empty_space() {
        use crate::wiki::{WikiBundleEntry, WikiCommitInput, WikiImportInput};

        let app = test_app_state("wiki_replay_src");
        let source = create_loaded_space(&app, "wiki_replay_source").await;
        for (title, body) in [
            ("部署指南", "# 部署指南\n\n回滚使用上一版本快照。\n"),
            ("安全政策", "# 安全政策\n\n密钥必须存放在 KMS。\n"),
        ] {
            let mut input = WikiCommitInput {
                title: title.to_string(),
                content: body.to_string(),
                ..Default::default()
            };
            input.namespace = Some("kb".to_string());
            source
                .wiki
                .commit("op".to_string(), input, unix_ms())
                .await
                .unwrap();
        }
        let export = source
            .wiki
            .export_bundle("op".to_string(), Some("kb".to_string()), unix_ms())
            .await
            .unwrap();
        let manifest: serde_json::Value = serde_json::from_str(
            &export
                .entries
                .iter()
                .find(|e| e.path == "manifest.json")
                .unwrap()
                .content,
        )
        .unwrap();

        // Replay into a brand-new space.
        let replay_app = test_app_state("wiki_replay_dst");
        let target = create_loaded_space(&replay_app, "wiki_replay_target").await;
        let entries: Vec<WikiBundleEntry> = export
            .entries
            .iter()
            .filter(|e| e.path.ends_with(".md"))
            .cloned()
            .collect();
        let imported = target
            .wiki
            .import_bundle(
                "op".to_string(),
                WikiImportInput {
                    entries,
                    namespace: Some("kb".to_string()),
                },
                unix_ms(),
            )
            .await
            .unwrap();
        assert_eq!(imported.created, export.docs);

        // Every replayed document matches the manifest checksum: the bundle
        // is a faithful backup.
        for doc in manifest["docs"].as_array().unwrap() {
            let path = doc["path"].as_str().unwrap();
            let checksum = doc["checksum"].as_str().unwrap();
            let restored = imported
                .docs
                .iter()
                .find(|d| d.path == path)
                .unwrap_or_else(|| panic!("missing {path}"));
            let info = target.wiki.get_doc(restored.doc_id).await.unwrap();
            assert_eq!(info.current_checksum, checksum, "checksum drift for {path}");
        }

        // SpaceInfo exposes the M4 wiki metrics.
        let info = target.get_info();
        assert_eq!(info.wiki_docs, export.docs);
        assert!(info.wiki_versions >= export.docs);
    }

    /// Replays scripted completion responses in order; used to drive the
    /// wiki digest extraction deterministically.
    #[derive(Debug)]
    struct ScriptedCompleter(std::sync::Mutex<std::collections::VecDeque<String>>);

    impl CompletionFeaturesDyn for ScriptedCompleter {
        fn model_name(&self) -> String {
            "scripted-test-model".to_string()
        }

        fn completion(&self, _req: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
            let next = self
                .0
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| r#"{"facts": []}"#.to_string());
            Box::pin(async move {
                Ok(AgentOutput {
                    content: next,
                    ..Default::default()
                })
            })
        }
    }

    #[tokio::test]
    async fn wiki_digest_extracts_supersedes_and_verifies() {
        use crate::wiki::WikiCommitInput;
        use anda_cognitive_nexus::ConceptPK;
        use anda_kip::parse_kql;

        let extraction_v1 = serde_json::json!({
            "concepts": [
                {"type": "Organization", "name": "Acme", "attributes": {"description": "发布政策的组织"}}
            ],
            "facts": [
                {
                    "subject": {"type": "Organization", "name": "Acme"},
                    "predicate": "publishes",
                    "object": {"type": "Policy", "name": "安全政策"},
                    "confidence": 0.95,
                    "anchor": "安全政策-0"
                },
                {
                    "subject": {"type": "Policy", "name": "安全政策"},
                    "predicate": "requires",
                    "object": {"type": "Procedure", "name": "密钥轮换"},
                    "confidence": 0.9,
                    "anchor": "no-such-anchor"
                }
            ]
        })
        .to_string();
        let extraction_v2 = serde_json::json!({
            "facts": [
                {
                    "subject": {"type": "Organization", "name": "Acme"},
                    "predicate": "publishes",
                    "object": {"type": "Policy", "name": "安全政策"},
                    "confidence": 0.95,
                    "anchor": "安全政策-0"
                },
                {
                    "subject": {"type": "Policy", "name": "安全政策"},
                    "predicate": "requires",
                    "object": {"type": "Procedure", "name": "双因素认证"},
                    "confidence": 0.9,
                    "anchor": "安全政策-0"
                }
            ]
        })
        .to_string();

        let models = Models::default();
        models.set_model(Model::with_completer(Arc::new(ScriptedCompleter(
            std::sync::Mutex::new([extraction_v1, extraction_v2].into_iter().collect()),
        ))));
        let app = test_app_state_with_models("wiki_digest_app", Arc::new(models));
        let space = create_loaded_space(&app, "wiki_digest_space").await;

        // RecallAgent exposes the wiki evidence tools to its LLM loop.
        {
            use anda_core::Agent;
            let deps = space.recall.tool_dependencies();
            assert!(deps.contains(&"wiki_search".to_string()));
            assert!(deps.contains(&"wiki_read".to_string()));
        }

        // Digest is opt-in: disabled spaces refuse to run.
        let err = space.run_wiki_digest(SELF_USER_ID).await.unwrap_err();
        assert!(err.to_string().contains("disabled"));
        space
            .update(
                crate::types::UpdateSpaceInput {
                    wiki_digest: Some(true),
                    ..Default::default()
                },
                unix_ms(),
            )
            .await
            .unwrap();
        assert!(space.wiki_digest_enabled());

        let v1 = space
            .wiki
            .commit(
                "tester".to_string(),
                WikiCommitInput {
                    title: "安全政策".to_string(),
                    content: "# 安全政策\n\n所有系统必须启用密钥轮换。\n".to_string(),
                    ..Default::default()
                },
                unix_ms(),
            )
            .await
            .unwrap();

        let report = space.run_wiki_digest(SELF_USER_ID).await.unwrap();
        assert_eq!(report.digested, 1);
        assert_eq!(report.facts, 2);
        assert_eq!(report.superseded, 0);
        assert!(report.citations_checked >= 2);
        assert_eq!(report.citations_invalid, 0);

        // The graph now holds the concepts and a proposition whose metadata
        // carries the wiki citation and extractor fingerprint.
        assert!(
            space
                .memory
                .nexus
                .has_concept(&ConceptPK::Object {
                    r#type: "Organization".to_string(),
                    name: "Acme".to_string(),
                })
                .await
        );
        let (meta, _) = space
            .memory
            .nexus
            .execute_kql(
                parse_kql(
                    "FIND(?link.metadata) WHERE { ?link ({type: \"Organization\", name: \"Acme\"}, \"publishes\", {type: \"Policy\", name: \"安全政策\"}) }",
                )
                .unwrap(),
            )
            .await
            .unwrap();
        let meta_text = meta.to_string();
        assert!(meta_text.contains("wiki://"), "metadata: {meta_text}");
        assert!(
            meta_text.contains("wiki_digest@v1"),
            "metadata: {meta_text}"
        );
        assert!(
            meta_text.contains(&format!("@{}", v1.version.id)),
            "citation should pin version {}: {meta_text}",
            v1.version.id
        );

        // Digest ledger event recorded with both facts.
        let events = space
            .wiki
            .list_events(Some("DigestExtracted".to_string()), None, None, Some(10))
            .await
            .unwrap();
        assert_eq!(events.events.len(), 1);

        // No pending versions: the next run is a no-op (cursor advanced).
        let report = space.run_wiki_digest(SELF_USER_ID).await.unwrap();
        assert_eq!(report.digested, 0);

        // Revision drops the 密钥轮换 requirement; digesting it must mark the
        // stale proposition superseded while the surviving fact stays live.
        let v2 = space
            .wiki
            .commit(
                "tester".to_string(),
                WikiCommitInput {
                    doc_id: Some(v1.doc.id),
                    parent_version: Some(v1.version.id),
                    title: "安全政策".to_string(),
                    content: "# 安全政策\n\n所有系统必须启用双因素认证。\n".to_string(),
                    ..Default::default()
                },
                unix_ms(),
            )
            .await
            .unwrap();
        let report = space.run_wiki_digest(SELF_USER_ID).await.unwrap();
        assert_eq!(report.digested, 1);
        assert_eq!(report.superseded, 1);

        let (stale, _) = space
            .memory
            .nexus
            .execute_kql(
                parse_kql(
                    "FIND(?link.metadata) WHERE { ?link ({type: \"Policy\", name: \"安全政策\"}, \"requires\", {type: \"Procedure\", name: \"密钥轮换\"}) }",
                )
                .unwrap(),
            )
            .await
            .unwrap();
        let stale_text = stale.to_string();
        assert!(stale_text.contains("superseded"), "stale: {stale_text}");
        assert!(
            stale_text.contains(&format!("@{}", v2.version.id)),
            "superseded_by should pin version {}: {stale_text}",
            v2.version.id
        );
        let (live, _) = space
            .memory
            .nexus
            .execute_kql(
                parse_kql(
                    "FIND(?link.metadata) WHERE { ?link ({type: \"Organization\", name: \"Acme\"}, \"publishes\", {type: \"Policy\", name: \"安全政策\"}) }",
                )
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(!live.to_string().contains("superseded"));
    }
}

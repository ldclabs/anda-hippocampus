use anda_cognitive_nexus::{CognitiveNexus, ConceptPK};
use anda_core::{
    AgentInput, AgentOutput, BoxError, FunctionDefinition, Principal, Resource, Usage,
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
    memory::{Conversation, Conversations, MemoryManagement, MemoryTool},
    model::{ModelConfig as EngineModelConfig, Models, reqwest},
    rfc3339_datetime_now, unix_ms,
};
use anda_kip::{
    KipError, KipErrorCode, META_SELF_NAME, PERSON_SELF_KIP, PERSON_SYSTEM_KIP, PERSON_TYPE,
    parse_kml,
};
use ic_auth_types::ByteBufB64;
use ic_cose_types::cose::{
    SIGN1_TAG, cwt::cwt_from, ed25519::VerifyingKey, sign1::cose_sign1_from, skip_prefix,
};
use object_store::ObjectStore;
use serde_json::json;
use std::{
    collections::BTreeMap,
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
    payload::StringOr,
    types::{
        AddSpaceTokenInput, CWToken, FormationInput, FormationStatus, MaintenanceInput,
        MaintenanceScope, ModelConfig, RecallInput, SpaceInfo, SpaceTier, SpaceToken, TokenScope,
        UpdateSpaceInput,
    },
    wiki::{WikiCommitTool, WikiReadTool, WikiSearchTool, WikiService},
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
            app_name,
            app_version,
            sharding,
        }
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
                Space::connect(
                    self.object_store.clone(),
                    db_config,
                    self.management.clone(),
                    self.http_client.clone(),
                    self.models.clone(),
                    pinned,
                )
                .await
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
    pub formation: Arc<FormationAgent>,
    pub recall: Arc<RecallAgent>,
    pub db: Arc<AndaDB>,
    pub memory: Arc<MemoryManagement>,
    pub wiki: Arc<WikiService>,
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
        if changed {
            self.db.flush_metadata(now_ms).await?;
        }
        Ok(())
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

    pub async fn query(
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

    pub async fn maintenance(
        &self,
        user: Principal,
        mut input: MaintenanceInput,
    ) -> Result<AgentOutput, BoxError> {
        input.formation_id = self.formation.get_processed().unwrap_or_default();
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
        let this = Arc::new(Self {
            id,
            db: db.clone(),
            http_client,
            models,
            formation,
            recall,
            maintenance,
            memory,
            wiki,
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
            // Resume formation if it was interrupted before. A missing marker
            // means nothing was processed yet, so resume from the beginning.
            let conversation = this_clone.formation.get_processed().unwrap_or_default();
            let _ = this_clone
                .restart_formation(SELF_USER_ID, conversation + 1)
                .await;
        });

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
            }
            "maintenance_memory" => {
                let _ = self
                    .db
                    .set_extension_from_with("maintenance_usage".to_string(), |v| {
                        let mut usage: Usage = v.unwrap_or_default();
                        usage.accumulate(&conversation.usage);
                        Some(usage)
                    });
            }
            "formation_memory" => {
                let _ = self
                    .db
                    .set_extension_from_with("formation_usage".to_string(), |v| {
                        let mut usage: Usage = v.unwrap_or_default();
                        usage.accumulate(&conversation.usage);
                        Some(usage)
                    });
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
            AddSpaceTokenInput, FormationInput, InputContext, MaintenanceInput, MaintenanceScope,
            ModelConfig, RecallInput, SpaceTier, TokenScope, UpdateSpaceInput,
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
                    description: None,
                    public: None,
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
                description: None,
                public: None,
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
                    name: None,
                    description: None,
                    public: None,
                },
                3000,
            )
            .await
            .unwrap();
        assert!(space.get_byok().is_some());
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
}

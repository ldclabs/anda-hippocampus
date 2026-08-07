use anda_core::{BoxError, ModelEffort, Principal, Usage, model::Message};
use anda_db::storage::StorageStats;
use anda_engine::model::ModelConfig as EngineModelConfig;
use ic_cose_types::cose::cwt::{ClaimsSet, get_scope};
#[cfg(feature = "mcp")]
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, de};
use std::{collections::BTreeMap, str::FromStr};

#[derive(Deserialize)]
pub struct Pagination {
    pub cursor: Option<String>,
    pub limit: Option<usize>,
    /// Conversation collection: "formation" (default), "recall", "maintenance".
    pub collection: Option<String>,
}

#[derive(Deserialize)]
pub struct ConversationDeltaQuery {
    pub messages_offset: Option<usize>,
    pub artifacts_offset: Option<usize>,
    /// Conversation collection: "formation" (default), "recall", "maintenance".
    pub collection: Option<String>,
}

#[derive(Debug, Default, Deserialize, Serialize, Clone)]
pub struct SpaceInfo {
    pub id: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub owner: String,
    pub db_stats: StorageStats,
    pub concepts: usize,
    pub propositions: usize,
    pub conversations: usize,
    pub public: bool,
    pub tier: SpaceTier,
    pub formation_usage: Usage,
    pub recall_usage: Usage,
    pub maintenance_usage: Usage,
    pub formation_processed_id: u64,
    pub maintenance_processed_id: u64,
    pub maintenance_at: MaintenanceAt,
    #[cfg(feature = "wiki")]
    #[serde(default)]
    pub wiki_docs: usize,
    #[cfg(feature = "wiki")]
    #[serde(default)]
    pub wiki_chunks: usize,
    #[cfg(feature = "wiki")]
    #[serde(default)]
    pub wiki_versions: usize,
    #[cfg(feature = "wiki")]
    #[serde(default)]
    pub wiki_queries: u64,
    /// Digest high-water mark (largest digested wiki version id).
    #[cfg(feature = "wiki")]
    #[serde(default)]
    pub wiki_digested: u64,
    /// From the last housekeeping stale scan.
    #[cfg(feature = "wiki")]
    #[serde(default)]
    pub wiki_stale_docs: u64,
}

#[derive(Debug, Default, Deserialize, Serialize, Clone)]
pub struct FormationStatus {
    pub id: String,
    pub concepts: usize,
    pub propositions: usize,
    pub conversations: usize,
    pub formation_processing: bool,
    pub maintenance_processing: bool,
    pub formation_processed_id: u64,
    pub maintenance_processed_id: u64,
    pub maintenance_at: MaintenanceAt,
}

pub struct CWToken {
    pub user: Principal,
    pub audience: String,
    pub scope: TokenScope,
}

impl CWToken {
    pub fn from_claims(claims: ClaimsSet) -> Result<Self, BoxError> {
        let scope = TokenScope::from_str(&get_scope(&claims).unwrap_or_default())?;
        let user = claims
            .subject
            .ok_or("missing 'sub' claim")?
            .parse::<Principal>()
            .map_err(|_| "invalid 'sub' claim")?;

        let audience = claims.audience.unwrap_or_default();
        Ok(Self {
            user,
            audience,
            scope,
        })
    }
}

#[derive(Debug, Default, Deserialize, Serialize, Clone)]
pub struct ModelConfig {
    // "gemini", "anthropic", "openai", "deepseek" etc.
    #[serde(alias = "f")]
    pub family: String,

    #[serde(alias = "m")]
    pub model: String,

    #[serde(alias = "ab")]
    pub api_base: String,

    #[serde(alias = "ak")]
    pub api_key: String,

    #[serde(default, alias = "d")]
    pub disabled: bool,

    #[serde(default, alias = "l")]
    pub label: Option<String>,

    #[serde(default, alias = "e")]
    pub effort: Option<ModelEffort>,

    #[serde(default, alias = "b")]
    pub bearer_auth: bool,

    #[serde(default, alias = "s")]
    pub stream: bool,

    #[serde(default, alias = "cw")]
    pub context_window: usize,

    #[serde(default, alias = "mo")]
    pub max_output: usize,
}

#[derive(Debug, Serialize, Clone)]
pub struct ModelConfigRef<'a> {
    #[serde(rename = "f")]
    pub family: &'a str,

    #[serde(rename = "m")]
    pub model: &'a str,

    #[serde(rename = "ab")]
    pub api_base: &'a str,

    #[serde(rename = "ak")]
    pub api_key: &'a str,

    #[serde(rename = "d")]
    pub disabled: bool,

    #[serde(rename = "l")]
    pub label: &'a Option<String>,

    #[serde(rename = "e")]
    pub effort: Option<ModelEffort>,

    #[serde(rename = "b")]
    pub bearer_auth: bool,

    #[serde(rename = "s")]
    pub stream: bool,

    #[serde(default, rename = "cw")]
    pub context_window: usize,

    #[serde(default, rename = "mo")]
    pub max_output: usize,
}

impl ModelConfig {
    pub fn to_ref<'a>(&'a self) -> ModelConfigRef<'a> {
        ModelConfigRef {
            family: &self.family,
            model: &self.model,
            api_base: &self.api_base,
            api_key: &self.api_key,
            disabled: self.disabled,
            label: &self.label,
            effort: self.effort,
            bearer_auth: self.bearer_auth,
            stream: self.stream,
            context_window: self.context_window,
            max_output: self.max_output,
        }
    }
}

impl From<ModelConfig> for EngineModelConfig {
    fn from(config: ModelConfig) -> Self {
        EngineModelConfig {
            family: config.family,
            model: config.model,
            api_base: config.api_base,
            api_key: config.api_key,
            disabled: config.disabled,
            labels: config.label.map(|l| vec![l]).unwrap_or_default(),
            effort: config.effort,
            bearer_auth: config.bearer_auth,
            stream: config.stream,
            context_window: config.context_window,
            max_output: config.max_output,
        }
    }
}

#[derive(Debug, Default, Deserialize, Serialize, Clone)]
pub struct SpaceTier {
    #[serde(default, alias = "t")]
    pub tier: u32,

    #[serde(default, alias = "u")]
    pub updated_at: u64,
}

impl SpaceTier {
    pub fn to_ref(&self) -> SpaceTierRef {
        SpaceTierRef {
            tier: self.tier,
            updated_at: self.updated_at,
        }
    }

    // tier 0 (free) allows 100 nodes, tier 1 allows 1k, etc.
    pub fn allow_nodes(&self) -> u64 {
        self.tier
            .checked_add(2)
            .and_then(|exponent| 10u64.checked_pow(exponent))
            .unwrap_or(u64::MAX)
    }
}

#[derive(Debug, Serialize, Clone)]
pub struct SpaceTierRef {
    #[serde(rename = "t", alias = "tier")]
    pub tier: u32,
    #[serde(rename = "u", alias = "updated_at")]
    pub updated_at: u64,
}

#[derive(Debug, Default, Deserialize, Serialize, Clone)]
pub struct SpaceToken {
    #[serde(default, alias = "n")]
    pub name: String,

    #[serde(default)]
    pub token: String,

    #[serde(alias = "s")]
    pub scope: TokenScope,

    #[serde(default, alias = "u")]
    pub usage: u64,

    #[serde(default, alias = "ca")]
    pub created_at: u64,

    #[serde(default, alias = "ua")]
    pub updated_at: u64,

    #[serde(default, alias = "ea")]
    pub expires_at: Option<u64>,

    /// Wiki ACL labels this token may read (None = unrestricted).
    #[serde(default, alias = "lb")]
    pub labels: Option<Vec<String>>,
}

impl SpaceToken {
    pub fn to_ref<'a>(&'a self) -> SpaceTokenRef<'a> {
        SpaceTokenRef {
            name: &self.name,
            scope: &self.scope,
            usage: self.usage,
            created_at: self.created_at,
            updated_at: self.updated_at,
            expires_at: self.expires_at,
            labels: &self.labels,
        }
    }
}

#[derive(Debug, Serialize, Clone)]
pub struct SpaceTokenRef<'a> {
    #[serde(rename = "n", alias = "name")]
    pub name: &'a str,
    #[serde(rename = "s", alias = "scope")]
    pub scope: &'a TokenScope,
    #[serde(rename = "u", alias = "usage")]
    pub usage: u64,
    #[serde(rename = "ca", alias = "created_at")]
    pub created_at: u64,
    #[serde(rename = "ua", alias = "updated_at")]
    pub updated_at: u64,
    #[serde(rename = "ea", alias = "expires_at")]
    pub expires_at: Option<u64>,
    #[serde(
        rename = "lb",
        alias = "labels",
        skip_serializing_if = "Option::is_none"
    )]
    pub labels: &'a Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq, Default)]
pub enum TokenScope {
    #[serde(rename = "read")]
    #[default]
    Read,
    #[serde(rename = "write")]
    Write,
    #[serde(rename = "*")]
    All,
}

impl TokenScope {
    pub fn allows(&self, required: Self) -> bool {
        *self == Self::All || *self == required
    }
}

impl FromStr for TokenScope {
    type Err = BoxError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "read" => Ok(Self::Read),
            "write" => Ok(Self::Write),
            "*" => Ok(Self::All),
            _ => Err("invalid scope".into()),
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct AddSpaceTokenInput {
    pub scope: TokenScope,
    #[serde(default)]
    pub name: String,
    pub expires_at: Option<u64>,
    /// Wiki ACL labels this token may read. `None` = unrestricted; `Some`
    /// grants unlabeled content plus the listed labels (PRD §8.2).
    #[serde(default)]
    pub labels: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct RevokeSpaceTokenInput {
    /// The full token value to revoke. May be empty when `name` is given.
    #[serde(default)]
    pub token: String,
    /// Revoke by (unique, required-at-mint) token name instead.
    /// `list_space_tokens` no longer echoes full token values, so this is
    /// the path for managers who did not save the value at mint time.
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Default, Deserialize, Serialize, Clone)]
pub struct UpdateSpaceInput {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub public: Option<bool>,
    /// Enables/disables the WikiDigest background extraction for this space
    /// (PRD §7.3; disabled by default).
    #[cfg(feature = "wiki")]
    #[serde(default)]
    pub wiki_digest: Option<bool>,
    /// Enables/disables read auditing for external wiki reads (PRD §3.4;
    /// disabled by default — agent reads are covered by recall logs).
    #[cfg(feature = "wiki")]
    #[serde(default)]
    pub wiki_audit_reads: Option<bool>,
    /// Namespace → default ACL label map applied to newly created wiki
    /// documents (replaces the whole map when present).
    #[cfg(feature = "wiki")]
    #[serde(default)]
    pub wiki_acl_defaults: Option<BTreeMap<String, String>>,
    /// Replaces the space's memory policy (validated before applying).
    /// Absent policy means [`MemoryPolicy::default`], which reproduces the
    /// compiled-in behavior.
    #[serde(default)]
    pub memory_policy: Option<MemoryPolicy>,
}

// `JsonSchema` (MCP tool schemas) documents the object form only; the custom
// `Deserialize` below additionally accepts a JSON-string body on both the
// HTTP and MCP channels (LLM callers frequently double-encode `context`).
#[derive(Debug, Default, Serialize, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "mcp", derive(JsonSchema))]
pub struct InputContext {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub counterparty: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct InputContextFields {
    #[serde(alias = "user")]
    counterparty: Option<String>,
    agent: Option<String>,
    source: Option<String>,
    topic: Option<String>,
}

impl From<InputContextFields> for InputContext {
    fn from(fields: InputContextFields) -> Self {
        Self {
            counterparty: fields.counterparty,
            agent: fields.agent,
            source: fields.source,
            topic: fields.topic,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum InputContextWire {
    Fields(InputContextFields),
    JsonString(String),
}

impl<'de> Deserialize<'de> for InputContext {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        match InputContextWire::deserialize(deserializer)? {
            InputContextWire::Fields(fields) => Ok(fields.into()),
            InputContextWire::JsonString(value) => input_context_from_json_string(&value),
        }
    }
}

fn input_context_from_json_string<E>(value: &str) -> Result<InputContext, E>
where
    E: de::Error,
{
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("null") {
        return Ok(InputContext::default());
    }

    if let Ok(fields) = serde_json::from_str::<InputContextFields>(trimmed) {
        return Ok(fields.into());
    }

    if let Ok(inner) = serde_json::from_str::<String>(trimmed) {
        let inner = inner.trim();
        if inner.is_empty() || inner.eq_ignore_ascii_case("null") {
            return Ok(InputContext::default());
        }

        return serde_json::from_str::<InputContextFields>(inner)
            .map(InputContext::from)
            .map_err(|err| E::custom(format!("context string must contain a JSON object: {err}")));
    }

    serde_json::from_str::<InputContextFields>(trimmed)
        .map(InputContext::from)
        .map_err(|err| E::custom(format!("context string must contain a JSON object: {err}")))
}

#[derive(Debug, Default, Deserialize, Serialize, Clone)]
pub struct RecallInput {
    pub query: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<InputContext>,
}

#[derive(Debug, Serialize, Clone)]
pub struct RecallInputRef<'a> {
    pub query: &'a str,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: &'a Option<InputContext>,
}

impl<'a> From<&'a RecallInput> for RecallInputRef<'a> {
    fn from(input: &'a RecallInput) -> Self {
        Self {
            query: &input.query,
            context: &input.context,
        }
    }
}

#[derive(Debug, Default, Deserialize, Serialize, Clone)]
pub struct FormationInput {
    pub messages: Vec<Message>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<InputContext>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
}

#[derive(Debug, Serialize, Clone)]
pub struct FormationInputRef<'a> {
    pub messages: &'a [Message],

    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: &'a Option<InputContext>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: &'a Option<String>,
}

impl<'a> From<&'a FormationInput> for FormationInputRef<'a> {
    fn from(input: &'a FormationInput) -> Self {
        Self {
            messages: &input.messages,
            context: &input.context,
            timestamp: &input.timestamp,
        }
    }
}

#[derive(Debug, Default, Deserialize, Serialize, Clone)]
pub struct FormationRestartInput {
    pub conversation: u64,
}

#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "mcp", derive(JsonSchema))]
pub enum MaintenanceScope {
    #[serde(rename = "daydream")]
    #[default]
    Daydream,
    #[serde(rename = "full")]
    Full,
    #[serde(rename = "quick")]
    Quick,
}

impl FromStr for MaintenanceScope {
    type Err = BoxError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "full" => Ok(Self::Full),
            "quick" => Ok(Self::Quick),
            "daydream" => Ok(Self::Daydream),
            _ => Err("invalid scope".into()),
        }
    }
}

impl std::fmt::Display for MaintenanceScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Full => "full",
            Self::Quick => "quick",
            Self::Daydream => "daydream",
        };
        write!(f, "{s}")
    }
}

#[derive(Debug, Default, Deserialize, Serialize, Clone)]
pub struct MaintenanceAt {
    pub daydream: u64,
    pub full: u64,
    pub quick: u64,
    /// Start time of the latest maintenance task in unix milliseconds, 0 if none started.
    #[serde(default)]
    pub start_at: u64,
}

#[derive(Debug, Default, Deserialize, Serialize, Clone)]
pub struct MaintenanceInput {
    /// `"scheduled"` | `"threshold"` | `"on_demand"`
    #[serde(default = "default_trigger")]
    pub trigger: String,

    /// `"full"` (complete sleep cycle) | `"quick"` (lightweight check only) | `"daydream"` (idle-time salience scoring and micro-consolidation).
    #[serde(default)]
    pub scope: MaintenanceScope,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<MaintenanceParameters>,

    /// The ID of the formation conversation that processed.
    #[serde(default)]
    pub formation_id: u64,
}

fn default_trigger() -> String {
    "on_demand".to_string()
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[cfg_attr(feature = "mcp", derive(JsonSchema))]
pub struct MaintenanceParameters {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stale_event_threshold_days: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence_decay_factor: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unsorted_max_backlog: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub orphan_max_count: Option<u32>,
}

impl MaintenanceParameters {
    /// Same bounds as [`MemoryPolicy::validate`]: these values are settable
    /// per run over HTTP/MCP and go straight into the KIP-writing
    /// maintenance prompt, so an out-of-range value (e.g. a negative decay
    /// factor) must be rejected at the entry point, not trusted downstream.
    pub fn validate(&self) -> Result<(), BoxError> {
        if let Some(decay) = self.confidence_decay_factor
            && !(decay.is_finite() && 0.0 < decay && decay <= 1.0)
        {
            return Err("maintenance parameter `confidence_decay_factor` must be in (0, 1]".into());
        }
        if let Some(days) = self.stale_event_threshold_days
            && !(1..=365).contains(&days)
        {
            return Err(
                "maintenance parameter `stale_event_threshold_days` must be in [1, 365]".into(),
            );
        }
        if let Some(backlog) = self.unsorted_max_backlog
            && !(1..=10_000).contains(&backlog)
        {
            return Err(
                "maintenance parameter `unsorted_max_backlog` must be in [1, 10000]".into(),
            );
        }
        if let Some(orphans) = self.orphan_max_count
            && !(1..=10_000).contains(&orphans)
        {
            return Err("maintenance parameter `orphan_max_count` must be in [1, 10000]".into());
        }
        Ok(())
    }
}

/// Evolvable memory-policy knobs (memory evolution plan, module M-P; see
/// `docs/memory_evolution_plan_cn.md`). Stored per space in the
/// `"memory_policy"` extension — like the `"byok"` model config — and an
/// absent policy means [`Default`], which reproduces the compiled-in
/// behavior exactly, so introducing the policy is not a behavior change.
///
/// The policy is the L3 evolution genome: fields marked "consumed from Px"
/// are declared ahead of their consumers so the schema stays stable across
/// phases; setting them today validates and persists but has no effect yet.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryPolicy {
    /// Schema version of this policy object.
    #[serde(default = "MemoryPolicy::default_version")]
    pub version: u32,

    /// Confidence multiplier maintenance decay applies per cycle. Matches
    /// the default documented in BrainMaintenance.md.
    #[serde(default = "MemoryPolicy::default_confidence_decay_factor")]
    pub confidence_decay_factor: f64,

    /// Stability gain per successful recall use (consumed from P1).
    #[serde(default = "MemoryPolicy::default_recall_reinforcement")]
    pub recall_reinforcement: f64,

    /// Confidence multiplier applied to corrected memories (consumed from P1).
    #[serde(default = "MemoryPolicy::default_correction_penalty")]
    pub correction_penalty: f64,

    /// Lower bound decay may not push confidence below (consumed from P1).
    #[serde(default = "MemoryPolicy::default_decay_floor")]
    pub decay_floor: f64,

    /// Events older than this are candidates for consolidation.
    #[serde(default = "MemoryPolicy::default_stale_event_threshold_days")]
    pub stale_event_threshold_days: u32,

    /// Unsorted-inbox size that maintenance should keep the graph under.
    #[serde(default = "MemoryPolicy::default_unsorted_max_backlog")]
    pub unsorted_max_backlog: u32,

    /// Orphan-concept count that maintenance should keep the graph under.
    #[serde(default = "MemoryPolicy::default_orphan_max_count")]
    pub orphan_max_count: u32,

    /// Dream self-test queries per daydream cycle; 0 disables
    /// (consumed from P2).
    #[serde(default = "MemoryPolicy::default_self_test_queries_per_cycle")]
    pub self_test_queries_per_cycle: u32,

    /// Token budget for one self-test pass (consumed from P2).
    #[serde(default = "MemoryPolicy::default_self_test_token_budget")]
    pub self_test_token_budget: u64,

    /// Semantic search threshold for recall-side probes (consumed from P1).
    #[serde(default = "MemoryPolicy::default_recall_search_threshold")]
    pub recall_search_threshold: f64,

    /// Model-turn limit for one recall run (consumed from P1; until then the
    /// compiled `RECALL_MAX_MODEL_TURNS` applies).
    #[serde(default = "MemoryPolicy::default_recall_max_rounds")]
    pub recall_max_rounds: u32,

    /// Shadow-evolution replay sample size (consumed from P4).
    #[serde(default = "MemoryPolicy::default_shadow_replay_sample")]
    pub shadow_replay_sample: u32,
}

impl Default for MemoryPolicy {
    fn default() -> Self {
        Self {
            version: Self::default_version(),
            confidence_decay_factor: Self::default_confidence_decay_factor(),
            recall_reinforcement: Self::default_recall_reinforcement(),
            correction_penalty: Self::default_correction_penalty(),
            decay_floor: Self::default_decay_floor(),
            stale_event_threshold_days: Self::default_stale_event_threshold_days(),
            unsorted_max_backlog: Self::default_unsorted_max_backlog(),
            orphan_max_count: Self::default_orphan_max_count(),
            self_test_queries_per_cycle: Self::default_self_test_queries_per_cycle(),
            self_test_token_budget: Self::default_self_test_token_budget(),
            recall_search_threshold: Self::default_recall_search_threshold(),
            recall_max_rounds: Self::default_recall_max_rounds(),
            shadow_replay_sample: Self::default_shadow_replay_sample(),
        }
    }
}

/// Process-wide policy override for optimizer runs (plan M10). Like the
/// prompt override layer (`agents::prompts`), production never sets it: the
/// eval CLI installs candidate policies here so run-scoped spaces (which
/// have no stored policy) pick them up, and clears it when the run ends.
static EVAL_POLICY_OVERRIDE: std::sync::RwLock<Option<MemoryPolicy>> = std::sync::RwLock::new(None);

impl MemoryPolicy {
    /// The space extension key the policy is stored under.
    pub const EXTENSION_KEY: &'static str = "memory_policy";

    /// The active eval policy override, when one is installed.
    pub fn eval_override() -> Option<MemoryPolicy> {
        EVAL_POLICY_OVERRIDE
            .read()
            .expect("policy override lock poisoned")
            .clone()
    }

    /// Installs (`Some`) or clears (`None`) the process-wide eval override.
    pub fn set_eval_override(policy: Option<MemoryPolicy>) {
        *EVAL_POLICY_OVERRIDE
            .write()
            .expect("policy override lock poisoned") = policy;
    }
}

/// Arms a drop-time clear of the process-wide eval policy override. The
/// optimizer holds one for the duration of a policy-genome run, so an early
/// `?` return or panic can never leak a candidate policy into later evals
/// (or parallel tests) through the global.
pub struct EvalPolicyOverrideGuard(());

impl EvalPolicyOverrideGuard {
    pub fn arm() -> Self {
        Self(())
    }
}

impl Drop for EvalPolicyOverrideGuard {
    fn drop(&mut self) {
        MemoryPolicy::set_eval_override(None);
    }
}

impl MemoryPolicy {
    fn default_version() -> u32 {
        1
    }
    fn default_confidence_decay_factor() -> f64 {
        0.95
    }
    fn default_recall_reinforcement() -> f64 {
        0.1
    }
    fn default_correction_penalty() -> f64 {
        0.5
    }
    // Matches the `confidence > 0.3` lower bound the maintenance prompt's
    // decay pass has always used, so the default policy reproduces it.
    fn default_decay_floor() -> f64 {
        0.3
    }
    fn default_stale_event_threshold_days() -> u32 {
        7
    }
    fn default_unsorted_max_backlog() -> u32 {
        20
    }
    fn default_orphan_max_count() -> u32 {
        20
    }
    fn default_self_test_queries_per_cycle() -> u32 {
        4
    }
    fn default_self_test_token_budget() -> u64 {
        20_000
    }
    fn default_recall_search_threshold() -> f64 {
        0.35
    }
    fn default_recall_max_rounds() -> u32 {
        7
    }
    // Matches the shadow-eval endpoint's default replay size (its hard cap
    // is 16).
    fn default_shadow_replay_sample() -> u32 {
        4
    }

    /// Range checks; every f64 must be finite and every integer knob capped.
    /// The caps matter as much as the floors: this object is settable over
    /// HTTP by space managers, and unbounded budgets (e.g.
    /// `self_test_queries_per_cycle`) would turn every maintenance cycle
    /// into an unbounded LLM bill. Run before persisting so a bad policy can
    /// never be stored, only rejected.
    pub fn validate(&self) -> Result<(), BoxError> {
        fn in_range(name: &str, value: f64, min_exclusive: f64, max: f64) -> Result<(), BoxError> {
            if value.is_finite() && min_exclusive < value && value <= max {
                Ok(())
            } else {
                Err(format!("memory policy `{name}` must be in ({min_exclusive}, {max}]").into())
            }
        }
        fn int_range(name: &str, value: u64, min: u64, max: u64) -> Result<(), BoxError> {
            if (min..=max).contains(&value) {
                Ok(())
            } else {
                Err(format!("memory policy `{name}` must be in [{min}, {max}]").into())
            }
        }

        in_range(
            "confidence_decay_factor",
            self.confidence_decay_factor,
            0.0,
            1.0,
        )?;
        in_range("correction_penalty", self.correction_penalty, 0.0, 1.0)?;
        in_range(
            "recall_search_threshold",
            self.recall_search_threshold,
            0.0,
            1.0,
        )?;
        if !(self.recall_reinforcement.is_finite()
            && (0.0..=1.0).contains(&self.recall_reinforcement))
        {
            return Err("memory policy `recall_reinforcement` must be in [0, 1]".into());
        }
        if !(self.decay_floor.is_finite() && (0.0..1.0).contains(&self.decay_floor)) {
            return Err("memory policy `decay_floor` must be in [0, 1)".into());
        }
        int_range("version", u64::from(self.version), 1, u64::from(u32::MAX))?;
        int_range(
            "stale_event_threshold_days",
            u64::from(self.stale_event_threshold_days),
            1,
            365,
        )?;
        int_range(
            "unsorted_max_backlog",
            u64::from(self.unsorted_max_backlog),
            1,
            10_000,
        )?;
        int_range(
            "orphan_max_count",
            u64::from(self.orphan_max_count),
            1,
            10_000,
        )?;
        // 0 disables the self-test.
        int_range(
            "self_test_queries_per_cycle",
            u64::from(self.self_test_queries_per_cycle),
            0,
            100,
        )?;
        int_range(
            "self_test_token_budget",
            self.self_test_token_budget,
            1_000,
            1_000_000,
        )?;
        int_range(
            "recall_max_rounds",
            u64::from(self.recall_max_rounds),
            1,
            50,
        )?;
        int_range(
            "shadow_replay_sample",
            u64::from(self.shadow_replay_sample),
            1,
            16,
        )?;
        Ok(())
    }

    /// The maintenance-input view of this policy, passed to the Maintenance
    /// agent on every cycle that does not carry explicit parameters.
    pub fn maintenance_parameters(&self) -> MaintenanceParameters {
        MaintenanceParameters {
            stale_event_threshold_days: Some(self.stale_event_threshold_days),
            confidence_decay_factor: Some(self.confidence_decay_factor),
            unsorted_max_backlog: Some(self.unsorted_max_backlog),
            orphan_max_count: Some(self.orphan_max_count),
        }
    }
}

/// One memory the recall trace shows was retrieved for an answer
/// (memory evolution plan, module M4). Extracted deterministically from
/// tool outputs — never from the model's own claims.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct MemoryCitation {
    /// Graph entity id: `"C:<id>"` or `"P:<id>:<predicate>"`.
    pub entity: String,

    /// Concept type, or the predicate for propositions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// `metadata.confidence` when the tool output carried it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,

    /// `metadata.source` when the tool output carried it (first entry for
    /// multi-source facts).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,

    /// `metadata.created_at` (RFC3339) when the tool output carried it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
}

/// Machine-readable recall result (memory evolution plan, module M4): the
/// answer plus the provenance a business agent needs to decide whether to
/// assert, hedge, or ask.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct RecallOutput {
    /// The synthesized answer with the self-report footer stripped.
    pub answer: String,

    /// Whether the graph held relevant memory. From the model's self-report
    /// when present, otherwise inferred from the retrieval trace.
    pub found: bool,

    /// Model-reported uncertainty, 0 (certain) ..= 1 (guessing).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uncertainty: Option<f64>,

    /// Memories the retrieval trace shows were surfaced for this answer.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub memories: Vec<MemoryCitation>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub conversation: Option<u64>,

    pub usage: Usage,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed_reason: Option<String>,
}

/// Per-source correction statistics (memory evolution plan, module M3),
/// aggregated at settlement time into the `source_reliability` space
/// extension. High correction counts mark a source whose facts deserve a
/// lower initial confidence at encode time.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct SourceReliability {
    pub corrections: u64,
    pub last_corrected_at: u64,
}

/// Outcome of one deterministic memory-metabolism settlement
/// (memory evolution plan, module M2). Stored in the `memory_settlement`
/// space extension after every maintenance cycle.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct MemorySettlementReport {
    pub settled_at: u64,

    /// Propositions whose usage counters were flushed onto graph metadata.
    pub reinforced: u64,

    /// Propositions decayed by the bulk pass (full scope only).
    pub decayed: u64,

    /// Whether the decay pass ran this cycle.
    pub decay_ran: bool,

    /// Superseded memories newly observed and recorded as corrections.
    pub new_corrections: u64,

    /// Ledger rows whose graph flush failed and stayed dirty for the next
    /// settlement to retry.
    #[serde(default)]
    pub flush_retries: u64,

    /// Set when the bulk decay pass failed (e.g. the engine's full-scan
    /// solution cap on large graphs) — decay did not complete this cycle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decay_error: Option<String>,

    /// Set when the correction-discovery scan failed — new corrections were
    /// not recorded this cycle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correction_scan_error: Option<String>,
}

/// Outcome of one dream self-test pass (memory evolution plan, module M7).
/// Stored in the `memory_self_test` space extension.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct SelfTestReport {
    pub tested_at: u64,

    /// Memories probed this pass.
    pub tested: u64,

    /// Memories whose synthetic query surfaced them via search.
    pub grounded: u64,

    /// Review SleepTasks enqueued for ungroundable memories.
    pub reencode_tasks: u64,

    /// LLM cost of the query-generation call.
    pub usage: Usage,
}

impl SelfTestReport {
    /// Fraction of tested memories that search could surface; `None` when
    /// nothing was tested.
    pub fn groundability(&self) -> Option<f64> {
        (self.tested > 0).then(|| self.grounded as f64 / self.tested as f64)
    }
}

/// Input of the metamemory probe (memory evolution plan, module M5).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProbeInput {
    pub query: String,

    /// Max hits to return (default 8).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

/// Result of the metamemory probe: a cheap, LLM-free existence check that
/// tells an agent whether a full recall is worth its latency and tokens.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ProbeOutput {
    /// Whether the graph holds anything matching the query.
    pub found: bool,

    /// True when answered from the negative-knowledge cache without touching
    /// the graph.
    pub negative_cached: bool,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hits: Vec<MemoryCitation>,
}

/// Pins (or unpins) one graph entity (memory evolution plan, module M6).
/// Pinned memories are exempt from confidence decay; entity ids come from
/// `recall_structured` citations or probe hits.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MemoryPinInput {
    /// `"C:<id>"` or `"P:<id>:<predicate>"`.
    pub entity: String,

    /// `true` to pin, `false` to unpin.
    #[serde(default = "default_true")]
    pub pinned: bool,
}

fn default_true() -> bool {
    true
}

/// Privacy-grade deletion request (memory evolution plan, module M6).
/// Always run with `dry_run: true` first; the report shows what would go.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MemoryForgetInput {
    /// Entity ids to delete: `"C:<id>"` (detaches and removes the concept
    /// and all its propositions) or `"P:<id>:<predicate>"`.
    pub entities: Vec<String>,

    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct MemoryForgetReport {
    pub dry_run: bool,
    pub deleted_concepts: u64,
    pub deleted_propositions: u64,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entities: Vec<MemoryForgetEntity>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct MemoryForgetEntity {
    pub entity: String,

    /// Whether the entity existed at request time.
    pub existed: bool,

    /// Deletion error for this entity, when one occurred (e.g. KIP_3004
    /// protecting system nodes). Other entities still proceed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Incrementally-updated memory observability counters (memory evolution
/// plan, module M12). Stored in the `memory_metrics` space extension; every
/// memory-evolution module bumps its own counters at write time, so reading
/// the status never requires heavy queries.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct MemoryMetrics {
    /// Completed production recalls the usage ledger recorded.
    pub recalls_completed: u64,

    /// Graph entities those recalls surfaced (with repetition).
    pub entities_recalled: u64,

    /// Metamemory probes that found something.
    pub probe_hits: u64,

    /// Probes that found nothing (fresh misses).
    pub probe_misses: u64,

    /// Probes answered from the negative-knowledge cache.
    pub negative_cache_hits: u64,

    /// Memories checked by dream self-tests (cumulative).
    pub self_test_tested: u64,

    /// Of those, memories search could surface.
    pub self_test_grounded: u64,

    /// Review SleepTasks the self-test enqueued.
    pub reencode_tasks: u64,

    /// Corrections (superseded memories) settlement discovered.
    pub corrections: u64,

    /// Propositions decayed by settlement.
    pub decayed: u64,

    /// Propositions whose usage counters were flushed onto the graph.
    pub reinforced: u64,

    /// Structured recalls that carried an uncertainty self-report.
    pub uncertainty_reports: u64,

    /// Sum of reported uncertainties (mean = sum / reports).
    pub uncertainty_sum: f64,

    /// Entities physically removed by forget.
    pub forgotten_entities: u64,

    pub updated_at: u64,
}

/// The `memory_status` endpoint payload: counters plus derived rates and
/// the latest module reports.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct MemoryStatus {
    pub metrics: MemoryMetrics,

    /// Cumulative self-test groundability (`self_test_grounded / tested`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub groundability: Option<f64>,

    /// `probe_hits / (probe_hits + probe_misses)`; cache hits excluded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probe_hit_rate: Option<f64>,

    /// `corrections / recalls_completed` — how often remembered facts turn
    /// out wrong relative to how often memory is used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correction_rate: Option<f64>,

    /// Mean self-reported recall uncertainty; the calibration audit
    /// (predicted vs actual corrections) builds on this.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avg_uncertainty: Option<f64>,

    /// Maintenance tokens spent per completed recall — the "memory ROI"
    /// proxy: how much upkeep each act of remembering costs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maintenance_tokens_per_recall: Option<f64>,

    pub graph: MemoryGraphCounters,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_settlement: Option<MemorySettlementReport>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_self_test: Option<SelfTestReport>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_shadow: Option<ShadowReport>,
}

/// Graph-level counters included in `memory_status`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct MemoryGraphCounters {
    pub concepts: u64,
    pub propositions: u64,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub unsorted: Option<u64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub orphans: Option<u64>,

    /// Registered `$PropositionType` count — the schema-sprawl indicator
    /// (plan module M8).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub predicate_types: Option<u64>,

    /// When these counters were censused. They refresh at settlement time
    /// (M12 principle: readers never pay heavy queries), so `memory_status`
    /// reports the graph as of the last maintenance cycle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub as_of: Option<u64>,
}

/// Per-predicate link census (memory evolution plan, module M8), stored in
/// the `schema_audit` extension by full-scope settlements. Feeds both the
/// schema-sprawl metric and the Maintenance prompt's merge guidance.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct SchemaAudit {
    pub audited_at: u64,

    /// Registered predicate → number of links using it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub predicates: BTreeMap<String, u64>,
}

/// Input of the on-demand shadow evaluation (memory evolution plan, module
/// M11): compare a candidate memory policy against the current one on
/// forked copies of this space, replaying recent real recall queries.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ShadowEvalInput {
    /// The candidate policy to evaluate.
    pub policy: MemoryPolicy,

    /// Recent recall queries to replay (defaults to the space policy's
    /// `shadow_replay_sample`, capped at 16 — every query costs two recall
    /// runs plus a judge call).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_sample: Option<usize>,
}

/// Outcome of one shadow evaluation, stored in the `shadow_report`
/// extension. Promotion stays human: read the report, then `update_space`
/// with the candidate policy if it won.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ShadowReport {
    pub compared_at: u64,
    pub replayed: u64,
    pub baseline_wins: u64,
    pub candidate_wins: u64,
    pub ties: u64,
    pub judge_errors: u64,
    pub candidate_policy: MemoryPolicy,

    /// LLM cost of the whole comparison (replays + judging).
    pub usage: Usage,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub samples: Vec<ShadowSample>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ShadowSample {
    pub query: String,

    /// `"baseline"` | `"candidate"` | `"tie"` | `"error"`.
    pub winner: String,

    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct CreateOrUpdateSpaceInput {
    pub user: Principal,
    pub space_id: String,
    pub tier: u32,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct GetOrInitUserInput {
    pub user: String,
    pub name: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::{
        CWToken, FormationInput, FormationInputRef, InputContext, MaintenanceInput,
        MaintenanceScope, MemoryPolicy, ModelConfig, RecallInput, RecallInputRef, SpaceTier,
        SpaceToken, TokenScope,
    };
    use anda_core::Principal;
    use anda_engine::model::ModelConfig as EngineModelConfig;
    use cose2::{CoseMap, Label, Value, cwt::Claims, iana};
    use serde_json::json;
    use std::str::FromStr;

    fn scope_claim(scope: &str) -> CoseMap {
        CoseMap::from_iter([(
            Label::Int(iana::CWTClaimScope),
            Value::Text(scope.to_string()),
        )])
    }

    #[test]
    fn space_token_scope_serde_roundtrip() {
        let read = serde_json::to_string(&TokenScope::Read).unwrap();
        let write = serde_json::to_string(&TokenScope::Write).unwrap();
        let all = serde_json::to_string(&TokenScope::All).unwrap();

        assert_eq!(read, "\"read\"");
        assert_eq!(write, "\"write\"");
        assert_eq!(all, "\"*\"");

        assert_eq!(
            serde_json::from_str::<TokenScope>("\"read\"").unwrap(),
            TokenScope::Read
        );
        assert_eq!(
            serde_json::from_str::<TokenScope>("\"write\"").unwrap(),
            TokenScope::Write
        );
        assert_eq!(
            serde_json::from_str::<TokenScope>("\"*\"").unwrap(),
            TokenScope::All
        );
    }

    #[test]
    fn space_token_scope_from_str_and_allows() {
        assert_eq!(TokenScope::from_str("read").unwrap(), TokenScope::Read);
        assert_eq!(TokenScope::from_str("write").unwrap(), TokenScope::Write);
        assert_eq!(TokenScope::from_str("*").unwrap(), TokenScope::All);
        assert!(TokenScope::All.allows(TokenScope::Read));
        assert!(TokenScope::All.allows(TokenScope::Write));
        assert!(TokenScope::Read.allows(TokenScope::Read));
        assert!(!TokenScope::Read.allows(TokenScope::Write));
        assert!(TokenScope::from_str("unknown").is_err());
    }

    #[test]
    fn cw_token_extracts_user_audience_and_scope_from_claims() {
        let user = Principal::from_slice(&[42]);
        let claims = Claims {
            subject: Some(user.to_string()),
            audience: Some("memory-space".to_string()),
            extra: scope_claim("write"),
            ..Default::default()
        };

        let token = CWToken::from_claims(claims).unwrap();
        assert_eq!(token.user, user);
        assert_eq!(token.audience, "memory-space");
        assert_eq!(token.scope, TokenScope::Write);
    }

    #[test]
    fn cw_token_rejects_missing_or_invalid_claims() {
        let missing_subject = Claims {
            extra: scope_claim("read"),
            ..Default::default()
        };
        assert!(CWToken::from_claims(missing_subject).is_err());

        let invalid_scope = Claims {
            subject: Some(Principal::from_slice(&[1]).to_string()),
            extra: scope_claim("admin"),
            ..Default::default()
        };
        assert!(CWToken::from_claims(invalid_scope).is_err());

        let invalid_subject = Claims {
            subject: Some("not a principal".to_string()),
            extra: scope_claim("*"),
            ..Default::default()
        };
        assert!(CWToken::from_claims(invalid_subject).is_err());
    }

    #[test]
    fn space_token_deserialize_accepts_verbose_and_compact_fields() {
        let verbose = r#"{"scope":"write","usage":3,"created_at":11,"updated_at":12}"#;
        let compact = r#"{"s":"read","u":7,"ca":21,"ua":22}"#;

        let verbose_token: SpaceToken = serde_json::from_str(verbose).unwrap();
        assert_eq!(verbose_token.scope, TokenScope::Write);
        assert_eq!(verbose_token.usage, 3);
        assert_eq!(verbose_token.created_at, 11);
        assert_eq!(verbose_token.updated_at, 12);

        let compact_token: SpaceToken = serde_json::from_str(compact).unwrap();
        assert_eq!(compact_token.scope, TokenScope::Read);
        assert_eq!(compact_token.usage, 7);
        assert_eq!(compact_token.created_at, 21);
        assert_eq!(compact_token.updated_at, 22);
    }

    #[test]
    fn space_token_serialize_uses_verbose_field_names() {
        let token = SpaceToken {
            token: "abc123".to_string(),
            scope: TokenScope::Write,
            usage: 9,
            created_at: 101,
            updated_at: 102,
            ..Default::default()
        };

        let value = serde_json::to_value(&token).unwrap();
        assert_eq!(value["scope"], "write");
        assert_eq!(value["usage"], 9);
        assert_eq!(value["created_at"], 101);
        assert_eq!(value["updated_at"], 102);
        assert!(value.get("s").is_none());
        assert!(value.get("u").is_none());
        assert!(value.get("ca").is_none());
        assert!(value.get("ua").is_none());
    }

    #[test]
    fn space_tier_allow_nodes_saturates_on_large_tiers() {
        assert_eq!(
            SpaceTier {
                tier: 0,
                updated_at: 0
            }
            .allow_nodes(),
            100
        );
        assert_eq!(
            SpaceTier {
                tier: u32::MAX,
                updated_at: 0
            }
            .allow_nodes(),
            u64::MAX
        );
    }

    #[test]
    fn input_context_deserializes_object_and_legacy_user_alias() {
        let context: InputContext =
            serde_json::from_str(r#"{"user":"alice","agent":"bot","topic":"settings"}"#).unwrap();

        assert_eq!(context.counterparty.as_deref(), Some("alice"));
        assert_eq!(context.agent.as_deref(), Some("bot"));
        assert_eq!(context.topic.as_deref(), Some("settings"));
    }

    #[test]
    fn recall_input_context_accepts_json_string() {
        let input: RecallInput = serde_json::from_str(
            r#"{"query":"preferences","context":"{\"counterparty\":\"bob\",\"source\":\"thread-1\",\"topic\":\"memory\"}"}"#,
        )
        .unwrap();
        let context = input.context.unwrap();

        assert_eq!(context.counterparty.as_deref(), Some("bob"));
        assert_eq!(context.source.as_deref(), Some("thread-1"));
        assert_eq!(context.topic.as_deref(), Some("memory"));
    }

    #[test]
    fn formation_input_context_accepts_json_string_with_user_alias() {
        let input: FormationInput = serde_json::from_str(
            r#"{"messages":[],"context":"{\"user\":\"carol\",\"agent\":\"agent-1\"}"}"#,
        )
        .unwrap();
        let context = input.context.unwrap();

        assert_eq!(context.counterparty.as_deref(), Some("carol"));
        assert_eq!(context.agent.as_deref(), Some("agent-1"));
    }

    #[test]
    fn maintenance_input_defaults_trigger_and_scope() {
        let input: MaintenanceInput = serde_json::from_str(r#"{}"#).unwrap();

        assert_eq!(input.trigger, "on_demand");
        assert_eq!(input.scope, MaintenanceScope::Daydream);
    }

    #[test]
    fn model_config_accepts_compact_aliases_and_converts_to_engine_config() {
        let config: ModelConfig = serde_json::from_str(
            r#"{"f":"openai","m":"gpt-test","ab":"https://api.example","ak":"secret","d":true,"l":"primary","b":true,"s":true,"cw":128,"mo":64}"#,
        )
        .unwrap();

        assert_eq!(config.family, "openai");
        assert_eq!(config.model, "gpt-test");
        assert_eq!(config.api_base, "https://api.example");
        assert_eq!(config.api_key, "secret");
        assert!(config.disabled);
        assert_eq!(config.label.as_deref(), Some("primary"));
        assert!(config.bearer_auth);
        assert!(config.stream);
        assert_eq!(config.context_window, 128);
        assert_eq!(config.max_output, 64);

        let engine_config: EngineModelConfig = config.into();
        assert_eq!(engine_config.family, "openai");
        assert_eq!(engine_config.model, "gpt-test");
        assert_eq!(engine_config.labels, vec!["primary"]);
        assert!(engine_config.disabled);
        assert!(engine_config.bearer_auth);
        assert!(engine_config.stream);
        assert_eq!(engine_config.context_window, 128);
        assert_eq!(engine_config.max_output, 64);
    }

    #[test]
    fn compact_refs_serialize_with_storage_field_names() {
        let tier = SpaceTier {
            tier: 2,
            updated_at: 99,
        };
        assert_eq!(
            serde_json::to_value(tier.to_ref()).unwrap(),
            json!({"t": 2, "u": 99})
        );

        let token = SpaceToken {
            token: "runtime-token".to_string(),
            name: "automation".to_string(),
            scope: TokenScope::All,
            usage: 4,
            created_at: 10,
            updated_at: 20,
            expires_at: Some(30),
            labels: None,
        };
        let value = serde_json::to_value(token.to_ref()).unwrap();

        assert_eq!(value["n"], "automation");
        assert_eq!(value["s"], "*");
        assert_eq!(value["u"], 4);
        assert_eq!(value["ca"], 10);
        assert_eq!(value["ua"], 20);
        assert_eq!(value["ea"], 30);
        assert!(value.get("token").is_none());
    }

    #[test]
    fn input_context_accepts_double_encoded_json_strings_and_nullish_values() {
        let inner = serde_json::to_string(&json!({"user": "dana", "source": "mail"})).unwrap();
        let double_encoded = serde_json::to_string(&inner).unwrap();
        let input: RecallInput = serde_json::from_value(json!({
            "query": "preferences",
            "context": double_encoded,
        }))
        .unwrap();
        let context = input.context.unwrap();

        assert_eq!(context.counterparty.as_deref(), Some("dana"));
        assert_eq!(context.source.as_deref(), Some("mail"));

        let input: RecallInput = serde_json::from_str(r#"{"query":"x","context":"null"}"#).unwrap();
        assert_eq!(input.context, Some(InputContext::default()));
    }

    #[test]
    fn input_context_rejects_json_strings_that_are_not_objects() {
        for context in ["[1,2,3]", "\"[1,2,3]\""] {
            let err = serde_json::from_value::<RecallInput>(json!({
                "query": "bad context",
                "context": context,
            }))
            .unwrap_err();

            assert!(
                err.to_string()
                    .contains("context string must contain a JSON object")
            );
        }
    }

    #[test]
    fn input_refs_borrow_request_fields_without_reencoding() {
        let recall = RecallInput {
            query: "find user preferences".to_string(),
            context: Some(InputContext {
                counterparty: Some("alice".to_string()),
                ..Default::default()
            }),
        };
        let recall_ref = RecallInputRef::from(&recall);

        assert_eq!(recall_ref.query, recall.query);
        assert_eq!(recall_ref.context, &recall.context);

        let formation = FormationInput {
            messages: Vec::new(),
            context: recall.context.clone(),
            timestamp: Some("2026-06-05T00:00:00Z".to_string()),
        };
        let formation_ref = FormationInputRef::from(&formation);

        assert!(formation_ref.messages.is_empty());
        assert_eq!(formation_ref.context, &formation.context);
        assert_eq!(formation_ref.timestamp, &formation.timestamp);
    }

    #[test]
    fn maintenance_scope_from_str_and_display_are_inverse() {
        for (wire, scope) in [
            ("full", MaintenanceScope::Full),
            ("quick", MaintenanceScope::Quick),
            ("daydream", MaintenanceScope::Daydream),
        ] {
            assert_eq!(MaintenanceScope::from_str(wire).unwrap(), scope);
            assert_eq!(scope.to_string(), wire);
        }
        assert!(MaintenanceScope::from_str("nightly").is_err());
    }

    #[test]
    fn memory_policy_defaults_match_documented_maintenance_defaults() {
        // These four values must stay in lockstep with the defaults the
        // BrainMaintenance.md Input Format documents, so an unset policy is
        // not a behavior change.
        let policy = MemoryPolicy::default();
        assert_eq!(policy.stale_event_threshold_days, 7);
        assert_eq!(policy.confidence_decay_factor, 0.95);
        assert_eq!(policy.unsorted_max_backlog, 20);
        assert_eq!(policy.orphan_max_count, 20);
        assert!(policy.validate().is_ok());

        let parameters = policy.maintenance_parameters();
        assert_eq!(parameters.stale_event_threshold_days, Some(7));
        assert_eq!(parameters.confidence_decay_factor, Some(0.95));
        assert_eq!(parameters.unsorted_max_backlog, Some(20));
        assert_eq!(parameters.orphan_max_count, Some(20));
    }

    #[test]
    fn memory_policy_validate_rejects_out_of_range_values() {
        let cases: Vec<(MemoryPolicy, &str)> = vec![
            (
                MemoryPolicy {
                    confidence_decay_factor: 0.0,
                    ..Default::default()
                },
                "confidence_decay_factor",
            ),
            (
                MemoryPolicy {
                    confidence_decay_factor: f64::NAN,
                    ..Default::default()
                },
                "confidence_decay_factor",
            ),
            (
                MemoryPolicy {
                    correction_penalty: 1.5,
                    ..Default::default()
                },
                "correction_penalty",
            ),
            (
                MemoryPolicy {
                    recall_reinforcement: -0.1,
                    ..Default::default()
                },
                "recall_reinforcement",
            ),
            (
                MemoryPolicy {
                    decay_floor: 1.0,
                    ..Default::default()
                },
                "decay_floor",
            ),
            (
                MemoryPolicy {
                    stale_event_threshold_days: 0,
                    ..Default::default()
                },
                "stale_event_threshold_days",
            ),
            (
                MemoryPolicy {
                    recall_max_rounds: 0,
                    ..Default::default()
                },
                "recall_max_rounds",
            ),
        ];
        for (policy, field) in cases {
            let err = policy.validate().unwrap_err().to_string();
            assert!(err.contains(field), "expected `{field}` in error: {err}");
        }
    }

    #[test]
    fn memory_policy_parses_partial_json_and_rejects_unknown_fields() {
        // Partial JSON fills the rest with defaults, so stored policies stay
        // readable when later phases add fields.
        let policy: MemoryPolicy =
            serde_json::from_str(r#"{"confidence_decay_factor": 0.9}"#).unwrap();
        assert_eq!(policy.confidence_decay_factor, 0.9);
        assert_eq!(policy.unsorted_max_backlog, 20);
        assert_eq!(policy.version, 1);

        // Typos fail loudly instead of silently configuring nothing.
        assert!(serde_json::from_str::<MemoryPolicy>(r#"{"decay_factor": 0.9}"#).is_err());
    }
}

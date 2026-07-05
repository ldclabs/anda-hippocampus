use anda_core::{BoxError, ModelEffort, Principal, Usage, model::Message};
use anda_db::storage::StorageStats;
use anda_engine::model::ModelConfig as EngineModelConfig;
use ic_cose_types::cose::cwt::{ClaimsSet, get_scope};
use serde::{Deserialize, Deserializer, Serialize, de};
use std::str::FromStr;

#[derive(Deserialize)]
pub struct Pagination {
    pub cursor: Option<String>,
    pub limit: Option<usize>,
    /// Conversation collection: "recall", "maintenance".
    pub collection: Option<String>,
}

#[derive(Deserialize)]
pub struct ConversationDeltaQuery {
    pub messages_offset: Option<usize>,
    pub artifacts_offset: Option<usize>,
    /// Conversation collection: "recall", "maintenance".
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
    #[serde(default)]
    pub wiki_docs: usize,
    #[serde(default)]
    pub wiki_chunks: usize,
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
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct RevokeSpaceTokenInput {
    pub token: String,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct UpdateSpaceInput {
    pub name: Option<String>,
    pub description: Option<String>,
    pub public: Option<bool>,
}

#[derive(Debug, Default, Serialize, Clone, PartialEq, Eq)]
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
        MaintenanceScope, ModelConfig, RecallInput, RecallInputRef, SpaceTier, SpaceToken,
        TokenScope,
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
}

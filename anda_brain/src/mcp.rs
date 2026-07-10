use anda_core::{
    BoxError, Principal,
    model::{ContentPart, Message},
};
use anda_engine::unix_ms;
use axum::extract::OriginalUri;
use http::{HeaderMap, header};
use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResult, Content, Implementation, InitializeResult, ServerCapabilities, ServerInfo,
    },
    schemars::JsonSchema,
    service::RequestContext,
    tool, tool_handler, tool_router,
    transport::{
        stdio,
        streamable_http_server::{
            StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
        },
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::{sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

use crate::{
    agents::SELF_USER_ID,
    payload::StringOr,
    space::{AppState, Space},
    types::{
        FormationInput, InputContext, MaintenanceInput, MaintenanceParameters, MaintenanceScope,
        RecallInput, TokenScope,
    },
    wiki::{
        WikiCommitInput, WikiError, WikiReadInput, WikiSearchInput, WikiSearchMode, WikiSelector,
        WikiVerifyInput,
    },
};

#[derive(Debug, Clone)]
pub struct McpServerConfig {
    pub space_id: String,
    pub auth_token: Option<String>,
    pub auto_create_space: bool,
    pub auto_create_tier: u32,
    pub dynamic_space_from_path: bool,
    pub remote_path_prefix: String,
}

impl McpServerConfig {
    pub fn stdio(space_id: String, auth_token: Option<String>) -> Self {
        Self {
            space_id,
            auth_token,
            auto_create_space: false,
            auto_create_tier: 1,
            dynamic_space_from_path: false,
            remote_path_prefix: "/mcp".to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct McpHttpServerConfig {
    pub path_prefix: String,
    pub auto_create_space: bool,
    pub auto_create_tier: u32,
    pub allowed_hosts: Vec<String>,
    pub allowed_origins: Vec<String>,
    pub stateful_mode: bool,
    pub json_response: bool,
    pub sse_keep_alive_secs: Option<u64>,
}

#[derive(Clone)]
pub struct AndaBrainMcpServer {
    app: AppState,
    config: McpServerConfig,
    tool_router: ToolRouter<Self>,
}

#[derive(Debug, Clone)]
struct McpAccess {
    space_id: String,
    auth_token: String,
    sharding: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct McpInputContext {
    #[serde(alias = "user", skip_serializing_if = "Option::is_none")]
    pub counterparty: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
}

impl<'de> Deserialize<'de> for McpInputContext {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Reuse the HTTP channel's wire format, which also accepts a
        // JSON-string body (LLM callers frequently double-encode `context`);
        // the MCP channel must not be stricter than the HTTP one.
        let context = InputContext::deserialize(deserializer)?;
        Ok(Self {
            counterparty: context.counterparty,
            agent: context.agent,
            source: context.source,
            topic: context.topic,
        })
    }
}

impl From<McpInputContext> for InputContext {
    fn from(context: McpInputContext) -> Self {
        Self {
            counterparty: context.counterparty,
            agent: context.agent,
            source: context.source,
            topic: context.topic,
        }
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum McpMessageContent {
    Text(String),
    Parts(Vec<Value>),
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct McpMessage {
    /// Message role: "system", "user", "assistant", or "tool".
    pub role: String,
    /// Message body. Pass a string for plain text, or an array of Anda content parts.
    pub content: McpMessageContent,
    /// Optional participant or tool name.
    pub name: Option<String>,
    /// Optional sender principal ID.
    pub user: Option<String>,
    /// Optional Unix timestamp in milliseconds.
    pub timestamp: Option<u64>,
}

impl McpMessage {
    fn into_message(self) -> Result<Message, ErrorData> {
        let content = match self.content {
            McpMessageContent::Text(text) => vec![ContentPart::Text { text }],
            McpMessageContent::Parts(parts) => parts
                .into_iter()
                .map(|part| serde_json::from_value::<ContentPart>(part).map_err(invalid_params))
                .collect::<Result<Vec<_>, _>>()?,
        };

        let user = match self.user {
            Some(user) => Some(Principal::from_text(&user).map_err(invalid_params)?),
            None => None,
        };

        Ok(Message {
            role: self.role,
            content,
            name: self.name,
            user,
            timestamp: self.timestamp,
        })
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct RememberConversationInput {
    pub messages: Vec<McpMessage>,
    pub context: Option<McpInputContext>,
    /// Optional ISO 8601 timestamp for the represented conversation.
    pub timestamp: Option<String>,
}

impl RememberConversationInput {
    fn into_formation_input(self) -> Result<FormationInput, ErrorData> {
        Ok(FormationInput {
            messages: self
                .messages
                .into_iter()
                .map(McpMessage::into_message)
                .collect::<Result<Vec<_>, _>>()?,
            context: self.context.map(InputContext::from),
            timestamp: self.timestamp,
        })
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct RecallMemoryInput {
    pub query: String,
    pub context: Option<McpInputContext>,
}

impl From<RecallMemoryInput> for RecallInput {
    fn from(input: RecallMemoryInput) -> Self {
        Self {
            query: input.query,
            context: input.context.map(InputContext::from),
        }
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum McpMaintenanceScope {
    Daydream,
    Full,
    Quick,
}

impl From<McpMaintenanceScope> for MaintenanceScope {
    fn from(scope: McpMaintenanceScope) -> Self {
        match scope {
            McpMaintenanceScope::Daydream => Self::Daydream,
            McpMaintenanceScope::Full => Self::Full,
            McpMaintenanceScope::Quick => Self::Quick,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct McpMaintenanceParameters {
    pub stale_event_threshold_days: Option<u32>,
    pub confidence_decay_factor: Option<f64>,
    pub unsorted_max_backlog: Option<u32>,
    pub orphan_max_count: Option<u32>,
}

impl From<McpMaintenanceParameters> for MaintenanceParameters {
    fn from(parameters: McpMaintenanceParameters) -> Self {
        Self {
            stale_event_threshold_days: parameters.stale_event_threshold_days,
            confidence_decay_factor: parameters.confidence_decay_factor,
            unsorted_max_backlog: parameters.unsorted_max_backlog,
            orphan_max_count: parameters.orphan_max_count,
        }
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct RunMaintenanceInput {
    /// Maintenance trigger label. Defaults to "on_demand".
    pub trigger: Option<String>,
    /// Maintenance scope. Defaults to "daydream".
    pub scope: Option<McpMaintenanceScope>,
    /// Optional ISO 8601 timestamp.
    pub timestamp: Option<String>,
    pub parameters: Option<McpMaintenanceParameters>,
}

impl From<RunMaintenanceInput> for MaintenanceInput {
    fn from(input: RunMaintenanceInput) -> Self {
        Self {
            trigger: input.trigger.unwrap_or_else(|| "on_demand".to_string()),
            scope: input.scope.map(MaintenanceScope::from).unwrap_or_default(),
            timestamp: input.timestamp,
            parameters: input.parameters.map(MaintenanceParameters::from),
            formation_id: 0,
        }
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct GetOrInitUserToolInput {
    /// Stable user principal or external user identifier used by Brain.
    pub user: String,
    /// Optional display name to attach when the user is first created.
    pub name: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum McpKipCommandItem {
    Simple(String),
    WithParams {
        command: String,
        #[serde(default)]
        parameters: Map<String, Value>,
    },
}

impl From<McpKipCommandItem> for anda_kip::CommandItem {
    fn from(command: McpKipCommandItem) -> Self {
        match command {
            McpKipCommandItem::Simple(command) => Self::Simple(command),
            McpKipCommandItem::WithParams {
                command,
                parameters,
            } => Self::WithParams {
                command,
                parameters,
            },
        }
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ExecuteKipReadonlyInput {
    /// A single KIP command. Mutually exclusive with commands.
    pub command: Option<String>,
    /// Batch KIP commands. KQL/META commands are allowed; write commands are rejected.
    #[serde(default)]
    pub commands: Vec<McpKipCommandItem>,
    /// Shared placeholder parameters for command strings.
    #[serde(default)]
    pub parameters: Map<String, Value>,
    /// Validate syntax and logic without executing.
    #[serde(default)]
    pub dry_run: bool,
}

impl ExecuteKipReadonlyInput {
    fn into_request(self) -> Result<anda_kip::Request, ErrorData> {
        if self.command.is_some() && !self.commands.is_empty() {
            return Err(ErrorData::invalid_params(
                "pass either command or commands, not both",
                None,
            ));
        }

        Ok(anda_kip::Request {
            command: self.command.unwrap_or_default(),
            commands: self.commands.into_iter().map(Into::into).collect(),
            parameters: self.parameters,
            dry_run: self.dry_run,
            readonly: true,
        })
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ListConversationsInput {
    /// Conversation collection: "formation" (default), "recall", or "maintenance".
    pub collection: Option<String>,
    pub cursor: Option<String>,
    /// Page size. Values are clamped to 1..=100.
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct GetConversationInput {
    pub conversation_id: u64,
    /// Conversation collection: "formation" (default), "recall", or "maintenance".
    pub collection: Option<String>,
    /// Return only incremental messages/artifacts when true.
    pub delta: Option<bool>,
    /// Offset used when delta is true.
    pub messages_offset: Option<usize>,
    /// Offset used when delta is true.
    pub artifacts_offset: Option<usize>,
}

impl AndaBrainMcpServer {
    pub fn new(app: AppState, config: McpServerConfig) -> Self {
        Self {
            app,
            config,
            tool_router: Self::tool_router(),
        }
    }

    fn access_from_context(
        &self,
        context: &RequestContext<RoleServer>,
    ) -> Result<McpAccess, ErrorData> {
        if !self.config.dynamic_space_from_path {
            return Ok(McpAccess {
                space_id: self.config.space_id.clone(),
                auth_token: self.config.auth_token.clone().unwrap_or_default(),
                sharding: None,
            });
        }

        let parts = context.extensions.get::<http::request::Parts>().ok_or_else(|| {
            ErrorData::invalid_request(
                "HTTP MCP request context is missing; use the stdio mcp subcommand for local fixed-space mode",
                None,
            )
        })?;
        let path = parts
            .extensions
            .get::<OriginalUri>()
            .map(|uri| uri.path())
            .unwrap_or_else(|| parts.uri.path());

        Ok(McpAccess {
            space_id: space_id_from_mcp_path(path, &self.config.remote_path_prefix)?,
            auth_token: bearer_token_from_headers(&parts.headers).unwrap_or_default(),
            sharding: sharding_from_headers(&parts.headers).or(Some(0)),
        })
    }

    /// `owner` is the verified caller when one exists (remote auto-create
    /// requires a write CWT); the local stdio mode falls back to
    /// `SELF_USER_ID`. Recording the real principal keeps
    /// `get_space_info.owner` truthful.
    async fn load_configured_space(
        &self,
        space_id: &str,
        owner: Principal,
    ) -> Result<Arc<Space>, ErrorData> {
        match self.app.load_space(space_id, false).await {
            Ok(space) => Ok(space),
            Err(load_err) if self.config.auto_create_space => {
                let create_result = self
                    .app
                    .admin_create_space(
                        SELF_USER_ID,
                        owner,
                        space_id.to_string(),
                        self.config.auto_create_tier,
                        unix_ms(),
                    )
                    .await;

                match create_result {
                    Ok(_) => self
                        .app
                        .load_space(space_id, false)
                        .await
                        .map_err(internal_error),
                    Err(create_err) if create_err.to_string().contains("already exists") => self
                        .app
                        .load_space(space_id, false)
                        .await
                        .map_err(internal_error),
                    Err(create_err) => Err(internal_error(format!(
                        "failed to load space after auto-create fallback: load error: {load_err}; create error: {create_err}"
                    ))),
                }
            }
            Err(err) => Err(internal_error(format!(
                "failed to load memory space '{}': {err}. Create it first or start MCP with --mcp-auto-create-space",
                space_id
            ))),
        }
    }

    async fn load_existing_space(&self, space_id: &str) -> Result<Arc<Space>, ErrorData> {
        self.app.load_space(space_id, false).await.map_err(|err| {
            internal_error(format!(
                "failed to load memory space '{}': {err}. Create it first or start MCP with --mcp-auto-create-space",
                space_id
            ))
        })
    }

    async fn load_authorized_space(
        &self,
        scope: TokenScope,
        access: &McpAccess,
    ) -> Result<Arc<Space>, ErrorData> {
        Ok(self
            .load_authorized_space_with_token(scope, access)
            .await?
            .0)
    }

    /// Like [`Self::load_authorized_space`] but also returns the verified
    /// CWT and space token so wiki tools can resolve the caller's ACL view
    /// and audit actor (PRD §8.2): CWT holders are unrestricted, space
    /// tokens carry their labels, and an anonymous public-space reader is
    /// neither — the caller must map that to the unlabeled-only view. A
    /// supplied space token is verified even on public spaces so a labeled
    /// token keeps its granted labels instead of degrading to anonymous.
    async fn load_authorized_space_with_token(
        &self,
        scope: TokenScope,
        access: &McpAccess,
    ) -> Result<
        (
            Arc<Space>,
            Option<crate::types::CWToken>,
            Option<crate::types::SpaceToken>,
        ),
        ErrorData,
    > {
        if let Some(sharding) = access.sharding
            && sharding != self.app.sharding
        {
            return Err(ErrorData::invalid_request(
                format!(
                    "space_id sharding {sharding} does not match server sharding {}",
                    self.app.sharding
                ),
                None,
            ));
        }

        let now_ms = unix_ms();
        let cwt = self
            .app
            .check_auth_if(&access.auth_token, &access.space_id, scope, now_ms)
            .map_err(|_| unauthorized(scope))?;

        let space = match self.load_existing_space(&access.space_id).await {
            Ok(space) => space,
            Err(_err) if self.config.auto_create_space => {
                if self.config.dynamic_space_from_path && !self.app.cwt_auth_enabled() {
                    return Err(ErrorData::invalid_request(
                        "remote MCP auto-create requires ED25519_PUBKEYS and a write CWT for the target space",
                        None,
                    ));
                }
                let creator = self
                    .app
                    .check_auth(
                        &access.auth_token,
                        &access.space_id,
                        TokenScope::Write,
                        now_ms,
                    )
                    .map_err(|_| unauthorized(TokenScope::Write))?;
                // The verified write-CWT principal owns the space it caused
                // to be created; `SELF_USER_ID` would misreport ownership.
                self.load_configured_space(&access.space_id, creator.user)
                    .await?
            }
            Err(err) => return Err(err),
        };
        let mut space_token = None;
        if cwt.is_none() {
            match space.verify_space_token(access.auth_token.clone(), scope, now_ms) {
                Ok(st) => space_token = Some(st),
                // Public-space reads fall back to the anonymous view only
                // when no valid credential was presented.
                Err(_) if scope == TokenScope::Read && space.is_public() => {}
                Err(_) => return Err(unauthorized(scope)),
            }
        }
        Ok((space, cwt, space_token))
    }

    pub async fn ensure_space_available(&self) -> Result<(), ErrorData> {
        if self.config.dynamic_space_from_path {
            return Ok(());
        }
        // Local fixed-space (stdio) mode has no authenticated caller; the
        // space belongs to the local dev identity.
        self.load_configured_space(&self.config.space_id, SELF_USER_ID)
            .await
            .map(|_| ())
    }

    async fn get_space_info_for(&self, access: &McpAccess) -> Result<CallToolResult, ErrorData> {
        let space = self.load_authorized_space(TokenScope::Read, access).await?;
        structured_result(space.get_info())
    }

    async fn get_formation_status_for(
        &self,
        access: &McpAccess,
    ) -> Result<CallToolResult, ErrorData> {
        let space = self.load_authorized_space(TokenScope::Read, access).await?;
        structured_result(space.formation_status())
    }

    async fn remember_conversation_for(
        &self,
        access: &McpAccess,
        input: RememberConversationInput,
    ) -> Result<CallToolResult, ErrorData> {
        let input = input.into_formation_input()?;
        let space = self
            .load_authorized_space(TokenScope::Write, access)
            .await?;
        let output = space
            .ingest(SELF_USER_ID, StringOr::Value(input))
            .await
            .map_err(internal_error)?;
        agent_output_result(output)
    }

    async fn recall_memory_for(
        &self,
        access: &McpAccess,
        input: RecallMemoryInput,
    ) -> Result<CallToolResult, ErrorData> {
        let input = RecallInput::from(input);
        let (space, _cwt, st) = self
            .load_authorized_space_with_token(TokenScope::Read, access)
            .await?;
        if st.as_ref().is_some_and(|st| st.labels.is_some()) {
            // RecallAgent's wiki tools span all labels; a label-restricted
            // token cannot use agentic recall (same guard as HTTP /recall).
            return Err(ErrorData::invalid_request(
                "recall requires an unrestricted token",
                None,
            ));
        }
        let output = space
            .query(SELF_USER_ID, StringOr::Value(input))
            .await
            .map_err(internal_error)?;
        agent_output_result(output)
    }

    async fn run_maintenance_for(
        &self,
        access: &McpAccess,
        input: RunMaintenanceInput,
    ) -> Result<CallToolResult, ErrorData> {
        let space = self
            .load_authorized_space(TokenScope::Write, access)
            .await?;
        if space.is_processing() {
            return Err(ErrorData::invalid_request(
                "formation or maintenance is already processing; retry after the current task finishes",
                None,
            ));
        }

        let output = space
            .maintenance(SELF_USER_ID, MaintenanceInput::from(input))
            .await
            .map_err(internal_error)?;
        agent_output_result(output)
    }

    async fn execute_kip_readonly_for(
        &self,
        access: &McpAccess,
        input: ExecuteKipReadonlyInput,
    ) -> Result<CallToolResult, ErrorData> {
        let request = input.into_request()?;
        let space = self.load_authorized_space(TokenScope::Read, access).await?;
        let response = space
            .execute_kip_readonly(request)
            .await
            .map_err(internal_error)?;
        structured_result(response)
    }

    async fn get_or_init_user_for(
        &self,
        access: &McpAccess,
        input: GetOrInitUserToolInput,
    ) -> Result<CallToolResult, ErrorData> {
        let space = self
            .load_authorized_space(TokenScope::Write, access)
            .await?;
        let concept = space
            .formation
            .get_or_init_counterparty(input.user, input.name)
            .await
            .map_err(internal_error)?;
        structured_result(concept)
    }

    async fn list_conversations_for(
        &self,
        access: &McpAccess,
        input: ListConversationsInput,
    ) -> Result<CallToolResult, ErrorData> {
        let (space, _cwt, st) = self
            .load_authorized_space_with_token(TokenScope::Read, access)
            .await?;
        if crate::handler::label_restricted(st.as_ref()) {
            // Conversations persist the full runner history (recall's wiki
            // tools run unrestricted); same guard as the HTTP channel.
            return Err(ErrorData::invalid_request(
                "conversations require an unrestricted token",
                None,
            ));
        }
        let (conversations, next_cursor) = space
            .list_conversations(input.collection, input.cursor, input.limit)
            .await
            .map_err(internal_error)?;
        structured_result(json!({
            "conversations": conversations,
            "next_cursor": next_cursor,
        }))
    }

    async fn get_conversation_for(
        &self,
        access: &McpAccess,
        input: GetConversationInput,
    ) -> Result<CallToolResult, ErrorData> {
        let (space, _cwt, st) = self
            .load_authorized_space_with_token(TokenScope::Read, access)
            .await?;
        if crate::handler::label_restricted(st.as_ref()) {
            // Same guard as list_conversations_for / the HTTP channel.
            return Err(ErrorData::invalid_request(
                "conversations require an unrestricted token",
                None,
            ));
        }
        let conversation = space
            .get_conversation(input.collection, input.conversation_id)
            .await
            .map_err(internal_error)?;

        if input.delta.unwrap_or(false) {
            structured_result(conversation.into_delta(
                input.messages_offset.unwrap_or_default(),
                input.artifacts_offset.unwrap_or_default(),
            ))
        } else {
            structured_result(conversation)
        }
    }

    async fn wiki_commit_for(
        &self,
        access: &McpAccess,
        input: WikiCommitToolInput,
    ) -> Result<CallToolResult, ErrorData> {
        let (space, cwt, st) = self
            .load_authorized_space_with_token(TokenScope::Write, access)
            .await?;
        // Real audit subject (PRD §8.1 hard requirement), same derivation as
        // the HTTP channel.
        let actor = crate::handler::wiki_actor(&cwt, st.as_ref());
        let output = space
            .wiki
            .commit(actor, input.into(), unix_ms())
            .await
            .map_err(wiki_tool_error)?;
        structured_result(output)
    }

    async fn wiki_search_for(
        &self,
        access: &McpAccess,
        input: WikiSearchToolInput,
    ) -> Result<CallToolResult, ErrorData> {
        let (space, cwt, st) = self
            .load_authorized_space_with_token(TokenScope::Read, access)
            .await?;
        let scope = crate::handler::wiki_read_access(&cwt, st.as_ref());
        let input = WikiSearchInput::try_from(input)?;
        let output = space
            .wiki
            .search_scoped(&scope, input, unix_ms())
            .await
            .map_err(wiki_tool_error)?;
        structured_result(output)
    }

    async fn wiki_read_for(
        &self,
        access: &McpAccess,
        input: WikiReadToolInput,
    ) -> Result<CallToolResult, ErrorData> {
        let (space, cwt, st) = self
            .load_authorized_space_with_token(TokenScope::Read, access)
            .await?;
        let scope = crate::handler::wiki_read_access(&cwt, st.as_ref());
        let output = space
            .wiki
            .read_scoped(&scope, input.try_into()?, unix_ms())
            .await
            .map_err(wiki_tool_error)?;
        structured_result(output)
    }

    async fn wiki_verify_for(
        &self,
        access: &McpAccess,
        input: WikiVerifyToolInput,
    ) -> Result<CallToolResult, ErrorData> {
        let (space, cwt, st) = self
            .load_authorized_space_with_token(TokenScope::Read, access)
            .await?;
        let scope = crate::handler::wiki_read_access(&cwt, st.as_ref());
        let output = space
            .wiki
            .verify_scoped(
                &scope,
                WikiVerifyInput {
                    uri: Some(input.uri),
                    checksum: input.checksum,
                    ..Default::default()
                },
                unix_ms(),
            )
            .await
            .map_err(wiki_tool_error)?;
        structured_result(output)
    }
}

fn wiki_tool_error(err: WikiError) -> ErrorData {
    match err {
        WikiError::Db(_) => internal_error(err),
        err => ErrorData::invalid_request(err.to_string(), None),
    }
}

/// Flat MCP input for `anda_brain_wiki_commit`.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct WikiCommitToolInput {
    /// Existing document id to update; omit to create a new document.
    pub doc_id: Option<u64>,
    /// Required for updates: the current_version this edit is based on. On a
    /// conflict error, re-read the document and retry with the reported
    /// current version.
    pub parent_version: Option<u64>,
    /// Logical partition such as "policy" or "engineering"; defaults to "default".
    pub namespace: Option<String>,
    /// Display slug; derived from the title when omitted.
    pub slug: Option<String>,
    pub title: String,
    /// Full Markdown content (the whole document, not a diff).
    pub content: String,
    pub tags: Option<Vec<String>>,
    /// External origin URI when importing.
    pub source_uri: Option<String>,
    /// Commit message: why this change.
    pub message: Option<String>,
    /// ACL label; omit to keep/inherit, empty string to clear.
    pub acl_label: Option<String>,
}

impl From<WikiCommitToolInput> for WikiCommitInput {
    fn from(input: WikiCommitToolInput) -> Self {
        Self {
            doc_id: input.doc_id,
            parent_version: input.parent_version,
            namespace: input.namespace,
            slug: input.slug,
            title: input.title,
            content: input.content,
            tags: input.tags,
            acl_label: input.acl_label,
            source_uri: input.source_uri,
            message: input.message,
            metadata: None,
        }
    }
}

/// Flat MCP input for `anda_brain_wiki_search`.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct WikiSearchToolInput {
    /// Keyword query; BM25 matching favors exact terms, product names and error codes.
    pub query: String,
    /// Restrict to these namespaces.
    pub namespaces: Option<Vec<String>>,
    /// Restrict to these document ids.
    pub doc_ids: Option<Vec<u64>>,
    /// Restrict to documents carrying any of these tags.
    pub tags: Option<Vec<String>>,
    /// Max hits (1-50, default 8).
    pub top_k: Option<usize>,
    /// "chunks" (default) for best passages, "docs" for one hit per document.
    pub mode: Option<String>,
    /// Neighbor expansion 0-2 (default 0): widen hits with adjacent passages.
    pub expand: Option<u8>,
}

impl TryFrom<WikiSearchToolInput> for WikiSearchInput {
    type Error = ErrorData;

    fn try_from(input: WikiSearchToolInput) -> Result<Self, Self::Error> {
        // The HTTP channel rejects unknown modes; the MCP channel must not
        // silently degrade them to "chunks" (an LLM typo like "documents"
        // would quietly change the result shape).
        let mode = match input.mode.as_deref() {
            Some("docs") => WikiSearchMode::Docs,
            Some("chunks") | None => WikiSearchMode::Chunks,
            Some(other) => {
                return Err(invalid_params(format!(
                    "invalid mode {other:?} (expected \"chunks\" or \"docs\")"
                )));
            }
        };
        Ok(Self {
            query: input.query,
            namespaces: input.namespaces.unwrap_or_default(),
            doc_ids: input.doc_ids.unwrap_or_default(),
            tags: input.tags.unwrap_or_default(),
            top_k: input.top_k,
            mode,
            expand: input.expand,
        })
    }
}

/// Flat MCP input for `anda_brain_wiki_read`.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct WikiReadToolInput {
    pub doc_id: u64,
    /// Version id for time-travel reads; omit for the current version.
    pub version: Option<u64>,
    /// "toc" | "section" | "range" | "full". Defaults to "section" when an
    /// anchor is given, "range" when start/end are given, else "toc".
    pub selector: Option<String>,
    /// Section anchor from a TOC entry or a search citation.
    pub anchor: Option<String>,
    pub start: Option<u64>,
    pub end: Option<u64>,
}

impl TryFrom<WikiReadToolInput> for WikiReadInput {
    type Error = ErrorData;

    fn try_from(input: WikiReadToolInput) -> Result<Self, ErrorData> {
        let selector = match input.selector.as_deref() {
            Some("toc") => WikiSelector::Toc,
            Some("full") => WikiSelector::Full,
            Some("section") => WikiSelector::Section {
                anchor: input.anchor.clone().ok_or_else(|| {
                    ErrorData::invalid_request("selector 'section' requires anchor", None)
                })?,
            },
            Some("range") => match (input.start, input.end) {
                (Some(start), Some(end)) => WikiSelector::Range { start, end },
                _ => {
                    return Err(ErrorData::invalid_request(
                        "selector 'range' requires start and end",
                        None,
                    ));
                }
            },
            Some(other) => {
                return Err(ErrorData::invalid_request(
                    format!("unknown selector: {other}"),
                    None,
                ));
            }
            None => match (&input.anchor, input.start, input.end) {
                (Some(anchor), _, _) => WikiSelector::Section {
                    anchor: anchor.clone(),
                },
                (None, Some(start), Some(end)) => WikiSelector::Range { start, end },
                _ => WikiSelector::Toc,
            },
        };
        Ok(Self {
            doc_id: input.doc_id,
            version: input.version,
            selector,
        })
    }
}

/// Flat MCP input for `anda_brain_wiki_verify`.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct WikiVerifyToolInput {
    /// Citation URI: wiki://{space}/{doc_id}@{version_id}#{start}-{end}
    pub uri: String,
    /// Optional cited checksum to compare against stored content.
    pub checksum: Option<String>,
}

#[tool_router]
impl AndaBrainMcpServer {
    /// Return statistics and metadata for the configured Anda Brain memory space.
    #[tool(
        name = "anda_brain_get_space_info",
        annotations(
            title = "Get Space Info",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub async fn get_space_info(
        &self,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let access = self.access_from_context(&context)?;
        self.get_space_info_for(&access).await
    }

    /// Return lightweight formation and maintenance progress for the configured memory space.
    #[tool(
        name = "anda_brain_get_formation_status",
        annotations(
            title = "Get Formation Status",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub async fn get_formation_status(
        &self,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let access = self.access_from_context(&context)?;
        self.get_formation_status_for(&access).await
    }

    /// Encode conversation messages into long-term structured memory.
    /// Returns quickly after queuing asynchronous formation; use
    /// anda_brain_get_formation_status or anda_brain_get_conversation to track processing.
    #[tool(
        name = "anda_brain_remember_conversation",
        annotations(
            title = "Remember Conversation",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    pub async fn remember_conversation(
        &self,
        Parameters(input): Parameters<RememberConversationInput>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let access = self.access_from_context(&context)?;
        self.remember_conversation_for(&access, input).await
    }

    /// Ask a natural-language question against long-term memory.
    /// This is a synchronous recall run and can take over 60 seconds for complex searches;
    /// wait for the tool result instead of assuming it is background work.
    #[tool(
        name = "anda_brain_recall_memory",
        annotations(
            title = "Recall Memory",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    pub async fn recall_memory(
        &self,
        Parameters(input): Parameters<RecallMemoryInput>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let access = self.access_from_context(&context)?;
        self.recall_memory_for(&access, input).await
    }

    /// Trigger a memory maintenance cycle for consolidation, pruning, and graph health.
    /// Returns quickly after starting asynchronous maintenance; use
    /// anda_brain_get_formation_status or anda_brain_get_conversation to track progress.
    #[tool(
        name = "anda_brain_run_maintenance",
        annotations(
            title = "Run Maintenance",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    pub async fn run_maintenance(
        &self,
        Parameters(input): Parameters<RunMaintenanceInput>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let access = self.access_from_context(&context)?;
        self.run_maintenance_for(&access, input).await
    }

    /// Execute KIP read-only commands against the Cognitive Nexus for advanced graph inspection.
    #[tool(
        name = "anda_brain_execute_kip_readonly",
        annotations(
            title = "Execute Read-Only KIP",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    pub async fn execute_kip_readonly(
        &self,
        Parameters(input): Parameters<ExecuteKipReadonlyInput>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let access = self.access_from_context(&context)?;
        self.execute_kip_readonly_for(&access, input).await
    }

    /// Get or create a counterparty concept for a user in memory formation context.
    #[tool(
        name = "anda_brain_get_or_init_user",
        annotations(
            title = "Get Or Init User",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub async fn get_or_init_user(
        &self,
        Parameters(input): Parameters<GetOrInitUserToolInput>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let access = self.access_from_context(&context)?;
        self.get_or_init_user_for(&access, input).await
    }

    /// List tracked formation, recall, or maintenance conversations with cursor pagination.
    #[tool(
        name = "anda_brain_list_conversations",
        annotations(
            title = "List Conversations",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    pub async fn list_conversations(
        &self,
        Parameters(input): Parameters<ListConversationsInput>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let access = self.access_from_context(&context)?;
        self.list_conversations_for(&access, input).await
    }

    /// Get one tracked conversation, optionally as a delta from message/artifact offsets.
    #[tool(
        name = "anda_brain_get_conversation",
        annotations(
            title = "Get Conversation",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    pub async fn get_conversation(
        &self,
        Parameters(input): Parameters<GetConversationInput>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let access = self.access_from_context(&context)?;
        self.get_conversation_for(&access, input).await
    }

    /// Search the space wiki (versioned reference documents) with keyword BM25
    /// and get snippets with verifiable wiki:// citations. Cite the URIs when
    /// using the evidence; retry with reformulated keywords if results are poor.
    #[tool(
        name = "anda_brain_wiki_search",
        annotations(
            title = "Search Wiki",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub async fn wiki_search(
        &self,
        Parameters(input): Parameters<WikiSearchToolInput>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let access = self.access_from_context(&context)?;
        self.wiki_search_for(&access, input).await
    }

    /// Read a wiki document progressively: TOC first, then one section by
    /// anchor, a byte range, or the bounded full text. Supports reading
    /// historical versions.
    #[tool(
        name = "anda_brain_wiki_read",
        annotations(
            title = "Read Wiki Document",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub async fn wiki_read(
        &self,
        Parameters(input): Parameters<WikiReadToolInput>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let access = self.access_from_context(&context)?;
        self.wiki_read_for(&access, input).await
    }

    /// Commit a wiki document as an immutable new version (git-like).
    /// Create: omit doc_id. Update: pass doc_id and parent_version; on a
    /// conflict, re-read, merge, and retry. Identical content is a no-op.
    #[tool(
        name = "anda_brain_wiki_commit",
        annotations(
            title = "Commit Wiki Document",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub async fn wiki_commit(
        &self,
        Parameters(input): Parameters<WikiCommitToolInput>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let access = self.access_from_context(&context)?;
        self.wiki_commit_for(&access, input).await
    }

    /// Verify a wiki citation URI against the immutable stored content:
    /// valid, superseded by a newer version, or invalid.
    #[tool(
        name = "anda_brain_wiki_verify",
        annotations(
            title = "Verify Wiki Citation",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub async fn wiki_verify(
        &self,
        Parameters(input): Parameters<WikiVerifyToolInput>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let access = self.access_from_context(&context)?;
        self.wiki_verify_for(&access, input).await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for AndaBrainMcpServer {
    fn get_info(&self) -> ServerInfo {
        InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(
                Implementation::new("anda-brain-mcp", env!("CARGO_PKG_VERSION"))
                    .with_title("Anda Brain MCP Server"),
            )
            .with_instructions(format!(
                "Use these tools to access Anda Brain long-term memory for {}. Prefer anda_brain_recall_memory for questions and anda_brain_remember_conversation after meaningful user-agent exchanges.",
                if self.config.dynamic_space_from_path {
                    "the memory space selected by this MCP HTTP URL".to_string()
                } else {
                    format!("space '{}'", self.config.space_id)
                }
            ))
    }
}

pub async fn run_stdio_server(app: AppState, config: McpServerConfig) -> Result<(), BoxError> {
    let server = AndaBrainMcpServer::new(app.clone(), config);
    server.ensure_space_available().await.map_err(|err| {
        format!(
            "failed to initialize Anda Brain MCP server: {}",
            err.message
        )
    })?;

    let cancel_token = CancellationToken::new();
    let background_app = app.clone();
    let background_cancel = cancel_token.clone();
    let background_handle = tokio::spawn(async move {
        background_app
            .start_background_tasks(background_cancel)
            .await;
    });

    let service = rmcp::serve_server(server, stdio()).await?;
    let service_result = service.waiting().await;
    cancel_token.cancel();
    let _ = background_handle.await;
    service_result?;
    Ok(())
}

pub fn build_streamable_http_service(
    app: AppState,
    config: McpHttpServerConfig,
    cancellation_token: CancellationToken,
) -> StreamableHttpService<AndaBrainMcpServer, LocalSessionManager> {
    let path_prefix = normalize_mcp_path_prefix(&config.path_prefix);
    let service_config = McpServerConfig {
        space_id: String::new(),
        auth_token: None,
        auto_create_space: config.auto_create_space,
        auto_create_tier: config.auto_create_tier,
        dynamic_space_from_path: true,
        remote_path_prefix: path_prefix,
    };
    let service_app = app.clone();
    let mut transport_config = StreamableHttpServerConfig::default()
        .with_cancellation_token(cancellation_token)
        .with_stateful_mode(config.stateful_mode)
        .with_json_response(config.json_response)
        .with_sse_keep_alive(config.sse_keep_alive_secs.map(Duration::from_secs));

    if !config.allowed_hosts.is_empty() {
        transport_config = if config.allowed_hosts.iter().any(|host| host == "*") {
            transport_config.disable_allowed_hosts()
        } else {
            transport_config.with_allowed_hosts(config.allowed_hosts)
        };
    }
    if !config.allowed_origins.is_empty() {
        transport_config = if config.allowed_origins.iter().any(|origin| origin == "*") {
            transport_config.disable_allowed_origins()
        } else {
            transport_config.with_allowed_origins(config.allowed_origins)
        };
    }

    StreamableHttpService::new(
        move || {
            Ok(AndaBrainMcpServer::new(
                service_app.clone(),
                service_config.clone(),
            ))
        },
        Default::default(),
        transport_config,
    )
}

fn structured_result<T>(value: T) -> Result<CallToolResult, ErrorData>
where
    T: Serialize,
{
    let value = serde_json::to_value(value).map_err(internal_error)?;
    Ok(CallToolResult::structured(value))
}

fn agent_output_result(output: anda_core::AgentOutput) -> Result<CallToolResult, ErrorData> {
    let is_error = output.failed_reason.is_some();
    let text = if output.content.trim().is_empty() {
        serde_json::to_string(&output).map_err(internal_error)?
    } else {
        output.content.clone()
    };
    let value = serde_json::to_value(output).map_err(internal_error)?;
    let mut result = CallToolResult::structured(value);
    result.content = vec![Content::text(text)];
    result.is_error = Some(is_error);
    Ok(result)
}

fn unauthorized(scope: TokenScope) -> ErrorData {
    ErrorData::invalid_request(
        format!(
            "{scope:?} access denied for Anda Brain MCP space. Configure MCP_AUTH_TOKEN or --mcp-auth-token with a CWT or space token that has the required scope."
        ),
        None,
    )
}

fn invalid_params(error: impl ToString) -> ErrorData {
    ErrorData::invalid_params(error.to_string(), None)
}

fn internal_error(error: impl ToString) -> ErrorData {
    ErrorData::internal_error(error.to_string(), None)
}

fn bearer_token_from_headers(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .map(|s| s.trim_start_matches("Bearer ").to_string())
        .filter(|s| !s.is_empty())
}

fn sharding_from_headers(headers: &HeaderMap) -> Option<u32> {
    headers
        .get("Shard-Id")
        .or_else(|| headers.get("X-Shard"))
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.parse::<u32>().ok())
}

fn normalize_mcp_path_prefix(prefix: &str) -> String {
    let trimmed = prefix.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        "/mcp".to_string()
    } else if trimmed.starts_with('/') {
        trimmed.to_string()
    } else {
        format!("/{trimmed}")
    }
}

fn space_id_from_mcp_path(path: &str, prefix: &str) -> Result<String, ErrorData> {
    let prefix = normalize_mcp_path_prefix(prefix);
    let rest = if path == prefix {
        ""
    } else if let Some(rest) = path.strip_prefix(&format!("{prefix}/")) {
        rest
    } else {
        path.trim_start_matches('/')
    };
    let space_id = rest.split('/').next().unwrap_or_default();
    if space_id.is_empty() {
        return Err(ErrorData::invalid_request(
            format!("MCP URL must include a space id, for example {prefix}/my_space_001"),
            None,
        ));
    }

    Ok(space_id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anda_core::{AgentOutput, BoxPinFut, CompletionRequest};
    use anda_db::{database::DBConfig, storage::StorageConfig};
    use anda_engine::{
        management::{BaseManagement, Visibility},
        model::{CompletionFeaturesDyn, Model, Models, reqwest},
    };
    use cose2::{CoseMap, Label, Sign1Message, Value as CoseValue, cwt::Claims, iana};
    use ic_auth_types::ByteBufB64;
    use ic_cose_types::cose::ed25519::{SigningKey, VerifyingKey, ed25519_sign};
    use object_store::memory::InMemory;
    use std::collections::BTreeSet;

    #[derive(Debug)]
    struct FinalCompleter;

    impl CompletionFeaturesDyn for FinalCompleter {
        fn model_name(&self) -> String {
            "mcp-test-model".to_string()
        }

        fn completion(&self, req: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
            Box::pin(async move {
                Ok(AgentOutput {
                    content: format!("mcp processed: {}", req.prompt),
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

    fn test_app_state(name: &str, pubkeys: Vec<VerifyingKey>) -> AppState {
        let management = Arc::new(BaseManagement {
            controller: SELF_USER_ID,
            managers: BTreeSet::new(),
            visibility: Visibility::Public,
        });
        let http_client = reqwest::Client::builder().build().unwrap();
        let models = Models::default();
        models.set_model(Model::with_completer(Arc::new(FinalCompleter)));

        AppState::new(
            Arc::new(InMemory::new()),
            Arc::new(test_db_config(name)),
            management,
            http_client,
            Arc::new(models),
            Arc::new(pubkeys),
            "anda_brain".to_string(),
            "test-version".to_string(),
            0,
        )
    }

    async fn create_server(space_id: &str) -> AndaBrainMcpServer {
        let app = test_app_state("mcp_tests", vec![]);
        app.admin_create_space(
            SELF_USER_ID,
            SELF_USER_ID,
            space_id.to_string(),
            1,
            unix_ms(),
        )
        .await
        .unwrap();
        AndaBrainMcpServer::new(
            app,
            McpServerConfig {
                space_id: space_id.to_string(),
                auth_token: None,
                auto_create_space: false,
                auto_create_tier: 1,
                dynamic_space_from_path: false,
                remote_path_prefix: "/mcp".to_string(),
            },
        )
    }

    fn test_access(space_id: &str) -> McpAccess {
        McpAccess {
            space_id: space_id.to_string(),
            auth_token: String::new(),
            sharding: None,
        }
    }

    fn test_signing_key() -> SigningKey {
        SigningKey::from_bytes(&[9u8; 32])
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
                CoseValue::Text(scope.to_string()),
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

    #[test]
    fn mcp_message_accepts_text_and_content_parts() {
        let text = McpMessage {
            role: "user".to_string(),
            content: McpMessageContent::Text("hello".to_string()),
            name: None,
            user: None,
            timestamp: Some(42),
        }
        .into_message()
        .unwrap();
        assert_eq!(text.text().as_deref(), Some("hello"));
        assert_eq!(text.timestamp, Some(42));

        let parts = McpMessage {
            role: "assistant".to_string(),
            content: McpMessageContent::Parts(vec![json!({
                "type": "Text",
                "text": "done"
            })]),
            name: None,
            user: None,
            timestamp: None,
        }
        .into_message()
        .unwrap();
        assert_eq!(parts.text().as_deref(), Some("done"));
    }

    #[tokio::test]
    async fn tool_router_exposes_core_memory_tools_with_annotations() {
        let server = create_server("mcp_tool_router").await;
        let tools = server.tool_router.list_all();
        let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_ref()).collect();

        assert!(names.contains(&"anda_brain_remember_conversation"));
        assert!(names.contains(&"anda_brain_recall_memory"));
        assert!(names.contains(&"anda_brain_run_maintenance"));
        assert!(names.contains(&"anda_brain_execute_kip_readonly"));

        let recall = tools
            .iter()
            .find(|tool| tool.name == "anda_brain_recall_memory")
            .unwrap();
        assert!(
            recall
                .description
                .as_deref()
                .is_some_and(|description| description.contains("Ask a natural-language question"))
        );
        assert!(
            recall
                .description
                .as_deref()
                .is_some_and(|description| description.contains("can take over 60 seconds"))
        );
        let recall_schema = Value::Object(recall.input_schema.as_ref().clone());
        let recall_properties = recall_schema["properties"].as_object().unwrap();
        assert_eq!(recall_properties["query"]["type"].as_str(), Some("string"));
        assert!(recall_properties.contains_key("context"));
        assert!(
            recall_schema["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|field| field.as_str() == Some("query"))
        );
        assert_eq!(
            recall.annotations.as_ref().unwrap().read_only_hint,
            Some(true)
        );

        let remember = tools
            .iter()
            .find(|tool| tool.name == "anda_brain_remember_conversation")
            .unwrap();
        assert!(
            remember
                .description
                .as_deref()
                .is_some_and(|description| description.contains("Encode conversation messages"))
        );
        assert!(
            remember
                .description
                .as_deref()
                .is_some_and(|description| description.contains("queuing asynchronous formation"))
        );
        let remember_schema = Value::Object(remember.input_schema.as_ref().clone());
        let remember_properties = remember_schema["properties"].as_object().unwrap();
        assert!(remember_properties.contains_key("messages"));
        assert!(
            remember_schema["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|field| field.as_str() == Some("messages"))
        );
        assert_eq!(
            remember.annotations.as_ref().unwrap().read_only_hint,
            Some(false)
        );

        let maintenance = tools
            .iter()
            .find(|tool| tool.name == "anda_brain_run_maintenance")
            .unwrap();
        assert!(
            maintenance
                .description
                .as_deref()
                .is_some_and(|description| description.contains("asynchronous maintenance"))
        );

        let info = tools
            .iter()
            .find(|tool| tool.name == "anda_brain_get_space_info")
            .unwrap();
        assert!(
            info.description
                .as_deref()
                .is_some_and(|description| description.contains("statistics and metadata"))
        );
        let info_schema = Value::Object(info.input_schema.as_ref().clone());
        assert_eq!(info_schema["type"].as_str(), Some("object"));
    }

    #[tokio::test]
    async fn space_info_tool_returns_configured_space() {
        let server = create_server("mcp_space_info").await;

        let result = server
            .get_space_info_for(&test_access("mcp_space_info"))
            .await
            .unwrap();
        let value = result.structured_content.unwrap();

        assert_eq!(value["id"], "mcp_space_info");
        assert_eq!(result.is_error, Some(false));
    }

    #[tokio::test]
    async fn remember_tool_runs_through_formation_agent() {
        let server = create_server("mcp_remember").await;
        let result = server
            .remember_conversation_for(
                &test_access("mcp_remember"),
                RememberConversationInput {
                    messages: vec![McpMessage {
                        role: "user".to_string(),
                        content: McpMessageContent::Text("Alice likes dark mode".to_string()),
                        name: None,
                        user: None,
                        timestamp: None,
                    }],
                    context: Some(McpInputContext {
                        counterparty: Some("alice".to_string()),
                        agent: Some("test-agent".to_string()),
                        source: Some("mcp-test".to_string()),
                        topic: Some("preferences".to_string()),
                    }),
                    timestamp: Some("2026-06-25T00:00:00Z".to_string()),
                },
            )
            .await
            .unwrap();

        assert_eq!(result.is_error, Some(false));
        assert!(result.content[0].as_text().is_some());
        let value = result.structured_content.as_ref().unwrap();
        assert!(value["conversation"].is_number());
    }

    #[tokio::test]
    async fn wiki_tools_scope_anonymous_and_labeled_callers() {
        use crate::types::{AddSpaceTokenInput, UpdateSpaceInput};
        use crate::wiki::WikiCommitInput;

        // Auth enabled: an empty bearer is an anonymous caller, not the
        // synthetic dev CWT.
        let signing_key = test_signing_key();
        let app = test_app_state("mcp_wiki_acl", vec![signing_key.verifying_key()]);
        let space_id = "mcp_wiki_acl_space";
        app.admin_create_space(
            SELF_USER_ID,
            SELF_USER_ID,
            space_id.to_string(),
            1,
            unix_ms(),
        )
        .await
        .unwrap();
        let space = app.load_space(space_id, false).await.unwrap();
        space
            .update(
                UpdateSpaceInput {
                    public: Some(true),
                    ..Default::default()
                },
                unix_ms(),
            )
            .await
            .unwrap();
        space
            .wiki
            .commit(
                "owner".to_string(),
                WikiCommitInput {
                    title: "公开文档".to_string(),
                    content: "# 公开文档\n\n公开探针：风铃海岸。\n".to_string(),
                    ..Default::default()
                },
                unix_ms(),
            )
            .await
            .unwrap();
        space
            .wiki
            .commit(
                "owner".to_string(),
                WikiCommitInput {
                    title: "机密文档".to_string(),
                    content: "# 机密文档\n\n机密探针:曙光矩阵。\n".to_string(),
                    acl_label: Some("secret".to_string()),
                    ..Default::default()
                },
                unix_ms(),
            )
            .await
            .unwrap();
        space
            .add_space_token(
                "STsecret-reader".to_string(),
                AddSpaceTokenInput {
                    scope: TokenScope::Read,
                    name: "sec".to_string(),
                    expires_at: None,
                    labels: Some(vec!["secret".to_string()]),
                },
                unix_ms(),
            )
            .await
            .unwrap();
        space
            .add_space_token(
                "STwiki-writer".to_string(),
                AddSpaceTokenInput {
                    scope: TokenScope::Write,
                    name: "writer".to_string(),
                    expires_at: None,
                    labels: None,
                },
                unix_ms(),
            )
            .await
            .unwrap();

        let server = AndaBrainMcpServer::new(
            app,
            McpServerConfig {
                space_id: space_id.to_string(),
                auth_token: None,
                auto_create_space: false,
                auto_create_tier: 1,
                dynamic_space_from_path: false,
                remote_path_prefix: "/mcp".to_string(),
            },
        );
        let hits_of = |result: CallToolResult| {
            result.structured_content.unwrap()["hits"]
                .as_array()
                .unwrap()
                .len()
        };

        // Launch review P0-1: an anonymous reader of a public space gets the
        // unlabeled-only view, never the unrestricted one.
        let anon = test_access(space_id);
        let secret_probe = server
            .wiki_search_for(
                &anon,
                WikiSearchToolInput {
                    query: "曙光矩阵".to_string(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(hits_of(secret_probe), 0);
        let open_probe = server
            .wiki_search_for(
                &anon,
                WikiSearchToolInput {
                    query: "风铃海岸".to_string(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(hits_of(open_probe), 1);

        // A labeled token keeps its granted labels on the public space
        // instead of degrading to anonymous.
        let labeled = McpAccess {
            space_id: space_id.to_string(),
            auth_token: "STsecret-reader".to_string(),
            sharding: None,
        };
        let granted = server
            .wiki_search_for(
                &labeled,
                WikiSearchToolInput {
                    query: "曙光矩阵".to_string(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(hits_of(granted), 1);

        // Launch review P1-3: label-restricted tokens cannot use agentic
        // recall (its wiki tools span all labels).
        let err = server
            .recall_memory_for(
                &labeled,
                RecallMemoryInput {
                    query: "机密内容是什么?".to_string(),
                    context: None,
                },
            )
            .await
            .unwrap_err();
        assert!(err.message.contains("unrestricted"), "{}", err.message);

        // Launch review P1-2: MCP commits record the real token subject.
        let writer = McpAccess {
            space_id: space_id.to_string(),
            auth_token: "STwiki-writer".to_string(),
            sharding: None,
        };
        let committed = server
            .wiki_commit_for(
                &writer,
                WikiCommitToolInput {
                    title: "白皮书".to_string(),
                    content: "# 白皮书\n\n提交自 MCP。\n".to_string(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let value = committed.structured_content.unwrap();
        assert_eq!(value["doc"]["created_by"].as_str(), Some("st:writer"));
        assert_eq!(value["version"]["author"].as_str(), Some("st:writer"));
    }

    #[tokio::test]
    async fn auth_enabled_requires_configured_token() {
        let mut bytes = [0x66; 32];
        bytes[0] = 0x58;
        let key = VerifyingKey::from_bytes(&bytes).unwrap();
        let app = test_app_state("mcp_auth", vec![key]);
        app.admin_create_space(
            SELF_USER_ID,
            SELF_USER_ID,
            "mcp_auth_space".to_string(),
            1,
            unix_ms(),
        )
        .await
        .unwrap();
        let server = AndaBrainMcpServer::new(
            app,
            McpServerConfig {
                space_id: "mcp_auth_space".to_string(),
                auth_token: None,
                auto_create_space: false,
                auto_create_tier: 1,
                dynamic_space_from_path: false,
                remote_path_prefix: "/mcp".to_string(),
            },
        );

        let err = server
            .get_space_info_for(&test_access("mcp_auth_space"))
            .await
            .unwrap_err();

        assert!(err.message.contains("access denied"));
    }

    #[tokio::test]
    async fn remote_auto_create_requires_cwt_auth_to_be_enabled() {
        let app = test_app_state("mcp_auto_create_no_auth", vec![]);
        let server = AndaBrainMcpServer::new(
            app.clone(),
            McpServerConfig {
                space_id: "unused".to_string(),
                auth_token: None,
                auto_create_space: true,
                auto_create_tier: 1,
                dynamic_space_from_path: true,
                remote_path_prefix: "/mcp".to_string(),
            },
        );
        let space_id = "mcp_auto_create_no_auth_space";

        let err = match server
            .load_authorized_space(TokenScope::Write, &test_access(space_id))
            .await
        {
            Ok(_) => panic!("expected auth-disabled remote auto-create to fail"),
            Err(err) => err,
        };

        assert!(err.message.contains("requires ED25519_PUBKEYS"));
        assert!(app.load_space(space_id, false).await.is_err());
    }

    #[tokio::test]
    async fn remote_auto_create_requires_write_cwt_before_creating_space() {
        let signing_key = test_signing_key();
        let app = test_app_state("mcp_auto_create_auth", vec![signing_key.verifying_key()]);
        let server = AndaBrainMcpServer::new(
            app.clone(),
            McpServerConfig {
                space_id: "unused".to_string(),
                auth_token: None,
                auto_create_space: true,
                auto_create_tier: 1,
                dynamic_space_from_path: true,
                remote_path_prefix: "/mcp".to_string(),
            },
        );
        let space_id = "mcp_auto_create_guard";

        let err = match server
            .load_authorized_space(TokenScope::Read, &test_access(space_id))
            .await
        {
            Ok(_) => panic!("expected unauthenticated auto-create to fail"),
            Err(err) => err,
        };
        assert!(err.message.contains("access denied"));
        assert!(app.load_space(space_id, false).await.is_err());

        let read_cwt = signed_token(&signing_key, SELF_USER_ID, space_id, "read");
        let err = match server
            .load_authorized_space(
                TokenScope::Read,
                &McpAccess {
                    space_id: space_id.to_string(),
                    auth_token: read_cwt,
                    sharding: None,
                },
            )
            .await
        {
            Ok(_) => panic!("expected read CWT auto-create to fail"),
            Err(err) => err,
        };
        assert!(err.message.contains("access denied"));
        assert!(app.load_space(space_id, false).await.is_err());

        let write_cwt = signed_token(&signing_key, SELF_USER_ID, space_id, "write");
        let created = server
            .load_authorized_space(
                TokenScope::Write,
                &McpAccess {
                    space_id: space_id.to_string(),
                    auth_token: write_cwt,
                    sharding: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(created.get_info().id, space_id);
    }

    #[tokio::test]
    async fn streamable_http_endpoint_uses_space_from_path() {
        let app = test_app_state("mcp_http", vec![]);
        app.admin_create_space(
            SELF_USER_ID,
            SELF_USER_ID,
            "mcp_http_space".to_string(),
            1,
            unix_ms(),
        )
        .await
        .unwrap();

        let cancel_token = CancellationToken::new();
        let service = build_streamable_http_service(
            app,
            McpHttpServerConfig {
                path_prefix: "/mcp".to_string(),
                auto_create_space: false,
                auto_create_tier: 1,
                allowed_hosts: vec!["127.0.0.1".to_string()],
                allowed_origins: vec![],
                stateful_mode: true,
                json_response: false,
                sse_keep_alive_secs: None,
            },
            cancel_token.child_token(),
        );
        let router = axum::Router::new().nest_service("/mcp", service);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_cancel = cancel_token.clone();
        let server_handle = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async move { server_cancel.cancelled_owned().await })
                .await
                .unwrap();
        });

        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let endpoint = format!("http://{addr}/mcp/mcp_http_space");
        let initialize = client
            .post(&endpoint)
            .header(header::ACCEPT, "application/json, text/event-stream")
            .header(header::CONTENT_TYPE, "application/json")
            .json(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {
                        "name": "anda-brain-test",
                        "version": "0.0.0"
                    }
                }
            }))
            .send()
            .await
            .unwrap();

        let initialize_status = initialize.status();
        let initialize_headers = initialize.headers().clone();
        let initialize_body = initialize.text().await.unwrap();
        assert!(
            initialize_status.is_success(),
            "initialize failed: {initialize_status} {initialize_body}"
        );
        let session_id = initialize_headers
            .get("mcp-session-id")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();

        let initialized = client
            .post(&endpoint)
            .header(header::ACCEPT, "application/json, text/event-stream")
            .header(header::CONTENT_TYPE, "application/json")
            .header("mcp-session-id", &session_id)
            .json(&json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized"
            }))
            .send()
            .await
            .unwrap();
        assert!(initialized.status().is_success());

        let call = client
            .post(&endpoint)
            .header(header::ACCEPT, "application/json, text/event-stream")
            .header(header::CONTENT_TYPE, "application/json")
            .header("mcp-session-id", &session_id)
            .json(&json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "anda_brain_get_space_info",
                    "arguments": {}
                }
            }))
            .send()
            .await
            .unwrap();

        assert!(call.status().is_success());
        let body = call.text().await.unwrap();
        assert!(body.contains("mcp_http_space"));

        cancel_token.cancel();
        server_handle.await.unwrap();
    }

    #[test]
    fn http_helpers_extract_space_token_and_sharding() {
        assert_eq!(
            space_id_from_mcp_path("/mcp/alice_space", "/mcp").unwrap(),
            "alice_space"
        );
        assert_eq!(
            space_id_from_mcp_path("/mcp/alice_space/events", "/mcp").unwrap(),
            "alice_space"
        );
        assert_eq!(
            space_id_from_mcp_path("/alice_space", "/mcp").unwrap(),
            "alice_space"
        );
        assert!(space_id_from_mcp_path("/mcp", "/mcp").is_err());

        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer ST-token".parse().unwrap());
        headers.insert("X-Shard", "7".parse().unwrap());

        assert_eq!(
            bearer_token_from_headers(&headers).as_deref(),
            Some("ST-token")
        );
        assert_eq!(sharding_from_headers(&headers), Some(7));
    }
}

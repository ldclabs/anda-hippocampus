use anda_engine::unix_ms;
use axum::{
    Json,
    body::Bytes,
    extract::{Path, Query, State},
    response::{IntoResponse, Response},
};
use http::StatusCode;
use ic_auth_types::ByteArrayB64;
use rand::Rng;
use serde::Deserialize;
use serde_json::json;

use crate::{
    agents::SELF_USER_ID,
    payload::{Accept, AppError, ContentType, HeaderVals, PayloadFormat, RpcResponse, StringOr},
    space::{AppState, Space},
    types::*,
    wiki::{
        WikiAccess, WikiCommitInput, WikiError, WikiImportInput, WikiListDocsInput, WikiReadInput,
        WikiSearchInput, WikiSelector, WikiVerifyInput,
    },
};

const SKILL_MARKDOWN: &str = include_str!("../SKILL.md");
const FAVICON: &[u8] = include_bytes!("../favicon.ico");
const APPLE_TOUCH_ICON: &[u8] = include_bytes!("../apple-touch-icon.webp");

fn ensure_sharding(app: &AppState, sharding: u32) -> Result<(), AppError> {
    if sharding != app.sharding {
        return Err(AppError::bad_request(format!(
            "space_id sharding {} does not match server sharding {}",
            sharding, app.sharding
        )));
    }
    Ok(())
}

pub async fn favicon() -> Response {
    Response::builder()
        .header("Content-Type", "image/x-icon")
        .body(FAVICON.into())
        .unwrap()
}

pub async fn apple_touch_icon() -> Response {
    Response::builder()
        .header("Content-Type", "image/webp")
        .body(APPLE_TOUCH_ICON.into())
        .unwrap()
}

pub async fn get_information(State(app): State<AppState>) -> impl IntoResponse {
    let info = json!({
        "name": app.app_name,
        "version": app.app_version,
        "sharding": app.sharding,
         "description": "Brain is a long-term memory system for LLM agents, providing persistent storage and retrieval of knowledge across interactions. It enables agents to remember facts, preferences, relationships, past events, and any other information that can be useful for answering questions and making decisions. Brain organizes memories in a structured way, allowing efficient search and recall based on natural language queries. By using Brain, agents can maintain context and continuity over time, improving their ability to assist users effectively.",
    });

    Json(info)
}

pub async fn get_skill(State(_app): State<AppState>) -> impl IntoResponse {
    ContentType::Markdown(true).response(SKILL_MARKDOWN)
}

/// GET /v1/{space_id}/info
pub async fn get_info(
    State(app): State<AppState>,
    Path(space_id): Path<String>,
    Accept(ct, _): Accept,
    HeaderVals(token, sharding): HeaderVals,
) -> Result<impl IntoResponse, AppError> {
    ensure_sharding(&app, sharding)?;

    let now_ms = unix_ms();
    let t = app
        .check_auth_if(&token, &space_id, TokenScope::Read, now_ms)
        .map_err(|_| AppError::unauthorized())?;

    let space = app
        .load_space(&space_id, false)
        .await
        .map_err(AppError::bad_request)?;

    if !space.is_public() && t.is_none() {
        // 如果空间不是公开的，且没有验证 CWToken，则验证 SpaceToken
        space
            .verify_space_token(token, TokenScope::Read, now_ms)
            .map_err(|_| AppError::unauthorized())?;
    }

    let rt = space.get_info();
    Ok(ct.response(RpcResponse::success(rt)))
}

/// GET /v1/{space_id}/formation_status
pub async fn get_formation_status(
    State(app): State<AppState>,
    Path(space_id): Path<String>,
    Accept(ct, _): Accept,
    HeaderVals(token, sharding): HeaderVals,
) -> Result<impl IntoResponse, AppError> {
    ensure_sharding(&app, sharding)?;

    let now_ms = unix_ms();
    let t = app
        .check_auth_if(&token, &space_id, TokenScope::Read, now_ms)
        .map_err(|_| AppError::unauthorized())?;

    let space = app
        .load_space(&space_id, false)
        .await
        .map_err(AppError::bad_request)?;

    if !space.is_public() && t.is_none() {
        // 如果空间不是公开的，且没有验证 CWToken，则验证 SpaceToken
        space
            .verify_space_token(token, TokenScope::Read, now_ms)
            .map_err(|_| AppError::unauthorized())?;
    }

    let rt = space.formation_status();
    Ok(ct.response(RpcResponse::success(rt)))
}

/// POST /v1/{space_id}/formation
pub async fn post_formation(
    State(app): State<AppState>,
    Path(space_id): Path<String>,
    Accept(ct, _): Accept,
    HeaderVals(token, sharding): HeaderVals,
    body: Bytes,
) -> Result<Response, AppError> {
    ensure_sharding(&app, sharding)?;

    let input: StringOr<FormationInput> = ct.parse_body(&body).map_err(AppError::bad_request)?;

    let now_ms = unix_ms();
    let t = app
        .check_auth_if(&token, &space_id, TokenScope::Write, now_ms)
        .map_err(|_| AppError::unauthorized())?;

    let space = app
        .load_space(&space_id, false)
        .await
        .map_err(AppError::bad_request)?;

    if t.is_none() {
        // 如果没有验证 CWToken，则验证 SpaceToken
        space
            .verify_space_token(token, TokenScope::Write, now_ms)
            .map_err(|_| AppError::unauthorized())?;
    }

    // 使用匿名 caller 进行 ingestions 和 queries
    let rt = space
        .ingest(SELF_USER_ID, input)
        .await
        .map_err(AppError::bad_request)?;
    match ct.response_type() {
        ContentType::Markdown(_) => Ok(ct.response(rt.content).into_response()),
        _ => Ok(ct.response(RpcResponse::success(rt)).into_response()),
    }
}

/// POST /v1/{space_id}/recall
pub async fn post_recall(
    State(app): State<AppState>,
    Path(space_id): Path<String>,
    Accept(ct, _): Accept,
    HeaderVals(token, sharding): HeaderVals,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    ensure_sharding(&app, sharding)?;

    let input: StringOr<RecallInput> = ct.parse_body(&body).map_err(AppError::bad_request)?;

    let now_ms = unix_ms();
    let t = app
        .check_auth_if(&token, &space_id, TokenScope::Read, now_ms)
        .map_err(|_| AppError::unauthorized())?;

    let space = app
        .load_space(&space_id, false)
        .await
        .map_err(AppError::bad_request)?;

    // A supplied token is verified even on public spaces so its ACL
    // restriction is honored rather than silently widened.
    let st = wiki_read_token(&space, &t, token, now_ms)?;
    if st.as_ref().is_some_and(|st| st.labels.is_some()) {
        // RecallAgent's wiki tools span all labels; a label-restricted token
        // cannot use agentic recall (mirrors the /wiki/events guard).
        return Err(AppError::with_status(
            StatusCode::FORBIDDEN,
            "recall requires an unrestricted token",
        ));
    }

    // 使用固定的 caller 进行 ingestions 和 queries
    let rt = space
        .query(SELF_USER_ID, input)
        .await
        .map_err(AppError::bad_request)?;
    Ok(ct.response(RpcResponse::success(rt)))
}

/// POST /v1/{space_id}/recall_structured
///
/// Recall with machine-readable provenance (memory evolution plan, M4):
/// answer + trace-derived memory citations + the model's self-reported
/// `found`/`uncertainty`, so callers can decide to assert, hedge, or ask.
pub async fn post_recall_structured(
    State(app): State<AppState>,
    Path(space_id): Path<String>,
    Accept(ct, _): Accept,
    HeaderVals(token, sharding): HeaderVals,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    ensure_sharding(&app, sharding)?;

    let input: StringOr<RecallInput> = ct.parse_body(&body).map_err(AppError::bad_request)?;

    let now_ms = unix_ms();
    let t = app
        .check_auth_if(&token, &space_id, TokenScope::Read, now_ms)
        .map_err(|_| AppError::unauthorized())?;

    let space = app
        .load_space(&space_id, false)
        .await
        .map_err(AppError::bad_request)?;

    let st = wiki_read_token(&space, &t, token, now_ms)?;
    if st.as_ref().is_some_and(|st| st.labels.is_some()) {
        // Same guard as /recall: the agent's wiki tools span all labels.
        return Err(AppError::with_status(
            StatusCode::FORBIDDEN,
            "recall requires an unrestricted token",
        ));
    }

    let rt = space
        .query_structured(SELF_USER_ID, input)
        .await
        .map_err(AppError::bad_request)?;
    Ok(ct.response(RpcResponse::success(rt)))
}

/// POST /v1/{space_id}/probe
///
/// Metamemory existence check (memory evolution plan, M5): LLM-free hybrid
/// search that tells the caller whether a full recall is worth paying for.
pub async fn post_probe(
    State(app): State<AppState>,
    Path(space_id): Path<String>,
    Accept(ct, _): Accept,
    HeaderVals(token, sharding): HeaderVals,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    ensure_sharding(&app, sharding)?;

    let input: StringOr<ProbeInput> = ct.parse_body(&body).map_err(AppError::bad_request)?;
    // A bare string body is the query itself.
    let input = match input {
        StringOr::String(query) => ProbeInput { query, limit: None },
        StringOr::Value(input) => input,
    };

    let now_ms = unix_ms();
    let t = app
        .check_auth_if(&token, &space_id, TokenScope::Read, now_ms)
        .map_err(|_| AppError::unauthorized())?;

    let space = app
        .load_space(&space_id, false)
        .await
        .map_err(AppError::bad_request)?;

    if !space.is_public() && t.is_none() {
        space
            .verify_space_token(token, TokenScope::Read, now_ms)
            .map_err(|_| AppError::unauthorized())?;
    }

    let rt = space
        .probe_memory(&input.query, input.limit)
        .await
        .map_err(AppError::bad_request)?;
    Ok(ct.response(RpcResponse::success(rt)))
}

/// POST /v1/{space_id}/memory/pin
///
/// Pins/unpins a graph entity (memory evolution plan, M6); pinned memories
/// are exempt from confidence decay.
pub async fn post_memory_pin(
    State(app): State<AppState>,
    Path(space_id): Path<String>,
    Accept(ct, _): Accept,
    HeaderVals(token, sharding): HeaderVals,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    ensure_sharding(&app, sharding)?;

    let input: StringOr<MemoryPinInput> = ct.parse_body(&body).map_err(AppError::bad_request)?;
    let input = input
        .value()
        .map_err(|_| AppError::bad_request("expected a JSON object body".to_string()))?;

    let now_ms = unix_ms();
    let t = app
        .check_auth_if(&token, &space_id, TokenScope::Write, now_ms)
        .map_err(|_| AppError::unauthorized())?;

    let space = app
        .load_space(&space_id, false)
        .await
        .map_err(AppError::bad_request)?;

    if t.is_none() {
        space
            .verify_space_token(token, TokenScope::Write, now_ms)
            .map_err(|_| AppError::unauthorized())?;
    }

    let updated = space
        .pin_memory(&input.entity, input.pinned)
        .await
        .map_err(AppError::bad_request)?;
    Ok(ct.response(RpcResponse::success(json!({
        "entity": input.entity,
        "pinned": input.pinned,
        "updated": updated,
    }))))
}

/// POST /v1/{space_id}/memory/forget
///
/// Privacy-grade deletion (memory evolution plan, M6). Run with
/// `dry_run: true` first; the report shows what would be removed.
pub async fn post_memory_forget(
    State(app): State<AppState>,
    Path(space_id): Path<String>,
    Accept(ct, _): Accept,
    HeaderVals(token, sharding): HeaderVals,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    ensure_sharding(&app, sharding)?;

    let input: StringOr<MemoryForgetInput> = ct.parse_body(&body).map_err(AppError::bad_request)?;
    let input = input
        .value()
        .map_err(|_| AppError::bad_request("expected a JSON object body".to_string()))?;

    let now_ms = unix_ms();
    let t = app
        .check_auth_if(&token, &space_id, TokenScope::Write, now_ms)
        .map_err(|_| AppError::unauthorized())?;

    let space = app
        .load_space(&space_id, false)
        .await
        .map_err(AppError::bad_request)?;

    if t.is_none() {
        space
            .verify_space_token(token, TokenScope::Write, now_ms)
            .map_err(|_| AppError::unauthorized())?;
    }

    let rt = space
        .forget_memory(input)
        .await
        .map_err(AppError::bad_request)?;
    Ok(ct.response(RpcResponse::success(rt)))
}

/// GET /v1/{space_id}/memory_status
///
/// Memory observability snapshot (memory evolution plan, M12):
/// incrementally-maintained counters, derived rates, graph counts, and the
/// latest settlement/self-test/shadow reports.
pub async fn get_memory_status(
    State(app): State<AppState>,
    Path(space_id): Path<String>,
    Accept(ct, _): Accept,
    HeaderVals(token, sharding): HeaderVals,
) -> Result<impl IntoResponse, AppError> {
    ensure_sharding(&app, sharding)?;

    let now_ms = unix_ms();
    let t = app
        .check_auth_if(&token, &space_id, TokenScope::Read, now_ms)
        .map_err(|_| AppError::unauthorized())?;

    let space = app
        .load_space(&space_id, false)
        .await
        .map_err(AppError::bad_request)?;

    if !space.is_public() && t.is_none() {
        space
            .verify_space_token(token, TokenScope::Read, now_ms)
            .map_err(|_| AppError::unauthorized())?;
    }

    let rt = space.memory_status().await;
    Ok(ct.response(RpcResponse::success(rt)))
}

/// POST /v1/{space_id}/management/shadow_eval
///
/// On-demand shadow evaluation (memory evolution plan, M11): compares a
/// candidate memory policy against the current one on forked copies of the
/// space, replaying recent real recall queries. Expensive (LLM replays +
/// judging); management-scoped.
pub async fn post_shadow_eval(
    State(app): State<AppState>,
    Path(space_id): Path<String>,
    Accept(ct, _): Accept,
    HeaderVals(token, sharding): HeaderVals,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    ensure_sharding(&app, sharding)?;

    let now_ms = unix_ms();
    let _ = app
        .check_auth(&token, &space_id, TokenScope::Write, now_ms)
        .map_err(|_| AppError::unauthorized())?;

    let input: ShadowEvalInput = ct
        .parse_body(&body)
        .map_err(AppError::bad_request)?
        .value()
        .map_err(|_| AppError::bad_request("expected a JSON object body".to_string()))?;

    let rt = app
        .run_shadow_eval(&space_id, input)
        .await
        .map_err(AppError::bad_request)?;
    Ok(ct.response(RpcResponse::success(rt)))
}

// ─── Wiki ─────────────────────────────────────────────────────────────────────

fn wiki_error(err: WikiError) -> AppError {
    match &err {
        WikiError::Conflict {
            current_version,
            current_checksum,
            updated_by,
            updated_at,
        } => AppError {
            status: StatusCode::CONFLICT,
            message: err.to_string(),
            data: Some(json!({
                "current_version": current_version,
                "current_checksum": current_checksum,
                "updated_by": updated_by,
                "updated_at": updated_at,
            })),
        },
        WikiError::TooLarge { .. } => {
            AppError::with_status(StatusCode::PAYLOAD_TOO_LARGE, err.to_string())
        }
        WikiError::NotFound(_) => AppError::with_status(StatusCode::NOT_FOUND, err.to_string()),
        WikiError::Invalid(_) => AppError::with_status(StatusCode::BAD_REQUEST, err.to_string()),
        WikiError::Db(_) => {
            AppError::with_status(StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
        }
    }
}

/// Resolves the audit actor: the authenticated CWT user, else a stable
/// space-token identity, else the local dev identity. Shared with the MCP
/// channel so both audit trails name identical subjects.
pub(crate) fn wiki_actor(t: &Option<CWToken>, st: Option<&SpaceToken>) -> String {
    if let Some(t) = t {
        return t.user.to_string();
    }
    match st {
        Some(st) if !st.name.trim().is_empty() => format!("st:{}", st.name.trim()),
        Some(_) => "st:unnamed".to_string(),
        None => SELF_USER_ID.to_string(),
    }
}

/// Verifies read access and returns the space token (for its ACL labels).
/// A supplied token is verified even on public spaces so a labeled token
/// keeps its granted labels instead of degrading to the anonymous view;
/// only absent/invalid credentials fall back to anonymous there.
fn wiki_read_token(
    space: &Space,
    t: &Option<CWToken>,
    token: String,
    now_ms: u64,
) -> Result<Option<SpaceToken>, AppError> {
    if t.is_some() {
        return Ok(None);
    }
    match space.verify_space_token(token, TokenScope::Read, now_ms) {
        Ok(st) => Ok(Some(st)),
        Err(_) if space.is_public() => Ok(None),
        Err(_) => Err(AppError::unauthorized()),
    }
}

/// Read scope resolution (PRD §8.2): CWT holders and label-less space
/// tokens are unrestricted; labeled tokens see unlabeled content plus their
/// labels; anonymous public-space readers see unlabeled content only.
/// Shared with the MCP channel so the two never diverge (the launch review's
/// P0-1 was exactly such a divergence).
pub(crate) fn wiki_read_access(t: &Option<CWToken>, st: Option<&SpaceToken>) -> WikiAccess {
    let labels = if t.is_some() {
        None
    } else if let Some(st) = st {
        st.labels.clone()
    } else {
        Some(Vec::new())
    };
    WikiAccess {
        actor: wiki_actor(t, st),
        labels,
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct WikiPageQuery {
    pub cursor: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
pub struct WikiDocsQuery {
    pub namespace: Option<String>,
    pub status: Option<String>,
    pub tag: Option<String>,
    pub cursor: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
pub struct WikiContentQuery {
    pub version: Option<u64>,
    pub anchor: Option<String>,
    pub start: Option<u64>,
    pub end: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
pub struct WikiEventsQuery {
    pub kind: Option<String>,
    pub doc_id: Option<u64>,
    pub cursor: Option<String>,
    pub limit: Option<usize>,
}

/// POST /v1/{space_id}/wiki/docs — commit (create or CAS update)
pub async fn post_wiki_commit(
    State(app): State<AppState>,
    Path(space_id): Path<String>,
    Accept(ct, _): Accept,
    HeaderVals(token, sharding): HeaderVals,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    ensure_sharding(&app, sharding)?;

    let input: StringOr<WikiCommitInput> = ct.parse_body(&body).map_err(AppError::bad_request)?;
    let input = match input {
        StringOr::String(content) => WikiCommitInput::from_markdown(content),
        StringOr::Value(input) => input,
    };

    let now_ms = unix_ms();
    let t = app
        .check_auth_if(&token, &space_id, TokenScope::Write, now_ms)
        .map_err(|_| AppError::unauthorized())?;
    let space = app
        .load_space(&space_id, false)
        .await
        .map_err(AppError::bad_request)?;
    let st = match &t {
        Some(_) => None,
        None => Some(
            space
                .verify_space_token(token, TokenScope::Write, now_ms)
                .map_err(|_| AppError::unauthorized())?,
        ),
    };

    let actor = wiki_actor(&t, st.as_ref());
    let rt = space
        .wiki
        .commit(actor, input, now_ms)
        .await
        .map_err(wiki_error)?;
    Ok(ct.response(RpcResponse::success(rt)))
}

/// GET /v1/{space_id}/wiki/docs
pub async fn list_wiki_docs(
    State(app): State<AppState>,
    Path(space_id): Path<String>,
    Query(q): Query<WikiDocsQuery>,
    Accept(ct, _): Accept,
    HeaderVals(token, sharding): HeaderVals,
) -> Result<impl IntoResponse, AppError> {
    ensure_sharding(&app, sharding)?;

    let now_ms = unix_ms();
    let t = app
        .check_auth_if(&token, &space_id, TokenScope::Read, now_ms)
        .map_err(|_| AppError::unauthorized())?;
    let space = app
        .load_space(&space_id, false)
        .await
        .map_err(AppError::bad_request)?;
    let st = wiki_read_token(&space, &t, token, now_ms)?;
    let access = wiki_read_access(&t, st.as_ref());

    let rt = space
        .wiki
        .list_docs_scoped(
            &access,
            WikiListDocsInput {
                namespace: q.namespace,
                status: q.status,
                tag: q.tag,
                cursor: q.cursor,
                limit: q.limit,
            },
        )
        .await
        .map_err(wiki_error)?;
    Ok(ct.response(RpcResponse {
        result: Some(rt.docs),
        error: None,
        next_cursor: rt.next_cursor,
    }))
}

/// GET /v1/{space_id}/wiki/docs/{doc_id} — metadata + TOC
pub async fn get_wiki_doc(
    State(app): State<AppState>,
    Path((space_id, doc_id)): Path<(String, u64)>,
    Accept(ct, _): Accept,
    HeaderVals(token, sharding): HeaderVals,
) -> Result<impl IntoResponse, AppError> {
    ensure_sharding(&app, sharding)?;

    let now_ms = unix_ms();
    let t = app
        .check_auth_if(&token, &space_id, TokenScope::Read, now_ms)
        .map_err(|_| AppError::unauthorized())?;
    let space = app
        .load_space(&space_id, false)
        .await
        .map_err(AppError::bad_request)?;
    let st = wiki_read_token(&space, &t, token, now_ms)?;
    let access = wiki_read_access(&t, st.as_ref());

    let doc = space
        .wiki
        .get_doc_scoped(&access, doc_id)
        .await
        .map_err(wiki_error)?;
    let toc = space
        .wiki
        .read_scoped(
            &access,
            WikiReadInput {
                doc_id,
                version: None,
                selector: WikiSelector::Toc,
            },
            now_ms,
        )
        .await
        .map_err(wiki_error)?;
    Ok(ct.response(RpcResponse::success(json!({
        "doc": doc,
        "toc": toc.toc,
    }))))
}

/// GET /v1/{space_id}/wiki/docs/{doc_id}/content
///
/// `?anchor=` reads one section, `?start=&end=` a byte range, neither reads
/// the bounded full text; `?version=` time-travels.
pub async fn get_wiki_content(
    State(app): State<AppState>,
    Path((space_id, doc_id)): Path<(String, u64)>,
    Query(q): Query<WikiContentQuery>,
    Accept(ct, _): Accept,
    HeaderVals(token, sharding): HeaderVals,
) -> Result<impl IntoResponse, AppError> {
    ensure_sharding(&app, sharding)?;

    let now_ms = unix_ms();
    let t = app
        .check_auth_if(&token, &space_id, TokenScope::Read, now_ms)
        .map_err(|_| AppError::unauthorized())?;
    let space = app
        .load_space(&space_id, false)
        .await
        .map_err(AppError::bad_request)?;
    let st = wiki_read_token(&space, &t, token, now_ms)?;
    let access = wiki_read_access(&t, st.as_ref());

    let selector = if let Some(anchor) = q.anchor {
        WikiSelector::Section { anchor }
    } else if let (Some(start), Some(end)) = (q.start, q.end) {
        WikiSelector::Range { start, end }
    } else {
        WikiSelector::Full
    };
    let rt = space
        .wiki
        .read_scoped(
            &access,
            WikiReadInput {
                doc_id,
                version: q.version,
                selector,
            },
            now_ms,
        )
        .await
        .map_err(wiki_error)?;
    Ok(ct.response(RpcResponse::success(rt)))
}

/// GET /v1/{space_id}/wiki/docs/{doc_id}/versions
pub async fn list_wiki_versions(
    State(app): State<AppState>,
    Path((space_id, doc_id)): Path<(String, u64)>,
    Query(pg): Query<WikiPageQuery>,
    Accept(ct, _): Accept,
    HeaderVals(token, sharding): HeaderVals,
) -> Result<impl IntoResponse, AppError> {
    ensure_sharding(&app, sharding)?;

    let now_ms = unix_ms();
    let t = app
        .check_auth_if(&token, &space_id, TokenScope::Read, now_ms)
        .map_err(|_| AppError::unauthorized())?;
    let space = app
        .load_space(&space_id, false)
        .await
        .map_err(AppError::bad_request)?;
    let st = wiki_read_token(&space, &t, token, now_ms)?;
    let access = wiki_read_access(&t, st.as_ref());

    let rt = space
        .wiki
        .list_versions_scoped(&access, doc_id, pg.cursor, pg.limit)
        .await
        .map_err(wiki_error)?;
    Ok(ct.response(RpcResponse {
        result: Some(rt.versions),
        error: None,
        next_cursor: rt.next_cursor,
    }))
}

/// POST /v1/{space_id}/wiki/docs/{doc_id}/archive
pub async fn post_wiki_archive(
    State(app): State<AppState>,
    Path((space_id, doc_id)): Path<(String, u64)>,
    Accept(ct, _): Accept,
    HeaderVals(token, sharding): HeaderVals,
) -> Result<impl IntoResponse, AppError> {
    wiki_set_archived(app, space_id, doc_id, ct, token, sharding, true).await
}

/// POST /v1/{space_id}/wiki/docs/{doc_id}/restore
pub async fn post_wiki_restore(
    State(app): State<AppState>,
    Path((space_id, doc_id)): Path<(String, u64)>,
    Accept(ct, _): Accept,
    HeaderVals(token, sharding): HeaderVals,
) -> Result<impl IntoResponse, AppError> {
    wiki_set_archived(app, space_id, doc_id, ct, token, sharding, false).await
}

async fn wiki_set_archived(
    app: AppState,
    space_id: String,
    doc_id: u64,
    ct: PayloadFormat,
    token: String,
    sharding: u32,
    archive: bool,
) -> Result<Response, AppError> {
    ensure_sharding(&app, sharding)?;

    let now_ms = unix_ms();
    let t = app
        .check_auth_if(&token, &space_id, TokenScope::Write, now_ms)
        .map_err(|_| AppError::unauthorized())?;
    let space = app
        .load_space(&space_id, false)
        .await
        .map_err(AppError::bad_request)?;
    let st = match &t {
        Some(_) => None,
        None => Some(
            space
                .verify_space_token(token, TokenScope::Write, now_ms)
                .map_err(|_| AppError::unauthorized())?,
        ),
    };

    let actor = wiki_actor(&t, st.as_ref());
    let rt = if archive {
        space.wiki.archive(actor, doc_id, now_ms).await
    } else {
        space.wiki.restore(actor, doc_id, now_ms).await
    }
    .map_err(wiki_error)?;
    Ok(ct.response(RpcResponse::success(rt)).into_response())
}

/// POST /v1/{space_id}/wiki/search
pub async fn post_wiki_search(
    State(app): State<AppState>,
    Path(space_id): Path<String>,
    Accept(ct, _): Accept,
    HeaderVals(token, sharding): HeaderVals,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    ensure_sharding(&app, sharding)?;

    let input: StringOr<WikiSearchInput> = ct.parse_body(&body).map_err(AppError::bad_request)?;
    let input = match input {
        StringOr::String(query) => WikiSearchInput::from_query(query),
        StringOr::Value(input) => input,
    };

    let now_ms = unix_ms();
    let t = app
        .check_auth_if(&token, &space_id, TokenScope::Read, now_ms)
        .map_err(|_| AppError::unauthorized())?;
    let space = app
        .load_space(&space_id, false)
        .await
        .map_err(AppError::bad_request)?;
    let st = wiki_read_token(&space, &t, token, now_ms)?;
    let access = wiki_read_access(&t, st.as_ref());

    let rt = space
        .wiki
        .search_scoped(&access, input, now_ms)
        .await
        .map_err(wiki_error)?;
    Ok(ct.response(RpcResponse::success(rt)))
}

/// POST /v1/{space_id}/wiki/verify — citation verification
pub async fn post_wiki_verify(
    State(app): State<AppState>,
    Path(space_id): Path<String>,
    Accept(ct, _): Accept,
    HeaderVals(token, sharding): HeaderVals,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    ensure_sharding(&app, sharding)?;

    let input: StringOr<WikiVerifyInput> = ct.parse_body(&body).map_err(AppError::bad_request)?;
    let input = match input {
        StringOr::String(uri) => WikiVerifyInput {
            uri: Some(uri),
            ..Default::default()
        },
        StringOr::Value(input) => input,
    };

    let now_ms = unix_ms();
    let t = app
        .check_auth_if(&token, &space_id, TokenScope::Read, now_ms)
        .map_err(|_| AppError::unauthorized())?;
    let space = app
        .load_space(&space_id, false)
        .await
        .map_err(AppError::bad_request)?;
    let st = wiki_read_token(&space, &t, token, now_ms)?;
    let access = wiki_read_access(&t, st.as_ref());
    let rt = space
        .wiki
        .verify_scoped(&access, input, now_ms)
        .await
        .map_err(wiki_error)?;
    Ok(ct.response(RpcResponse::success(rt)))
}

/// GET /v1/{space_id}/wiki/events
pub async fn list_wiki_events(
    State(app): State<AppState>,
    Path(space_id): Path<String>,
    Query(q): Query<WikiEventsQuery>,
    Accept(ct, _): Accept,
    HeaderVals(token, sharding): HeaderVals,
) -> Result<impl IntoResponse, AppError> {
    ensure_sharding(&app, sharding)?;

    let now_ms = unix_ms();
    let t = app
        .check_auth_if(&token, &space_id, TokenScope::Read, now_ms)
        .map_err(|_| AppError::unauthorized())?;
    let space = app
        .load_space(&space_id, false)
        .await
        .map_err(AppError::bad_request)?;
    let st = wiki_read_token(&space, &t, token, now_ms)?;
    let access = wiki_read_access(&t, st.as_ref());
    if access.labels.is_some() {
        // The audit log spans all labels; restricted tokens cannot read it.
        return Err(AppError::with_status(
            StatusCode::FORBIDDEN,
            "audit events require an unrestricted token",
        ));
    }

    let rt = space
        .wiki
        .list_events(q.kind, q.doc_id, q.cursor, q.limit)
        .await
        .map_err(wiki_error)?;
    Ok(ct.response(RpcResponse {
        result: Some(rt.events),
        error: None,
        next_cursor: rt.next_cursor,
    }))
}

#[derive(Debug, Default, Deserialize)]
pub struct WikiExportQuery {
    pub namespace: Option<String>,
}

/// POST /v1/{space_id}/wiki/import — OKF bundle import (requires All scope)
pub async fn post_wiki_import(
    State(app): State<AppState>,
    Path(space_id): Path<String>,
    Accept(ct, _): Accept,
    HeaderVals(token, sharding): HeaderVals,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    ensure_sharding(&app, sharding)?;

    let input: StringOr<WikiImportInput> = ct.parse_body(&body).map_err(AppError::bad_request)?;
    let StringOr::Value(input) = input else {
        return Err(AppError::bad_request(
            "wiki import expects a structured bundle body",
        ));
    };

    let now_ms = unix_ms();
    let t = app
        .check_auth_if(&token, &space_id, TokenScope::All, now_ms)
        .map_err(|_| AppError::unauthorized())?;
    let space = app
        .load_space(&space_id, false)
        .await
        .map_err(AppError::bad_request)?;
    let st = match &t {
        Some(_) => None,
        None => Some(
            space
                .verify_space_token(token, TokenScope::All, now_ms)
                .map_err(|_| AppError::unauthorized())?,
        ),
    };

    let actor = wiki_actor(&t, st.as_ref());
    let rt = space
        .wiki
        .import_bundle(actor, input, now_ms)
        .await
        .map_err(wiki_error)?;
    Ok(ct.response(RpcResponse::success(rt)))
}

/// GET /v1/{space_id}/wiki/export?namespace= — OKF bundle export (requires All scope)
pub async fn get_wiki_export(
    State(app): State<AppState>,
    Path(space_id): Path<String>,
    Query(q): Query<WikiExportQuery>,
    Accept(ct, _): Accept,
    HeaderVals(token, sharding): HeaderVals,
) -> Result<impl IntoResponse, AppError> {
    ensure_sharding(&app, sharding)?;

    let now_ms = unix_ms();
    let t = app
        .check_auth_if(&token, &space_id, TokenScope::All, now_ms)
        .map_err(|_| AppError::unauthorized())?;
    let space = app
        .load_space(&space_id, false)
        .await
        .map_err(AppError::bad_request)?;
    let st = match &t {
        Some(_) => None,
        None => Some(
            space
                .verify_space_token(token, TokenScope::All, now_ms)
                .map_err(|_| AppError::unauthorized())?,
        ),
    };

    let actor = wiki_actor(&t, st.as_ref());
    let rt = space
        .wiki
        .export_bundle(actor, q.namespace, now_ms)
        .await
        .map_err(wiki_error)?;
    Ok(ct.response(RpcResponse::success(rt)))
}

/// POST /v1/{space_id}/wiki/digest — distill pending wiki versions into the
/// Cognitive Nexus (requires the space to have wiki_digest enabled)
pub async fn post_wiki_digest(
    State(app): State<AppState>,
    Path(space_id): Path<String>,
    Accept(ct, _): Accept,
    HeaderVals(token, sharding): HeaderVals,
) -> Result<impl IntoResponse, AppError> {
    ensure_sharding(&app, sharding)?;

    let now_ms = unix_ms();
    let t = app
        .check_auth_if(&token, &space_id, TokenScope::Write, now_ms)
        .map_err(|_| AppError::unauthorized())?;
    let space = app
        .load_space(&space_id, false)
        .await
        .map_err(AppError::bad_request)?;
    if t.is_none() {
        space
            .verify_space_token(token, TokenScope::Write, now_ms)
            .map_err(|_| AppError::unauthorized())?;
    }

    let rt = space
        .run_wiki_digest(SELF_USER_ID)
        .await
        .map_err(AppError::bad_request)?;
    Ok(ct.response(RpcResponse::success(rt)))
}

/// POST /v1/{space_id}/maintenance
pub async fn post_maintenance(
    State(app): State<AppState>,
    Path(space_id): Path<String>,
    Accept(ct, _): Accept,
    HeaderVals(token, sharding): HeaderVals,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    ensure_sharding(&app, sharding)?;

    let input: StringOr<MaintenanceInput> = ct.parse_body(&body).map_err(AppError::bad_request)?;
    let input = input
        .value()
        .map_err(|_| AppError::bad_request("invalid input"))?;

    let now_ms = unix_ms();
    let t = app
        .check_auth_if(&token, &space_id, TokenScope::Write, now_ms)
        .map_err(|_| AppError::unauthorized())?;

    let space = app
        .load_space(&space_id, false)
        .await
        .map_err(AppError::bad_request)?;

    if t.is_none() {
        // 如果没有验证 CWToken，则验证 SpaceToken
        space
            .verify_space_token(token, TokenScope::Write, now_ms)
            .map_err(|_| AppError::unauthorized())?;
    }

    if space.is_processing() {
        return Err(AppError::bad_request(
            "Formation or Maintenance is processing, cannot start maintenance. It will automatically start after some time when the current formation/maintenance is finished.",
        ));
    }

    let rt = space
        .maintenance(SELF_USER_ID, input)
        .await
        .map_err(AppError::bad_request)?;

    Ok(ct.response(RpcResponse::success(rt)))
}

/// POST /v1/{space_id}/execute_kip_readonly
pub async fn execute_kip_readonly(
    State(app): State<AppState>,
    Path(space_id): Path<String>,
    Accept(ct, _): Accept,
    HeaderVals(token, sharding): HeaderVals,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    ensure_sharding(&app, sharding)?;

    let input: StringOr<anda_kip::Request> = ct.parse_body(&body).map_err(AppError::bad_request)?;
    let input = input
        .value()
        .map_err(|_| AppError::bad_request("invalid input"))?;

    let now_ms = unix_ms();
    let t = app
        .check_auth_if(&token, &space_id, TokenScope::Read, now_ms)
        .map_err(|_| AppError::unauthorized())?;

    let space = app
        .load_space(&space_id, false)
        .await
        .map_err(AppError::bad_request)?;

    if !space.is_public() && t.is_none() {
        // 如果没有验证 CWToken，则验证 SpaceToken
        space
            .verify_space_token(token, TokenScope::Read, now_ms)
            .map_err(|_| AppError::unauthorized())?;
    }

    let rt = space
        .execute_kip_readonly(input)
        .await
        .map_err(AppError::bad_request)?;

    Ok(ct.response(rt))
}

/// POST /v1/{space_id}/get_or_init_user
pub async fn get_or_init_user(
    State(app): State<AppState>,
    Path(space_id): Path<String>,
    Accept(ct, _): Accept,
    HeaderVals(token, sharding): HeaderVals,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    ensure_sharding(&app, sharding)?;

    let input: StringOr<GetOrInitUserInput> =
        ct.parse_body(&body).map_err(AppError::bad_request)?;
    let input = input
        .value()
        .map_err(|_| AppError::bad_request("invalid input"))?;

    let now_ms = unix_ms();
    let t = app
        .check_auth_if(&token, &space_id, TokenScope::Write, now_ms)
        .map_err(|_| AppError::unauthorized())?;

    let space = app
        .load_space(&space_id, false)
        .await
        .map_err(AppError::bad_request)?;

    if t.is_none() {
        // 如果没有验证 CWToken，则验证 SpaceToken
        space
            .verify_space_token(token, TokenScope::Write, now_ms)
            .map_err(|_| AppError::unauthorized())?;
    }

    // anda_cognitive_nexus::entity::Concept
    let concept = space
        .formation
        .get_or_init_counterparty(input.user, input.name)
        .await
        .map_err(AppError::bad_request)?;

    Ok(ct.response(RpcResponse::success(concept)))
}

/// GET /v1/{space_id}/conversations/{conversation_id}
pub async fn get_conversation(
    State(app): State<AppState>,
    Path((space_id, conversation_id)): Path<(String, String)>,
    Query(dq): Query<ConversationDeltaQuery>,
    Accept(ct, _): Accept,
    HeaderVals(token, sharding): HeaderVals,
) -> Result<impl IntoResponse, AppError> {
    ensure_sharding(&app, sharding)?;
    let conversation_id: u64 = conversation_id
        .parse()
        .map_err(|_| AppError::bad_request("invalid conversation_id"))?;

    let now_ms = unix_ms();
    let t = app
        .check_auth_if(&token, &space_id, TokenScope::Read, now_ms)
        .map_err(|_| AppError::unauthorized())?;

    let space = app
        .load_space(&space_id, false)
        .await
        .map_err(AppError::bad_request)?;

    if !space.is_public() && t.is_none() {
        // 如果空间不是公开的，且没有验证 CWToken，则验证 SpaceToken
        space
            .verify_space_token(token, TokenScope::Read, now_ms)
            .map_err(|_| AppError::unauthorized())?;
    }

    let rt = space
        .get_conversation(dq.collection, conversation_id)
        .await
        .map_err(AppError::bad_request)?;
    Ok(ct.response(RpcResponse::success(rt)))
}

/// GET /v1/{space_id}/conversations/{conversation_id}/delta
pub async fn get_conversation_delta(
    State(app): State<AppState>,
    Path((space_id, conversation_id)): Path<(String, String)>,
    Query(dq): Query<ConversationDeltaQuery>,
    Accept(ct, _): Accept,
    HeaderVals(token, sharding): HeaderVals,
) -> Result<impl IntoResponse, AppError> {
    ensure_sharding(&app, sharding)?;
    let conversation_id: u64 = conversation_id
        .parse()
        .map_err(|_| AppError::bad_request("invalid conversation_id"))?;

    let now_ms = unix_ms();
    let t = app
        .check_auth_if(&token, &space_id, TokenScope::Read, now_ms)
        .map_err(|_| AppError::unauthorized())?;

    let space = app
        .load_space(&space_id, false)
        .await
        .map_err(AppError::bad_request)?;

    if !space.is_public() && t.is_none() {
        // 如果空间不是公开的，且没有验证 CWToken，则验证 SpaceToken
        space
            .verify_space_token(token, TokenScope::Read, now_ms)
            .map_err(|_| AppError::unauthorized())?;
    }

    let rt = space
        .get_conversation(dq.collection, conversation_id)
        .await
        .map_err(AppError::bad_request)?;
    Ok(ct.response(RpcResponse::success(rt.into_delta(
        dq.messages_offset.unwrap_or_default(),
        dq.artifacts_offset.unwrap_or_default(),
    ))))
}

/// GET /v1/{space_id}/conversations
pub async fn list_conversations(
    State(app): State<AppState>,
    Path(space_id): Path<String>,
    Query(pg): Query<Pagination>,
    Accept(ct, _): Accept,
    HeaderVals(token, sharding): HeaderVals,
) -> Result<impl IntoResponse, AppError> {
    ensure_sharding(&app, sharding)?;

    let now_ms = unix_ms();
    let t = app
        .check_auth_if(&token, &space_id, TokenScope::Read, now_ms)
        .map_err(|_| AppError::unauthorized())?;

    let space = app
        .load_space(&space_id, false)
        .await
        .map_err(AppError::bad_request)?;

    if !space.is_public() && t.is_none() {
        space
            .verify_space_token(token, TokenScope::Read, now_ms)
            .map_err(|_| AppError::unauthorized())?;
    }

    let rt = space
        .list_conversations(pg.collection, pg.cursor, pg.limit)
        .await
        .map_err(AppError::bad_request)?;

    Ok(ct.response(RpcResponse {
        result: Some(rt.0),
        error: None,
        next_cursor: rt.1,
    }))
}

/* ===== User management API ===== */

/// GET /v1/{space_id}/management/space_tokens
pub async fn list_space_tokens(
    State(app): State<AppState>,
    Path(space_id): Path<String>,
    Accept(ct, _): Accept,
    HeaderVals(token, sharding): HeaderVals,
) -> Result<impl IntoResponse, AppError> {
    ensure_sharding(&app, sharding)?;

    let now_ms = unix_ms();
    let _ = app
        .check_auth(&token, &space_id, TokenScope::Write, now_ms)
        .map_err(|_| AppError::unauthorized())?;

    let space = app
        .load_space(&space_id, false)
        .await
        .map_err(AppError::bad_request)?;

    let rt = space.list_space_tokens().map_err(AppError::bad_request)?;
    Ok(ct.response(RpcResponse::success(rt)))
}

/// POST /v1/{space_id}/management/add_space_token
pub async fn add_space_token(
    State(app): State<AppState>,
    Path(space_id): Path<String>,
    Accept(ct, _): Accept,
    HeaderVals(token, sharding): HeaderVals,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    ensure_sharding(&app, sharding)?;

    let now_ms = unix_ms();
    let _ = app
        .check_auth(&token, &space_id, TokenScope::Write, now_ms)
        .map_err(|_| AppError::unauthorized())?;

    let input: AddSpaceTokenInput = ct
        .parse_body(&body)
        .map_err(AppError::bad_request)?
        .value()
        .map_err(|_| AppError::bad_request("invalid input"))?;

    let space = app
        .load_space(&space_id, false)
        .await
        .map_err(AppError::bad_request)?;

    let mut data: [u8; 20] = [0; 20];
    rand::rng().fill_bytes(&mut data);
    let token = format!("ST{}", ByteArrayB64(data));
    let rt = space
        .add_space_token(token.clone(), input, now_ms)
        .await
        .map_err(AppError::bad_request)?;
    Ok(ct.response(RpcResponse::success(rt)))
}

/// POST /v1/{space_id}/management/revoke_space_token
pub async fn revoke_space_token(
    State(app): State<AppState>,
    Path(space_id): Path<String>,
    Accept(ct, _): Accept,
    HeaderVals(token, sharding): HeaderVals,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    ensure_sharding(&app, sharding)?;

    let now_ms = unix_ms();
    let _ = app
        .check_auth(&token, &space_id, TokenScope::Write, now_ms)
        .map_err(|_| AppError::unauthorized())?;

    let input: RevokeSpaceTokenInput = ct
        .parse_body(&body)
        .map_err(AppError::bad_request)?
        .value()
        .map_err(|_| AppError::bad_request("invalid input"))?;

    let space = app
        .load_space(&space_id, false)
        .await
        .map_err(AppError::bad_request)?;

    let rt = space
        .revoke_space_token(&input.token)
        .await
        .map_err(AppError::bad_request)?;
    Ok(ct.response(RpcResponse::success(rt)))
}

/// PATCH /v1/{space_id}/management/update_space
pub async fn update_space(
    State(app): State<AppState>,
    Path(space_id): Path<String>,
    Accept(ct, _): Accept,
    HeaderVals(token, sharding): HeaderVals,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    ensure_sharding(&app, sharding)?;

    let now_ms = unix_ms();
    let _ = app
        .check_auth(&token, &space_id, TokenScope::Write, now_ms)
        .map_err(|_| AppError::unauthorized())?;

    let input: UpdateSpaceInput = ct
        .parse_body(&body)
        .map_err(AppError::bad_request)?
        .value()
        .map_err(|_| AppError::bad_request("invalid input"))?;

    let space = app
        .load_space(&space_id, false)
        .await
        .map_err(AppError::bad_request)?;

    space
        .update(input, now_ms)
        .await
        .map_err(AppError::bad_request)?;
    Ok(ct.response(RpcResponse::success(true)))
}

/// PATCH /v1/{space_id}/management/restart_formation
pub async fn restart_formation(
    State(app): State<AppState>,
    Path(space_id): Path<String>,
    Accept(ct, _): Accept,
    HeaderVals(token, sharding): HeaderVals,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    ensure_sharding(&app, sharding)?;

    let now_ms = unix_ms();
    let _ = app
        .check_auth(&token, &space_id, TokenScope::Write, now_ms)
        .map_err(|_| AppError::unauthorized())?;

    let input: FormationRestartInput = ct
        .parse_body(&body)
        .map_err(AppError::bad_request)?
        .value()
        .map_err(|_| AppError::bad_request("invalid input"))?;

    let space = app
        .load_space(&space_id, false)
        .await
        .map_err(AppError::bad_request)?;

    space
        .restart_formation(SELF_USER_ID, input.conversation)
        .await
        .map_err(AppError::bad_request)?;
    Ok(ct.response(RpcResponse::success(true)))
}

/// GET /v1/{space_id}/management/space_byok
pub async fn get_byok(
    State(app): State<AppState>,
    Path(space_id): Path<String>,
    Accept(ct, _): Accept,
    HeaderVals(token, sharding): HeaderVals,
) -> Result<impl IntoResponse, AppError> {
    ensure_sharding(&app, sharding)?;

    let now_ms = unix_ms();
    let _ = app
        .check_auth(&token, &space_id, TokenScope::Write, now_ms)
        .map_err(|_| AppError::unauthorized())?;

    let space = app
        .load_space(&space_id, false)
        .await
        .map_err(AppError::bad_request)?;

    let byok = space.get_byok();
    Ok(ct.response(RpcResponse::success(byok)))
}

/// PATCH /v1/{space_id}/management/space_byok
pub async fn update_byok(
    State(app): State<AppState>,
    Path(space_id): Path<String>,
    Accept(ct, _): Accept,
    HeaderVals(token, sharding): HeaderVals,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    ensure_sharding(&app, sharding)?;

    let now_ms = unix_ms();
    let _ = app
        .check_auth(&token, &space_id, TokenScope::Write, now_ms)
        .map_err(|_| AppError::unauthorized())?;

    let input: ModelConfig = ct
        .parse_body(&body)
        .map_err(AppError::bad_request)?
        .value()
        .map_err(|_| AppError::bad_request("invalid input"))?;

    let space = app
        .load_space(&space_id, false)
        .await
        .map_err(AppError::bad_request)?;

    space
        .update_byok(input)
        .await
        .map_err(AppError::bad_request)?;
    Ok(ct.response(RpcResponse::success(true)))
}

/* ===== Admin API ===== */

/// POST /admin/create_space
pub async fn create_space(
    State(app): State<AppState>,
    Accept(ct, _): Accept,
    HeaderVals(token, sharding): HeaderVals,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    let now_ms = unix_ms();
    let token = app
        .check_admin(&token, "*", TokenScope::Write, now_ms)
        .map_err(|_| AppError::unauthorized())?;

    let input: CreateOrUpdateSpaceInput = ct
        .parse_body(&body)
        .map_err(AppError::bad_request)?
        .value()
        .map_err(|_| AppError::bad_request("invalid input"))?;

    ensure_sharding(&app, sharding)?;

    let rt = app
        .admin_create_space(token.user, input.user, input.space_id, input.tier, now_ms)
        .await
        .map_err(AppError::bad_request)?;
    Ok(ct.response(RpcResponse::success(rt)))
}

/// POST /admin/{space_id}/update_space_tier
pub async fn update_space_tier(
    State(app): State<AppState>,
    Path(space_id): Path<String>,
    Accept(ct, _): Accept,
    HeaderVals(token, sharding): HeaderVals,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    let now_ms = unix_ms();
    let _ = app
        .check_admin(&token, "*", TokenScope::Write, now_ms)
        .map_err(|_| AppError::unauthorized())?;

    let input: CreateOrUpdateSpaceInput = ct
        .parse_body(&body)
        .map_err(AppError::bad_request)?
        .value()
        .map_err(|_| AppError::bad_request("invalid input"))?;

    ensure_sharding(&app, sharding)?;

    if input.space_id != space_id {
        return Err(AppError::bad_request(format!(
            "space_id in path {} does not match space_id in body {}",
            space_id, input.space_id
        )));
    }

    let space = app
        .load_space(&input.space_id, false)
        .await
        .map_err(AppError::bad_request)?;

    let rt = space
        .admin_update_tier(input.tier, now_ms)
        .await
        .map_err(AppError::bad_request)?;
    Ok(ct.response(RpcResponse::success(rt)))
}

#[cfg(test)]
mod tests {
    use super::{
        WikiContentQuery, WikiEventsQuery, WikiExportQuery, add_space_token, apple_touch_icon,
        create_space, execute_kip_readonly, favicon, get_byok, get_conversation,
        get_conversation_delta, get_formation_status, get_info, get_information, get_memory_status,
        get_or_init_user, get_skill, get_wiki_content, get_wiki_doc, get_wiki_export,
        list_conversations, list_space_tokens, list_wiki_events, post_formation, post_maintenance,
        post_memory_forget, post_memory_pin, post_probe, post_recall, post_recall_structured,
        post_shadow_eval, post_wiki_commit, post_wiki_import, post_wiki_search, post_wiki_verify,
        restart_formation, revoke_space_token, update_byok, update_space, update_space_tier,
        wiki_read_access,
    };
    use crate::{
        agents::SELF_USER_ID,
        payload::{Accept, AppError, HeaderVals, PayloadFormat},
        space::{AppState, Space},
        types::{
            AddSpaceTokenInput, ConversationDeltaQuery, CreateOrUpdateSpaceInput, FormationInput,
            FormationRestartInput, GetOrInitUserInput, InputContext, MaintenanceInput,
            MaintenanceScope, ModelConfig, Pagination, RecallInput, RevokeSpaceTokenInput,
            TokenScope, UpdateSpaceInput,
        },
    };
    use anda_core::{AgentOutput, BoxError, BoxPinFut, CompletionRequest, Message, Principal};
    use anda_db::{database::DBConfig, storage::StorageConfig};
    use anda_engine::{
        management::{BaseManagement, Visibility},
        memory::{Conversation, ConversationRef, ConversationStatus},
        model::{CompletionFeaturesDyn, Model, Models, reqwest},
        unix_ms,
    };
    use axum::{
        body::{Bytes, to_bytes},
        extract::{Path, Query, State},
        http::{HeaderMap, StatusCode, header},
        response::{IntoResponse, Response},
    };
    use cose2::{CoseMap, Label, Sign1Message, Value as CoseValue, cwt::Claims, iana};
    use ic_auth_types::ByteBufB64;
    use ic_cose_types::cose::ed25519::{SigningKey, VerifyingKey, ed25519_sign};
    use object_store::memory::InMemory;
    use serde::Serialize;
    use serde_json::{Value, json};
    use std::{collections::BTreeSet, sync::Arc};

    #[derive(Debug)]
    struct FinalCompleter;

    impl CompletionFeaturesDyn for FinalCompleter {
        fn model_name(&self) -> String {
            "handler-test-model".to_string()
        }

        fn completion(&self, req: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
            Box::pin(async move {
                Ok(AgentOutput {
                    content: "handler done".to_string(),
                    chat_history: vec![Message {
                        role: "assistant".to_string(),
                        content: vec![format!("handler processed: {}", req.prompt).into()],
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

    fn test_app_state(name: &str, sharding: u32) -> AppState {
        test_app_state_with_pubkeys(name, sharding, vec![])
    }

    fn test_app_state_with_auth_enabled(name: &str, sharding: u32) -> AppState {
        let mut bytes = [0x66; 32];
        bytes[0] = 0x58;
        let key = VerifyingKey::from_bytes(&bytes).unwrap();
        test_app_state_with_pubkeys(name, sharding, vec![key])
    }

    fn test_signing_key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    fn test_app_state_with_signing_key(
        name: &str,
        sharding: u32,
        signing_key: &SigningKey,
    ) -> AppState {
        test_app_state_with_pubkeys(name, sharding, vec![signing_key.verifying_key()])
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

    fn test_app_state_with_pubkeys(
        name: &str,
        sharding: u32,
        pubkeys: Vec<VerifyingKey>,
    ) -> AppState {
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
            sharding,
        )
    }

    async fn create_loaded_space(app: &AppState, id: &str) -> Arc<Space> {
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

    fn accept_from_headers(
        accept: Option<&str>,
        content_type: Option<&str>,
        lang: Option<&str>,
    ) -> Accept {
        let mut headers = HeaderMap::new();
        if let Some(value) = accept {
            headers.insert(header::ACCEPT, value.parse().unwrap());
        }
        if let Some(value) = content_type {
            headers.insert(header::CONTENT_TYPE, value.parse().unwrap());
        }
        if let Some(value) = lang {
            headers.insert(header::ACCEPT_LANGUAGE, value.parse().unwrap());
        }

        let is_cn = lang
            .map(|value| value.to_ascii_lowercase().contains("zh"))
            .unwrap_or(false);
        Accept(PayloadFormat::from_headers(&headers), is_cn)
    }

    fn accept_json() -> Accept {
        accept_from_headers(Some("application/json"), Some("application/json"), None)
    }

    fn headers(app: &AppState) -> HeaderVals {
        HeaderVals(String::new(), app.sharding)
    }

    fn json_bytes<T: Serialize>(value: &T) -> Bytes {
        Bytes::from(serde_json::to_vec(value).unwrap())
    }

    async fn response_text(response: Response) -> String {
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    async fn response_json(response: Response) -> Value {
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn ok_json<T: IntoResponse>(result: Result<T, AppError>) -> Value {
        match result {
            Ok(value) => {
                let response = value.into_response();
                assert_eq!(response.status(), StatusCode::OK);
                response_json(response).await
            }
            Err(err) => panic!("unexpected error: {}", err.message),
        }
    }

    async fn err_json<T: IntoResponse>(result: Result<T, AppError>, status: StatusCode) -> Value {
        match result {
            Ok(_) => panic!("expected handler error"),
            Err(err) => {
                let response = err.into_response();
                assert_eq!(response.status(), status);
                response_json(response).await
            }
        }
    }

    #[test]
    fn wiki_read_access_resolves_the_three_caller_states() {
        use crate::types::{CWToken, SpaceToken};

        // CWT holder: unrestricted, actor is the user principal.
        let cwt = Some(CWToken {
            user: SELF_USER_ID,
            audience: "sp".to_string(),
            scope: TokenScope::Read,
        });
        let access = wiki_read_access(&cwt, None);
        assert!(access.labels.is_none());
        assert_eq!(access.actor, SELF_USER_ID.to_string());

        // Labeled space token: restricted to unlabeled + granted labels.
        let st = SpaceToken {
            name: "auditor".to_string(),
            labels: Some(vec!["hr".to_string()]),
            ..Default::default()
        };
        let access = wiki_read_access(&None, Some(&st));
        assert_eq!(access.labels, Some(vec!["hr".to_string()]));
        assert_eq!(access.actor, "st:auditor");

        // Label-less space token: unrestricted, stable audit identity even
        // without a name.
        let unnamed = SpaceToken::default();
        let access = wiki_read_access(&None, Some(&unnamed));
        assert!(access.labels.is_none());
        assert_eq!(access.actor, "st:unnamed");

        // Anonymous public-space reader: unlabeled content only (P0-1).
        let access = wiki_read_access(&None, None);
        assert_eq!(access.labels, Some(Vec::new()));
    }

    #[tokio::test]
    async fn static_and_information_handlers_return_expected_formats() {
        let app = test_app_state("handler_static", 9);

        let favicon = favicon().await;
        assert_eq!(favicon.status(), StatusCode::OK);
        assert_eq!(
            favicon.headers().get(header::CONTENT_TYPE).unwrap(),
            "image/x-icon"
        );

        let icon = apple_touch_icon().await;
        assert_eq!(icon.status(), StatusCode::OK);
        assert_eq!(
            icon.headers().get(header::CONTENT_TYPE).unwrap(),
            "image/webp"
        );

        let info = get_information(State(app.clone())).await.into_response();
        let info = response_json(info).await;
        assert_eq!(info["name"], "anda_brain");
        assert_eq!(info["version"], "test-version");
        assert_eq!(info["sharding"], 9);

        let skill = get_skill(State(app)).await.into_response();
        assert_eq!(
            skill.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/markdown; charset=utf-8"
        );
        let skill_text = response_text(skill).await;
        assert!(skill_text.contains("Anda Brain"));
    }

    #[tokio::test]
    async fn admin_and_management_handlers_cover_space_lifecycle() {
        let app = test_app_state("handler_lifecycle", 3);
        let owner = Principal::from_slice(&[11]);
        let space_id = "handler_lifecycle_space".to_string();
        let create_input = CreateOrUpdateSpaceInput {
            user: owner,
            space_id: space_id.clone(),
            tier: 2,
        };

        let created = ok_json(
            create_space(
                State(app.clone()),
                accept_json(),
                headers(&app),
                json_bytes(&create_input),
            )
            .await,
        )
        .await;
        assert_eq!(created["result"]["id"], space_id);
        assert_eq!(created["result"]["owner"], owner.to_string());

        let info = ok_json(
            get_info(
                State(app.clone()),
                Path(space_id.clone()),
                accept_json(),
                headers(&app),
            )
            .await,
        )
        .await;
        assert_eq!(info["result"]["tier"]["tier"], 2);

        let update_input = UpdateSpaceInput {
            name: Some("Handler Brain".to_string()),
            description: Some("handler coverage".to_string()),
            public: Some(true),
            ..Default::default()
        };
        let updated = ok_json(
            update_space(
                State(app.clone()),
                Path(space_id.clone()),
                accept_json(),
                headers(&app),
                json_bytes(&update_input),
            )
            .await,
        )
        .await;
        assert_eq!(updated["result"], true);

        let info = ok_json(
            get_info(
                State(app.clone()),
                Path(space_id.clone()),
                accept_json(),
                HeaderVals("not-a-token".to_string(), app.sharding),
            )
            .await,
        )
        .await;
        assert_eq!(info["result"]["name"], "Handler Brain");
        assert_eq!(info["result"]["public"], true);

        let status = ok_json(
            get_formation_status(
                State(app.clone()),
                Path(space_id.clone()),
                accept_json(),
                headers(&app),
            )
            .await,
        )
        .await;
        assert_eq!(status["result"]["id"], space_id);
        assert_eq!(status["result"]["formation_processing"], false);

        let byok = ModelConfig {
            family: "openai".to_string(),
            model: "handler-model".to_string(),
            api_base: "https://api.example.test".to_string(),
            api_key: "test-key".to_string(),
            ..Default::default()
        };
        let byok_updated = ok_json(
            update_byok(
                State(app.clone()),
                Path(space_id.clone()),
                accept_json(),
                headers(&app),
                json_bytes(&byok),
            )
            .await,
        )
        .await;
        assert_eq!(byok_updated["result"], true);

        let byok_result = ok_json(
            get_byok(
                State(app.clone()),
                Path(space_id.clone()),
                accept_json(),
                headers(&app),
            )
            .await,
        )
        .await;
        assert_eq!(byok_result["result"]["model"], "handler-model");

        let token_input = AddSpaceTokenInput {
            scope: TokenScope::Read,
            name: "reader".to_string(),
            expires_at: None,
            labels: None,
        };
        let added = ok_json(
            add_space_token(
                State(app.clone()),
                Path(space_id.clone()),
                accept_json(),
                headers(&app),
                json_bytes(&token_input),
            )
            .await,
        )
        .await;
        let space_token = added["result"]["token"].as_str().unwrap().to_string();
        assert!(space_token.starts_with("ST"));

        let tokens = ok_json(
            list_space_tokens(
                State(app.clone()),
                Path(space_id.clone()),
                accept_json(),
                headers(&app),
            )
            .await,
        )
        .await;
        assert_eq!(tokens["result"].as_array().unwrap().len(), 1);

        let revoked = ok_json(
            revoke_space_token(
                State(app.clone()),
                Path(space_id.clone()),
                accept_json(),
                headers(&app),
                json_bytes(&RevokeSpaceTokenInput { token: space_token }),
            )
            .await,
        )
        .await;
        assert_eq!(revoked["result"], true);

        let mismatched_tier = err_json(
            update_space_tier(
                State(app.clone()),
                Path(space_id.clone()),
                accept_json(),
                headers(&app),
                json_bytes(&CreateOrUpdateSpaceInput {
                    user: owner,
                    space_id: "other_space".to_string(),
                    tier: 4,
                }),
            )
            .await,
            StatusCode::BAD_REQUEST,
        )
        .await;
        assert!(
            mismatched_tier["error"]["message"]
                .as_str()
                .unwrap()
                .contains("does not match")
        );

        let tier = ok_json(
            update_space_tier(
                State(app.clone()),
                Path(space_id.clone()),
                accept_json(),
                headers(&app),
                json_bytes(&CreateOrUpdateSpaceInput {
                    user: owner,
                    space_id: space_id.clone(),
                    tier: 4,
                }),
            )
            .await,
        )
        .await;
        assert_eq!(tier["result"]["tier"], 4);

        let sharding_err = err_json(
            get_info(
                State(app),
                Path(space_id),
                accept_json(),
                HeaderVals(String::new(), 99),
            )
            .await,
            StatusCode::BAD_REQUEST,
        )
        .await;
        assert!(
            sharding_err["error"]["message"]
                .as_str()
                .unwrap()
                .contains("does not match")
        );
    }

    #[tokio::test]
    async fn management_secret_handlers_require_write_cwt() {
        let signing_key = test_signing_key();
        let app = test_app_state_with_signing_key("handler_secret_scope", 0, &signing_key);
        let space_id = "handler_secret_scope_space";
        let space = create_loaded_space(&app, space_id).await;
        space
            .update_byok(ModelConfig {
                family: "openai".to_string(),
                model: "handler-secret-model".to_string(),
                api_base: "https://api.example.test".to_string(),
                api_key: "handler-secret-key".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();
        space
            .add_space_token(
                "SThandler-secret-token".to_string(),
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

        let read_cwt = signed_token(&signing_key, SELF_USER_ID, space_id, "read");
        let write_cwt = signed_token(&signing_key, SELF_USER_ID, space_id, "write");

        let byok_denied = err_json(
            get_byok(
                State(app.clone()),
                Path(space_id.to_string()),
                accept_json(),
                HeaderVals(read_cwt.clone(), 0),
            )
            .await,
            StatusCode::UNAUTHORIZED,
        )
        .await;
        assert_eq!(
            byok_denied["error"]["message"].as_str(),
            Some("authentication failed")
        );

        let tokens_denied = err_json(
            list_space_tokens(
                State(app.clone()),
                Path(space_id.to_string()),
                accept_json(),
                HeaderVals(read_cwt, 0),
            )
            .await,
            StatusCode::UNAUTHORIZED,
        )
        .await;
        assert_eq!(
            tokens_denied["error"]["message"].as_str(),
            Some("authentication failed")
        );

        let byok = ok_json(
            get_byok(
                State(app.clone()),
                Path(space_id.to_string()),
                accept_json(),
                HeaderVals(write_cwt.clone(), 0),
            )
            .await,
        )
        .await;
        assert_eq!(byok["result"]["api_key"], "handler-secret-key");

        let tokens = ok_json(
            list_space_tokens(
                State(app),
                Path(space_id.to_string()),
                accept_json(),
                HeaderVals(write_cwt, 0),
            )
            .await,
        )
        .await;
        assert_eq!(tokens["result"][0]["token"], "SThandler-secret-token");
    }

    #[tokio::test]
    async fn handlers_reject_mismatched_sharding_consistently() {
        let app = test_app_state("handler_sharding_errors", 5);
        let owner = Principal::from_slice(&[13]);
        let space_id = "handler_sharding_space".to_string();
        let wrong = || HeaderVals(String::new(), 99);

        let create_input = CreateOrUpdateSpaceInput {
            user: owner,
            space_id: space_id.clone(),
            tier: 1,
        };
        let token_input = AddSpaceTokenInput {
            scope: TokenScope::Read,
            name: "reader".to_string(),
            expires_at: None,
            labels: None,
        };
        let update_input = UpdateSpaceInput {
            name: Some("ignored".to_string()),
            ..Default::default()
        };

        let info = err_json(
            get_info(
                State(app.clone()),
                Path(space_id.clone()),
                accept_json(),
                wrong(),
            )
            .await,
            StatusCode::BAD_REQUEST,
        )
        .await;
        assert!(
            info["error"]["message"]
                .as_str()
                .unwrap()
                .contains("sharding")
        );

        let _ = err_json(
            get_formation_status(
                State(app.clone()),
                Path(space_id.clone()),
                accept_json(),
                wrong(),
            )
            .await,
            StatusCode::BAD_REQUEST,
        )
        .await;
        let _ = err_json(
            post_formation(
                State(app.clone()),
                Path(space_id.clone()),
                accept_json(),
                wrong(),
                Bytes::new(),
            )
            .await,
            StatusCode::BAD_REQUEST,
        )
        .await;
        let _ = err_json(
            post_recall(
                State(app.clone()),
                Path(space_id.clone()),
                accept_json(),
                wrong(),
                Bytes::new(),
            )
            .await,
            StatusCode::BAD_REQUEST,
        )
        .await;
        let _ = err_json(
            post_maintenance(
                State(app.clone()),
                Path(space_id.clone()),
                accept_json(),
                wrong(),
                Bytes::new(),
            )
            .await,
            StatusCode::BAD_REQUEST,
        )
        .await;
        let _ = err_json(
            execute_kip_readonly(
                State(app.clone()),
                Path(space_id.clone()),
                accept_json(),
                wrong(),
                Bytes::new(),
            )
            .await,
            StatusCode::BAD_REQUEST,
        )
        .await;
        let _ = err_json(
            get_or_init_user(
                State(app.clone()),
                Path(space_id.clone()),
                accept_json(),
                wrong(),
                Bytes::new(),
            )
            .await,
            StatusCode::BAD_REQUEST,
        )
        .await;
        let _ = err_json(
            get_conversation(
                State(app.clone()),
                Path((space_id.clone(), "1".to_string())),
                Query(ConversationDeltaQuery {
                    messages_offset: None,
                    artifacts_offset: None,
                    collection: None,
                }),
                accept_json(),
                wrong(),
            )
            .await,
            StatusCode::BAD_REQUEST,
        )
        .await;
        let _ = err_json(
            get_conversation_delta(
                State(app.clone()),
                Path((space_id.clone(), "1".to_string())),
                Query(ConversationDeltaQuery {
                    messages_offset: None,
                    artifacts_offset: None,
                    collection: None,
                }),
                accept_json(),
                wrong(),
            )
            .await,
            StatusCode::BAD_REQUEST,
        )
        .await;
        let _ = err_json(
            list_conversations(
                State(app.clone()),
                Path(space_id.clone()),
                Query(Pagination {
                    cursor: None,
                    limit: None,
                    collection: None,
                }),
                accept_json(),
                wrong(),
            )
            .await,
            StatusCode::BAD_REQUEST,
        )
        .await;

        let _ = err_json(
            list_space_tokens(
                State(app.clone()),
                Path(space_id.clone()),
                accept_json(),
                wrong(),
            )
            .await,
            StatusCode::BAD_REQUEST,
        )
        .await;
        let _ = err_json(
            add_space_token(
                State(app.clone()),
                Path(space_id.clone()),
                accept_json(),
                wrong(),
                json_bytes(&token_input),
            )
            .await,
            StatusCode::BAD_REQUEST,
        )
        .await;
        let _ = err_json(
            revoke_space_token(
                State(app.clone()),
                Path(space_id.clone()),
                accept_json(),
                wrong(),
                json_bytes(&RevokeSpaceTokenInput {
                    token: "STunused".to_string(),
                }),
            )
            .await,
            StatusCode::BAD_REQUEST,
        )
        .await;
        let _ = err_json(
            update_space(
                State(app.clone()),
                Path(space_id.clone()),
                accept_json(),
                wrong(),
                json_bytes(&update_input),
            )
            .await,
            StatusCode::BAD_REQUEST,
        )
        .await;
        let _ = err_json(
            restart_formation(
                State(app.clone()),
                Path(space_id.clone()),
                accept_json(),
                wrong(),
                json_bytes(&FormationRestartInput { conversation: 1 }),
            )
            .await,
            StatusCode::BAD_REQUEST,
        )
        .await;
        let _ = err_json(
            get_byok(
                State(app.clone()),
                Path(space_id.clone()),
                accept_json(),
                wrong(),
            )
            .await,
            StatusCode::BAD_REQUEST,
        )
        .await;
        let _ = err_json(
            update_byok(
                State(app.clone()),
                Path(space_id.clone()),
                accept_json(),
                wrong(),
                Bytes::new(),
            )
            .await,
            StatusCode::BAD_REQUEST,
        )
        .await;
        let _ = err_json(
            create_space(
                State(app.clone()),
                accept_json(),
                wrong(),
                json_bytes(&create_input),
            )
            .await,
            StatusCode::BAD_REQUEST,
        )
        .await;
        let _ = err_json(
            update_space_tier(
                State(app),
                Path(space_id),
                accept_json(),
                wrong(),
                json_bytes(&create_input),
            )
            .await,
            StatusCode::BAD_REQUEST,
        )
        .await;
    }

    #[tokio::test]
    async fn conversation_handlers_read_collections_and_deltas() {
        let app = test_app_state("handler_conversations", 0);
        let space_id = "handler_conversations_space";
        let space = create_loaded_space(&app, space_id).await;
        let now = unix_ms();

        let formation_id = space
            .memory
            .add_conversation(ConversationRef::from(&Conversation {
                user: SELF_USER_ID,
                status: ConversationStatus::Completed,
                label: Some("formation".to_string()),
                messages: vec![json!({"role": "user", "content": "hello"})],
                created_at: now,
                updated_at: now,
                ..Default::default()
            }))
            .await
            .unwrap();
        let recall_id = space
            .recall
            .conversations
            .add_conversation(ConversationRef::from(&Conversation {
                user: SELF_USER_ID,
                status: ConversationStatus::Completed,
                label: Some("recall".to_string()),
                created_at: now + 1,
                updated_at: now + 1,
                ..Default::default()
            }))
            .await
            .unwrap();
        let formation = ok_json(
            get_conversation(
                State(app.clone()),
                Path((space_id.to_string(), formation_id.to_string())),
                Query(ConversationDeltaQuery {
                    messages_offset: None,
                    artifacts_offset: None,
                    collection: None,
                }),
                accept_json(),
                headers(&app),
            )
            .await,
        )
        .await;
        assert_eq!(formation["result"]["label"], "formation");

        let delta = ok_json(
            get_conversation_delta(
                State(app.clone()),
                Path((space_id.to_string(), formation_id.to_string())),
                Query(ConversationDeltaQuery {
                    messages_offset: Some(1),
                    artifacts_offset: Some(0),
                    collection: None,
                }),
                accept_json(),
                headers(&app),
            )
            .await,
        )
        .await;
        assert_eq!(delta["result"]["_id"], formation_id);
        assert_eq!(delta["result"]["messages"].as_array().unwrap().len(), 0);

        let recall = ok_json(
            get_conversation(
                State(app.clone()),
                Path((space_id.to_string(), recall_id.to_string())),
                Query(ConversationDeltaQuery {
                    messages_offset: None,
                    artifacts_offset: None,
                    collection: Some("recall".to_string()),
                }),
                accept_json(),
                headers(&app),
            )
            .await,
        )
        .await;
        assert_eq!(recall["result"]["label"], "recall");

        let listed = ok_json(
            list_conversations(
                State(app.clone()),
                Path(space_id.to_string()),
                Query(Pagination {
                    cursor: None,
                    limit: Some(1),
                    collection: None,
                }),
                accept_json(),
                headers(&app),
            )
            .await,
        )
        .await;
        assert_eq!(listed["result"].as_array().unwrap().len(), 1);
        assert!(listed["next_cursor"].is_string());

        let invalid_id = err_json(
            get_conversation(
                State(app),
                Path((space_id.to_string(), "not-a-number".to_string())),
                Query(ConversationDeltaQuery {
                    messages_offset: None,
                    artifacts_offset: None,
                    collection: None,
                }),
                accept_json(),
                HeaderVals(String::new(), 0),
            )
            .await,
            StatusCode::BAD_REQUEST,
        )
        .await;
        assert!(
            invalid_id["error"]["message"]
                .as_str()
                .unwrap()
                .contains("invalid conversation_id")
        );
    }

    #[tokio::test]
    async fn runtime_handlers_cover_parse_auth_and_readonly_paths() {
        let app = test_app_state("handler_runtime", 0);
        let space_id = "handler_runtime_space";
        let space = create_loaded_space(&app, space_id).await;
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

        let formation_ok = match post_formation(
            State(app.clone()),
            Path(space_id.to_string()),
            accept_from_headers(Some("text/markdown"), Some("application/json"), None),
            headers(&app),
            json_bytes(&FormationInput {
                messages: vec![Message {
                    role: "user".to_string(),
                    content: vec!["remember handler success".to_string().into()],
                    ..Default::default()
                }],
                context: Some(InputContext {
                    counterparty: Some("handler-user".to_string()),
                    agent: Some("handler-agent".to_string()),
                    source: Some("handler-source".to_string()),
                    topic: Some("handler-topic".to_string()),
                }),
                timestamp: None,
            }),
        )
        .await
        {
            Ok(response) => response.into_response(),
            Err(err) => panic!("unexpected formation error: {}", err.message),
        };
        assert_eq!(formation_ok.status(), StatusCode::OK);
        for _ in 0..100 {
            if !space.is_processing() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(!space.is_processing());

        let recall_ok = ok_json(
            post_recall(
                State(app.clone()),
                Path(space_id.to_string()),
                accept_json(),
                headers(&app),
                json_bytes(&RecallInput {
                    query: "What did the handler remember?".to_string(),
                    context: Some(InputContext {
                        counterparty: Some("handler-user".to_string()),
                        agent: None,
                        source: None,
                        topic: Some("handler-topic".to_string()),
                    }),
                }),
            )
            .await,
        )
        .await;
        assert!(recall_ok["result"]["conversation"].is_number());

        let maintenance_ok = ok_json(
            post_maintenance(
                State(app.clone()),
                Path(space_id.to_string()),
                accept_json(),
                headers(&app),
                json_bytes(&MaintenanceInput {
                    scope: MaintenanceScope::Quick,
                    formation_id: 1,
                    ..Default::default()
                }),
            )
            .await,
        )
        .await;
        assert!(maintenance_ok["result"]["conversation"].is_number());

        let formation_err = err_json(
            post_formation(
                State(app.clone()),
                Path(space_id.to_string()),
                accept_json(),
                headers(&app),
                Bytes::from_static(b"{"),
            )
            .await,
            StatusCode::BAD_REQUEST,
        )
        .await;
        assert!(
            formation_err["error"]["message"]
                .as_str()
                .unwrap()
                .contains("parse JSON error")
        );

        let recall_err = err_json(
            post_recall(
                State(app.clone()),
                Path(space_id.to_string()),
                accept_json(),
                HeaderVals(String::new(), 1),
                Bytes::from_static(b"{}"),
            )
            .await,
            StatusCode::BAD_REQUEST,
        )
        .await;
        assert!(
            recall_err["error"]["message"]
                .as_str()
                .unwrap()
                .contains("does not match")
        );

        let maintenance_err = err_json(
            post_maintenance(
                State(app.clone()),
                Path(space_id.to_string()),
                accept_from_headers(Some("application/json"), Some("text/markdown"), None),
                headers(&app),
                Bytes::from_static(b"not json"),
            )
            .await,
            StatusCode::BAD_REQUEST,
        )
        .await;
        assert!(
            maintenance_err["error"]["message"]
                .as_str()
                .unwrap()
                .contains("invalid input")
        );

        let kip = ok_json(
            execute_kip_readonly(
                State(app.clone()),
                Path(space_id.to_string()),
                accept_json(),
                headers(&app),
                Bytes::from_static(br#"{"command":"DESCRIBE PRIMER"}"#),
            )
            .await,
        )
        .await;
        assert!(kip.as_object().is_some_and(|obj| !obj.is_empty()));

        let user = ok_json(
            get_or_init_user(
                State(app),
                Path(space_id.to_string()),
                accept_json(),
                HeaderVals(String::new(), 0),
                json_bytes(&GetOrInitUserInput {
                    user: "external-user-1".to_string(),
                    name: Some("External User".to_string()),
                }),
            )
            .await,
        )
        .await;
        assert_eq!(user["result"]["type"], "Person");
        assert!(user["result"].to_string().contains("external-user-1"));
    }

    #[tokio::test]
    async fn memory_evolution_endpoints_enforce_auth_matrix() {
        let signing_key = test_signing_key();
        let app = test_app_state_with_signing_key("mem_auth", 0, &signing_key);
        let space_id = "mem_auth_space";
        let space = create_loaded_space(&app, space_id).await;

        let probe_body = || json_bytes(&json!({"query": "anything"}));
        let recall_body = || json_bytes(&json!({"query": "anything"}));
        let pin_body = || json_bytes(&json!({"entity": "C:999", "pinned": true}));
        let forget_body = || json_bytes(&json!({"entities": ["C:999"], "dry_run": true}));
        let shadow_body = || json_bytes(&json!({"policy": crate::types::MemoryPolicy::default()}));
        let no_token = || HeaderVals(String::new(), 0);

        // Private space, no token: every evolution endpoint rejects.
        err_json(
            post_probe(
                State(app.clone()),
                Path(space_id.to_string()),
                accept_json(),
                no_token(),
                probe_body(),
            )
            .await,
            StatusCode::UNAUTHORIZED,
        )
        .await;
        err_json(
            post_recall_structured(
                State(app.clone()),
                Path(space_id.to_string()),
                accept_json(),
                no_token(),
                recall_body(),
            )
            .await,
            StatusCode::UNAUTHORIZED,
        )
        .await;
        err_json(
            get_memory_status(
                State(app.clone()),
                Path(space_id.to_string()),
                accept_json(),
                no_token(),
            )
            .await,
            StatusCode::UNAUTHORIZED,
        )
        .await;
        err_json(
            post_memory_pin(
                State(app.clone()),
                Path(space_id.to_string()),
                accept_json(),
                no_token(),
                pin_body(),
            )
            .await,
            StatusCode::UNAUTHORIZED,
        )
        .await;
        err_json(
            post_memory_forget(
                State(app.clone()),
                Path(space_id.to_string()),
                accept_json(),
                no_token(),
                forget_body(),
            )
            .await,
            StatusCode::UNAUTHORIZED,
        )
        .await;
        err_json(
            post_shadow_eval(
                State(app.clone()),
                Path(space_id.to_string()),
                accept_json(),
                no_token(),
                shadow_body(),
            )
            .await,
            StatusCode::UNAUTHORIZED,
        )
        .await;

        // Public space: read endpoints open up; write/management must not.
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
        ok_json(
            post_probe(
                State(app.clone()),
                Path(space_id.to_string()),
                accept_json(),
                no_token(),
                probe_body(),
            )
            .await,
        )
        .await;
        ok_json(
            get_memory_status(
                State(app.clone()),
                Path(space_id.to_string()),
                accept_json(),
                no_token(),
            )
            .await,
        )
        .await;
        ok_json(
            post_recall_structured(
                State(app.clone()),
                Path(space_id.to_string()),
                accept_json(),
                no_token(),
                recall_body(),
            )
            .await,
        )
        .await;
        err_json(
            post_memory_pin(
                State(app.clone()),
                Path(space_id.to_string()),
                accept_json(),
                no_token(),
                pin_body(),
            )
            .await,
            StatusCode::UNAUTHORIZED,
        )
        .await;
        err_json(
            post_memory_forget(
                State(app.clone()),
                Path(space_id.to_string()),
                accept_json(),
                no_token(),
                forget_body(),
            )
            .await,
            StatusCode::UNAUTHORIZED,
        )
        .await;
        err_json(
            post_shadow_eval(
                State(app.clone()),
                Path(space_id.to_string()),
                accept_json(),
                no_token(),
                shadow_body(),
            )
            .await,
            StatusCode::UNAUTHORIZED,
        )
        .await;

        // Write-scoped CWT: pin/forget pass the gate; shadow_eval passes
        // auth and fails only on the empty replay corpus — proving the
        // barrier really was authentication.
        let write_cwt = signed_token(&signing_key, SELF_USER_ID, space_id, "write");
        let with_token = || HeaderVals(write_cwt.clone(), 0);
        // The nonexistent entity draws a KIP domain error — not 401: the
        // request got through the gate and reached the graph.
        let pin_err = err_json(
            post_memory_pin(
                State(app.clone()),
                Path(space_id.to_string()),
                accept_json(),
                with_token(),
                pin_body(),
            )
            .await,
            StatusCode::BAD_REQUEST,
        )
        .await;
        assert!(
            pin_err["error"]["message"]
                .as_str()
                .unwrap()
                .contains("pin failed"),
            "{pin_err}"
        );
        let forget_ok = ok_json(
            post_memory_forget(
                State(app.clone()),
                Path(space_id.to_string()),
                accept_json(),
                with_token(),
                forget_body(),
            )
            .await,
        )
        .await;
        assert_eq!(forget_ok["result"]["dry_run"], true);
        // The recall_structured call above left a completed recall to
        // replay, so an authorized shadow evaluation runs end to end.
        let shadow_ok = ok_json(
            post_shadow_eval(
                State(app.clone()),
                Path(space_id.to_string()),
                accept_json(),
                with_token(),
                shadow_body(),
            )
            .await,
        )
        .await;
        assert!(
            shadow_ok["result"]["compared_at"].is_number(),
            "{shadow_ok}"
        );
    }

    #[tokio::test]
    async fn runtime_handlers_accept_space_tokens_when_cw_auth_is_enabled() {
        let app = test_app_state_with_auth_enabled("handler_space_token_auth", 0);
        let space_id = "handler_space_token_auth_space";
        let space = create_loaded_space(&app, space_id).await;
        let read_token = "SThandler-read".to_string();
        let write_token = "SThandler-write".to_string();
        space
            .add_space_token(
                read_token.clone(),
                AddSpaceTokenInput {
                    scope: TokenScope::Read,
                    name: "reader".to_string(),
                    expires_at: None,
                    labels: None,
                },
                unix_ms(),
            )
            .await
            .unwrap();
        space
            .add_space_token(
                write_token.clone(),
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

        let unauthorized = err_json(
            get_info(
                State(app.clone()),
                Path(space_id.to_string()),
                accept_json(),
                HeaderVals(String::new(), 0),
            )
            .await,
            StatusCode::UNAUTHORIZED,
        )
        .await;
        assert_eq!(
            unauthorized["error"]["message"].as_str(),
            Some("authentication failed")
        );

        let info = ok_json(
            get_info(
                State(app.clone()),
                Path(space_id.to_string()),
                accept_json(),
                HeaderVals(read_token.clone(), 0),
            )
            .await,
        )
        .await;
        assert_eq!(info["result"]["id"], space_id);

        let status = ok_json(
            get_formation_status(
                State(app.clone()),
                Path(space_id.to_string()),
                accept_json(),
                HeaderVals(read_token.clone(), 0),
            )
            .await,
        )
        .await;
        assert_eq!(status["result"]["id"], space_id);

        let recall = ok_json(
            post_recall(
                State(app.clone()),
                Path(space_id.to_string()),
                accept_json(),
                HeaderVals(read_token.clone(), 0),
                json_bytes(&RecallInput {
                    query: "Space token recall?".to_string(),
                    context: None,
                }),
            )
            .await,
        )
        .await;
        assert!(recall["result"]["conversation"].is_number());

        let kip = ok_json(
            execute_kip_readonly(
                State(app.clone()),
                Path(space_id.to_string()),
                accept_json(),
                HeaderVals(read_token.clone(), 0),
                Bytes::from_static(br#"{"command":"DESCRIBE PRIMER"}"#),
            )
            .await,
        )
        .await;
        assert!(kip.as_object().is_some_and(|obj| !obj.is_empty()));

        let user = ok_json(
            get_or_init_user(
                State(app.clone()),
                Path(space_id.to_string()),
                accept_json(),
                HeaderVals(write_token.clone(), 0),
                json_bytes(&GetOrInitUserInput {
                    user: "space-token-user".to_string(),
                    name: None,
                }),
            )
            .await,
        )
        .await;
        assert!(user["result"].to_string().contains("space-token-user"));

        let formation = match post_formation(
            State(app.clone()),
            Path(space_id.to_string()),
            accept_json(),
            HeaderVals(write_token.clone(), 0),
            json_bytes(&FormationInput {
                messages: vec![Message {
                    role: "user".to_string(),
                    content: vec!["remember via space token".to_string().into()],
                    ..Default::default()
                }],
                context: None,
                timestamp: None,
            }),
        )
        .await
        {
            Ok(value) => response_json(value.into_response()).await,
            Err(err) => panic!("unexpected formation error: {}", err.message),
        };
        let formation_id = formation["result"]["conversation"].as_u64().unwrap();
        for _ in 0..100 {
            if !space.is_processing() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(!space.is_processing());

        let conversation = ok_json(
            get_conversation(
                State(app.clone()),
                Path((space_id.to_string(), formation_id.to_string())),
                Query(ConversationDeltaQuery {
                    messages_offset: None,
                    artifacts_offset: None,
                    collection: None,
                }),
                accept_json(),
                HeaderVals(read_token.clone(), 0),
            )
            .await,
        )
        .await;
        assert_eq!(conversation["result"]["_id"], formation_id);

        let delta = ok_json(
            get_conversation_delta(
                State(app.clone()),
                Path((space_id.to_string(), formation_id.to_string())),
                Query(ConversationDeltaQuery {
                    messages_offset: Some(0),
                    artifacts_offset: Some(0),
                    collection: None,
                }),
                accept_json(),
                HeaderVals(read_token.clone(), 0),
            )
            .await,
        )
        .await;
        assert_eq!(delta["result"]["_id"], formation_id);

        let list = ok_json(
            list_conversations(
                State(app.clone()),
                Path(space_id.to_string()),
                Query(Pagination {
                    cursor: None,
                    limit: Some(5),
                    collection: None,
                }),
                accept_json(),
                HeaderVals(read_token, 0),
            )
            .await,
        )
        .await;
        assert!(
            list["result"]
                .as_array()
                .is_some_and(|items| !items.is_empty())
        );

        let maintenance = ok_json(
            post_maintenance(
                State(app),
                Path(space_id.to_string()),
                accept_json(),
                HeaderVals(write_token, 0),
                json_bytes(&MaintenanceInput {
                    scope: MaintenanceScope::Quick,
                    ..Default::default()
                }),
            )
            .await,
        )
        .await;
        assert!(maintenance["result"]["conversation"].is_number());
    }

    #[tokio::test]
    async fn wiki_handlers_commit_search_read_and_conflict_mapping() {
        let app = test_app_state("handler_wiki", 0);
        let space_id = "handler_wiki_space".to_string();
        create_loaded_space(&app, &space_id).await;

        // Commit via JSON body.
        let commit = ok_json(
            post_wiki_commit(
                State(app.clone()),
                Path(space_id.clone()),
                accept_json(),
                headers(&app),
                json_bytes(&crate::wiki::WikiCommitInput {
                    title: "回滚手册".to_string(),
                    content: "# 回滚手册\n\n生产事故时按引用校验和回滚到上一版本。\n".to_string(),
                    ..Default::default()
                }),
            )
            .await,
        )
        .await;
        let doc_id = commit["result"]["doc"]["id"].as_u64().unwrap();
        let version_id = commit["result"]["version"]["id"].as_u64().unwrap();
        assert!(commit["result"]["created"].as_bool().unwrap());

        // Search returns hits with citations.
        let search = ok_json(
            post_wiki_search(
                State(app.clone()),
                Path(space_id.clone()),
                accept_json(),
                headers(&app),
                json_bytes(&crate::wiki::WikiSearchInput::from_query(
                    "回滚".to_string(),
                )),
            )
            .await,
        )
        .await;
        let uri = search["result"]["hits"][0]["citation"]["uri"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(uri.starts_with(&format!("wiki://{space_id}/{doc_id}@{version_id}#")));

        // Doc detail with TOC, and full content read.
        let detail = ok_json(
            get_wiki_doc(
                State(app.clone()),
                Path((space_id.clone(), doc_id)),
                accept_json(),
                headers(&app),
            )
            .await,
        )
        .await;
        assert!(
            detail["result"]["toc"]
                .as_array()
                .is_some_and(|t| !t.is_empty())
        );
        let content = ok_json(
            get_wiki_content(
                State(app.clone()),
                Path((space_id.clone(), doc_id)),
                Query(WikiContentQuery::default()),
                accept_json(),
                headers(&app),
            )
            .await,
        )
        .await;
        assert!(
            content["result"]["content"]
                .as_str()
                .unwrap()
                .contains("回滚")
        );

        // Verify the citation URI over HTTP.
        let verify = ok_json(
            post_wiki_verify(
                State(app.clone()),
                Path(space_id.clone()),
                accept_json(),
                headers(&app),
                json_bytes(&crate::wiki::WikiVerifyInput {
                    uri: Some(
                        uri.replace(&format!("wiki://{space_id}"), "wiki://handler_wiki_space"),
                    ),
                    ..Default::default()
                }),
            )
            .await,
        )
        .await;
        assert_eq!(verify["result"]["status"].as_str(), Some("valid"));

        // Stale CAS maps to 409 with structured current_version data.
        let conflict = err_json(
            post_wiki_commit(
                State(app.clone()),
                Path(space_id.clone()),
                accept_json(),
                headers(&app),
                json_bytes(&crate::wiki::WikiCommitInput {
                    doc_id: Some(doc_id),
                    parent_version: Some(version_id + 999),
                    title: "回滚手册".to_string(),
                    content: "# 回滚手册\n\n不同内容。\n".to_string(),
                    ..Default::default()
                }),
            )
            .await,
            StatusCode::CONFLICT,
        )
        .await;
        assert_eq!(
            conflict["error"]["data"]["current_version"].as_u64(),
            Some(version_id)
        );

        // Unknown doc maps to 404.
        let _ = err_json(
            get_wiki_doc(
                State(app.clone()),
                Path((space_id.clone(), 999_999)),
                accept_json(),
                headers(&app),
            )
            .await,
            StatusCode::NOT_FOUND,
        )
        .await;
    }

    /// M4 acceptance over HTTP: a space token restricted to other labels
    /// cannot retrieve a labeled probe document; a token granted the label
    /// can. The filter runs inside the retrieval query itself.
    #[tokio::test]
    async fn wiki_restricted_token_cannot_see_labeled_probe() {
        let app = test_app_state_with_auth_enabled("handler_wiki_acl", 0);
        let space_id = "handler_wiki_acl_space".to_string();
        let space = create_loaded_space(&app, &space_id).await;

        // Seed one open and one labeled probe document directly.
        space
            .wiki
            .commit(
                "admin".to_string(),
                crate::wiki::WikiCommitInput {
                    title: "公开文档".to_string(),
                    content: "# 公开文档\n\n公开探针：晨雾灯塔。\n".to_string(),
                    ..Default::default()
                },
                unix_ms(),
            )
            .await
            .unwrap();
        space
            .wiki
            .commit(
                "admin".to_string(),
                crate::wiki::WikiCommitInput {
                    title: "机密文档".to_string(),
                    content: "# 机密文档\n\n机密探针：夜航坐标。\n".to_string(),
                    acl_label: Some("secret".to_string()),
                    ..Default::default()
                },
                unix_ms(),
            )
            .await
            .unwrap();

        // Two space tokens: one restricted to an unrelated label, one
        // granted "secret".
        space
            .add_space_token(
                "STouter".to_string(),
                AddSpaceTokenInput {
                    scope: TokenScope::Read,
                    name: "outsider".to_string(),
                    expires_at: None,
                    labels: Some(vec!["public-team".to_string()]),
                },
                unix_ms(),
            )
            .await
            .unwrap();
        space
            .add_space_token(
                "STinner".to_string(),
                AddSpaceTokenInput {
                    scope: TokenScope::Read,
                    name: "insider".to_string(),
                    expires_at: None,
                    labels: Some(vec!["secret".to_string()]),
                },
                unix_ms(),
            )
            .await
            .unwrap();

        let search = |token: &str, query: &str| {
            let app = app.clone();
            let space_id = space_id.clone();
            let token = token.to_string();
            let query = query.to_string();
            async move {
                ok_json(
                    post_wiki_search(
                        State(app),
                        Path(space_id),
                        accept_json(),
                        HeaderVals(token, 0),
                        json_bytes(&crate::wiki::WikiSearchInput::from_query(query)),
                    )
                    .await,
                )
                .await
            }
        };

        // Restricted token: the labeled probe is invisible at the database
        // level, unlabeled content still searchable.
        let miss = search("STouter", "夜航坐标").await;
        assert_eq!(miss["result"]["hits"].as_array().unwrap().len(), 0);
        assert_eq!(miss["result"]["total_docs_matched"].as_u64(), Some(0));
        let open_hit = search("STouter", "晨雾灯塔").await;
        assert_eq!(open_hit["result"]["hits"].as_array().unwrap().len(), 1);

        // Granted token sees the probe.
        let hit = search("STinner", "夜航坐标").await;
        assert_eq!(hit["result"]["hits"].as_array().unwrap().len(), 1);

        // Restricted tokens cannot read the audit log.
        let _ = err_json(
            list_wiki_events(
                State(app.clone()),
                Path(space_id.clone()),
                Query(WikiEventsQuery::default()),
                accept_json(),
                HeaderVals("STouter".to_string(), 0),
            )
            .await,
            StatusCode::FORBIDDEN,
        )
        .await;
    }

    #[tokio::test]
    async fn wiki_import_export_round_trip_over_http() {
        let app = test_app_state("handler_wiki_okf", 0);
        let space_id = "handler_wiki_okf_space".to_string();
        create_loaded_space(&app, &space_id).await;

        let import = ok_json(
            post_wiki_import(
                State(app.clone()),
                Path(space_id.clone()),
                accept_json(),
                headers(&app),
                json_bytes(&crate::wiki::WikiImportInput {
                    entries: vec![crate::wiki::WikiBundleEntry {
                        path: "guides/setup.md".to_string(),
                        content: "---\ntype: Guide\ncustom_key: 保留\n---\n\n# 安装指南\n\n执行安装脚本。\n"
                            .to_string(),
                    }],
                    namespace: Some("kb".to_string()),
                }),
            )
            .await,
        )
        .await;
        assert_eq!(import["result"]["created"].as_u64(), Some(1));

        let export = ok_json(
            get_wiki_export(
                State(app.clone()),
                Path(space_id.clone()),
                Query(WikiExportQuery {
                    namespace: Some("kb".to_string()),
                }),
                accept_json(),
                headers(&app),
            )
            .await,
        )
        .await;
        assert_eq!(export["result"]["docs"].as_u64(), Some(1));
        let entries = export["result"]["entries"].as_array().unwrap();
        let paths: Vec<&str> = entries
            .iter()
            .map(|e| e["path"].as_str().unwrap())
            .collect();
        assert!(paths.contains(&"guides/setup.md"));
        assert!(paths.contains(&"index.md"));
        assert!(paths.contains(&"manifest.json"));
        let doc_entry = entries
            .iter()
            .find(|e| e["path"] == "guides/setup.md")
            .unwrap();
        let content = doc_entry["content"].as_str().unwrap();
        assert!(content.contains("custom_key: 保留"));
        assert!(content.contains("x_anda_doc_id:"));
    }
}

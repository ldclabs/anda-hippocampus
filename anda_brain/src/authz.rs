//! Shared authorization layer for the HTTP and MCP channels.
//!
//! Every space-scoped endpoint runs the same prelude — sharding check, CWT
//! verification, space load, space-token verification — and both channels
//! must resolve the caller's audit actor and wiki ACL view identically (the
//! wiki launch review's P0-1 was exactly such a divergence). This module is
//! the single source for that logic: HTTP handlers call [`authorize`] (or
//! its [`ensure_sharding`]/[`check_cwt`]/[`load_space`] pieces when a body
//! parse sits between the historical steps and error precedence must be
//! preserved), and the MCP channel wraps [`authorize`] thinly, keeping only
//! auto-create on its side.
//!
//! Channel-specific error surfaces stay channel-local:
//! `From<AuthzError> for AppError` maps to HTTP statuses without leaking
//! storage paths (Debug detail goes to the log only), and the MCP side
//! provides an equivalent `authz_error_data` mapping in `mcp.rs`.

use http::StatusCode;
use std::sync::Arc;

use crate::{
    payload::AppError,
    space::{AppState, Space},
    types::{CWToken, SpaceToken, TokenScope},
    wiki::WikiAccess,
};

/// How an endpoint admits callers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthzMode {
    /// Read endpoints whose result depends on the caller's identity or ACL
    /// view (wiki reads, conversations, agentic recall): public spaces are
    /// anonymously readable, but a supplied space token is verified even
    /// there so a labeled token keeps its granted labels instead of
    /// silently widening to the anonymous view; only absent/invalid
    /// credentials fall back to anonymous.
    PublicRead,
    /// Read endpoints that never consume the caller's identity (info,
    /// status, probe, read-only KIP): token verification is skipped
    /// entirely on public spaces, preserving their historical semantics —
    /// including not bumping a supplied space token's usage counter there.
    PublicReadLenient,
    /// Endpoints that always require a credential with the given scope: a
    /// CWT, or failing that a space token.
    Credentialed,
    /// Management endpoints: only a CWT is accepted; space tokens are
    /// rejected outright.
    CwtOnly,
}

/// The verified caller: at most one of a CWT or a space token (the CWT wins
/// when both could apply, matching `check_auth_if`'s precedence).
pub struct Caller {
    pub cwt: Option<CWToken>,
    pub st: Option<SpaceToken>,
}

impl Caller {
    /// Resolves the audit actor: the authenticated CWT user, else a stable
    /// space-token identity, else the anonymous marker (public-space readers
    /// with no credential must not be recorded as the space's own identity).
    /// Shared with the MCP channel so both audit trails name identical
    /// subjects.
    pub fn actor(&self) -> String {
        wiki_actor(&self.cwt, self.st.as_ref())
    }

    /// The caller's wiki read scope; see [`wiki_read_access`].
    pub fn wiki_access(&self) -> WikiAccess {
        wiki_read_access(&self.cwt, self.st.as_ref())
    }

    /// True when the caller is label-restricted; see [`label_restricted`].
    pub fn label_restricted(&self) -> bool {
        label_restricted(self.st.as_ref())
    }

    /// Agentic-recall guard: RecallAgent's wiki tools span all labels, so a
    /// label-restricted token cannot use recall (mirrors the /wiki/events
    /// guard). Single source for HTTP `/recall{,_structured}` and the MCP
    /// recall tool, so the denial reason cannot drift between channels.
    pub fn recall_forbidden(&self) -> Option<&'static str> {
        if self.label_restricted() {
            Some("recall requires an unrestricted token")
        } else {
            None
        }
    }

    /// Conversation-read guard; see [`conversation_read_forbidden`].
    pub fn conversation_read_forbidden(&self, collection: Option<&str>) -> Option<&'static str> {
        conversation_read_forbidden(&self.cwt, self.st.as_ref(), collection)
    }
}

/// Authorization failure. Channel-specific renderings live in
/// `From<AuthzError> for AppError` (HTTP) and `mcp::authz_error_data` (MCP);
/// the `display`/`debug` load details are pre-rendered strings so no live
/// error object (which could embed storage paths) crosses this boundary.
#[derive(Debug)]
pub enum AuthzError {
    ShardMismatch {
        sharding: u32,
        expected: u32,
    },
    Unauthorized(TokenScope),
    /// The space does not exist (`DBError::NotFound`).
    SpaceNotFound {
        space_id: String,
        display: String,
        debug: String,
    },
    /// The space exists but failed to load.
    SpaceLoad {
        space_id: String,
        display: String,
        debug: String,
    },
    /// The credential is valid but not allowed on this surface (e.g. a
    /// label-restricted token reaching an all-labels endpoint).
    Forbidden(&'static str),
}

impl From<AuthzError> for AppError {
    fn from(err: AuthzError) -> Self {
        match err {
            AuthzError::ShardMismatch { sharding, expected } => AppError::bad_request(format!(
                "space_id sharding {} does not match server sharding {}",
                sharding, expected
            )),
            AuthzError::Unauthorized(_) => AppError::unauthorized(),
            // A nonexistent space is 404 with a generic message, anything
            // else 500. Either way the Debug detail stays in the log —
            // AndaDB/object-store errors can embed storage paths, which must
            // not leak into response bodies.
            AuthzError::SpaceNotFound {
                space_id, debug, ..
            } => {
                let space_id = space_id.as_str();
                log::warn!(target: "brain", space_id; "failed to load space: {debug}");
                AppError::with_status(StatusCode::NOT_FOUND, "space not found")
            }
            AuthzError::SpaceLoad {
                space_id, debug, ..
            } => {
                let space_id = space_id.as_str();
                log::warn!(target: "brain", space_id; "failed to load space: {debug}");
                AppError::with_status(StatusCode::INTERNAL_SERVER_ERROR, "failed to load space")
            }
            AuthzError::Forbidden(message) => AppError::with_status(StatusCode::FORBIDDEN, message),
        }
    }
}

pub fn ensure_sharding(app: &AppState, sharding: u32) -> Result<(), AuthzError> {
    if sharding != app.sharding {
        return Err(AuthzError::ShardMismatch {
            sharding,
            expected: app.sharding,
        });
    }
    Ok(())
}

/// Verifies a CWT with the required scope (management surfaces where space
/// tokens are never accepted).
pub fn check_cwt(
    app: &AppState,
    space_id: &str,
    token: &str,
    scope: TokenScope,
    now_ms: u64,
) -> Result<CWToken, AuthzError> {
    app.check_auth(token, space_id, scope, now_ms)
        .map_err(|_| AuthzError::Unauthorized(scope))
}

/// Loads a space, classifying the failure as [`AuthzError::SpaceNotFound`]
/// or [`AuthzError::SpaceLoad`]. The raw error is rendered here (Display for
/// the MCP message, Debug for the HTTP-side log) and dropped.
pub async fn load_space(app: &AppState, space_id: &str) -> Result<Arc<Space>, AuthzError> {
    app.load_space(space_id, false).await.map_err(|err| {
        let not_found = matches!(
            err.downcast_ref::<anda_db::error::DBError>(),
            Some(anda_db::error::DBError::NotFound { .. })
        );
        let space_id = space_id.to_string();
        let display = err.to_string();
        let debug = format!("{err:?}");
        if not_found {
            AuthzError::SpaceNotFound {
                space_id,
                display,
                debug,
            }
        } else {
            AuthzError::SpaceLoad {
                space_id,
                display,
                debug,
            }
        }
    })
}

/// The shared authorization prelude: sharding check (when `sharding` is
/// `Some`), CWT verification, space load, then space-token verification per
/// [`AuthzMode`]. Step order matters and mirrors what every call site did
/// historically: authentication failures surface before load failures, and
/// space-token verification (which bumps the token's usage counter) runs
/// only after the space is available.
pub async fn authorize(
    app: &AppState,
    space_id: &str,
    token: &str,
    sharding: Option<u32>,
    scope: TokenScope,
    mode: AuthzMode,
    now_ms: u64,
) -> Result<(Arc<Space>, Caller), AuthzError> {
    if let Some(sharding) = sharding {
        ensure_sharding(app, sharding)?;
    }

    let cwt = match mode {
        AuthzMode::CwtOnly => Some(check_cwt(app, space_id, token, scope, now_ms)?),
        _ => app
            .check_auth_if(token, space_id, scope, now_ms)
            .map_err(|_| AuthzError::Unauthorized(scope))?,
    };

    let space = load_space(app, space_id).await?;

    let st = match mode {
        AuthzMode::CwtOnly => None,
        _ if cwt.is_some() => None,
        AuthzMode::Credentialed => Some(
            space
                .verify_space_token(token.to_string(), scope, now_ms)
                .map_err(|_| AuthzError::Unauthorized(scope))?,
        ),
        AuthzMode::PublicRead => {
            // A supplied token is verified even on public spaces so its ACL
            // restriction is honored rather than silently widened; only
            // absent/invalid credentials fall back to the anonymous view.
            match space.verify_space_token(token.to_string(), scope, now_ms) {
                Ok(st) => Some(st),
                Err(_) if space.is_public() => None,
                Err(_) => return Err(AuthzError::Unauthorized(scope)),
            }
        }
        AuthzMode::PublicReadLenient => {
            if space.is_public() {
                None
            } else {
                Some(
                    space
                        .verify_space_token(token.to_string(), scope, now_ms)
                        .map_err(|_| AuthzError::Unauthorized(scope))?,
                )
            }
        }
    };

    Ok((space, Caller { cwt, st }))
}

/// Resolves the audit actor: the authenticated CWT user, else a stable
/// space-token identity, else the anonymous marker (public-space readers
/// with no credential must not be recorded as the space's own identity).
/// Shared by the HTTP and MCP channels so both audit trails name identical
/// subjects.
pub fn wiki_actor(t: &Option<CWToken>, st: Option<&SpaceToken>) -> String {
    if let Some(t) = t {
        return t.user.to_string();
    }
    match st {
        Some(st) if !st.name.trim().is_empty() => format!("st:{}", st.name.trim()),
        Some(_) => "st:unnamed".to_string(),
        None => "anonymous".to_string(),
    }
}

/// True when the space token is label-restricted (a read-only wiki viewer,
/// PRD §8.2). Such tokens must not reach surfaces that span all labels:
/// agentic recall (its wiki tools run unrestricted), the audit event log,
/// and stored conversations — recall conversations persist the full runner
/// history, including wiki tool output from behind any label. Shared by the
/// HTTP and MCP channels so the guards cannot drift apart.
pub fn label_restricted(st: Option<&SpaceToken>) -> bool {
    st.is_some_and(|st| st.labels.is_some())
}

/// Conversation-read guard shared by HTTP and MCP (so the channels cannot
/// drift apart). Label-restricted tokens are denied for every collection:
/// stored conversations persist the full runner history, which would bypass
/// the token's wiki ACL. Recall conversations additionally deny the
/// anonymous public fallback: recall runs on a then-private space embed
/// labeled wiki tool output verbatim, and flipping the space public later
/// must not hand that history to the world.
pub fn conversation_read_forbidden(
    t: &Option<CWToken>,
    st: Option<&SpaceToken>,
    collection: Option<&str>,
) -> Option<&'static str> {
    if label_restricted(st) {
        return Some("conversations require an unrestricted token");
    }
    if collection == Some("recall") && t.is_none() && st.is_none() {
        return Some(
            "recall conversations require a credential; anonymous public access is denied",
        );
    }
    None
}

/// Read scope resolution (PRD §8.2): CWT holders and label-less space
/// tokens are unrestricted; labeled tokens see unlabeled content plus their
/// labels; anonymous public-space readers see unlabeled content only.
/// Shared by the HTTP and MCP channels so the two never diverge (the launch
/// review's P0-1 was exactly such a divergence).
pub fn wiki_read_access(t: &Option<CWToken>, st: Option<&SpaceToken>) -> WikiAccess {
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

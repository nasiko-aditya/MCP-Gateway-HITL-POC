//! Agent-facing HTTP surface: `POST /mcp` (the JSON-RPC gateway) and three
//! small advisory/write endpoints that exist only so the agent can make its
//! own HITL decisions without the gateway making them on its behalf:
//!
//! - `GET /policy/{connector}/{tool}` — a read-only wrapper over
//!   `PermissionPolicy::decide`. Answering "the stance is `ask`" is a fact
//!   lookup, not the gateway deciding to ask a human.
//! - `GET /connectors/{connector}/status` — a read-only wrapper over
//!   whichever connection check applies to that connector's *type*: the
//!   local `CredentialStore` for a mock connector, Composio's real
//!   connected-account status for the live connector. Never a per-tool or
//!   per-connector-name special case.
//! - `POST /connectors/{connector}/credentials` — write endpoint used only
//!   by the agent's `authenticate` resume path for local/mock connectors (a
//!   Composio connector authenticates via `auth_url` + out-of-band OAuth
//!   instead, and this endpoint refuses it).
//! - `DELETE /connectors/{connector}/credentials` — the symmetric "log out"
//!   for either connector kind: deletes the local credential row for a
//!   mock connector, or deletes (and best-effort revokes) the real
//!   Composio connected account for the live one. Not a HITL concern
//!   either way — it only changes what the *next* `CredentialCheck` finds.
//!
//! POC simplification (unchanged from before): the real Nasiko gateway
//! authenticates the caller via a signed delegation JWT
//! (`x-nasiko-agent-token`, see `oss/auth/src/jwt.rs`). This POC instead
//! reads plain `x-user-id`/`x-agent-id` headers, plus one more POC-only
//! header, `x-agent-preflight: passed`, which the agent's own
//! `GatewayClient` sets on every dispatch it has already pre-flighted (see
//! `protocol.rs` module docs for exactly what that header does and doesn't
//! authorize). A production gateway would fold this into the same signed
//! token rather than a second unauthenticated header.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use super::protocol::{self, RequestContext};
use crate::state::GatewayState;

pub fn router(state: Arc<GatewayState>) -> Router {
    Router::new()
        .route("/mcp", post(handle_mcp))
        .route("/policy/:connector/:tool", get(get_policy))
        .route("/connectors/:connector/status", get(get_connector_status))
        .route(
            "/connectors/:connector/credentials",
            post(post_connector_credentials).delete(delete_connector_credentials),
        )
        .with_state(state)
}

pub(crate) fn identity_from(headers: &HeaderMap) -> (String, String) {
    let user_id = headers
        .get("x-user-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("demo-user")
        .to_string();
    let agent_id = headers
        .get("x-agent-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("demo-agent")
        .to_string();
    (user_id, agent_id)
}

fn call_id_from(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-call-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

fn preflight_approved_from(headers: &HeaderMap) -> bool {
    headers
        .get("x-agent-preflight")
        .and_then(|v| v.to_str().ok())
        == Some("passed")
}

async fn handle_mcp(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    let (user_id, agent_id) = identity_from(&headers);
    let call_id_hint = call_id_from(&headers);
    let rctx = RequestContext {
        user_id: &user_id,
        agent_id: &agent_id,
        call_id_hint: call_id_hint.as_deref(),
        preflight_approved: preflight_approved_from(&headers),
    };
    let response = protocol::handle_request(&state, &rctx, &body).await;
    Json(response.unwrap_or(Value::Null))
}

async fn get_policy(
    State(state): State<Arc<GatewayState>>,
    Path((connector, tool)): Path<(String, String)>,
) -> Json<Value> {
    let stance = state.policy.decide(&connector, &tool);
    Json(json!({ "connector": connector, "tool_name": tool, "stance": stance }))
}

/// `connected: false` here means "checked, and genuinely not connected yet"
/// — the agent's normal `AuthRequired` trigger. A failure of the check
/// itself (Composio API error, DB error) is a different thing entirely and
/// must not be reported the same way, or the agent would pause asking a
/// human to authenticate a connector that's actually just unreachable right
/// now. Both branches below return a hard error in that case instead.
///
/// A connector name that isn't registered at all (typo, or simply never
/// configured) is a third, even more different thing: it must not be
/// checked against the local `CredentialStore` at all, because that store
/// has no idea a connector is supposed to exist and will happily answer
/// "not connected" for any string — indistinguishable from a real
/// `AuthRequired` pause on a real connector. Rejected up front instead, so
/// the agent's pre-flight sees a clear "no such connector" error rather than
/// pausing to ask a human to authenticate something that was never wired up.
async fn get_connector_status(
    State(state): State<Arc<GatewayState>>,
    Path(connector): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let (user_id, _) = identity_from(&headers);

    if !protocol::connector_known(&state, &connector) {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("unknown connector '{connector}'") })),
        ));
    }

    if let Some(slot) = &state.composio {
        if slot.connector_name == connector {
            let connected = slot.client.is_connected(&user_id).await.map_err(|e| {
                tracing::error!(connector = %connector, error = %e, "composio connected-account check failed");
                (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({
                        "error": format!("composio connected-account check failed for '{connector}': {e}")
                    })),
                )
            })?;
            let auth_url = if connected {
                None
            } else {
                let url = slot.client.auth_url(&user_id).await.map_err(|e| {
                    tracing::error!(connector = %connector, error = %e, "composio auth-url request failed");
                    (
                        StatusCode::BAD_GATEWAY,
                        Json(json!({
                            "error": format!("composio auth-url request failed for '{connector}': {e}")
                        })),
                    )
                })?;
                Some(url)
            };
            return Ok(Json(
                json!({ "connector": connector, "connected": connected, "auth_url": auth_url }),
            ));
        }
    }

    let connected = state.credentials.is_valid(&connector).await.map_err(|e| {
        tracing::error!(connector = %connector, error = %e, "credential store lookup failed");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("credential lookup failed for '{connector}': {e}") })),
        )
    })?;
    Ok(Json(
        json!({ "connector": connector, "connected": connected, "auth_url": Value::Null }),
    ))
}

#[derive(Deserialize)]
struct CredentialsRequest {
    token: String,
}

async fn post_connector_credentials(
    State(state): State<Arc<GatewayState>>,
    Path(connector): Path<String>,
    Json(body): Json<CredentialsRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if let Some(slot) = &state.composio {
        if slot.connector_name == connector {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": format!(
                        "connector '{connector}' authenticates via Composio's auth_url (OAuth), \
                         not a submitted token"
                    )
                })),
            ));
        }
    }

    state
        .credentials
        .store(&connector, &body.token, None)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
        })?;
    Ok(Json(json!({ "connector": connector, "stored": true })))
}

/// `DELETE /connectors/{connector}/credentials` — logs a connector out.
/// Mirrors `post_connector_credentials`'s Composio special-case, but here
/// Composio *is* supported (unlike the token-write path, which it refuses)
/// since disconnecting is a real, well-defined operation on Composio's side
/// (`ComposioConnector::disconnect`), not a submitted-token concept.
async fn delete_connector_credentials(
    State(state): State<Arc<GatewayState>>,
    Path(connector): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let (user_id, _) = identity_from(&headers);

    if let Some(slot) = &state.composio {
        if slot.connector_name == connector {
            let accounts_removed = slot.client.disconnect(&user_id).await.map_err(|e| {
                tracing::error!(connector = %connector, error = %e, "composio disconnect failed");
                (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({
                        "error": format!("composio disconnect failed for '{connector}': {e}")
                    })),
                )
            })?;
            return Ok(Json(
                json!({ "connector": connector, "disconnected": true, "accounts_removed": accounts_removed }),
            ));
        }
    }

    state.credentials.delete(&connector).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
    })?;
    Ok(Json(
        json!({ "connector": connector, "disconnected": true }),
    ))
}

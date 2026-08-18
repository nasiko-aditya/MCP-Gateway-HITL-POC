//! The Human Action API — the only interface a human (or a UI standing in
//! for one) needs to drive HITL: list what's pending, inspect one
//! checkpoint, and respond to it. `POST /hitl/{id}/respond` is also the
//! entire "resume" mechanism: a brand-new HTTP request that reloads the
//! checkpoint from the DB and re-enters the agent's own pre-flight loop
//! (`agent::preflight`) used for a fresh call — nothing about the original
//! request is kept open or replayed from memory. This lives entirely on
//! the agent side of the process boundary: the gateway is never involved in
//! deciding or resolving a pause, only in the final `Dispatch` step that
//! `finish_resume` reaches through `agent::preflight::resume`.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::agent::checkpoint::{
    Checkpoint, CheckpointStatus, HumanAction, PauseReason, ToolOutcome,
};
use crate::agent::preflight::{self, PreflightOutcome, TaskContext};
use crate::agent::state::AgentState;

pub fn router(state: Arc<AgentState>) -> Router {
    Router::new()
        .route("/hitl/pending", get(list_pending))
        .route("/hitl/:id", get(get_checkpoint))
        .route("/hitl/:id/respond", post(respond))
        .route("/audit/:call_id", get(get_audit))
        .with_state(state)
}

type ApiResult = Result<Json<Value>, (StatusCode, Json<Value>)>;

fn bad_request(message: impl Into<String>) -> (StatusCode, Json<Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": message.into() })),
    )
}

fn to_view(cp: &Checkpoint) -> Value {
    json!({
        "id": cp.id,
        "call_id": cp.call_id,
        "user_id": cp.user_id,
        "agent_id": cp.agent_id,
        "connector": cp.connector,
        "tool_name": cp.tool_name,
        "tool_arguments": cp.tool_arguments,
        "reason": cp.reason,
        "question": cp.reason.question(),
        "expected_action": cp.reason.expected_action(),
        "status": cp.status.as_str(),
        "human_response": cp.human_response,
        "result": cp.result,
        "error": cp.error,
        "created_at": cp.created_at,
        "updated_at": cp.updated_at,
    })
}

async fn list_pending(State(state): State<Arc<AgentState>>) -> ApiResult {
    let pending = state.checkpoints.list_pending().await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
    })?;
    Ok(Json(
        json!({ "checkpoints": pending.iter().map(to_view).collect::<Vec<_>>() }),
    ))
}

/// `GET /audit/{call_id}` — the full audit trail for one task: who
/// initiated it, why HITL triggered (if it did), the human's action, and
/// the final status — including the gateway's own dispatch-side entries
/// for the same `call_id` (both services share the audit log). Read-only,
/// append-only history; never contains a credential token (see
/// `redacted_response_json`).
async fn get_audit(State(state): State<Arc<AgentState>>, Path(call_id): Path<String>) -> ApiResult {
    let entries = state.audit.list_for_call(&call_id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
    })?;
    Ok(Json(json!({ "call_id": call_id, "entries": entries })))
}

async fn get_checkpoint(State(state): State<Arc<AgentState>>, Path(id): Path<Uuid>) -> ApiResult {
    let cp = state
        .checkpoints
        .get(id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
        })?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": format!("no checkpoint '{id}'") })),
            )
        })?;
    Ok(Json(to_view(&cp)))
}

/// Validates that `action` is a legal response to `reason` — `Deny` is
/// universally valid; the other three each match exactly one `PauseReason`.
/// This is the check behind the "invalid HITL responses are rejected" test
/// — e.g. `approve` against an `InputRequired` checkpoint.
fn action_matches_reason(action: &HumanAction, reason: &PauseReason) -> bool {
    match action {
        HumanAction::Deny => true,
        HumanAction::Approve => matches!(reason, PauseReason::ApprovalRequired { .. }),
        HumanAction::Authenticate { .. } => matches!(reason, PauseReason::AuthRequired { .. }),
        HumanAction::Input { .. } => matches!(reason, PauseReason::InputRequired { .. }),
    }
}

async fn respond(
    State(state): State<Arc<AgentState>>,
    Path(id): Path<Uuid>,
    Json(action): Json<HumanAction>,
) -> ApiResult {
    let cp = state
        .checkpoints
        .get(id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
        })?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": format!("no checkpoint '{id}'") })),
            )
        })?;

    if !cp.status.is_pending() {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({
                "error": format!("checkpoint '{id}' is not pending (status: {})", cp.status.as_str()),
            })),
        ));
    }

    if !action_matches_reason(&action, &cp.reason) {
        return Err(bad_request(format!(
            "action does not match this checkpoint's pending reason (expected '{}')",
            cp.reason.expected_action()
        )));
    }

    let human_response = redacted_response_json(&action);
    let claimed = state
        .checkpoints
        .claim_pending(id, &human_response)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
        })?;
    let Some(_claimed) = claimed else {
        // Lost the race: a concurrent request already claimed this exact
        // checkpoint (duplicate response, or a second concurrent resume).
        return Err((
            StatusCode::CONFLICT,
            Json(json!({ "error": format!("checkpoint '{id}' was already responded to") })),
        ));
    };

    let ctx = TaskContext {
        call_id: cp.call_id.clone(),
        user_id: cp.user_id.clone(),
        agent_id: cp.agent_id.clone(),
        connector: cp.connector.clone(),
        tool_name: cp.tool_name.clone(),
    };

    match action {
        HumanAction::Deny => {
            state
                .checkpoints
                .finalize(id, CheckpointStatus::Denied, None, None)
                .await
                .map_err(|e| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": e.to_string() })),
                    )
                })?;
            audit(&state, &cp, "denied", &json!({})).await;
            Ok(Json(json!({ "status": "denied", "checkpoint_id": id })))
        }

        HumanAction::Approve => {
            audit(&state, &cp, "approved", &json!({})).await;
            finish_resume(&state, &cp, &ctx, &cp.tool_arguments, id).await
        }

        HumanAction::Authenticate { token } => {
            // Generic across connector kinds: if a token was supplied
            // (the local/mock path), forward it to the gateway's
            // credential store on a best-effort basis — a Composio
            // connector rejects this write, which is fine, since
            // `finish_resume` re-verifies the real connection status
            // (via `PauseReason::resume_from` re-entering
            // `CredentialCheck`) rather than trusting this write to have
            // succeeded. Neither branch ever matches on a connector name.
            if let Some(token) = &token {
                let _ = state.gateway.store_credential(&cp.connector, token).await;
            }
            audit(
                &state,
                &cp,
                "authenticated",
                &json!({ "connector": cp.connector, "token_provided": token.is_some() }),
            )
            .await;
            finish_resume(&state, &cp, &ctx, &cp.tool_arguments, id).await
        }

        HumanAction::Input { fields } => {
            let Some(fields_obj) = fields.as_object() else {
                return Err(bad_request("'fields' must be a JSON object"));
            };
            let mut merged = cp.tool_arguments.clone();
            let merged_obj = merged.as_object_mut().ok_or_else(|| {
                bad_request("checkpoint's stored tool_arguments is not a JSON object")
            })?;
            for (k, v) in fields_obj {
                merged_obj.insert(k.clone(), v.clone());
            }
            state
                .checkpoints
                .update_arguments(id, &merged)
                .await
                .map_err(|e| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": e.to_string() })),
                    )
                })?;
            audit(&state, &cp, "input_provided", &json!({ "fields": fields })).await;
            finish_resume(&state, &cp, &ctx, &merged, id).await
        }
    }
}

/// Human-provided data recorded on the checkpoint row. Redacts a supplied
/// auth token — the audit/checkpoint history must never carry a credential.
fn redacted_response_json(action: &HumanAction) -> Value {
    match action {
        HumanAction::Deny => json!({ "action": "deny" }),
        HumanAction::Approve => json!({ "action": "approve" }),
        HumanAction::Authenticate { token } => {
            json!({ "action": "authenticate", "token_provided": token.is_some() })
        }
        HumanAction::Input { fields } => json!({ "action": "input", "fields": fields }),
    }
}

async fn audit(state: &AgentState, cp: &Checkpoint, action: &str, detail: &Value) {
    let _ = state
        .audit
        .record(
            Some(cp.id),
            &cp.call_id,
            &cp.user_id,
            &cp.agent_id,
            &cp.connector,
            &cp.tool_name,
            action,
            detail,
        )
        .await;
}

/// Continues the pre-flight loop from `cp.resume_from` and turns the result
/// into the checkpoint's terminal (or re-paused) state. Shared by every
/// "positive" resume action (`approve`/`authenticate`/`input`) — none of
/// them know or care which step comes next, only `PauseReason` does (see
/// `agent::checkpoint::PauseReason::resume_from`).
async fn finish_resume(
    state: &AgentState,
    cp: &Checkpoint,
    ctx: &TaskContext,
    arguments: &Value,
    id: Uuid,
) -> ApiResult {
    let outcome = preflight::resume(state, ctx, arguments, cp.resume_from)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
        })?;

    match outcome {
        PreflightOutcome::Paused { reason } => {
            state.checkpoints.re_pause(id, &reason).await.map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": e.to_string() })),
                )
            })?;
            audit(state, cp, "re_paused", &json!({ "reason": reason })).await;
            Ok(Json(json!({
                "status": "pending",
                "checkpoint_id": id,
                "reason": reason,
                "question": reason.question(),
            })))
        }
        PreflightOutcome::Done(ToolOutcome::Success(result)) => {
            state
                .checkpoints
                .finalize(id, CheckpointStatus::Resolved, Some(&result), None)
                .await
                .map_err(|e| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": e.to_string() })),
                    )
                })?;
            audit(state, cp, "resumed_success", &json!({})).await;
            Ok(Json(
                json!({ "status": "resolved", "checkpoint_id": id, "result": result }),
            ))
        }
        PreflightOutcome::Done(ToolOutcome::Failed(error)) => {
            state
                .checkpoints
                .finalize(id, CheckpointStatus::Failed, None, Some(&error))
                .await
                .map_err(|e| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": e.to_string() })),
                    )
                })?;
            audit(state, cp, "resumed_failed", &json!({ "error": error })).await;
            Ok(Json(
                json!({ "status": "failed", "checkpoint_id": id, "error": error }),
            ))
        }
        PreflightOutcome::Rejected { reason } => {
            // Never actually reachable: resuming always starts past
            // `Decide` (see `PauseReason::resume_from`), which is the only
            // step that can reject. Handled defensively rather than
            // panicking.
            state
                .checkpoints
                .finalize(id, CheckpointStatus::Failed, None, Some(&reason))
                .await
                .map_err(|e| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": e.to_string() })),
                    )
                })?;
            Ok(Json(
                json!({ "status": "failed", "checkpoint_id": id, "error": reason }),
            ))
        }
        PreflightOutcome::Done(_) => unreachable!("Dispatch only ever produces Success or Failed"),
    }
}

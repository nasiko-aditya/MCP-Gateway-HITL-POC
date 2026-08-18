//! The agent's own task-intake API — `POST /agent/act` is where a task
//! (`connector`, `tool_name`, `arguments`) enters this POC's version of "the
//! agent decided it needs to call a tool." Everything from here on
//! (deciding whether HITL is needed, creating the checkpoint, eventually
//! dispatching) is `agent::preflight` + `agent::hitl`, never the gateway.
//!
//! `GET /agent/result/{call_id}` is the polling counterpart: since a
//! checkpoint may be resolved by a human acting through `POST
//! /hitl/{id}/respond` — a different request than the one that started the
//! task — the original caller needs a way to find out how it turned out.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::agent::checkpoint::ToolOutcome;
use crate::agent::preflight::{self, PreflightOutcome, TaskContext};
use crate::agent::state::AgentState;
use crate::gateway::routes::identity_from;

pub fn router(state: Arc<AgentState>) -> Router {
    Router::new()
        .route("/agent/act", post(act))
        .route("/agent/result/:call_id", get(get_result))
        .with_state(state)
}

#[derive(Deserialize)]
struct ActRequest {
    connector: String,
    tool_name: String,
    #[serde(default)]
    arguments: Value,
}

async fn act(
    State(state): State<Arc<AgentState>>,
    headers: HeaderMap,
    Json(req): Json<ActRequest>,
) -> Json<Value> {
    let (user_id, agent_id) = identity_from(&headers);
    let call_id = Uuid::new_v4().to_string();

    let arguments = match normalize_arguments(req.arguments) {
        Ok(v) => v,
        Err(message) => {
            return Json(json!({ "status": "error", "call_id": call_id, "error": message }))
        }
    };

    let ctx = TaskContext {
        call_id: call_id.clone(),
        user_id,
        agent_id,
        connector: req.connector,
        tool_name: req.tool_name,
    };

    let _ = state
        .audit
        .record(
            None,
            &ctx.call_id,
            &ctx.user_id,
            &ctx.agent_id,
            &ctx.connector,
            &ctx.tool_name,
            "call_received",
            &json!({ "arguments": &arguments }),
        )
        .await;

    let outcome = match preflight::act(&state, &ctx, &arguments).await {
        Ok(o) => o,
        Err(e) => {
            return Json(
                json!({ "status": "error", "call_id": ctx.call_id, "error": e.to_string() }),
            )
        }
    };

    Json(render_outcome(&state, &ctx, &arguments, outcome).await)
}

/// `arguments` must be a JSON object by the time anything downstream sees
/// it — every schema-diff, checkpoint merge (`agent::hitl::routes::respond`'s
/// `Input` branch), and MCP `tools/call` assumes it. An omitted `arguments`
/// field deserializes to `Value::Null` (`ActRequest`'s `#[serde(default)]`),
/// which is a legitimate "no arguments supplied" and is normalized to `{}`
/// here rather than left as `Null` — otherwise a checkpoint created from
/// such a call would fail to ever resume (`merged.as_object_mut()` has
/// nothing to merge into). Anything else that isn't an object (a string, a
/// number, an array) is a malformed request, not "every required field is
/// missing" — rejected here as a clean validation error instead of being
/// allowed to fall through and look like a genuine `InputRequired` pause.
fn normalize_arguments(arguments: Value) -> Result<Value, String> {
    match arguments {
        Value::Null => Ok(json!({})),
        Value::Object(_) => Ok(arguments),
        other => Err(format!(
            "'arguments' must be a JSON object, got {}",
            json_type_name(&other)
        )),
    }
}

fn json_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Turns a fresh [`PreflightOutcome`] into the task-intake API's response,
/// persisting a `Checkpoint` when the outcome is `Paused` — the only place
/// (alongside `agent::hitl::routes::finish_resume`'s re-pause branch) a
/// `Checkpoint` is ever created, and it happens entirely agent-side.
async fn render_outcome(
    state: &AgentState,
    ctx: &TaskContext,
    arguments: &Value,
    outcome: PreflightOutcome,
) -> Value {
    match outcome {
        PreflightOutcome::Rejected { reason } => {
            let _ = state
                .audit
                .record(
                    None,
                    &ctx.call_id,
                    &ctx.user_id,
                    &ctx.agent_id,
                    &ctx.connector,
                    &ctx.tool_name,
                    "blocked",
                    &json!({ "reason": &reason }),
                )
                .await;
            json!({ "status": "blocked", "call_id": ctx.call_id, "reason": reason })
        }
        PreflightOutcome::Paused { reason } => {
            let checkpoint = match state
                .checkpoints
                .create(
                    &ctx.call_id,
                    &ctx.user_id,
                    &ctx.agent_id,
                    &ctx.connector,
                    &ctx.tool_name,
                    arguments,
                    reason.clone(),
                )
                .await
            {
                Ok(c) => c,
                Err(e) => {
                    return json!({ "status": "error", "call_id": ctx.call_id, "error": e.to_string() })
                }
            };
            let _ = state
                .audit
                .record(
                    Some(checkpoint.id),
                    &ctx.call_id,
                    &ctx.user_id,
                    &ctx.agent_id,
                    &ctx.connector,
                    &ctx.tool_name,
                    "paused",
                    &json!({ "reason": &reason }),
                )
                .await;
            json!({
                "status": "pending",
                "call_id": ctx.call_id,
                "checkpoint_id": checkpoint.id,
                "reason": reason,
                "question": reason.question(),
            })
        }
        PreflightOutcome::Done(ToolOutcome::Success(result)) => {
            let _ = state
                .audit
                .record(
                    None,
                    &ctx.call_id,
                    &ctx.user_id,
                    &ctx.agent_id,
                    &ctx.connector,
                    &ctx.tool_name,
                    "success",
                    &json!({}),
                )
                .await;
            json!({ "status": "success", "call_id": ctx.call_id, "result": result })
        }
        PreflightOutcome::Done(ToolOutcome::Failed(error)) => {
            let _ = state
                .audit
                .record(
                    None,
                    &ctx.call_id,
                    &ctx.user_id,
                    &ctx.agent_id,
                    &ctx.connector,
                    &ctx.tool_name,
                    "failed",
                    &json!({ "error": &error }),
                )
                .await;
            json!({ "status": "failed", "call_id": ctx.call_id, "error": error })
        }
        PreflightOutcome::Done(_) => unreachable!("Dispatch only ever produces Success or Failed"),
    }
}

async fn get_result(
    State(state): State<Arc<AgentState>>,
    Path(call_id): Path<String>,
) -> Json<Value> {
    match state.checkpoints.find_by_call_id(&call_id).await {
        Ok(Some(cp)) => Json(json!({
            "call_id": call_id,
            "checkpoint_id": cp.id,
            "status": cp.status.as_str(),
            "result": cp.result,
            "error": cp.error,
        })),
        Ok(None) => Json(json!({ "error": format!("no call found for call_id '{call_id}'") })),
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}

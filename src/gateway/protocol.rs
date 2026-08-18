//! JSON-RPC dispatch for the agent-facing gateway — `initialize`,
//! `tools/list` (aggregate + namespace every connector, mock or Composio),
//! `tools/call` (route, enforce the two things the gateway is still allowed
//! to enforce unilaterally, then dispatch).
//!
//! This is deliberately thin, and deliberately NOT where HITL is decided
//! anymore. `handle_tools_call` runs exactly two checks before dispatch,
//! both hard, human-free, non-checkpointing decisions:
//!
//! 1. `Block` — always enforced here too, as defense in depth. Not a HITL
//!    decision (no human is ever asked), so refusing it here doesn't make
//!    the gateway a HITL orchestrator.
//! 2. `Ask` reached without `x-agent-preflight: passed` — refused with a
//!    distinct error telling the caller to route through the agent. This
//!    is a bypass guard, not the gateway independently deciding to pause:
//!    the gateway never creates a `Checkpoint`, never asks a human, and
//!    never runs a credential/schema check. All three of those checks, and
//!    the checkpoint itself, live entirely in `agent::preflight` /
//!    `agent::checkpoint` — see `agent/hitl/routes.rs`'s `respond` for the
//!    human-facing side of that flow.

use serde_json::{json, Value};
use uuid::Uuid;

use crate::state::GatewayState;
use crate::types::{codes, PROTOCOL_VERSION};

/// Everything `handle_request` needs about the caller and the call, beyond
/// the JSON-RPC body itself — identity (for audit + Composio's per-user
/// scoping), an optional caller-supplied `call_id` (so the agent's own
/// checkpoint/audit rows and the gateway's dispatch audit rows can be
/// correlated under one id), and whether this call already passed the
/// agent's own pre-flight (see module docs).
pub struct RequestContext<'a> {
    pub user_id: &'a str,
    pub agent_id: &'a str,
    pub call_id_hint: Option<&'a str>,
    pub preflight_approved: bool,
}

fn ok(req_id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": req_id, "result": result })
}

fn err(req_id: &Value, code: i64, message: impl Into<String>) -> Value {
    json!({ "jsonrpc": "2.0", "id": req_id, "error": { "code": code, "message": message.into() } })
}

/// Full JSON-RPC dispatch. Returns `None` for a notification (no `id`).
pub async fn handle_request(
    state: &GatewayState,
    rctx: &RequestContext<'_>,
    body: &Value,
) -> Option<Value> {
    let method = body.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let req_id = body.get("id").cloned()?;

    let result = match method {
        "initialize" => handle_initialize(&req_id),
        "ping" => json!({ "jsonrpc": "2.0", "id": req_id, "result": {} }),
        "tools/list" => handle_tools_list(state, &req_id, rctx.user_id).await,
        "tools/call" => {
            let params = body.get("params").cloned().unwrap_or_else(|| json!({}));
            handle_tools_call(state, rctx, &req_id, &params).await
        }
        other => err(
            &req_id,
            codes::METHOD_NOT_FOUND,
            format!("Method not found: {other}"),
        ),
    };
    Some(result)
}

fn handle_initialize(req_id: &Value) -> Value {
    ok(
        req_id,
        json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "MCP Gateway HITL POC", "version": "0.1.0" },
        }),
    )
}

/// Namespaces every downstream tool as `<connector>__<tool>`, across both
/// the mock connectors (`ConnectorRegistry`) and the live Composio
/// connector, if configured — a single flat tool list an agent can route
/// back to the right connector at call time, and the only place either
/// kind of connector's schema is exposed (the agent's own `SchemaValidator`
/// caches it from here; the gateway no longer keeps a schema cache itself).
async fn handle_tools_list(state: &GatewayState, req_id: &Value, user_id: &str) -> Value {
    let mut all_tools = Vec::new();

    for connector in state.registry.connectors() {
        let Some(base_url) = state.registry.url_for(&connector) else {
            continue;
        };
        let Ok(tools) = state.client.list_tools(base_url).await else {
            continue;
        };
        namespace_tools(&connector, &tools, &mut all_tools);
    }

    if let Some(slot) = &state.composio {
        if let Ok(tools) = slot.client.list_tools(user_id).await {
            namespace_tools(&slot.connector_name, &tools, &mut all_tools);
        }
    }

    ok(req_id, json!({ "tools": all_tools }))
}

fn namespace_tools(connector: &str, tools: &[Value], out: &mut Vec<Value>) {
    for tool in tools {
        let name = tool.get("name").and_then(|v| v.as_str()).unwrap_or("");
        out.push(json!({
            "name": format!("{connector}__{name}"),
            "description": tool.get("description").cloned().unwrap_or(Value::Null),
            "inputSchema": tool.get("inputSchema").cloned().unwrap_or(json!({"type": "object"})),
        }));
    }
}

/// `<connector>__<tool_name>` -> `(connector, tool_name)`.
fn route_tool(namespaced: &str) -> Option<(&str, &str)> {
    namespaced.split_once("__")
}

/// `pub(crate)` so `gateway::routes::get_connector_status` can reject an
/// unknown connector before ever consulting a credential store or Composio
/// for it — see that function's doc comment for why this matters: without
/// it, a typo'd or never-registered connector name looks exactly like an
/// ordinary "not connected yet" fact and pauses the agent on a misleading
/// `AuthRequired` instead of surfacing a clear "no such connector" error.
pub(crate) fn connector_known(state: &GatewayState, connector: &str) -> bool {
    state.registry.url_for(connector).is_some()
        || state
            .composio
            .as_ref()
            .is_some_and(|slot| slot.connector_name == connector)
}

async fn dispatch_tool(
    state: &GatewayState,
    user_id: &str,
    connector: &str,
    tool_name: &str,
    arguments: &Value,
) -> Result<Value, String> {
    if let Some(slot) = &state.composio {
        if slot.connector_name == connector {
            return slot.client.call_tool(user_id, tool_name, arguments).await;
        }
    }
    let Some(base_url) = state.registry.url_for(connector) else {
        return Err(format!(
            "no downstream MCP server registered for connector '{connector}'"
        ));
    };
    state.client.call_tool(base_url, tool_name, arguments).await
}

/// `tools/call` — route, run the two gateway-owned checks described in the
/// module docs, and dispatch. Every outcome is synchronous: unlike the old
/// pipeline, the gateway never pauses and never returns a `"status":
/// "pending"` result — pausing is exclusively the agent's job now.
async fn handle_tools_call(
    state: &GatewayState,
    rctx: &RequestContext<'_>,
    req_id: &Value,
    params: &Value,
) -> Value {
    let namespaced = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    let Some((connector, tool_name)) = route_tool(namespaced) else {
        return err(
            req_id,
            codes::INVALID_PARAMS,
            format!("Tool name '{namespaced}' is not namespaced as '<connector>__<tool>'."),
        );
    };
    if !connector_known(state, connector) {
        return err(
            req_id,
            codes::INVALID_PARAMS,
            format!("Unknown connector '{connector}'."),
        );
    }

    let call_id = rctx
        .call_id_hint
        .map(str::to_string)
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    let _ = state
        .audit
        .record(
            None,
            &call_id,
            rctx.user_id,
            rctx.agent_id,
            connector,
            tool_name,
            "call_received",
            &json!({ "arguments": arguments }),
        )
        .await;

    match state.policy.decide(connector, tool_name) {
        crate::policy::Stance::Block => {
            let reason =
                format!("Tool '{tool_name}' on connector '{connector}' is blocked by policy.");
            let _ = state
                .audit
                .record(
                    None,
                    &call_id,
                    rctx.user_id,
                    rctx.agent_id,
                    connector,
                    tool_name,
                    "blocked",
                    &json!({ "reason": &reason }),
                )
                .await;
            err(req_id, codes::TOOL_BLOCKED, reason)
        }
        crate::policy::Stance::Ask if !rctx.preflight_approved => {
            let reason = format!(
                "Tool '{tool_name}' on connector '{connector}' requires approval — route this \
                 call through the agent's HITL flow instead of calling the gateway directly."
            );
            let _ = state
                .audit
                .record(
                    None,
                    &call_id,
                    rctx.user_id,
                    rctx.agent_id,
                    connector,
                    tool_name,
                    "rejected_requires_agent",
                    &json!({ "reason": &reason }),
                )
                .await;
            err(req_id, codes::TOOL_ASK, reason)
        }
        crate::policy::Stance::Ask | crate::policy::Stance::Allow => {
            match dispatch_tool(state, rctx.user_id, connector, tool_name, &arguments).await {
                Ok(result) => {
                    let _ = state
                        .audit
                        .record(
                            None,
                            &call_id,
                            rctx.user_id,
                            rctx.agent_id,
                            connector,
                            tool_name,
                            "success",
                            &json!({}),
                        )
                        .await;
                    ok(req_id, result)
                }
                Err(error) => {
                    let _ = state
                        .audit
                        .record(
                            None,
                            &call_id,
                            rctx.user_id,
                            rctx.agent_id,
                            connector,
                            tool_name,
                            "failed",
                            &json!({ "error": &error }),
                        )
                        .await;
                    err(req_id, codes::INTERNAL_ERROR, error)
                }
            }
        }
    }
}

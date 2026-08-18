//! Two tiny standalone MCP servers used as this POC's "downstream
//! connectors" — real axum services reached over real HTTP, exactly the
//! shape a production connector would be. Neither knows anything about
//! HITL, checkpoints, or policy; they just answer `tools/list`/`tools/call`
//! like any MCP server. The gateway treats them identically (see
//! `provider.rs`), which is what proves the HITL mechanism is generic
//! across connectors rather than hardcoded to either one.

pub mod github;
pub mod notion;

use std::sync::Arc;

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};

#[derive(Clone)]
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
}

#[derive(Clone)]
struct MockState {
    name: &'static str,
    tools: Arc<Vec<ToolSpec>>,
}

pub fn build_mock_server(name: &'static str, tools: Vec<ToolSpec>) -> Router {
    let state = MockState {
        name,
        tools: Arc::new(tools),
    };
    Router::new().route("/mcp", post(handle)).with_state(state)
}

async fn handle(State(state): State<MockState>, Json(body): Json<Value>) -> Json<Value> {
    let method = body.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let id = body.get("id").cloned().unwrap_or(Value::Null);

    let response = match method {
        "tools/list" => {
            let tools: Vec<Value> = state
                .tools
                .iter()
                .map(|t| {
                    json!({
                        "name": t.name,
                        "description": t.description,
                        "inputSchema": t.input_schema,
                    })
                })
                .collect();
            json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": tools } })
        }
        "tools/call" => {
            let params = body.get("params").cloned().unwrap_or_else(|| json!({}));
            let tool_name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            if state.tools.iter().any(|t| t.name == tool_name) {
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "connector": state.name,
                        "tool": tool_name,
                        "arguments": arguments,
                        "status": "ok",
                    }
                })
            } else {
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32601, "message": format!("unknown tool '{tool_name}'") }
                })
            }
        }
        other => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32601, "message": format!("method not found: {other}") }
        }),
    };
    Json(response)
}

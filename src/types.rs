//! Core JSON-RPC 2.0 envelope, mirroring the shape Nasiko's real MCP gateway
//! speaks (`oss/mcp-gateway/src/types.rs`) so this POC's wire format is
//! recognizable as "the same protocol," not a bespoke one.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: &str = "2024-11-05";

#[derive(Debug, Clone, Deserialize)]
pub struct JsonRpcRequest {
    #[serde(default = "default_jsonrpc")]
    pub jsonrpc: String,
    #[serde(default)]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Option<Value>,
}

fn default_jsonrpc() -> String {
    "2.0".to_string()
}

#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// JSON-RPC error codes. `-32000`/`-32001` mirror Nasiko's own
/// `TOOL_BLOCKED`/`TOOL_ASK` constants (`oss/mcp-gateway/src/types.rs::codes`).
/// `TOOL_ASK` is returned only as the gateway's defense-in-depth backstop —
/// a `tools/call` for an `ask`-gated tool that reaches the gateway directly
/// without having gone through the agent's own approval flow (see
/// `gateway::protocol::handle_tools_call`). It is never returned for a call
/// that *did* go through the agent, since the agent is the only place a
/// HITL pause is decided and checkpointed.
pub mod codes {
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;
    pub const TOOL_BLOCKED: i64 = -32000;
    pub const TOOL_ASK: i64 = -32001;
}

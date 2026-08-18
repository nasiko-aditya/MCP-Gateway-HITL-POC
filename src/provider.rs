//! Thin outbound JSON-RPC HTTP client to a downstream MCP server — the POC
//! equivalent of Nasiko's `GenericMcpProvider::call_tool`
//! (`oss/mcp-gateway/src/provider/generic.rs`). One implementation, used
//! identically for every connector; nothing here knows a connector's name.

use std::collections::HashMap;

use serde_json::{json, Value};

/// Maps a connector name to the base URL of its (real or mock) downstream
/// MCP server. Populated once at startup from the ports the mock servers
/// were spawned on.
#[derive(Clone, Default)]
pub struct ConnectorRegistry {
    urls: HashMap<String, String>,
}

impl ConnectorRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, connector: &str, base_url: &str) {
        self.urls
            .insert(connector.to_string(), base_url.to_string());
    }

    pub fn url_for(&self, connector: &str) -> Option<&str> {
        self.urls.get(connector).map(|s| s.as_str())
    }

    pub fn connectors(&self) -> Vec<String> {
        self.urls.keys().cloned().collect()
    }
}

pub struct McpClient {
    http: reqwest::Client,
}

impl Default for McpClient {
    fn default() -> Self {
        Self::new()
    }
}

impl McpClient {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }

    /// `tools/list` against a downstream MCP server. Returns the raw `tools`
    /// array (each entry carries `name`, `description`, `inputSchema`,
    /// unmodified).
    pub async fn list_tools(&self, base_url: &str) -> anyhow::Result<Vec<Value>> {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" });
        let resp: Value = self
            .http
            .post(base_url)
            .json(&body)
            .send()
            .await?
            .json()
            .await?;
        Ok(resp
            .get("result")
            .and_then(|r| r.get("tools"))
            .and_then(|t| t.as_array())
            .cloned()
            .unwrap_or_default())
    }

    /// `tools/call` against a downstream MCP server. Returns `Ok(result)` on
    /// a JSON-RPC success, `Err(message)` on a JSON-RPC error or transport
    /// failure — the caller (`pipeline::dispatch`) turns this straight into
    /// `ToolOutcome::Success`/`ToolOutcome::Failed`.
    pub async fn call_tool(
        &self,
        base_url: &str,
        tool_name: &str,
        arguments: &Value,
    ) -> Result<Value, String> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": tool_name, "arguments": arguments }
        });
        let resp = self
            .http
            .post(base_url)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("connection to downstream MCP server failed: {e}"))?;
        let resp: Value = resp
            .json()
            .await
            .map_err(|e| format!("malformed response from downstream MCP server: {e}"))?;

        if let Some(err) = resp.get("error") {
            let message = err
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("downstream tool call failed");
            return Err(message.to_string());
        }
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }
}

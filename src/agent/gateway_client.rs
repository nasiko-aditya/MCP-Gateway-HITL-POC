//! Thin HTTP client wrapping the gateway's agent-facing surface — the only
//! way anything in `agent::*` talks to the gateway. Never imports a gateway
//! Rust type directly (not `PermissionPolicy`, not `CredentialStore`): the
//! agent only ever sees non-secret facts serialized over HTTP, exactly the
//! boundary `HITL_UPDATE_PLAN.md` §4 calls for. Every method here maps
//! 1:1 onto one gateway endpoint.

use serde_json::{json, Value};

use crate::policy::Stance;

pub struct GatewayClient {
    http: reqwest::Client,
    base_url: String,
}

/// What `GET /connectors/{connector}/status` reports — never a secret, just
/// enough for the agent to decide whether to pause for `AuthRequired`.
pub struct ConnectorStatus {
    pub connected: bool,
    pub auth_url: Option<String>,
}

impl GatewayClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.into(),
        }
    }

    fn identity_headers(
        &self,
        req: reqwest::RequestBuilder,
        user_id: &str,
        agent_id: &str,
    ) -> reqwest::RequestBuilder {
        req.header("x-user-id", user_id)
            .header("x-agent-id", agent_id)
    }

    /// `tools/list` against the gateway, namespaced `<connector>__<tool>`
    /// across every connector the gateway knows about (mocks + Composio).
    pub async fn list_tools(&self, user_id: &str, agent_id: &str) -> anyhow::Result<Vec<Value>> {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" });
        let req = self.identity_headers(
            self.http.post(format!("{}/mcp", self.base_url)),
            user_id,
            agent_id,
        );
        let resp = req
            .json(&body)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("gateway unreachable while listing tools: {e}"))?;
        let resp: Value = resp
            .json()
            .await
            .map_err(|e| anyhow::anyhow!("malformed tools/list response from gateway: {e}"))?;
        Ok(resp
            .get("result")
            .and_then(|r| r.get("tools"))
            .and_then(|t| t.as_array())
            .cloned()
            .unwrap_or_default())
    }

    /// `GET /policy/{connector}/{tool}` — a fact lookup, not a HITL decision.
    pub async fn policy(&self, connector: &str, tool_name: &str) -> anyhow::Result<Stance> {
        let resp = self
            .http
            .get(format!("{}/policy/{connector}/{tool_name}", self.base_url))
            .send()
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "gateway unreachable while checking policy for '{connector}/{tool_name}': {e}"
                )
            })?;
        let resp: Value = resp.json().await.map_err(|e| {
            anyhow::anyhow!(
                "malformed policy response from gateway for '{connector}/{tool_name}': {e}"
            )
        })?;
        let stance = resp
            .get("stance")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("gateway policy response missing 'stance': {resp}"))?;
        Ok(serde_json::from_value(stance)?)
    }

    /// `GET /connectors/{connector}/status`. A non-2xx response means the
    /// status check itself failed (Composio API error, DB error, etc.) —
    /// distinct from a 2xx body reporting `connected: false`, which means
    /// the check succeeded and genuinely found no live connection. Callers
    /// (`preflight::execute`'s `CredentialCheck`) must see the former as an
    /// `Err`, not as "needs auth," or a real outage would look identical to
    /// a routine `AuthRequired` pause.
    pub async fn connector_status(
        &self,
        user_id: &str,
        connector: &str,
    ) -> anyhow::Result<ConnectorStatus> {
        let resp = self
            .http
            .get(format!("{}/connectors/{connector}/status", self.base_url))
            .header("x-user-id", user_id)
            .send()
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "gateway unreachable while checking status of connector '{connector}': {e}"
                )
            })?;
        let status = resp.status();
        let body: Value = resp.json().await.map_err(|e| {
            anyhow::anyhow!(
                "malformed connector-status response from gateway for '{connector}': {e}"
            )
        })?;
        if !status.is_success() {
            let message = body
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("connector status check failed");
            anyhow::bail!("gateway connector status check for '{connector}' failed: {message}");
        }
        Ok(ConnectorStatus {
            connected: body
                .get("connected")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            auth_url: body
                .get("auth_url")
                .and_then(|v| v.as_str())
                .map(str::to_string),
        })
    }

    /// `POST /connectors/{connector}/credentials` — only meaningful for a
    /// local/mock connector; the gateway itself refuses this for a Composio
    /// connector (see `gateway::routes::post_connector_credentials`).
    pub async fn store_credential(&self, connector: &str, token: &str) -> anyhow::Result<()> {
        let resp = self
            .http
            .post(format!(
                "{}/connectors/{connector}/credentials",
                self.base_url
            ))
            .json(&json!({ "token": token }))
            .send()
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "gateway unreachable while storing credential for '{connector}': {e}"
                )
            })?;
        if !resp.status().is_success() {
            let body: Value = resp.json().await.unwrap_or(Value::Null);
            anyhow::bail!("gateway rejected credential store for '{connector}': {body}");
        }
        Ok(())
    }

    /// `tools/call` against the gateway — the one call that actually
    /// dispatches to a downstream connector. `x-agent-preflight: passed`
    /// tells the gateway this call already went through the agent's own
    /// Decide/CredentialCheck/SchemaCheck sequence (see `protocol.rs`
    /// module docs for exactly what that header does and doesn't
    /// authorize). `call_id` correlates this dispatch's gateway-side audit
    /// rows with the agent's own checkpoint/audit rows for the same task.
    pub async fn call_tool(
        &self,
        user_id: &str,
        agent_id: &str,
        call_id: &str,
        connector: &str,
        tool_name: &str,
        arguments: &Value,
    ) -> Result<Value, String> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": format!("{connector}__{tool_name}"), "arguments": arguments }
        });
        let req = self
            .identity_headers(
                self.http.post(format!("{}/mcp", self.base_url)),
                user_id,
                agent_id,
            )
            .header("x-call-id", call_id)
            .header("x-agent-preflight", "passed");
        let resp = req
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("connection to gateway failed: {e}"))?;
        let resp: Value = resp
            .json()
            .await
            .map_err(|e| format!("malformed response from gateway: {e}"))?;

        if let Some(err) = resp.get("error") {
            let message = err
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("gateway tool call failed");
            return Err(message.to_string());
        }
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }
}

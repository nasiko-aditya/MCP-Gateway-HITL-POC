//! Real Composio-backed connector — a port of `HITL-POC/src/tools/composio/
//! {rest.rs,mcp_client.rs}` (read-only reference, not modified) retargeted
//! at this POC's generic `ConnectorRegistry`/`McpClient` shape instead of
//! A2A's `ToolExecutor`. Nothing here is specific to any one Composio
//! toolkit or tool name — `ComposioConfig` (see `config.rs`) supplies which
//! auth config, MCP server, and tool slugs to use; this file only speaks
//! Composio's REST API and the MCP-over-HTTP protocol generically.
//!
//! Registered into `GatewayState` only when `COMPOSIO_API_KEY` and
//! `COMPOSIO_AUTH_CONFIG_ID` are set (see `config::ComposioConfig::from_env`)
//! — absent otherwise, so `cargo test` and a plain `cargo run` never need
//! live Composio credentials.

use serde_json::{json, Value};

use crate::config::ComposioConfig;

/// Thin wrapper around the Composio v3 REST API — just the calls this POC
/// needs (connected-account status/link, MCP server get-or-create/generate).
pub struct ComposioRestClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
}

pub struct McpServer {
    pub id: String,
    pub mcp_url: String,
}

impl ComposioRestClient {
    pub fn new(base_url: String, api_key: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url,
            api_key,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    async fn get(&self, path: &str, query: &[(&str, &str)]) -> anyhow::Result<Value> {
        let resp = self
            .http
            .get(self.url(path))
            .header("x-api-key", &self.api_key)
            .query(query)
            .send()
            .await?;
        Self::body(resp).await
    }

    async fn post(&self, path: &str, body: &Value) -> anyhow::Result<Value> {
        let resp = self
            .http
            .post(self.url(path))
            .header("x-api-key", &self.api_key)
            .header("content-type", "application/json")
            .json(body)
            .send()
            .await?;
        Self::body(resp).await
    }

    async fn body(resp: reqwest::Response) -> anyhow::Result<Value> {
        let status = resp.status();
        let text = resp.text().await?;
        let json: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if !status.is_success() {
            anyhow::bail!(
                "composio {status}: {}",
                if json.is_null() {
                    text
                } else {
                    json.to_string()
                }
            );
        }
        Ok(json)
    }

    /// True if `user_id` has an ACTIVE connected account for this auth
    /// config. `GET /api/v3/connected_accounts` returns
    /// `{"items": [{"status": "ACTIVE", ...}], ...}`.
    pub async fn is_connected(&self, user_id: &str, auth_config_id: &str) -> anyhow::Result<bool> {
        let body = self
            .get(
                "/api/v3/connected_accounts",
                &[("user_ids", user_id), ("auth_config_ids", auth_config_id)],
            )
            .await?;
        let items = body
            .get("items")
            .and_then(|v| v.as_array())
            .ok_or_else(|| {
                anyhow::anyhow!("connected_accounts response missing 'items': {body}")
            })?;
        Ok(items
            .iter()
            .any(|item| item.get("status").and_then(|s| s.as_str()) == Some("ACTIVE")))
    }

    /// Ids of every currently-ACTIVE connected account for `user_id` under
    /// this auth config — used only by the disconnect/logout path. A user
    /// can accumulate more than one (e.g. a stale link retried before
    /// completing OAuth), so this returns all of them rather than assuming
    /// exactly one.
    pub async fn active_connected_account_ids(
        &self,
        user_id: &str,
        auth_config_id: &str,
    ) -> anyhow::Result<Vec<String>> {
        let body = self
            .get(
                "/api/v3/connected_accounts",
                &[("user_ids", user_id), ("auth_config_ids", auth_config_id)],
            )
            .await?;
        let items = body
            .get("items")
            .and_then(|v| v.as_array())
            .ok_or_else(|| {
                anyhow::anyhow!("connected_accounts response missing 'items': {body}")
            })?;
        Ok(items
            .iter()
            .filter(|item| item.get("status").and_then(|s| s.as_str()) == Some("ACTIVE"))
            .filter_map(|item| item.get("id").and_then(|v| v.as_str()).map(str::to_string))
            .collect())
    }

    /// Deletes a connected account and, via `revoke_on_delete=true`, best-
    /// effort revokes the upstream OAuth grant too (a background job on
    /// Composio's side) — this POC's "log out for real" for the Composio
    /// path. `DELETE /api/v3/connected_accounts/{id}?revoke_on_delete=true`.
    pub async fn delete_connected_account(&self, connected_account_id: &str) -> anyhow::Result<()> {
        let resp = self
            .http
            .delete(self.url(&format!(
                "/api/v3/connected_accounts/{connected_account_id}"
            )))
            .header("x-api-key", &self.api_key)
            .query(&[("revoke_on_delete", "true")])
            .send()
            .await?;
        Self::body(resp).await?;
        Ok(())
    }

    /// Starts (or restarts) an OAuth link session for `user_id` and returns
    /// the URL the human needs to open to authenticate. `POST
    /// /api/v3/connected_accounts/link` returns `{"redirect_url", ...}`.
    pub async fn initiate_link(
        &self,
        user_id: &str,
        auth_config_id: &str,
    ) -> anyhow::Result<String> {
        let body = self
            .post(
                "/api/v3/connected_accounts/link",
                &json!({ "auth_config_id": auth_config_id, "user_id": user_id }),
            )
            .await?;
        body.get("redirect_url")
            .or_else(|| body.get("redirectUrl"))
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| {
                anyhow::anyhow!("connected_accounts/link response missing 'redirect_url': {body}")
            })
    }

    /// Returns the id/url of an MCP server already scoped to
    /// `auth_config_id`/`allowed_tools`, creating one if none exists yet.
    /// Called once at gateway startup, not per task.
    pub async fn get_or_create_mcp_server(
        &self,
        name: &str,
        auth_config_id: &str,
        allowed_tools: &[String],
    ) -> anyhow::Result<McpServer> {
        let list = self.get("/api/v3/mcp/servers", &[]).await?;
        if let Some(items) = list.get("items").and_then(|v| v.as_array()) {
            if let Some(existing) = items
                .iter()
                .find(|s| s.get("name").and_then(|n| n.as_str()) == Some(name))
            {
                return Self::parse_server(existing);
            }
        }

        let created = self
            .post(
                "/api/v3/mcp/servers",
                &json!({
                    "name": name,
                    "auth_config_ids": [auth_config_id],
                    "allowed_tools": allowed_tools,
                }),
            )
            .await?;
        Self::parse_server(&created)
    }

    fn parse_server(v: &Value) -> anyhow::Result<McpServer> {
        let id = v
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("mcp server response missing 'id': {v}"))?
            .to_string();
        let mcp_url = v
            .get("mcp_url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("mcp server response missing 'mcp_url': {v}"))?
            .to_string();
        Ok(McpServer { id, mcp_url })
    }

    /// The per-user MCP endpoint URL for an existing server. `POST
    /// /api/v3/mcp/servers/generate` returns `{"user_ids_url": [...]}` (or
    /// falls back to `{"mcp_url"}` + a `?user_id=` query param).
    pub async fn generate_user_mcp_url(
        &self,
        server_id: &str,
        user_id: &str,
    ) -> anyhow::Result<String> {
        let body = self
            .post(
                "/api/v3/mcp/servers/generate",
                &json!({ "mcp_server_id": server_id, "user_ids": [user_id] }),
            )
            .await?;
        if let Some(url) = body
            .get("user_ids_url")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .and_then(|v| v.as_str())
        {
            return Ok(url.to_string());
        }
        body.get("mcp_url")
            .and_then(|v| v.as_str())
            .map(|base| format!("{base}?user_id={user_id}"))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "mcp servers/generate response missing 'user_ids_url'/'mcp_url': {body}"
                )
            })
    }
}

/// Sends one JSON-RPC request to a Composio-hosted MCP server and returns
/// its `result` field, unmodified — kept exactly as generic as
/// `provider::McpClient`'s handling of the mock connectors so the gateway's
/// dispatch code never has to know which kind of connector it's talking to.
/// Composio's MCP server needs no `initialize` handshake and answers either
/// a plain JSON body or one SSE `data: <json>` line depending on content
/// negotiation, so both are parsed here.
async fn mcp_request(
    http: &reqwest::Client,
    mcp_url: &str,
    api_key: &str,
    method: &str,
    params: Value,
) -> Result<Value, String> {
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
    let resp = http
        .post(mcp_url)
        .header("x-api-key", api_key)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("connection to Composio MCP server failed: {e}"))?;

    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("malformed response from Composio MCP server: {e}"))?;
    if !status.is_success() {
        return Err(format!("Composio MCP server returned {status}: {text}"));
    }

    let envelope = parse_jsonrpc_envelope(&text)?;
    if let Some(err) = envelope.get("error") {
        let message = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("Composio tool call failed");
        return Err(message.to_string());
    }
    Ok(envelope.get("result").cloned().unwrap_or(Value::Null))
}

/// Parses either a bare JSON-RPC response body or an SSE stream containing
/// exactly one `data: <json>` message.
fn parse_jsonrpc_envelope(body: &str) -> Result<Value, String> {
    if let Ok(v) = serde_json::from_str::<Value>(body) {
        return Ok(v);
    }
    for line in body.lines() {
        if let Some(data) = line.strip_prefix("data:") {
            if let Ok(v) = serde_json::from_str::<Value>(data.trim()) {
                return Ok(v);
            }
        }
    }
    Err(format!("could not parse MCP response body: {body}"))
}

/// One live Composio-backed connector, bootstrapped once at gateway
/// startup. `list_tools`/`call_tool` are scoped per `user_id` (Composio's
/// own connected-account concept), matching this POC's existing convention
/// of deriving identity from the `x-user-id` header.
pub struct ComposioConnector {
    http: reqwest::Client,
    rest: ComposioRestClient,
    api_key: String,
    auth_config_id: String,
    server_id: String,
}

impl ComposioConnector {
    pub async fn bootstrap(cfg: &ComposioConfig) -> anyhow::Result<Self> {
        let rest = ComposioRestClient::new(cfg.base_url.clone(), cfg.api_key.clone());
        let server = rest
            .get_or_create_mcp_server(&cfg.server_name, &cfg.auth_config_id, &cfg.allowed_tools)
            .await?;
        tracing::info!(
            connector = %cfg.connector_name,
            mcp_server_id = %server.id,
            "composio connector ready"
        );
        Ok(Self {
            http: reqwest::Client::new(),
            rest,
            api_key: cfg.api_key.clone(),
            auth_config_id: cfg.auth_config_id.clone(),
            server_id: server.id,
        })
    }

    pub async fn is_connected(&self, user_id: &str) -> anyhow::Result<bool> {
        self.rest.is_connected(user_id, &self.auth_config_id).await
    }

    pub async fn auth_url(&self, user_id: &str) -> anyhow::Result<String> {
        self.rest.initiate_link(user_id, &self.auth_config_id).await
    }

    /// Disconnects every active connected account `user_id` has for this
    /// connector and revokes the upstream OAuth grant — the real "log out"
    /// for the Composio path. Returns how many accounts were removed (0 is
    /// a valid, non-error result: "already logged out").
    pub async fn disconnect(&self, user_id: &str) -> anyhow::Result<usize> {
        let ids = self
            .rest
            .active_connected_account_ids(user_id, &self.auth_config_id)
            .await?;
        for id in &ids {
            self.rest.delete_connected_account(id).await?;
        }
        Ok(ids.len())
    }

    async fn user_mcp_url(&self, user_id: &str) -> anyhow::Result<String> {
        self.rest
            .generate_user_mcp_url(&self.server_id, user_id)
            .await
    }

    /// `tools/list` scoped to `user_id`'s MCP endpoint. Returns the raw
    /// `tools` array, same shape `provider::McpClient::list_tools` returns
    /// for a mock connector.
    pub async fn list_tools(&self, user_id: &str) -> anyhow::Result<Vec<Value>> {
        let mcp_url = self.user_mcp_url(user_id).await?;
        let result = mcp_request(&self.http, &mcp_url, &self.api_key, "tools/list", json!({}))
            .await
            .map_err(|e| anyhow::anyhow!(e))?;
        Ok(result
            .get("tools")
            .and_then(|t| t.as_array())
            .cloned()
            .unwrap_or_default())
    }

    pub async fn call_tool(
        &self,
        user_id: &str,
        tool_name: &str,
        arguments: &Value,
    ) -> Result<Value, String> {
        let mcp_url = self
            .user_mcp_url(user_id)
            .await
            .map_err(|e| format!("could not resolve Composio MCP endpoint: {e}"))?;
        mcp_request(
            &self.http,
            &mcp_url,
            &self.api_key,
            "tools/call",
            json!({ "name": tool_name, "arguments": arguments }),
        )
        .await
    }
}

/// A `ComposioConnector` plus the connector name it's registered under —
/// what `GatewayState` actually holds, so routing code compares against
/// `slot.connector_name` rather than any hardcoded string.
pub struct ComposioSlot {
    pub connector_name: String,
    pub client: ComposioConnector,
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::State;
    use axum::routing::get;
    use axum::Router;
    use std::sync::Arc;

    /// Spins up a tiny in-process HTTP server standing in for Composio's own
    /// REST API, answering `GET /api/v3/connected_accounts` with whatever
    /// fixed `(status, body)` the test wants — enough to exercise
    /// `ComposioRestClient::is_connected` against a real HTTP round trip
    /// without any live Composio credentials. This is what lets the "a
    /// Composio outage/API failure must not look like an ordinary
    /// not-connected fact" property be verified deterministically in
    /// `cargo test` rather than only by hand against the real API.
    async fn spawn_fake_composio(status: u16, body: Value) -> String {
        #[derive(Clone)]
        struct Fixture {
            status: axum::http::StatusCode,
            body: Value,
        }
        async fn handler(State(fx): State<Arc<Fixture>>) -> (axum::http::StatusCode, Json<Value>) {
            (fx.status, Json(fx.body.clone()))
        }
        use axum::Json;
        let fixture = Arc::new(Fixture {
            status: axum::http::StatusCode::from_u16(status).unwrap(),
            body,
        });
        let app = Router::new()
            .route("/api/v3/connected_accounts", get(handler))
            .with_state(fixture);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    /// The genuine "checked, and there's no active connected account" case
    /// — a normal 2xx with an empty `items` array. Must come back as
    /// `Ok(false)`, the fact that becomes an ordinary `AuthRequired` pause.
    #[tokio::test]
    async fn is_connected_false_on_genuine_empty_result() {
        let base_url = spawn_fake_composio(200, json!({ "items": [] })).await;
        let client = ComposioRestClient::new(base_url, "test-api-key".to_string());
        let connected = client.is_connected("user-1", "auth-cfg-1").await.unwrap();
        assert!(
            !connected,
            "an empty items array means genuinely not connected"
        );
    }

    /// A real Composio API failure (bad key, service error, whatever a 5xx
    /// or 4xx represents) must come back as `Err`, never coerced into the
    /// same `Ok(false)` an ordinary not-connected fact would produce —
    /// that's the entire distinction this POC's `AuthRequired` handling
    /// depends on (see `gateway::routes::get_connector_status`'s doc
    /// comment). The error message must also stay clean: it should
    /// describe the failure without echoing the caller's API key.
    #[tokio::test]
    async fn is_connected_surfaces_api_failure_distinctly_from_not_connected() {
        let base_url = spawn_fake_composio(
            403,
            json!({ "error": "APIKey_InsufficientPermissions", "suggested_fix": "grant access" }),
        )
        .await;
        let secret_key = "sk-super-secret-do-not-leak";
        let client = ComposioRestClient::new(base_url, secret_key.to_string());
        let result = client.is_connected("user-1", "auth-cfg-1").await;
        let err = result.expect_err("a 403 API failure must not report Ok(false)");
        let message = err.to_string();
        assert!(
            message.contains("403"),
            "error should identify the failure came from Composio's API: {message}"
        );
        assert!(
            !message.contains(secret_key),
            "error message must never echo the API key: {message}"
        );
    }
}

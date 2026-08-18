//! End-to-end tests against the real gateway + agent axum apps and two real
//! in-process mock MCP servers, reached over real HTTP — exactly the same
//! topology `main.rs` boots (gateway, agent, two mocks, one shared SQLite
//! pool), just on ephemeral ports with an in-memory DB so tests can run
//! isolated and in parallel.
//!
//! Every scenario below drives the *agent's* endpoints (`/agent/act`,
//! `/hitl/...`) — never the gateway's `/mcp` directly — except the
//! `gateway_*_backstop` tests, which deliberately call the gateway directly
//! to prove it never independently creates a HITL checkpoint of its own.

use std::sync::Arc;

use mcp_gateway_hitl_poc::agent::gateway_client::GatewayClient;
use mcp_gateway_hitl_poc::agent::state::AgentState;
use mcp_gateway_hitl_poc::provider::ConnectorRegistry;
use mcp_gateway_hitl_poc::state::GatewayState;
use mcp_gateway_hitl_poc::{agent, db, gateway, mock_connectors, policy};
use serde_json::{json, Value};

/// Matches the shape of the real `policy.toml`: `github/wipe_org` blocked,
/// `github/delete_repo` and `notion/create_page` ask-gated, everything else
/// (including `github/list_repos`, `github/get_latest_pr`,
/// `github/phantom_tool`, `notion/search_pages`) defaults to allow.
const TEST_POLICY: &str = r#"
[[rules]]
connector = "github"
pattern = "wipe_org"
stance = "block"

[[rules]]
connector = "github"
pattern = "delete_repo"
stance = "ask"

[[rules]]
connector = "github"
pattern = "phantom_tool"
stance = "ask"

[[rules]]
connector = "notion"
pattern = "create_page"
stance = "ask"
"#;

struct TestApp {
    client: reqwest::Client,
    agent_base: String,
    gateway_base: String,
    gateway_state: Arc<GatewayState>,
}

async fn spawn_on_ephemeral(router: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://{addr}")
}

async fn spawn_app() -> TestApp {
    spawn_app_with_extra_connectors(&[]).await
}

/// Same topology as `spawn_app`, plus whatever extra `(name, base_url)`
/// connector registrations a test needs — e.g. one pointing at a port
/// nothing listens on, to exercise a downstream-connector-unavailable
/// scenario deterministically instead of against real network flakiness.
/// Kept separate from `spawn_app` so an always-unreachable connector isn't
/// registered (and silently probed on every `tools/list`) for every other
/// test in this file.
async fn spawn_app_with_extra_connectors(extra: &[(&str, &str)]) -> TestApp {
    let github_url = spawn_on_ephemeral(mock_connectors::github::router()).await;
    let notion_url = spawn_on_ephemeral(mock_connectors::notion::router()).await;

    let mut registry = ConnectorRegistry::new();
    registry.register("github", &format!("{github_url}/mcp"));
    registry.register("notion", &format!("{notion_url}/mcp"));
    for (name, base_url) in extra {
        registry.register(name, base_url);
    }

    let pool = db::connect("sqlite::memory:").await.unwrap();
    let permission_policy = policy::PermissionPolicy::from_toml_str(TEST_POLICY).unwrap();
    // `composio: None` — automated tests never require live Composio
    // credentials; the registry-backed mock path is exercised exclusively.
    let gateway_state = Arc::new(GatewayState::new(
        pool.clone(),
        permission_policy,
        registry,
        None,
    ));
    let gateway_base = spawn_on_ephemeral(gateway::routes::router(gateway_state.clone())).await;

    let gateway_client = GatewayClient::new(gateway_base.clone());
    let agent_state = Arc::new(AgentState::new(pool, gateway_client));
    let agent_base = spawn_on_ephemeral(agent::router(agent_state)).await;

    TestApp {
        client: reqwest::Client::new(),
        agent_base,
        gateway_base,
        gateway_state,
    }
}

impl TestApp {
    async fn seed_credential(&self, connector: &str) {
        self.gateway_state
            .credentials
            .store(connector, "demo-token", None)
            .await
            .unwrap();
    }

    /// Drives a task through the *agent's* task-intake endpoint — the only
    /// entrypoint a real agent uses. Never calls the gateway directly.
    async fn act(&self, connector: &str, tool_name: &str, arguments: Value) -> Value {
        self.client
            .post(format!("{}/agent/act", self.agent_base))
            .json(
                &json!({ "connector": connector, "tool_name": tool_name, "arguments": arguments }),
            )
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    /// Calls the gateway's `POST /mcp` directly, bypassing the agent
    /// entirely — used only by the `gateway_*_backstop` tests below, which
    /// exist specifically to prove HITL is never independently triggered
    /// by the gateway itself.
    async fn call_gateway_directly(&self, namespaced_tool: &str, arguments: Value) -> Value {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": namespaced_tool, "arguments": arguments }
        });
        self.client
            .post(format!("{}/mcp", self.gateway_base))
            .json(&body)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    /// Same as `call_gateway_directly`, but with `x-agent-preflight: passed`
    /// set — simulating a call the agent claims already went through its
    /// own pre-flight, the way a real resumed dispatch would. Used only to
    /// prove the gateway's `Block` backstop is unconditional defense-in-depth
    /// (§ "policy changes between pause and resume"): it must still win even
    /// when the caller claims pre-flight already approved the call.
    async fn call_gateway_directly_as_preflighted(
        &self,
        namespaced_tool: &str,
        arguments: Value,
    ) -> Value {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": namespaced_tool, "arguments": arguments }
        });
        self.client
            .post(format!("{}/mcp", self.gateway_base))
            .header("x-agent-preflight", "passed")
            .json(&body)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    /// Posts a raw, possibly-malformed body straight to `/agent/act` —
    /// bypasses `serde_json::json!`/`reqwest::json` entirely so a genuinely
    /// invalid JSON payload can be sent, proving the server responds with a
    /// clean client error instead of crashing or hanging.
    async fn act_raw_body(&self, raw_body: &str) -> (u16, String) {
        let resp = self
            .client
            .post(format!("{}/agent/act", self.agent_base))
            .header("content-type", "application/json")
            .body(raw_body.to_string())
            .send()
            .await
            .unwrap();
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap();
        (status, text)
    }

    async fn get_checkpoint(&self, id: &str) -> Value {
        self.client
            .get(format!("{}/hitl/{id}", self.agent_base))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    async fn pending_count(&self) -> usize {
        let resp: Value = self
            .client
            .get(format!("{}/hitl/pending", self.agent_base))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        resp["checkpoints"].as_array().unwrap().len()
    }

    async fn respond(&self, id: &str, action: Value) -> (u16, Value) {
        let resp = self
            .client
            .post(format!("{}/hitl/{id}/respond", self.agent_base))
            .json(&action)
            .send()
            .await
            .unwrap();
        let status = resp.status().as_u16();
        let body = resp.json().await.unwrap();
        (status, body)
    }

    /// Calls the gateway's `GET /connectors/{connector}/status` directly —
    /// used only by the tests proving a real backing-store failure surfaces
    /// as a hard error rather than a false-positive `connected: false`.
    async fn connector_status_direct(&self, connector: &str) -> (u16, Value) {
        let resp = self
            .client
            .get(format!(
                "{}/connectors/{connector}/status",
                self.gateway_base
            ))
            .send()
            .await
            .unwrap();
        let status = resp.status().as_u16();
        let body = resp.json().await.unwrap();
        (status, body)
    }

    /// `DELETE /connectors/{connector}/credentials` — the mock/local
    /// connector's "log out."
    async fn logout(&self, connector: &str) -> (u16, Value) {
        let resp = self
            .client
            .delete(format!(
                "{}/connectors/{connector}/credentials",
                self.gateway_base
            ))
            .send()
            .await
            .unwrap();
        let status = resp.status().as_u16();
        let body = resp.json().await.unwrap();
        (status, body)
    }
}

fn checkpoint_id_of(pending_response: &Value) -> String {
    pending_response["checkpoint_id"]
        .as_str()
        .expect("expected a pending response with a checkpoint_id")
        .to_string()
}

// ── 1. ALLOW executes immediately ───────────────────────────────────────

#[tokio::test]
async fn allow_executes_immediately() {
    let app = spawn_app().await;
    app.seed_credential("github").await;

    let resp = app.act("github", "list_repos", json!({})).await;

    assert_eq!(resp["status"], json!("success"), "{resp}");
    assert_eq!(resp["result"]["status"], json!("ok"), "{resp}");
}

// ── 2. BLOCK rejects ─────────────────────────────────────────────────────

#[tokio::test]
async fn block_rejects_and_never_dispatches() {
    let app = spawn_app().await;
    app.seed_credential("github").await;

    let resp = app.act("github", "wipe_org", json!({})).await;

    assert_eq!(resp["status"], json!("blocked"), "{resp}");
}

// ── 3. ASK creates a pending checkpoint ─────────────────────────────────

#[tokio::test]
async fn ask_creates_pending_checkpoint() {
    let app = spawn_app().await;

    let resp = app
        .act("github", "delete_repo", json!({ "repository": "a/b" }))
        .await;

    assert_eq!(resp["status"], json!("pending"), "{resp}");
    assert_eq!(resp["reason"]["kind"], json!("approval_required"), "{resp}");
}

// ── 4. APPROVE resumes execution ────────────────────────────────────────

#[tokio::test]
async fn approve_resumes_and_dispatches() {
    let app = spawn_app().await;
    app.seed_credential("github").await;

    let paused = app
        .act("github", "delete_repo", json!({ "repository": "a/b" }))
        .await;
    let id = checkpoint_id_of(&paused);

    let (status, body) = app.respond(&id, json!({ "action": "approve" })).await;

    assert_eq!(status, 200, "{body}");
    assert_eq!(body["status"], json!("resolved"), "{body}");
    assert_eq!(body["result"]["status"], json!("ok"), "{body}");
}

// ── 5. DENY prevents execution ──────────────────────────────────────────

#[tokio::test]
async fn deny_prevents_execution() {
    let app = spawn_app().await;
    let paused = app
        .act("github", "delete_repo", json!({ "repository": "a/b" }))
        .await;
    let id = checkpoint_id_of(&paused);

    let (status, body) = app.respond(&id, json!({ "action": "deny" })).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["status"], json!("denied"), "{body}");

    let cp = app.get_checkpoint(&id).await;
    assert_eq!(cp["status"], json!("denied"), "{cp}");
    assert!(cp["result"].is_null(), "tool must not have executed: {cp}");
}

// ── 6. InputRequired pauses ──────────────────────────────────────────────

#[tokio::test]
async fn input_required_pauses_on_missing_field() {
    let app = spawn_app().await;
    app.seed_credential("github").await;

    let resp = app.act("github", "get_latest_pr", json!({})).await;

    assert_eq!(resp["status"], json!("pending"), "{resp}");
    assert_eq!(resp["reason"]["kind"], json!("input_required"), "{resp}");
    let missing = resp["reason"]["missing"].as_array().unwrap();
    assert!(
        missing.iter().any(|m| m["name"] == json!("repository")),
        "{resp}"
    );
}

// ── 7. Input response resumes execution ─────────────────────────────────

#[tokio::test]
async fn input_response_resumes_execution() {
    let app = spawn_app().await;
    app.seed_credential("github").await;

    let paused = app.act("github", "get_latest_pr", json!({})).await;
    let id = checkpoint_id_of(&paused);

    let (status, body) = app
        .respond(
            &id,
            json!({ "action": "input", "fields": { "repository": "Nasiko-Labs/nasiko-cloud-rs" } }),
        )
        .await;

    assert_eq!(status, 200, "{body}");
    assert_eq!(body["status"], json!("resolved"), "{body}");
    assert_eq!(
        body["result"]["arguments"]["repository"],
        json!("Nasiko-Labs/nasiko-cloud-rs"),
        "{body}"
    );
}

// ── 8. AuthRequired pauses ────────────────────────────────────────────────

#[tokio::test]
async fn auth_required_pauses_when_credential_missing() {
    let app = spawn_app().await;

    let resp = app.act("github", "list_repos", json!({})).await;

    assert_eq!(resp["status"], json!("pending"), "{resp}");
    assert_eq!(resp["reason"]["kind"], json!("auth_required"), "{resp}");
    assert_eq!(resp["reason"]["connector"], json!("github"), "{resp}");
}

// ── 9. Authentication allows resume ─────────────────────────────────────

#[tokio::test]
async fn authenticate_resumes_execution() {
    let app = spawn_app().await;

    let paused = app.act("github", "list_repos", json!({})).await;
    let id = checkpoint_id_of(&paused);

    let (status, body) = app
        .respond(
            &id,
            json!({ "action": "authenticate", "token": "user-supplied-demo-token" }),
        )
        .await;

    assert_eq!(status, 200, "{body}");
    assert_eq!(body["status"], json!("resolved"), "{body}");
}

// ── 10. Tool is never executed before required human action ────────────

#[tokio::test]
async fn tool_not_executed_while_checkpoint_pending() {
    let app = spawn_app().await;
    let paused = app
        .act("github", "delete_repo", json!({ "repository": "a/b" }))
        .await;
    let id = checkpoint_id_of(&paused);

    // Still pending, no result yet — the downstream dispatch step (the only
    // code path that ever talks to the mock connector) has not run.
    let cp = app.get_checkpoint(&id).await;
    assert_eq!(cp["status"], json!("pending"), "{cp}");
    assert!(cp["result"].is_null(), "{cp}");

    // Only after approval does dispatch happen and `result` populate.
    app.seed_credential("github").await;
    let (_, body) = app.respond(&id, json!({ "action": "approve" })).await;
    assert_eq!(body["status"], json!("resolved"), "{body}");
    let cp = app.get_checkpoint(&id).await;
    assert!(cp["result"].is_object(), "{cp}");
}

// ── 11. Invalid HITL responses are rejected ─────────────────────────────

#[tokio::test]
async fn invalid_action_for_pause_reason_is_rejected() {
    let app = spawn_app().await;
    app.seed_credential("github").await;

    let paused = app.act("github", "get_latest_pr", json!({})).await; // InputRequired
    let id = checkpoint_id_of(&paused);

    // "approve" is not a valid response to an InputRequired checkpoint.
    let (status, body) = app.respond(&id, json!({ "action": "approve" })).await;
    assert_eq!(status, 400, "{body}");

    // Rejected action must not have consumed the checkpoint.
    let cp = app.get_checkpoint(&id).await;
    assert_eq!(cp["status"], json!("pending"), "{cp}");
}

// ── 12. Duplicate responses are handled safely ──────────────────────────

#[tokio::test]
async fn duplicate_response_is_rejected_not_double_applied() {
    let app = spawn_app().await;
    let paused = app
        .act("github", "delete_repo", json!({ "repository": "a/b" }))
        .await;
    let id = checkpoint_id_of(&paused);

    let (first_status, first_body) = app.respond(&id, json!({ "action": "deny" })).await;
    assert_eq!(first_status, 200, "{first_body}");

    // Same response sent again after the checkpoint is no longer pending —
    // `claim_pending`'s atomic `UPDATE ... WHERE status = 'pending'` is what
    // makes this a safe rejection rather than a second denial being applied.
    let (second_status, second_body) = app.respond(&id, json!({ "action": "deny" })).await;
    assert_eq!(second_status, 409, "{second_body}");
}

// ── 13. Completed checkpoints cannot be resumed again ───────────────────

#[tokio::test]
async fn completed_checkpoint_cannot_be_resumed_again() {
    let app = spawn_app().await;
    app.seed_credential("github").await;
    let paused = app
        .act("github", "delete_repo", json!({ "repository": "a/b" }))
        .await;
    let id = checkpoint_id_of(&paused);

    let (status, body) = app.respond(&id, json!({ "action": "approve" })).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["status"], json!("resolved"), "{body}");

    let (status, body) = app.respond(&id, json!({ "action": "approve" })).await;
    assert_eq!(status, 409, "{body}");
}

// ── 14. MCP execution failure after resume is handled correctly ────────

#[tokio::test]
async fn dispatch_failure_after_resume_marks_checkpoint_failed() {
    let app = spawn_app().await;
    app.seed_credential("github").await;

    // `phantom_tool` is ask-gated by policy but doesn't actually exist on
    // the mock GitHub server — it pauses for approval like any other
    // ask-gated tool, but its resumed dispatch fails downstream.
    let paused = app.act("github", "phantom_tool", json!({})).await;
    let id = checkpoint_id_of(&paused);

    let (status, body) = app.respond(&id, json!({ "action": "approve" })).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["status"], json!("failed"), "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("phantom_tool"),
        "{body}"
    );

    let cp = app.get_checkpoint(&id).await;
    assert_eq!(cp["status"], json!("failed"), "{cp}");
}

// ── 15. Genericity: the same pre-flight code path handles both connectors ──

#[tokio::test]
async fn auth_required_is_generic_across_connectors() {
    let app = spawn_app().await;

    for (connector, tool) in [("github", "list_repos"), ("notion", "search_pages")] {
        let resp = app.act(connector, tool, json!({})).await;
        assert_eq!(
            resp["status"],
            json!("pending"),
            "{connector}/{tool}: {resp}"
        );
        assert_eq!(
            resp["reason"]["kind"],
            json!("auth_required"),
            "{connector}/{tool}: {resp}"
        );
        assert_eq!(
            resp["reason"]["connector"],
            json!(connector),
            "{connector}/{tool}: {resp}"
        );
    }
}

// ── 16/17. HITL is triggered by the agent, never independently by the
//           gateway — the gateway only ever enforces a hard, human-free
//           backstop (Block always; Ask when bypassed without going
//           through the agent's pre-flight). Neither case ever creates a
//           checkpoint. ─────────────────────────────────────────────────

#[tokio::test]
async fn gateway_block_backstop_rejects_direct_call_without_agent() {
    let app = spawn_app().await;
    app.seed_credential("github").await;
    let before = app.pending_count().await;

    let resp = app
        .call_gateway_directly("github__wipe_org", json!({}))
        .await;

    assert!(resp.get("result").is_none(), "{resp}");
    assert_eq!(resp["error"]["code"], json!(-32000), "{resp}");
    assert_eq!(
        app.pending_count().await,
        before,
        "the gateway must never create a checkpoint on its own"
    );
}

#[tokio::test]
async fn gateway_rejects_ask_tool_reached_directly_without_agent_preflight() {
    let app = spawn_app().await;
    let before = app.pending_count().await;

    // Bypasses `/agent/act` entirely and calls the gateway's `tools/call`
    // directly for an `ask`-gated tool — the gateway must refuse this
    // outright (a distinct error code from `Block`) rather than either
    // silently executing it or independently creating a HITL checkpoint of
    // its own. Only the agent's own pre-flight (`/agent/act`) is allowed to
    // turn an `ask` stance into a paused checkpoint.
    let resp = app
        .call_gateway_directly("github__delete_repo", json!({ "repository": "a/b" }))
        .await;

    assert!(resp.get("result").is_none(), "{resp}");
    assert_eq!(resp["error"]["code"], json!(-32001), "{resp}");
    assert_eq!(
        app.pending_count().await,
        before,
        "the gateway must never independently create a checkpoint for an ask-gated tool"
    );
}

// ── Real connector-status failures must surface, not silently become
// "not connected" ─────────────────────────────────────────────────────────
//
// `gateway::routes::get_connector_status` used to collapse both "checked,
// and it's not connected" and "the check itself failed" into the same
// `connected: false` response (`.unwrap_or(false)` / `.ok()`). That made a
// real Composio outage or a broken credential store indistinguishable from
// an ordinary `AuthRequired` pause — the agent would ask a human to
// authenticate a connector it never actually managed to check. These tests
// force a real backing-store failure (closing the shared SQLite pool) and
// confirm it now comes back as a hard error at both the gateway's own
// endpoint and through the agent's `/agent/act`, which must surface it as
// `{"status":"error",...}` rather than a false `auth_required` pause.

#[tokio::test]
async fn connector_status_surfaces_real_store_failure_not_false() {
    let app = spawn_app().await;
    app.gateway_state.pool.close().await;

    let (status, body) = app.connector_status_direct("github").await;

    assert_ne!(status, 200, "{body}");
    assert!(body.get("error").is_some(), "{body}");
    assert!(
        body.get("connected").is_none(),
        "a failed check must not also claim a connected/not-connected fact: {body}"
    );
}

#[tokio::test]
async fn credential_store_failure_surfaces_as_agent_error_not_auth_required() {
    let app = spawn_app().await;
    app.gateway_state.pool.close().await;

    let resp = app.act("github", "list_repos", json!({})).await;

    assert_eq!(resp["status"], json!("error"), "{resp}");
    assert!(
        resp["error"]
            .as_str()
            .unwrap_or_default()
            .contains("credential"),
        "expected the real credential-store failure reason to be visible: {resp}"
    );
    assert!(
        resp.get("checkpoint_id").is_none(),
        "a failed pre-flight check must never be persisted as a pending checkpoint: {resp}"
    );
}

// ── Logout (DELETE /connectors/{connector}/credentials) ────────────────────

#[tokio::test]
async fn logout_makes_a_connected_mock_connector_require_auth_again() {
    let app = spawn_app().await;
    app.seed_credential("github").await;

    let allowed = app.act("github", "list_repos", json!({})).await;
    assert_eq!(allowed["status"], json!("success"), "{allowed}");

    let (status, body) = app.logout("github").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["disconnected"], json!(true), "{body}");

    let paused = app.act("github", "list_repos", json!({})).await;
    assert_eq!(paused["status"], json!("pending"), "{paused}");
    assert_eq!(paused["reason"]["kind"], json!("auth_required"), "{paused}");
}

#[tokio::test]
async fn logout_of_a_never_connected_connector_is_a_safe_no_op() {
    let app = spawn_app().await;
    let (status, body) = app.logout("notion").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["disconnected"], json!(true), "{body}");
}

// ── Operational failures must stay operational failures — never a false
// HITL condition, never a checkpoint, never silent success. ────────────────

#[tokio::test]
async fn gateway_unavailable_surfaces_clean_error_not_a_false_checkpoint() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    // Nothing listens on this port — every gateway call the agent makes
    // fails at the transport level, exactly like the gateway process
    // having been stopped out from under it.
    let dead_gateway = GatewayClient::new("http://127.0.0.1:1".to_string());
    let agent_state = Arc::new(AgentState::new(pool, dead_gateway));
    let agent_base = spawn_on_ephemeral(agent::router(agent_state)).await;

    let resp: Value = reqwest::Client::new()
        .post(format!("{agent_base}/agent/act"))
        .json(&json!({ "connector": "github", "tool_name": "list_repos", "arguments": {} }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(resp["status"], json!("error"), "{resp}");
    assert!(
        resp.get("checkpoint_id").is_none(),
        "a gateway outage must never be persisted as a pending checkpoint: {resp}"
    );
    let error = resp["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("gateway unreachable"),
        "expected a clear gateway-unreachable message, got: {error}"
    );
}

#[tokio::test]
async fn downstream_connector_unavailable_surfaces_clean_failure_not_success_or_auth() {
    // A connector that's registered (so it's not the "unknown connector"
    // case) and has a valid stored credential (so it's not AuthRequired
    // either) but whose downstream MCP server address nothing listens on —
    // the closest deterministic stand-in for "the mock connector process
    // was stopped."
    let app = spawn_app_with_extra_connectors(&[("deadconnector", "http://127.0.0.1:1/mcp")]).await;
    app.seed_credential("deadconnector").await;

    let resp = app.act("deadconnector", "anything", json!({})).await;

    assert_eq!(resp["status"], json!("failed"), "{resp}");
    assert!(
        resp.get("checkpoint_id").is_none(),
        "a downstream outage must never be persisted as a pending checkpoint: {resp}"
    );
    let error = resp["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("downstream MCP server"),
        "expected a clear downstream-connector error, got: {error}"
    );
}

#[tokio::test]
async fn unknown_connector_surfaces_clean_error_not_misleading_auth_required() {
    let app = spawn_app().await;

    // No credential seeded and no such connector ever registered — before
    // the fix, `CredentialCheck` asked the local `CredentialStore` about
    // this name anyway, got a routine "no row" answer, and the agent paused
    // asking a human to authenticate a connector that was never wired up at
    // all.
    let resp = app
        .act("totally_unregistered_connector", "whatever", json!({}))
        .await;

    assert_eq!(resp["status"], json!("error"), "{resp}");
    assert!(resp.get("checkpoint_id").is_none(), "{resp}");
    let error = resp["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("unknown connector"),
        "expected a clear unknown-connector error, not a false AuthRequired: {error}"
    );
}

#[tokio::test]
async fn nonexistent_tool_produces_clean_failure_not_a_checkpoint() {
    let app = spawn_app().await;
    app.seed_credential("github").await;

    let resp = app.act("github", "totally_bogus_tool", json!({})).await;

    assert_eq!(resp["status"], json!("failed"), "{resp}");
    assert!(resp.get("checkpoint_id").is_none(), "{resp}");
    assert!(
        resp["error"]
            .as_str()
            .unwrap_or_default()
            .contains("totally_bogus_tool"),
        "{resp}"
    );
}

#[tokio::test]
async fn non_object_arguments_is_a_clean_validation_error_not_input_required() {
    let app = spawn_app().await;

    let (status, body) = app
        .act_raw_body(
            r#"{"connector":"github","tool_name":"get_latest_pr","arguments":"not-an-object"}"#,
        )
        .await;

    assert_eq!(status, 200, "{body}"); // this endpoint always answers 200 with a status field
    let resp: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(resp["status"], json!("error"), "{resp}");
    assert!(resp.get("checkpoint_id").is_none(), "{resp}");
    assert!(
        resp["error"]
            .as_str()
            .unwrap_or_default()
            .contains("must be a JSON object"),
        "wrong argument type must not be treated as 'every field is missing': {resp}"
    );
}

#[tokio::test]
async fn omitted_arguments_defaults_to_empty_object_and_input_resume_still_works() {
    let app = spawn_app().await;
    app.seed_credential("github").await;

    let (status, body) = app
        .act_raw_body(r#"{"connector":"github","tool_name":"get_latest_pr"}"#)
        .await;
    assert_eq!(status, 200, "{body}");
    let paused: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(paused["status"], json!("pending"), "{paused}");
    assert_eq!(
        paused["reason"]["kind"],
        json!("input_required"),
        "{paused}"
    );
    let id = checkpoint_id_of(&paused);

    let (resp_status, resp_body) = app
        .respond(
            &id,
            json!({ "action": "input", "fields": { "repository": "a/b" } }),
        )
        .await;
    assert_eq!(resp_status, 200, "{resp_body}");
    assert_eq!(resp_body["status"], json!("resolved"), "{resp_body}");
}

#[tokio::test]
async fn malformed_json_body_is_rejected_cleanly_and_the_server_stays_healthy() {
    let app = spawn_app().await;

    let (status, _body) = app.act_raw_body("{not valid json").await;
    assert_ne!(
        status, 200,
        "malformed JSON must not be accepted as a valid task"
    );
    assert!(status < 500, "a malformed request must not 500");

    // Confirm the server (and its axum extractor state) is still healthy.
    app.seed_credential("github").await;
    let resp = app.act("github", "list_repos", json!({})).await;
    assert_eq!(resp["status"], json!("success"), "{resp}");
}

// ── HITL state edge cases: partial/stale human responses must not slip
// straight through to dispatch. ─────────────────────────────────────────────

#[tokio::test]
async fn partial_input_response_repauses_input_required_instead_of_dispatching() {
    let app = spawn_app().await;
    app.seed_credential("github").await;

    let paused = app.act("github", "get_latest_pr", json!({})).await;
    let id = checkpoint_id_of(&paused);

    // The human "responds" without actually supplying the missing field.
    let (status, body) = app
        .respond(&id, json!({ "action": "input", "fields": {} }))
        .await;

    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["status"],
        json!("pending"),
        "an incomplete input response must re-pause, not dispatch: {body}"
    );
    assert_eq!(body["reason"]["kind"], json!("input_required"), "{body}");

    let cp = app.get_checkpoint(&id).await;
    assert_eq!(cp["status"], json!("pending"), "{cp}");
    assert!(
        cp["result"].is_null(),
        "must not have dispatched with a required field still missing: {cp}"
    );
}

#[tokio::test]
async fn authenticate_without_a_real_credential_repauses_auth_required() {
    let app = spawn_app().await;

    let paused = app.act("github", "list_repos", json!({})).await;
    let id = checkpoint_id_of(&paused);

    // The human claims to have authenticated but supplies no token, and
    // none is actually stored — resume must re-check the real connection
    // status rather than trust the claim.
    let (status, body) = app.respond(&id, json!({ "action": "authenticate" })).await;

    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["status"],
        json!("pending"),
        "an unverified authenticate claim must re-pause, not resolve: {body}"
    );
    assert_eq!(body["reason"]["kind"], json!("auth_required"), "{body}");

    let cp = app.get_checkpoint(&id).await;
    assert_eq!(cp["status"], json!("pending"), "{cp}");
}

#[tokio::test]
async fn denied_checkpoint_cannot_be_resumed_again() {
    let app = spawn_app().await;
    let paused = app
        .act("github", "delete_repo", json!({ "repository": "a/b" }))
        .await;
    let id = checkpoint_id_of(&paused);

    let (status, body) = app.respond(&id, json!({ "action": "deny" })).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["status"], json!("denied"), "{body}");

    let (status, body) = app.respond(&id, json!({ "action": "approve" })).await;
    assert_eq!(status, 409, "{body}");
}

#[tokio::test]
async fn failed_checkpoint_cannot_be_resumed_again() {
    let app = spawn_app().await;
    app.seed_credential("github").await;
    let paused = app.act("github", "phantom_tool", json!({})).await;
    let id = checkpoint_id_of(&paused);

    let (status, body) = app.respond(&id, json!({ "action": "approve" })).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["status"], json!("failed"), "{body}");

    let (status, body) = app.respond(&id, json!({ "action": "approve" })).await;
    assert_eq!(status, 409, "{body}");
}

// ── Policy re-check semantics: Block is unconditional defense-in-depth,
// re-enforced on every dispatch regardless of an earlier agent decision. ──

#[tokio::test]
async fn gateway_block_backstop_wins_even_with_preflight_approved_header() {
    let app = spawn_app().await;
    app.seed_credential("github").await;

    // Simulates what a resumed, previously-approved dispatch looks like on
    // the wire (`x-agent-preflight: passed` set) for a tool the *current*
    // policy blocks. Block must win regardless — this is what makes "policy
    // changed to Block between pause and resume" safe even though the
    // agent's own resume path does not re-run `Decide`.
    let resp = app
        .call_gateway_directly_as_preflighted("github__wipe_org", json!({}))
        .await;

    assert!(resp.get("result").is_none(), "{resp}");
    assert_eq!(resp["error"]["code"], json!(-32000), "{resp}");
}

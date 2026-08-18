use sqlx::SqlitePool;

use crate::audit::AuditLog;
use crate::credentials::CredentialStore;
use crate::gateway::composio::ComposioSlot;
use crate::policy::PermissionPolicy;
use crate::provider::{ConnectorRegistry, McpClient};

/// Everything the gateway's routes share — composed once at startup and
/// handed to axum as `Arc<GatewayState>`. Deliberately narrow: the gateway
/// only ever executes MCP calls and answers advisory policy/connector-status
/// questions. It owns no `CheckpointStore` and creates no HITL checkpoint —
/// that state and decision now live entirely in `agent::state::AgentState`.
pub struct GatewayState {
    pub pool: SqlitePool,
    pub policy: PermissionPolicy,
    pub credentials: CredentialStore,
    pub registry: ConnectorRegistry,
    pub client: McpClient,
    pub audit: AuditLog,
    /// The one live Composio-backed connector for the demo, if configured
    /// (`COMPOSIO_API_KEY`/`COMPOSIO_AUTH_CONFIG_ID` set) — `None` in tests
    /// and in a plain `cargo run` with no Composio credentials.
    pub composio: Option<ComposioSlot>,
}

impl GatewayState {
    pub fn new(
        pool: SqlitePool,
        policy: PermissionPolicy,
        registry: ConnectorRegistry,
        composio: Option<ComposioSlot>,
    ) -> Self {
        Self {
            credentials: CredentialStore::new(pool.clone()),
            client: McpClient::new(),
            audit: AuditLog::new(pool.clone()),
            pool,
            policy,
            registry,
            composio,
        }
    }
}

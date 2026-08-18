use sqlx::SqlitePool;

use crate::agent::checkpoint::CheckpointStore;
use crate::agent::gateway_client::GatewayClient;
use crate::audit::AuditLog;
use crate::schema_validator::SchemaValidator;

/// Everything the agent's routes share. Holds the pause/resume state
/// (`CheckpointStore`) and the pre-flight loop's own schema cache — the two
/// pieces of state that moved here from the gateway — plus the
/// `GatewayClient`, which is the agent's *only* channel to the gateway.
/// Notably absent: `PermissionPolicy`, `CredentialStore`, any connector
/// registry, any Composio client — those stay exclusively gateway-side.
pub struct AgentState {
    pub pool: SqlitePool,
    pub checkpoints: CheckpointStore,
    pub audit: AuditLog,
    pub schema: SchemaValidator,
    pub gateway: GatewayClient,
}

impl AgentState {
    pub fn new(pool: SqlitePool, gateway: GatewayClient) -> Self {
        Self {
            checkpoints: CheckpointStore::new(pool.clone()),
            audit: AuditLog::new(pool.clone()),
            pool,
            schema: SchemaValidator::new(),
            gateway,
        }
    }
}

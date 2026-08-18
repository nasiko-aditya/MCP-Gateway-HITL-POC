//! Append-only audit trail: who initiated the call, which agent/connector/
//! tool, why HITL triggered (if it did), what the human decided, and the
//! final status. `detail` must never carry a credential token — callers
//! pass only small, already-redacted JSON.

use chrono::Utc;
use serde_json::Value;
use sqlx::SqlitePool;
use uuid::Uuid;

pub struct AuditLog {
    pool: SqlitePool,
}

impl AuditLog {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn record(
        &self,
        checkpoint_id: Option<Uuid>,
        call_id: &str,
        user_id: &str,
        agent_id: &str,
        connector: &str,
        tool_name: &str,
        action: &str,
        detail: &Value,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO audit_log
                (id, checkpoint_id, call_id, user_id, agent_id, connector, tool_name, action, detail, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(checkpoint_id.map(|id| id.to_string()))
        .bind(call_id)
        .bind(user_id)
        .bind(agent_id)
        .bind(connector)
        .bind(tool_name)
        .bind(action)
        .bind(serde_json::to_string(detail)?)
        .bind(Utc::now().to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn list_for_call(&self, call_id: &str) -> anyhow::Result<Vec<Value>> {
        let rows: Vec<AuditRow> = sqlx::query_as(
            "SELECT id, checkpoint_id, call_id, user_id, agent_id, connector, tool_name, action, detail, created_at
             FROM audit_log WHERE call_id = ? ORDER BY created_at ASC",
        )
        .bind(call_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| {
                serde_json::json!({
                    "id": r.id,
                    "checkpoint_id": r.checkpoint_id,
                    "call_id": r.call_id,
                    "user_id": r.user_id,
                    "agent_id": r.agent_id,
                    "connector": r.connector,
                    "tool_name": r.tool_name,
                    "action": r.action,
                    "detail": serde_json::from_str::<Value>(&r.detail).unwrap_or(Value::Null),
                    "created_at": r.created_at,
                })
            })
            .collect())
    }
}

#[derive(sqlx::FromRow)]
struct AuditRow {
    id: String,
    checkpoint_id: Option<String>,
    call_id: String,
    user_id: String,
    agent_id: String,
    connector: String,
    tool_name: String,
    action: String,
    detail: String,
    created_at: String,
}

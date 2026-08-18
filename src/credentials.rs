//! Generic per-connector credential store — the `AuthRequired` detection
//! mechanism from HITL_INVESTIGATION.md §6: "is there a live token for this
//! connector," never "is this GitHub." A deliberate POC abstraction: real
//! Nasiko scopes credentials per `(user, connector)` and stores them
//! AES-encrypted with OAuth2 refresh (`oss/mcp-gateway/src/oauth.rs`); this
//! POC scopes per-connector only and stores a plaintext demo token. See
//! HITL_POC.md §10 for what a production version would need instead.

use chrono::Utc;
use sqlx::SqlitePool;

pub struct CredentialStore {
    pool: SqlitePool,
}

impl CredentialStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// True only when a credential exists for `connector` and isn't expired.
    pub async fn is_valid(&self, connector: &str) -> anyhow::Result<bool> {
        let row = sqlx::query_as::<_, (Option<String>,)>(
            "SELECT expires_at FROM credentials WHERE connector = ?",
        )
        .bind(connector)
        .fetch_optional(&self.pool)
        .await?;

        Ok(match row {
            None => false,
            Some((None,)) => true,
            Some((Some(expires_at),)) => chrono::DateTime::parse_from_rfc3339(&expires_at)
                .map(|exp| exp > Utc::now())
                .unwrap_or(false),
        })
    }

    /// Records a credential as "obtained" — the POC stand-in for a real
    /// OAuth exchange or API-key entry. `token` is never logged (see
    /// `audit.rs`) and is stored in plaintext, acceptable only because this
    /// is a demo credential store, never a real secret.
    pub async fn store(
        &self,
        connector: &str,
        token: &str,
        expires_at: Option<chrono::DateTime<Utc>>,
    ) -> anyhow::Result<()> {
        let now = Utc::now().to_rfc3339();
        let expires_at = expires_at.map(|t| t.to_rfc3339());
        sqlx::query(
            "INSERT INTO credentials (connector, token, expires_at, created_at)
             VALUES (?, ?, ?, ?)
             ON CONFLICT(connector) DO UPDATE SET token = excluded.token,
                expires_at = excluded.expires_at, created_at = excluded.created_at",
        )
        .bind(connector)
        .bind(token)
        .bind(expires_at)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Removes any stored credential for `connector` — this POC's "log out"
    /// for the mock/local path. Deleting a row that was never there is a
    /// safe no-op, not an error, so the caller doesn't need to check
    /// `is_valid` first.
    pub async fn delete(&self, connector: &str) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM credentials WHERE connector = ?")
            .bind(connector)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    async fn store() -> CredentialStore {
        let pool = crate::db::connect("sqlite::memory:").await.unwrap();
        CredentialStore::new(pool)
    }

    #[tokio::test]
    async fn missing_credential_is_invalid() {
        let s = store().await;
        assert!(!s.is_valid("github").await.unwrap());
    }

    #[tokio::test]
    async fn stored_credential_without_expiry_is_valid() {
        let s = store().await;
        s.store("github", "tok", None).await.unwrap();
        assert!(s.is_valid("github").await.unwrap());
    }

    #[tokio::test]
    async fn expired_credential_is_invalid() {
        let s = store().await;
        s.store("github", "tok", Some(Utc::now() - Duration::hours(1)))
            .await
            .unwrap();
        assert!(!s.is_valid("github").await.unwrap());
    }

    #[tokio::test]
    async fn re_storing_overwrites_expiry() {
        let s = store().await;
        s.store("github", "tok", Some(Utc::now() - Duration::hours(1)))
            .await
            .unwrap();
        assert!(!s.is_valid("github").await.unwrap());
        s.store("github", "tok2", None).await.unwrap();
        assert!(s.is_valid("github").await.unwrap());
    }

    /// A real backing-store failure must come back as `Err`, never silently
    /// coerced to `false` — callers (`gateway::routes::get_connector_status`)
    /// rely on this to distinguish "genuinely not connected" from "the check
    /// itself broke."
    #[tokio::test]
    async fn is_valid_surfaces_real_store_errors() {
        let pool = crate::db::connect("sqlite::memory:").await.unwrap();
        let s = CredentialStore::new(pool.clone());
        pool.close().await;
        assert!(s.is_valid("github").await.is_err());
    }

    #[tokio::test]
    async fn delete_logs_out_a_connected_connector() {
        let s = store().await;
        s.store("github", "tok", None).await.unwrap();
        assert!(s.is_valid("github").await.unwrap());
        s.delete("github").await.unwrap();
        assert!(!s.is_valid("github").await.unwrap());
    }

    #[tokio::test]
    async fn delete_of_never_connected_connector_is_a_safe_no_op() {
        let s = store().await;
        assert!(s.delete("github").await.is_ok());
    }
}

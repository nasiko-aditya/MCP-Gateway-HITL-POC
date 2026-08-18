use std::str::FromStr;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;

/// Connects to `database_url` and runs migrations. An in-memory URL
/// (`sqlite::memory:`, used by tests) is capped at one pooled connection —
/// sqlite's in-memory databases are private per-connection, so a second
/// pooled connection would see an empty database.
pub async fn connect(database_url: &str) -> anyhow::Result<SqlitePool> {
    let is_memory = database_url.contains(":memory:");
    let options = SqliteConnectOptions::from_str(database_url)?.create_if_missing(true);

    let pool = SqlitePoolOptions::new()
        .max_connections(if is_memory { 1 } else { 5 })
        .connect_with(options)
        .await?;

    sqlx::migrate!("./migrations").run(&pool).await?;
    Ok(pool)
}

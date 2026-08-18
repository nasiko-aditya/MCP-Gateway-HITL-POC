use std::sync::Arc;

use mcp_gateway_hitl_poc::agent::gateway_client::GatewayClient;
use mcp_gateway_hitl_poc::agent::state::AgentState;
use mcp_gateway_hitl_poc::config::Config;
use mcp_gateway_hitl_poc::gateway::composio::{ComposioConnector, ComposioSlot};
use mcp_gateway_hitl_poc::provider::ConnectorRegistry;
use mcp_gateway_hitl_poc::state::GatewayState;
use mcp_gateway_hitl_poc::{agent, db, gateway, mock_connectors, policy};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env().add_directive("info".parse()?),
        )
        .init();

    let config = Config::from_env();

    // Two independent mock downstream MCP servers, spawned in-process so the
    // whole POC boots with one `cargo run` — but the gateway still reaches
    // them over real HTTP, exactly like a real connector (see provider.rs).
    spawn_mock(
        mock_connectors::github::router(),
        config.github_mock_port,
        "github",
    )
    .await?;
    spawn_mock(
        mock_connectors::notion::router(),
        config.notion_mock_port,
        "notion",
    )
    .await?;

    let mut registry = ConnectorRegistry::new();
    registry.register(
        "github",
        &format!("http://127.0.0.1:{}/mcp", config.github_mock_port),
    );
    registry.register(
        "notion",
        &format!("http://127.0.0.1:{}/mcp", config.notion_mock_port),
    );

    // Only bootstrapped when COMPOSIO_API_KEY/COMPOSIO_AUTH_CONFIG_ID are
    // set — absent in tests and in a plain `cargo run`. A failure here is
    // fatal (it means the env vars were set but the credentials/config are
    // wrong), so the operator finds out immediately rather than the
    // Composio connector silently never appearing in `tools/list`.
    let composio = match &config.composio {
        Some(cfg) => Some(ComposioSlot {
            connector_name: cfg.connector_name.clone(),
            client: ComposioConnector::bootstrap(cfg).await?,
        }),
        None => {
            tracing::info!("COMPOSIO_API_KEY not set — running with mock connectors only");
            None
        }
    };

    let pool = db::connect(&config.database_url).await?;
    let permission_policy = policy::PermissionPolicy::load(&config.policy_path)?;
    let gateway_state = Arc::new(GatewayState::new(
        pool.clone(),
        permission_policy,
        registry,
        composio,
    ));

    let gateway_app = gateway::routes::router(gateway_state);
    let gateway_listener =
        tokio::net::TcpListener::bind(format!("0.0.0.0:{}", config.gateway_port)).await?;
    tracing::info!(port = config.gateway_port, "MCP Gateway listening");
    let gateway_task =
        tokio::spawn(async move { axum::serve(gateway_listener, gateway_app).await });

    let gateway_client = GatewayClient::new(format!("http://127.0.0.1:{}", config.gateway_port));
    let agent_state = Arc::new(AgentState::new(pool, gateway_client));
    let agent_app = agent::router(agent_state);
    let agent_listener =
        tokio::net::TcpListener::bind(format!("0.0.0.0:{}", config.agent_port)).await?;
    tracing::info!(port = config.agent_port, "Agent listening");
    let agent_task = tokio::spawn(async move { axum::serve(agent_listener, agent_app).await });

    let (gateway_result, agent_result) = tokio::try_join!(gateway_task, agent_task)?;
    gateway_result?;
    agent_result?;
    Ok(())
}

async fn spawn_mock(router: axum::Router, port: u16, name: &'static str) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}")).await?;
    tracing::info!(port, name, "mock connector listening");
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, router).await {
            tracing::error!(name, error = %e, "mock connector server crashed");
        }
    });
    Ok(())
}

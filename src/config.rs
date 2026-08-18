//! Single env-driven config, deliberately tiny (POC scope, not
//! `nasiko-config`'s ~60-field struct).
//!
//! `.env` (see `.env.example`) is loaded once, best-effort, at process
//! start — convenience only, never required: every field below still has a
//! working default or is `Option` when genuinely optional.

#[derive(Debug, Clone)]
pub struct Config {
    pub gateway_port: u16,
    pub agent_port: u16,
    pub github_mock_port: u16,
    pub notion_mock_port: u16,
    pub database_url: String,
    pub policy_path: String,
    pub composio: Option<ComposioConfig>,
}

/// Present only when a live Composio-backed connector should be registered
/// alongside the mocks. Absent by default so `cargo test`/a plain `cargo
/// run` never requires Composio credentials — see `gateway::composio`.
#[derive(Debug, Clone)]
pub struct ComposioConfig {
    pub api_key: String,
    pub base_url: String,
    pub auth_config_id: String,
    /// The connector name this toolkit is registered under (used for
    /// gateway routing, `policy.toml` rules, and `tools/list` namespacing —
    /// e.g. `composio_github__GITHUB_CREATE_AN_ISSUE`).
    pub connector_name: String,
    /// Name of the Composio-hosted MCP server to get-or-create at startup.
    pub server_name: String,
    /// Which tools from the toolkit to expose through that MCP server.
    pub allowed_tools: Vec<String>,
}

impl Config {
    pub fn from_env() -> Self {
        let _ = dotenvy::dotenv();

        Self {
            gateway_port: env_port("GATEWAY_PORT", 8080),
            agent_port: env_port("AGENT_PORT", 8090),
            github_mock_port: env_port("GITHUB_MOCK_PORT", 8081),
            notion_mock_port: env_port("NOTION_MOCK_PORT", 8082),
            database_url: std::env::var("DATABASE_URL")
                .unwrap_or_else(|_| "sqlite://./hitl_poc.db".to_string()),
            policy_path: std::env::var("POLICY_PATH").unwrap_or_else(|_| "policy.toml".to_string()),
            composio: ComposioConfig::from_env(),
        }
    }
}

impl ComposioConfig {
    /// `None` unless both `COMPOSIO_API_KEY` and `COMPOSIO_AUTH_CONFIG_ID`
    /// are set — the two values that can't have a sane default. Everything
    /// else falls back to a demo-appropriate GitHub toolkit, overridable so
    /// a different Composio auth config's real tool slugs can be used
    /// without a code change.
    fn from_env() -> Option<Self> {
        let api_key = std::env::var("COMPOSIO_API_KEY").ok()?;
        let auth_config_id = std::env::var("COMPOSIO_AUTH_CONFIG_ID").ok()?;
        let allowed_tools = std::env::var("COMPOSIO_ALLOWED_TOOLS")
            .unwrap_or_else(|_| {
                "GITHUB_LIST_REPOSITORIES_FOR_THE_AUTHENTICATED_USER,GITHUB_CREATE_AN_ISSUE"
                    .to_string()
            })
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        Some(Self {
            api_key,
            base_url: std::env::var("COMPOSIO_BASE_URL")
                .unwrap_or_else(|_| "https://backend.composio.dev".to_string()),
            auth_config_id,
            connector_name: std::env::var("COMPOSIO_CONNECTOR_NAME")
                .unwrap_or_else(|_| "composio_github".to_string()),
            server_name: std::env::var("COMPOSIO_SERVER_NAME")
                .unwrap_or_else(|_| "mcp-gateway-hitl-poc-demo".to_string()),
            allowed_tools,
        })
    }
}

fn env_port(key: &str, default: u16) -> u16 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

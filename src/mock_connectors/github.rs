//! Mock GitHub MCP server — the concrete demo connector from
//! HITL_INVESTIGATION.md §7. Its four tools are chosen to exercise each
//! outcome once `policy.toml` and the credential store are applied on top:
//! `list_repos` (plain allow), `wipe_org` (blocked), `delete_repo` (ask),
//! `get_latest_pr` (allow, but requires `repository`).

use axum::Router;
use serde_json::json;

use super::{build_mock_server, ToolSpec};

pub fn router() -> Router {
    build_mock_server(
        "github",
        vec![
            ToolSpec {
                name: "list_repos",
                description: "List repositories the authenticated user can see.",
                input_schema: json!({ "type": "object", "properties": {}, "required": [] }),
            },
            ToolSpec {
                name: "wipe_org",
                description: "Irreversibly delete an entire GitHub organization.",
                input_schema: json!({ "type": "object", "properties": {}, "required": [] }),
            },
            ToolSpec {
                name: "delete_repo",
                description: "Delete a repository.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "repository": { "type": "string", "description": "owner/repo" }
                    },
                    "required": ["repository"]
                }),
            },
            ToolSpec {
                name: "get_latest_pr",
                description: "Get the most recently opened pull request for a repository.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "repository": {
                            "type": "string",
                            "description": "owner/repo, e.g. Nasiko-Labs/nasiko-cloud-rs"
                        }
                    },
                    "required": ["repository"]
                }),
            },
        ],
    )
}

//! Second mock connector — exists purely to prove the HITL mechanism isn't
//! GitHub-specific (HITL_INVESTIGATION.md §7/§9). No pipeline code
//! branches on "notion" anywhere; it goes through the exact same
//! decide -> credential_check -> schema_check -> dispatch pipeline as
//! GitHub, with its own independent credential and policy state.

use axum::Router;
use serde_json::json;

use super::{build_mock_server, ToolSpec};

pub fn router() -> Router {
    build_mock_server(
        "notion",
        vec![
            ToolSpec {
                name: "search_pages",
                description: "Search workspace pages.",
                input_schema: json!({ "type": "object", "properties": {}, "required": [] }),
            },
            ToolSpec {
                name: "create_page",
                description: "Create a new page.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "title": { "type": "string", "description": "page title" }
                    },
                    "required": ["title"]
                }),
            },
        ],
    )
}

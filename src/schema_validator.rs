//! `InputRequired` detection — HITL_INVESTIGATION.md §6: reads the tool's
//! own standard MCP `inputSchema` (learned once from the downstream
//! connector's `tools/list`, then cached) and diffs its `required` array
//! against the keys actually present in the call's `arguments`. This is
//! the same schema Nasiko's `aggregator::aggregate_tools` already sees at
//! list time and today just discards — nothing here is GitHub-specific;
//! it only reads the generic JSON Schema shape every MCP tool declares.

use std::collections::HashMap;

use serde_json::Value;
use tokio::sync::RwLock;

/// One field the call is missing, with whatever the schema says about it —
/// enough for a human-readable HITL prompt without any connector-specific
/// knowledge.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct MissingField {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Default)]
pub struct SchemaValidator {
    // (connector, tool_name) -> inputSchema
    cache: RwLock<HashMap<(String, String), Value>>,
}

impl SchemaValidator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Populates the cache from a downstream connector's `tools/list`
    /// response — called once per connector, lazily, the first time a
    /// `tools/call` needs a schema this process hasn't seen yet.
    pub async fn ingest_tools_list(&self, connector: &str, tools: &[Value]) {
        let mut cache = self.cache.write().await;
        for tool in tools {
            let (Some(name), Some(schema)) = (
                tool.get("name").and_then(|v| v.as_str()),
                tool.get("inputSchema"),
            ) else {
                continue;
            };
            cache.insert((connector.to_string(), name.to_string()), schema.clone());
        }
    }

    pub async fn has_schema(&self, connector: &str, tool_name: &str) -> bool {
        self.cache
            .read()
            .await
            .contains_key(&(connector.to_string(), tool_name.to_string()))
    }

    /// Diffs `schema.required` against the keys present in `arguments`.
    /// Returns an empty vec when every required field is present, or when
    /// no schema is cached at all (nothing to validate against).
    pub async fn missing_required_fields(
        &self,
        connector: &str,
        tool_name: &str,
        arguments: &Value,
    ) -> Vec<MissingField> {
        let cache = self.cache.read().await;
        let Some(schema) = cache.get(&(connector.to_string(), tool_name.to_string())) else {
            return Vec::new();
        };
        let Some(required) = schema.get("required").and_then(|v| v.as_array()) else {
            return Vec::new();
        };
        let properties = schema.get("properties");
        let provided = arguments.as_object();

        required
            .iter()
            .filter_map(|v| v.as_str())
            .filter(|field| {
                !provided
                    .map(|obj| obj.contains_key(*field))
                    .unwrap_or(false)
            })
            .map(|field| {
                let prop = properties.and_then(|p| p.get(field));
                MissingField {
                    name: field.to_string(),
                    field_type: prop
                        .and_then(|p| p.get("type"))
                        .and_then(|t| t.as_str())
                        .map(str::to_string),
                    description: prop
                        .and_then(|p| p.get("description"))
                        .and_then(|d| d.as_str())
                        .map(str::to_string),
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_tool() -> Value {
        json!({
            "name": "get_latest_pr",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repository": { "type": "string", "description": "owner/repo" }
                },
                "required": ["repository"]
            }
        })
    }

    #[tokio::test]
    async fn no_schema_cached_means_nothing_missing() {
        let v = SchemaValidator::new();
        let missing = v
            .missing_required_fields("github", "unknown_tool", &json!({}))
            .await;
        assert!(missing.is_empty());
    }

    #[tokio::test]
    async fn detects_missing_required_field_with_type_and_description() {
        let v = SchemaValidator::new();
        v.ingest_tools_list("github", &[sample_tool()]).await;
        let missing = v
            .missing_required_fields("github", "get_latest_pr", &json!({}))
            .await;
        assert_eq!(
            missing,
            vec![MissingField {
                name: "repository".to_string(),
                field_type: Some("string".to_string()),
                description: Some("owner/repo".to_string()),
            }]
        );
    }

    #[tokio::test]
    async fn present_required_field_is_not_missing() {
        let v = SchemaValidator::new();
        v.ingest_tools_list("github", &[sample_tool()]).await;
        let missing = v
            .missing_required_fields("github", "get_latest_pr", &json!({"repository": "a/b"}))
            .await;
        assert!(missing.is_empty());
    }
}

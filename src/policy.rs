//! Deterministic, config-driven tool permission policy — a straight port of
//! the *shape* of Nasiko's real `PermissionContext::decide`
//! (`oss/mcp-gateway/src/permissions.rs`): glob-matched tool-name patterns
//! per connector, priority `block > ask > allow`, default `allow`. No LLM
//! call exists anywhere in this file — every test in `tests/hitl_flow.rs`
//! that exercises `decide` proves that directly.
//!
//! Nasiko additionally gates on whether the *connector* is enabled for the
//! agent at all (a separate on/off toggle backed by Postgres). This POC
//! skips that layer — both mock connectors are always "enabled" — and
//! implements only the per-tool stance layer, since that's the layer the
//! HITL mechanism actually hooks into (`Ask` → pause).

use serde::{Deserialize, Serialize};

/// Also `Serialize`: this is the exact vocabulary the gateway's advisory
/// `GET /policy/{connector}/{tool}` endpoint returns to the agent (see
/// `gateway::routes::get_policy`) — the agent decides what an `Ask`/`Block`
/// answer means, the gateway just answers the question.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stance {
    Allow,
    Ask,
    Block,
}

#[derive(Debug, Clone, Deserialize)]
struct Rule {
    connector: String,
    pattern: String,
    stance: Stance,
}

#[derive(Debug, Deserialize)]
struct RulesFile {
    #[serde(default)]
    rules: Vec<Rule>,
}

#[derive(Debug, Clone)]
pub struct PermissionPolicy {
    rules: Vec<Rule>,
}

impl PermissionPolicy {
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path)?;
        Self::from_toml_str(&raw)
    }

    /// Same format as `policy.toml`, loaded from an in-memory string —
    /// used by integration tests to exercise specific rule sets without a
    /// file on disk.
    pub fn from_toml_str(raw: &str) -> anyhow::Result<Self> {
        let parsed: RulesFile = toml::from_str(raw)?;
        Ok(Self {
            rules: parsed.rules,
        })
    }

    pub fn empty() -> Self {
        Self { rules: Vec::new() }
    }

    /// The single decision point every `tools/call` must go through before
    /// dispatch. Priority `block > ask > allow`; a tool matched by no rule
    /// defaults to `allow`.
    pub fn decide(&self, connector: &str, tool_name: &str) -> Stance {
        let tool_lower = tool_name.to_ascii_lowercase();
        let matching: Vec<Stance> = self
            .rules
            .iter()
            .filter(|r| {
                r.connector.eq_ignore_ascii_case(connector)
                    && wildcard_match(&r.pattern.to_ascii_lowercase(), &tool_lower)
            })
            .map(|r| r.stance)
            .collect();

        for priority in [Stance::Block, Stance::Ask, Stance::Allow] {
            if matching.contains(&priority) {
                return priority;
            }
        }
        Stance::Allow
    }
}

/// Case-sensitive glob match supporting `*` and `?`. Callers lowercase both
/// sides first (see `decide`).
fn wildcard_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut star_ti = 0usize;

    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            star_ti = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(rules: Vec<(&str, &str, Stance)>) -> PermissionPolicy {
        PermissionPolicy {
            rules: rules
                .into_iter()
                .map(|(connector, pattern, stance)| Rule {
                    connector: connector.to_string(),
                    pattern: pattern.to_string(),
                    stance,
                })
                .collect(),
        }
    }

    #[test]
    fn defaults_to_allow() {
        let p = PermissionPolicy::empty();
        assert_eq!(p.decide("github", "anything"), Stance::Allow);
    }

    #[test]
    fn block_beats_ask_beats_allow() {
        let p = policy(vec![
            ("github", "*", Stance::Allow),
            ("github", "delete_*", Stance::Ask),
            ("github", "delete_repo", Stance::Block),
        ]);
        assert_eq!(p.decide("github", "delete_repo"), Stance::Block);
        assert_eq!(p.decide("github", "delete_branch"), Stance::Ask);
        assert_eq!(p.decide("github", "list_repos"), Stance::Allow);
    }

    #[test]
    fn rules_are_scoped_per_connector() {
        let p = policy(vec![("github", "*", Stance::Block)]);
        assert_eq!(p.decide("notion", "anything"), Stance::Allow);
    }
}

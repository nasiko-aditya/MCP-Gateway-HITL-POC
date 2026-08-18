//! The generic pause/resume model — a port of `HITL-POC/src/services/checkpoint.rs`
//! and `HITL-POC/src/tools/executor.rs`'s `ToolOutcome`, retargeted at MCP's
//! `call_id`/`tool_name`/`tool_arguments` instead of A2A's task model. Owned
//! by the agent (relocated here from the top-level `checkpoint` module,
//! unchanged in shape) since the agent is now the only thing that decides a
//! call needs to pause and the only thing that persists a `Checkpoint`.
//!
//! [`ToolOutcome`] is the only vocabulary the agent's pre-flight loop
//! (`agent::preflight`) produces: what a `tools/call` attempt (fresh or
//! resumed) actually did. [`PauseReason`] is the narrower vocabulary of *why
//! a checkpoint is dormant* — computed from a `ToolOutcome` by
//! [`pause_reason_for_outcome`]. Splitting these (rather than storing a raw
//! `ToolOutcome` on the checkpoint) is what lets [`PauseReason::resume_from`]
//! be the *only* place that knows "what does resuming this kind of pause
//! mean" — `agent::preflight` never matches on a tool or connector name to
//! decide.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::schema_validator::MissingField;

/// What one `tools/call` attempt (fresh or a resumed retry) produced. The
/// only vocabulary `pipeline.rs` returns to its caller — the gateway layer
/// never inspects a tool or connector name to decide what happened, only
/// which of these variants came back.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolOutcome {
    Success(Value),
    InputRequired {
        missing: Vec<MissingField>,
    },
    AuthRequired {
        connector: String,
        auth_url: Option<String>,
    },
    ApprovalRequired {
        summary: String,
    },
    Failed(String),
}

/// Why a checkpoint is dormant — the persisted subset of [`ToolOutcome`]
/// that actually pauses a call (`Success`/`Failed` never do; see
/// [`pause_reason_for_outcome`]).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PauseReason {
    ApprovalRequired {
        summary: String,
    },
    AuthRequired {
        connector: String,
        auth_url: Option<String>,
    },
    InputRequired {
        missing: Vec<MissingField>,
    },
}

/// Maps a fresh [`ToolOutcome`] onto the [`PauseReason`] that should be
/// checkpointed, or `None` for `Success`/`Failed` (which never pause
/// anything — the caller should handle those directly). The one place a
/// `ToolOutcome` turns into "why we're pausing."
pub fn pause_reason_for_outcome(outcome: &ToolOutcome) -> Option<PauseReason> {
    match outcome {
        ToolOutcome::Success(_) | ToolOutcome::Failed(_) => None,
        ToolOutcome::InputRequired { missing } => Some(PauseReason::InputRequired {
            missing: missing.clone(),
        }),
        ToolOutcome::AuthRequired {
            connector,
            auth_url,
        } => Some(PauseReason::AuthRequired {
            connector: connector.clone(),
            auth_url: auth_url.clone(),
        }),
        ToolOutcome::ApprovalRequired { summary } => Some(PauseReason::ApprovalRequired {
            summary: summary.clone(),
        }),
    }
}

/// The pipeline stage a paused call should resume at — see `pipeline.rs`'s
/// fixed step order (`Decide -> CredentialCheck -> SchemaCheck -> Dispatch`).
/// Each `PauseReason` has exactly one correct successor: whichever step
/// comes right after the one that produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PipelineStep {
    CredentialCheck,
    SchemaCheck,
    Dispatch,
}

impl PauseReason {
    /// `ApprovalRequired` pauses at the decide step, so approving resumes
    /// at the very next step, `CredentialCheck`. `InputRequired` pauses at
    /// `SchemaCheck`, so resumes straight at `Dispatch`.
    ///
    /// `AuthRequired` pauses at `CredentialCheck` and also *resumes* there
    /// (re-runs it) rather than skipping past it — this is what makes a
    /// tokenless `authenticate` (the Composio case: the human completed
    /// OAuth out-of-band, there's nothing to store) actually re-verify the
    /// connection instead of blindly trusting the human's claim. For a
    /// local/mock connector this costs one extra, already-passing check
    /// (the token was just stored), so applying it unconditionally keeps
    /// the resume path connector-agnostic rather than branching on
    /// connector type.
    ///
    /// `InputRequired` resumes at `SchemaCheck` (re-runs it) rather than
    /// skipping straight to `Dispatch`, for the identical reason: an
    /// `input` response isn't guaranteed to have actually supplied every
    /// field that was missing — a human can submit a partial `fields`
    /// object, or the schema cache can have picked up more required fields
    /// than were known when the checkpoint first paused. Re-running the
    /// diff against the merged arguments before dispatch means an
    /// incomplete resume re-pauses on the fields still missing instead of
    /// dispatching with a call the tool's own schema says is invalid.
    pub fn resume_from(&self) -> PipelineStep {
        match self {
            PauseReason::ApprovalRequired { .. } => PipelineStep::CredentialCheck,
            PauseReason::AuthRequired { .. } => PipelineStep::CredentialCheck,
            PauseReason::InputRequired { .. } => PipelineStep::SchemaCheck,
        }
    }

    pub fn question(&self) -> String {
        match self {
            PauseReason::ApprovalRequired { summary } => {
                format!("Approval required: {summary}. Approve or deny this tool call?")
            }
            PauseReason::AuthRequired {
                connector,
                auth_url,
            } => match auth_url {
                Some(url) => {
                    format!("Authentication required for '{connector}'. Complete auth at: {url}")
                }
                None => format!("Authentication required for '{connector}'."),
            },
            PauseReason::InputRequired { missing } => {
                let names: Vec<&str> = missing.iter().map(|m| m.name.as_str()).collect();
                format!("Missing required input: {}", names.join(", "))
            }
        }
    }

    /// The one action kind this pause reason accepts as its "positive"
    /// resume (in addition to `deny`, which is always valid on any pending
    /// checkpoint — see `HumanAction::Deny`).
    pub fn expected_action(&self) -> &'static str {
        match self {
            PauseReason::ApprovalRequired { .. } => "approve",
            PauseReason::AuthRequired { .. } => "authenticate",
            PauseReason::InputRequired { .. } => "input",
        }
    }
}

/// A human's response to a pending checkpoint. `Deny` is universal — it's a
/// valid response to any pending checkpoint regardless of `PauseReason`, and
/// always means "the tool must not execute." The other three are each valid
/// only against the matching `PauseReason` (enforced by
/// `agent::preflight::resume`).
///
/// `Authenticate.token` is optional to cover two connector kinds generically:
/// a local/mock connector's human pastes a token to store; a Composio
/// connector's human instead completes OAuth out-of-band via `auth_url` and
/// there is nothing to submit — `Authenticate { token: None }` there just
/// means "re-check now" (see `PauseReason::resume_from`).
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum HumanAction {
    Approve,
    Deny,
    Input {
        fields: Value,
    },
    Authenticate {
        #[serde(default)]
        token: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointStatus {
    Pending,
    /// Short-lived: claimed by `claim_pending` and being acted on. Never
    /// observable by a client under normal operation — a request either
    /// finishes the resume (landing on `Resolved`/`Failed`) or re-pauses
    /// (`re_pause` puts it back to `Pending`) within the same handler.
    Processing,
    Denied,
    Resolved,
    Failed,
}

impl CheckpointStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            CheckpointStatus::Pending => "pending",
            CheckpointStatus::Processing => "processing",
            CheckpointStatus::Denied => "denied",
            CheckpointStatus::Resolved => "resolved",
            CheckpointStatus::Failed => "failed",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "processing" => CheckpointStatus::Processing,
            "denied" => CheckpointStatus::Denied,
            "resolved" => CheckpointStatus::Resolved,
            "failed" => CheckpointStatus::Failed,
            _ => CheckpointStatus::Pending,
        }
    }

    pub fn is_pending(&self) -> bool {
        matches!(self, CheckpointStatus::Pending)
    }
}

/// Everything needed to resume a paused `tools/call` exactly where it
/// stopped — the MCP-shaped equivalent of the reference's `Checkpoint`
/// (`call_id`/`tool_name`/`tool_arguments` instead of A2A's task fields).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub id: Uuid,
    pub call_id: String,
    pub user_id: String,
    pub agent_id: String,
    pub connector: String,
    pub tool_name: String,
    pub tool_arguments: Value,
    pub reason: PauseReason,
    pub resume_from: PipelineStep,
    pub status: CheckpointStatus,
    pub human_response: Option<Value>,
    pub result: Option<Value>,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub struct CheckpointStore {
    pool: SqlitePool,
}

impl CheckpointStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create(
        &self,
        call_id: &str,
        user_id: &str,
        agent_id: &str,
        connector: &str,
        tool_name: &str,
        tool_arguments: &Value,
        reason: PauseReason,
    ) -> anyhow::Result<Checkpoint> {
        let id = Uuid::new_v4();
        let now = Utc::now();
        let resume_from = reason.resume_from();
        let cp = Checkpoint {
            id,
            call_id: call_id.to_string(),
            user_id: user_id.to_string(),
            agent_id: agent_id.to_string(),
            connector: connector.to_string(),
            tool_name: tool_name.to_string(),
            tool_arguments: tool_arguments.clone(),
            reason,
            resume_from,
            status: CheckpointStatus::Pending,
            human_response: None,
            result: None,
            error: None,
            created_at: now,
            updated_at: now,
        };

        sqlx::query(
            "INSERT INTO checkpoints
                (id, call_id, user_id, agent_id, connector, tool_name, tool_arguments,
                 reason, resume_from, status, human_response, result, error, created_at, updated_at, expires_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, NULL, NULL, ?, ?, NULL)",
        )
        .bind(cp.id.to_string())
        .bind(&cp.call_id)
        .bind(&cp.user_id)
        .bind(&cp.agent_id)
        .bind(&cp.connector)
        .bind(&cp.tool_name)
        .bind(serde_json::to_string(&cp.tool_arguments)?)
        .bind(serde_json::to_string(&cp.reason)?)
        .bind(serde_json::to_string(&cp.resume_from)?.replace('"', ""))
        .bind(cp.status.as_str())
        .bind(cp.created_at.to_rfc3339())
        .bind(cp.updated_at.to_rfc3339())
        .execute(&self.pool)
        .await?;

        Ok(cp)
    }

    pub async fn get(&self, id: Uuid) -> anyhow::Result<Option<Checkpoint>> {
        let row = sqlx::query_as::<_, CheckpointRow>("SELECT * FROM checkpoints WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(Into::into))
    }

    pub async fn list_pending(&self) -> anyhow::Result<Vec<Checkpoint>> {
        let rows = sqlx::query_as::<_, CheckpointRow>(
            "SELECT * FROM checkpoints WHERE status = 'pending' ORDER BY created_at ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Atomically claims a pending checkpoint for resume by flipping
    /// `status` from `'pending'` to `'processing'` — only succeeds if it
    /// was still `'pending'` at that instant. This is what makes a
    /// duplicate human response, or a second concurrent resume of the same
    /// checkpoint, a safe no-op rather than a race: whichever request's
    /// `UPDATE` lands first wins the row, the other sees `rows_affected() ==
    /// 0` and must not touch the checkpoint any further. The SQLite
    /// equivalent of the reference's Postgres `SELECT ... FOR UPDATE` row
    /// lock (SQLite serializes writers itself, so a plain conditional
    /// `UPDATE` is enough here). Returns the claimed checkpoint (now
    /// `Processing`), or `None` if it was already claimed/resolved/denied/
    /// failed by a prior request. Callers must follow up with either
    /// `finalize` or `re_pause` — `Processing` is never a resting state.
    pub async fn claim_pending(
        &self,
        id: Uuid,
        human_response: &Value,
    ) -> anyhow::Result<Option<Checkpoint>> {
        let now = Utc::now().to_rfc3339();
        let result = sqlx::query(
            "UPDATE checkpoints SET status = 'processing', human_response = ?, updated_at = ?
             WHERE id = ? AND status = 'pending'",
        )
        .bind(serde_json::to_string(human_response)?)
        .bind(&now)
        .bind(id.to_string())
        .execute(&self.pool)
        .await?;

        if result.rows_affected() == 0 {
            return Ok(None);
        }
        self.get(id).await
    }

    /// An `Input` response merges supplied fields into the original call's
    /// arguments before resuming — persisted separately from the terminal
    /// status change so the merged arguments survive even if the resumed
    /// attempt pauses again (`re_pause`) rather than finishing.
    pub async fn update_arguments(&self, id: Uuid, tool_arguments: &Value) -> anyhow::Result<()> {
        sqlx::query("UPDATE checkpoints SET tool_arguments = ? WHERE id = ?")
            .bind(serde_json::to_string(tool_arguments)?)
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Records the terminal outcome of a resume: resolved (dispatch
    /// succeeded), denied (human declined), or failed (dispatch itself
    /// failed after resuming — a downstream error, not a HITL concern).
    /// Only ever called after `claim_pending` has already won the race for
    /// this checkpoint.
    pub async fn finalize(
        &self,
        id: Uuid,
        status: CheckpointStatus,
        result: Option<&Value>,
        error: Option<&str>,
    ) -> anyhow::Result<()> {
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "UPDATE checkpoints SET status = ?, result = ?, error = ?, updated_at = ? WHERE id = ?",
        )
        .bind(status.as_str())
        .bind(result.map(serde_json::to_string).transpose()?)
        .bind(error)
        .bind(&now)
        .bind(id.to_string())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Re-pauses a checkpoint that was retried on resume but produced
    /// another pause (e.g. `authenticate` was called but the token didn't
    /// actually work) — goes back to `pending` under the (possibly new)
    /// reason, so it can be responded to again.
    pub async fn re_pause(&self, id: Uuid, reason: &PauseReason) -> anyhow::Result<()> {
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "UPDATE checkpoints SET status = 'pending', reason = ?, resume_from = ?,
                human_response = NULL, updated_at = ? WHERE id = ?",
        )
        .bind(serde_json::to_string(reason)?)
        .bind(serde_json::to_string(&reason.resume_from())?.replace('"', ""))
        .bind(&now)
        .bind(id.to_string())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn find_by_call_id(&self, call_id: &str) -> anyhow::Result<Option<Checkpoint>> {
        let row = sqlx::query_as::<_, CheckpointRow>(
            "SELECT * FROM checkpoints WHERE call_id = ? ORDER BY created_at DESC LIMIT 1",
        )
        .bind(call_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Into::into))
    }
}

#[derive(sqlx::FromRow)]
struct CheckpointRow {
    id: String,
    call_id: String,
    user_id: String,
    agent_id: String,
    connector: String,
    tool_name: String,
    tool_arguments: String,
    reason: String,
    resume_from: String,
    status: String,
    human_response: Option<String>,
    result: Option<String>,
    error: Option<String>,
    created_at: String,
    updated_at: String,
    #[allow(dead_code)]
    expires_at: Option<String>,
}

impl From<CheckpointRow> for Checkpoint {
    fn from(r: CheckpointRow) -> Self {
        Checkpoint {
            id: Uuid::parse_str(&r.id).unwrap_or_else(|_| Uuid::nil()),
            call_id: r.call_id,
            user_id: r.user_id,
            agent_id: r.agent_id,
            connector: r.connector,
            tool_name: r.tool_name,
            tool_arguments: serde_json::from_str(&r.tool_arguments).unwrap_or(Value::Null),
            reason: serde_json::from_str(&r.reason).expect("stored PauseReason must deserialize"),
            resume_from: match r.resume_from.as_str() {
                "credential_check" => PipelineStep::CredentialCheck,
                "schema_check" => PipelineStep::SchemaCheck,
                _ => PipelineStep::Dispatch,
            },
            status: CheckpointStatus::parse(&r.status),
            human_response: r.human_response.and_then(|s| serde_json::from_str(&s).ok()),
            result: r.result.and_then(|s| serde_json::from_str(&s).ok()),
            error: r.error,
            created_at: DateTime::parse_from_rfc3339(&r.created_at)
                .map(|d| d.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now()),
            updated_at: DateTime::parse_from_rfc3339(&r.updated_at)
                .map(|d| d.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins down the exact resume semantics — the answer to "what happens
    /// if policy/auth/schema changes between pause and resume" for each
    /// pause reason. `ApprovalRequired` deliberately does *not* re-run
    /// `Decide`: the policy decision that produced this specific pause
    /// stands even if `policy.toml` changes before a human responds (the
    /// gateway's own `Block` backstop is the safety net for that specific
    /// case — see `gateway::protocol::handle_tools_call`, which re-checks
    /// `Block` unconditionally on every dispatch regardless of this
    /// checkpoint's history). `AuthRequired` and `InputRequired` both
    /// re-run the check that produced them, so neither a stale auth claim
    /// nor a partial/incomplete human response can slip straight through
    /// to `Dispatch`.
    #[test]
    fn resume_from_reflects_the_pipeline_step_right_after_the_one_that_paused() {
        assert_eq!(
            PauseReason::ApprovalRequired {
                summary: "x".into()
            }
            .resume_from(),
            PipelineStep::CredentialCheck,
        );
        assert_eq!(
            PauseReason::AuthRequired {
                connector: "github".into(),
                auth_url: None
            }
            .resume_from(),
            PipelineStep::CredentialCheck,
        );
        assert_eq!(
            PauseReason::InputRequired { missing: vec![] }.resume_from(),
            PipelineStep::SchemaCheck,
        );
    }
}

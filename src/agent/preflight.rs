//! The agent's own generic tool-call pre-flight — this is the piece that
//! used to be `pipeline.rs`, reincarnated on the agent side of the process
//! boundary. Same fixed, ordered sequence of checks as before, same
//! never-matches-on-a-tool-or-connector-name property, but every check now
//! asks the gateway a question over HTTP instead of reading gateway-owned
//! state directly:
//!
//! ```text
//! Decide (GET /policy/...)        -- Block --> Rejected (terminal, never a checkpoint)
//!      |
//!    Allow / Ask
//!      |-- Ask --> Paused(ApprovalRequired)
//!      v
//! CredentialCheck (GET /connectors/.../status)  -- not connected --> Paused(AuthRequired)
//!      v
//! SchemaCheck (tools/list + schema_validator)   -- missing required field --> Paused(InputRequired)
//!      v
//! Dispatch (POST /mcp tools/call)               --> Done(Success | Failed)
//! ```
//!
//! This is the one and only place HITL is decided in this POC. The gateway
//! never independently creates a checkpoint — see
//! `gateway::protocol::handle_tools_call`'s module docs for what little it
//! still enforces on its own (a hard `Block`, and a bypass guard on `Ask`).

use serde_json::Value;

use crate::agent::checkpoint::{pause_reason_for_outcome, PauseReason, PipelineStep, ToolOutcome};
use crate::agent::state::AgentState;
use crate::policy::Stance;

/// Identifies one task the agent has taken on — everything the pre-flight
/// checks need to know about *who* is calling *what*.
#[derive(Debug, Clone)]
pub struct TaskContext {
    pub call_id: String,
    pub user_id: String,
    pub agent_id: String,
    pub connector: String,
    pub tool_name: String,
}

/// What running (or resuming) the pre-flight loop produced.
#[derive(Debug)]
pub enum PreflightOutcome {
    /// `Decide` returned `Block`. Terminal — never checkpointed.
    Rejected { reason: String },
    /// Paused at some step; the caller persists this as a `Checkpoint`.
    Paused { reason: PauseReason },
    /// Ran all the way to `Dispatch`.
    Done(ToolOutcome),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Decide,
    CredentialCheck,
    SchemaCheck,
    Dispatch,
}

impl From<PipelineStep> for Step {
    fn from(s: PipelineStep) -> Self {
        match s {
            PipelineStep::CredentialCheck => Step::CredentialCheck,
            PipelineStep::SchemaCheck => Step::SchemaCheck,
            PipelineStep::Dispatch => Step::Dispatch,
        }
    }
}

/// Starts a brand-new task from the very first check.
pub async fn act(
    state: &AgentState,
    ctx: &TaskContext,
    arguments: &Value,
) -> anyhow::Result<PreflightOutcome> {
    execute(state, ctx, arguments, Step::Decide).await
}

/// Resumes a paused task from the step [`PauseReason::resume_from`]
/// determined when it paused. `arguments` may already reflect a human's
/// `Input` response (the caller merges before calling this).
pub async fn resume(
    state: &AgentState,
    ctx: &TaskContext,
    arguments: &Value,
    start: PipelineStep,
) -> anyhow::Result<PreflightOutcome> {
    execute(state, ctx, arguments, start.into()).await
}

async fn execute(
    state: &AgentState,
    ctx: &TaskContext,
    arguments: &Value,
    start: Step,
) -> anyhow::Result<PreflightOutcome> {
    let mut step = start;
    loop {
        step = match step {
            Step::Decide => match state.gateway.policy(&ctx.connector, &ctx.tool_name).await? {
                Stance::Block => {
                    return Ok(PreflightOutcome::Rejected {
                        reason: format!(
                            "Tool '{}' on connector '{}' is blocked by policy.",
                            ctx.tool_name, ctx.connector
                        ),
                    });
                }
                Stance::Ask => {
                    let outcome = ToolOutcome::ApprovalRequired {
                        summary: format!(
                            "agent '{}' wants to call '{}' on connector '{}'",
                            ctx.agent_id, ctx.tool_name, ctx.connector
                        ),
                    };
                    return Ok(PreflightOutcome::Paused {
                        reason: pause_reason_for_outcome(&outcome)
                            .expect("ApprovalRequired always pauses"),
                    });
                }
                Stance::Allow => Step::CredentialCheck,
            },

            Step::CredentialCheck => {
                let status = state
                    .gateway
                    .connector_status(&ctx.user_id, &ctx.connector)
                    .await?;
                if !status.connected {
                    let outcome = ToolOutcome::AuthRequired {
                        connector: ctx.connector.clone(),
                        auth_url: status.auth_url,
                    };
                    return Ok(PreflightOutcome::Paused {
                        reason: pause_reason_for_outcome(&outcome)
                            .expect("AuthRequired always pauses"),
                    });
                }
                Step::SchemaCheck
            }

            Step::SchemaCheck => {
                ensure_schema_cached(state, ctx).await?;
                let missing = state
                    .schema
                    .missing_required_fields(&ctx.connector, &ctx.tool_name, arguments)
                    .await;
                if !missing.is_empty() {
                    let outcome = ToolOutcome::InputRequired { missing };
                    return Ok(PreflightOutcome::Paused {
                        reason: pause_reason_for_outcome(&outcome)
                            .expect("InputRequired always pauses"),
                    });
                }
                Step::Dispatch
            }

            Step::Dispatch => {
                let outcome = match state
                    .gateway
                    .call_tool(
                        &ctx.user_id,
                        &ctx.agent_id,
                        &ctx.call_id,
                        &ctx.connector,
                        &ctx.tool_name,
                        arguments,
                    )
                    .await
                {
                    Ok(result) => ToolOutcome::Success(result),
                    Err(message) => ToolOutcome::Failed(message),
                };
                return Ok(PreflightOutcome::Done(outcome));
            }
        };
    }
}

/// Populates the agent's own schema cache from the gateway's aggregated
/// `tools/list` the first time any tool on `ctx.connector` needs one this
/// process hasn't seen yet. Mirrors how the old gateway-side pipeline
/// lazily cached schema from the connector directly — the only difference
/// is the source is now the gateway's namespaced list instead of a
/// connector's raw one.
async fn ensure_schema_cached(state: &AgentState, ctx: &TaskContext) -> anyhow::Result<()> {
    if state
        .schema
        .has_schema(&ctx.connector, &ctx.tool_name)
        .await
    {
        return Ok(());
    }
    let prefix = format!("{}__", ctx.connector);
    let tools = state
        .gateway
        .list_tools(&ctx.user_id, &ctx.agent_id)
        .await?;
    let unnamespaced: Vec<Value> = tools
        .into_iter()
        .filter_map(|mut tool| {
            let bare = tool
                .get("name")
                .and_then(|v| v.as_str())?
                .strip_prefix(&prefix)?
                .to_string();
            tool.as_object_mut()?
                .insert("name".to_string(), Value::String(bare));
            Some(tool)
        })
        .collect();
    state
        .schema
        .ingest_tools_list(&ctx.connector, &unnamespaced)
        .await;
    Ok(())
}

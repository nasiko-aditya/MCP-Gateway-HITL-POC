//! The agent side of the process boundary: the piece of this POC that
//! decides whether a tool call needs human intervention before the gateway
//! is ever asked to execute anything. See `preflight` for the decision
//! loop, `checkpoint` for the pause/resume model it persists to, and `hitl`
//! for the human-facing API that resumes a paused task.

pub mod checkpoint;
pub mod gateway_client;
pub mod hitl;
pub mod preflight;
pub mod routes;
pub mod state;

use std::sync::Arc;

use axum::Router;

pub use state::AgentState;

/// The agent's full router: task intake (`/agent/act`, `/agent/result/...`)
/// merged with the Human Action API (`/hitl/...`, `/audit/...`).
pub fn router(state: Arc<AgentState>) -> Router {
    routes::router(state.clone()).merge(hitl::routes::router(state))
}

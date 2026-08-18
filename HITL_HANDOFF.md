# Handoff — MCP Gateway HITL POC

Concise engineer-to-engineer handoff. For the full design rationale see
[`HITL_POC.md`](./HITL_POC.md).

## Implementation summary

A standalone Rust/axum project (single binary, one `cargo run`) that now boots **two logically
independent services** — an **agent** and a **gateway** — plus two mock downstream MCP servers and,
optionally, one real Composio-backed connector. The agent owns a generic pre-flight loop
(`Decide → CredentialCheck → SchemaCheck → Dispatch`) that decides whether a task needs human
intervention, persists that decision as a resumable `Checkpoint`, and only then calls the gateway
to actually dispatch. The gateway is a pure MCP execution/communication layer: it answers two
read-only advisory questions the agent needs (policy stance, connector status), executes
`tools/call`, and enforces exactly two hard, human-free backstops (`Block`, and refusing a
bypassed `Ask`) — it never independently creates a checkpoint. Covers all 5 outcomes
(`ALLOW`/`BLOCK`/`ApprovalRequired`/`InputRequired`/`AuthRequired`) generically across two
independent mock connectors (GitHub-shaped, Notion-shaped) and one real Composio connector, with a
Human Action API to approve/deny/supply-input/authenticate and resume. 49 tests (16 unit + 33
end-to-end), zero `cargo clippy` warnings.

**This is the second iteration.** The first iteration ran the whole decide/check/dispatch sequence
*inside* the gateway's `tools/call` handler, which made the gateway the thing deciding HITL was
needed. This iteration moves that decision, and the checkpoint it produces, entirely into a
separate agent process/router — see `HITL_POC.md` §4 for the exact before/after and why.

**This handoff reflects a third pass: live Composio verification + cleanup.** The Composio
connector is no longer just compiling and skipping cleanly without credentials — it has been run
against a real Composio account and a real GitHub repository, completing a real OAuth connection
and a real `GITHUB_CREATE_AN_ISSUE` write through the full agent → HITL → resume → gateway →
Composio chain. That pass also found and fixed one real bug (silent error-swallowing in the
gateway's connector-status endpoint) and one real script issue (the ask-gated write path in
`demo/demo_composio.sh` used to stop at `InputRequired` without completing it). See "This cleanup
pass" below for exactly what changed.

**This handoff reflects a fourth pass: a dedicated error-handling / edge-case review.** With the
mechanism itself working end to end, this pass asked what happens when something *besides* a HITL
condition goes wrong — an unreachable gateway/connector/Composio, a nonexistent tool or connector,
a malformed or partial HITL response, a stale or duplicate one — and fixed the two real gaps it
found (an unregistered connector could pause on a misleading `AuthRequired`; a malformed
`arguments` value could pause on a misleading `InputRequired`), plus one related resume-semantics
bug (a partial `input` response could dispatch an incomplete call). No architecture change, no new
features. See "This error-handling pass" below for exactly what changed.

## Repository structure

```
MCP-Gateway-HITL-POC/
├── Cargo.toml
├── policy.toml                       # allow/ask/block rules (no LLM) — POC substitute for
│                                      #   Nasiko's real per-agent UI/backend tool configuration
├── .env.example                      # Composio env vars, all optional
├── migrations/0001_init.sql          # checkpoints, credentials, audit_log — shared by both services
├── src/
│   ├── main.rs                       # boots gateway + agent (2 routers, 2 ports) + 2 mock
│   │                                  #   connectors + optional Composio bootstrap, all in one binary
│   ├── lib.rs, config.rs, db.rs, types.rs
│   ├── policy.rs                     # PermissionPolicy::decide — glob, block>ask>allow (gateway-owned data)
│   ├── credentials.rs                # CredentialStore — per-connector token + expiry (gateway-owned)
│   ├── schema_validator.rs           # required-field diff vs cached inputSchema (shared, pure, agent-used)
│   ├── provider.rs                   # outbound JSON-RPC client + ConnectorRegistry (gateway-owned)
│   ├── audit.rs                      # shared audit log, both services write to the same table
│   ├── state.rs                      # GatewayState — policy/credentials/registry/composio/audit
│   ├── gateway/
│   │   ├── protocol.rs               # POST /mcp dispatch: Block backstop, reject bypassed Ask, dispatch
│   │   ├── routes.rs                 # POST /mcp, GET /policy/.., GET/POST /connectors/..
│   │   └── composio.rs               # real Composio REST + MCP client, one ComposioSlot
│   ├── agent/
│   │   ├── state.rs                  # AgentState — checkpoints/schema-cache/audit/GatewayClient
│   │   ├── gateway_client.rs         # the agent's ONLY channel to the gateway (HTTP, no shared structs)
│   │   ├── preflight.rs              # the Decide->CredentialCheck->SchemaCheck->Dispatch loop (was pipeline.rs)
│   │   ├── checkpoint.rs             # Checkpoint / PauseReason / ToolOutcome / HumanAction (agent-owned now)
│   │   ├── routes.rs                 # POST /agent/act, GET /agent/result/{call_id}
│   │   └── hitl/routes.rs            # GET /hitl/pending, GET /hitl/{id}, POST /hitl/{id}/respond, GET /audit/{call_id}
│   └── mock_connectors/{github,notion}.rs
├── tests/hitl_flow.rs                 # 33 end-to-end scenarios, agent + gateway + 2 mocks, one shared in-memory DB
├── demo/demo.sh                       # curl-driven walkthrough of every outcome, through the agent
├── demo/demo_composio.sh              # same, against the live Composio connector; skips cleanly without credentials
├── README.md, HITL_POC.md, HITL_HANDOFF.md
```

## Important files/functions

| Where | What |
| --- | --- |
| `agent::preflight::execute` (`src/agent/preflight.rs`) | The entire HITL trigger mechanism — one function, four steps, no connector/tool-name branching. Fresh calls start at `Decide`; resumes start at `PauseReason::resume_from()`. Every step but `Dispatch` is answered by a gateway GET endpoint. |
| `PauseReason::resume_from`/`expected_action`/`question` (`src/agent/checkpoint.rs`) | The only place "what does resuming this kind of pause mean" lives. `AuthRequired` deliberately resumes back into `CredentialCheck` (not past it) so a tokenless Composio re-check actually re-verifies — see the doc comment there. |
| `CheckpointStore::claim_pending` (`src/agent/checkpoint.rs`) | Atomic `UPDATE ... WHERE status='pending'` — the entire duplicate-response / double-resume guard. |
| `agent::routes::render_outcome` | Turns a fresh `PreflightOutcome` into the task-intake API's response; persists a `Checkpoint` on pause. |
| `agent::hitl::routes::respond` / `finish_resume` | The Human Action API's core handler: validate action-vs-reason, claim, act, call `agent::preflight::resume`. |
| `gateway::protocol::handle_tools_call` | The gateway's *only* remaining decision logic: `Block` backstop, reject a bypassed `Ask` (`x-agent-preflight` header check), else dispatch. Never creates a `Checkpoint`. |
| `agent::gateway_client::GatewayClient` | The agent's only channel to the gateway — no gateway Rust type is ever imported agent-side. |
| `SchemaValidator::missing_required_fields` | `InputRequired` detection — reads the tool's own cached `inputSchema.required`, nothing tool-specific; shared module, now populated from the gateway's namespaced `tools/list` instead of a connector directly. |
| `gateway::composio::ComposioConnector` | Real Composio REST (`is_connected`, `initiate_link`, `get_or_create_mcp_server`, `generate_user_mcp_url`) + minimal MCP-over-HTTP client, ported from `HITL-POC/src/tools/composio/*`. |
| `gateway::routes::get_connector_status` | `GET /connectors/{connector}/status` — as of this cleanup pass, distinguishes "checked, genuinely not connected" (a normal 2xx fact the agent turns into `AuthRequired`) from "the check itself failed" (Composio API error or DB error, now a hard 4xx/5xx with the real message, never silently coerced to `connected: false`). |
| `agent::gateway_client::GatewayClient::connector_status` | The agent-side half of the same fix — treats a non-2xx response as `anyhow::Error`, which `preflight::execute`'s existing `?` on this call already propagates correctly up to `{"status":"error",...}` at `/agent/act`. No change was needed in `preflight.rs` or the route handlers; only this leaky boundary was fixed. |

## Architecture decisions

- **Two Axum routers on two ports, one binary** — the smallest change that makes "the gateway
  doesn't decide HITL" a real, HTTP-enforced boundary (no shared `PermissionPolicy`/
  `CredentialStore`/`CheckpointStore` Rust struct crosses the line) without inventing a second
  deployable or repo for a POC. See `HITL_POC.md` §4.
- **One shared SQLite file, not two** — at POC scale (single machine, one `cargo run`) there's no
  reason to split storage; SQLite serializes writers itself, and sharing it is what makes
  `GET /audit/{call_id}` show one coherent trail spanning both services under one `call_id`.
- **`AuthRequired` resumes back into `CredentialCheck`, not past it** — the one behavioral change
  from the first iteration's checkpoint model, needed to make a tokenless Composio "I completed
  OAuth out-of-band" claim actually get re-verified rather than trusted blindly. Applied
  unconditionally (both connector kinds), so the resume path stays connector-agnostic rather than
  branching on connector type.
- **`x-agent-preflight: passed` as a plaintext trust-boundary header, not a signed token** — a
  deliberate, documented POC simplification (see `HITL_POC.md` §14) standing in for what
  production would fold into the existing signed delegation JWT.
- **Composio modeled as just another `ConnectorRegistry`-shaped entry** — the gateway's dispatch
  code has one two-arm match (`is this the configured Composio connector name, or look it up in
  the registry`), never a per-tool special case. Tests never require live Composio credentials;
  `demo/demo_composio.sh` is the only thing that touches the real API.
- **Two enums, `ToolOutcome` and `PauseReason`** — mirrors `HITL-POC`'s
  `ToolOutcome`/`PauseReason` split rather than collapsing them, so the "what a call produced"
  vocabulary and the "why a checkpoint is dormant" vocabulary stay independently extensible.
- **Non-standard `{"status":"pending",...}` result, not an error** — a pause isn't a failure, and
  this is now scoped to the agent's own task-intake API rather than MCP's `tools/call` itself
  (which is fully synchronous again).
- **Credentials scoped per-connector, not per-user**, for the mock/local path — a deliberate
  simplification (see `HITL_POC.md` §14); flagged everywhere it appears in code comments. The
  Composio path is real, scoped per `user_id`.
- **A failed status check must never look like "not connected"** — added in this cleanup pass.
  `get_connector_status` and `GatewayClient::connector_status` used to collapse a genuine Composio
  API failure (bad permissions, network error) and the DB credential store failing into the same
  `connected: false` / `auth_url: null` response the agent also gets for a routine "no connection
  yet." That made a real outage indistinguishable from an ordinary `AuthRequired` pause — the agent
  would ask a human to authenticate a connector it never actually managed to check. Fixed by making
  the endpoint return a real HTTP error (502 for Composio, 500 for the local store) with the
  underlying message, and making the agent's HTTP client treat a non-2xx response as `Err` rather
  than parsing whatever body came back. No architectural change — `preflight::execute` and both
  route handlers already handled an `Err` from this call correctly; they just never received one.

## Tests

`cargo test` — 16 unit tests (`policy`, `credentials`, `schema_validator`, `agent::checkpoint`,
`gateway::composio`) + 33 end-to-end tests in `tests/hitl_flow.rs`, each spinning up the real
gateway, the real agent, and both real mock connectors on ephemeral ports with an isolated
in-memory DB shared by the two services. Covers: allow, block, ask→pending, approve→resume,
deny→terminal, input-required→pending, input→resume, auth-required→pending, authenticate→resume,
"never executed before human action," invalid-action rejection, duplicate-response rejection,
resumed-checkpoint-can't-resume-again, downstream dispatch failure after a successful resume,
genericity across both mock connectors, two tests proving HITL is triggered by the agent and never
independently by the gateway (`gateway_block_backstop_rejects_direct_call_without_agent`,
`gateway_rejects_ask_tool_reached_directly_without_agent_preflight`), one unit test proving
`CredentialStore::is_valid` surfaces a real backing-store failure as `Err` rather than `false`
(`credentials::tests::is_valid_surfaces_real_store_errors`), two end-to-end tests proving that
failure propagates all the way to a clear error at both the gateway's own endpoint and through the
agent (`connector_status_surfaces_real_store_failure_not_false`,
`credential_store_failure_surfaces_as_agent_error_not_auth_required`), plus the operational-failure
and HITL-state-edge-case suite added in the error-handling pass described below (gateway/downstream
connector unreachable, unknown connector/tool, malformed and omitted `arguments`, malformed JSON
body, partial `input` re-pauses, unverified `authenticate` re-pauses, every terminal checkpoint
status rejecting a further response, and `Block` winning over a claimed pre-flight-approved
header) and two Composio unit tests against a fake HTTP server proving an API failure is
distinguishable in *kind* from a genuine "no connected account" fact.

## This cleanup pass

Scope: fix the one real error-handling bug and the one real script issue found while verifying
the live Composio integration end-to-end; no architecture change, no new features, no change to
`nasiko-cloud-rs` or `HITL-POC`.

**Files changed:**

| File | Change |
| --- | --- |
| `src/gateway/routes.rs` | `get_connector_status` now returns `Result<Json<Value>, (StatusCode, Json<Value>)>` instead of always `Json<Value>` — a real Composio or credential-store failure becomes a 502/500 with the underlying message (and a `tracing::error!` log line), instead of being silently coerced into `connected: false, auth_url: null`. |
| `src/agent/gateway_client.rs` | `GatewayClient::connector_status` now checks the HTTP status of the gateway's response; a non-2xx body is turned into an `anyhow::Error` carrying the gateway's message instead of being parsed as if it were a normal "not connected" fact. |
| `src/credentials.rs` | Added `is_valid_surfaces_real_store_errors`, a unit test proving a genuine DB failure (closed pool) surfaces as `Err`, not `false` — the property the routes-level fix now relies on. |
| `tests/hitl_flow.rs` | Added `connector_status_direct` test helper plus two end-to-end tests exercising the fix through the real gateway endpoint and through `/agent/act`. |
| `demo/demo_composio.sh` | The ask-gated write path used to stop at `InputRequired` (missing `owner`/`repo`/`title`, which no generic default can supply) and then print a "demo complete" message that could be misread as a completed write. Now: honors optional `COMPOSIO_ASK_OWNER`/`COMPOSIO_ASK_REPO`/`COMPOSIO_ASK_TITLE` to actually supply the input and complete a real dispatch when set, and prints an honest "no write was dispatched, here's exactly why and what to set" message when they aren't. Also hardened the `AuthRequired` OAuth-wait step to detect a non-interactive stdin (`[[ -t 0 ]]`) and exit with a clear instruction instead of either hanging or aborting silently on `read`'s EOF. |
| `DEMO_GUIDE.md` | Rewritten as a demo runbook: setup, architecture diagram, all six demos (including the real Composio one with a verified real result), the gateway backstop, audit inspection, cleanup, and demo notes. |
| `HITL_HANDOFF.md` | This file — updated test counts, this section, current status, known issues, and production considerations. |

**Real Composio result obtained during this pass:** a real GitHub issue created through the full
chain (`/agent/act` → `ApprovalRequired` → approve → `InputRequired` → supply `owner`/`repo`/
`title` → resume → gateway → real Composio MCP server → real GitHub API), confirmed by a real
`html_url`, a real issue `number`, `mercury_last_http_status_code: 201`, and a real Composio
`log_id` — e.g. `https://github.com/nasiko-aditya/HITL-MCP-POC/issues/3`. The read path
(`GITHUB_LIST_REPOSITORIES_FOR_THE_AUTHENTICATED_USER`) and the `AuthRequired` pause with a real
`https://connect.composio.dev/link/...` OAuth URL were verified the same way.

**Two real Composio account gotchas found and worked around (not a POC bug, but worth recording
for whoever sets this up next):** a freshly created Composio API key may be scoped without write
access to the `sessions` permission (blocks `POST /api/v3/mcp/servers`, i.e. the connector's own
startup bootstrap) and/or the `connected_accounts` permission (blocks `POST
/api/v3/connected_accounts/link`, i.e. issuing a real OAuth URL) — both return a `403
APIKey_InsufficientPermissions` with a `suggested_fix` naming the exact permission to grant. Grant
both (or use a full-access key) before expecting `cargo run` to bootstrap successfully or
`AuthRequired` to produce a real `auth_url`. Also: since `Config::from_env()` reads `.env` once at
process startup via `dotenvy`, updating `.env` (including rotating `COMPOSIO_API_KEY` or having its
permissions changed) requires restarting `cargo run` to take effect — a running process keeps
whatever it loaded at boot.

## This error-handling pass

Scope: a final error-handling / edge-case pass over `MCP-Gateway-HITL-POC` only — verify how a
failure at any layer (gateway, downstream connector/Composio, malformed request, stale/duplicate
HITL response) is classified and surfaced, fix what wasn't correctly distinguished from a genuine
HITL condition, and add deterministic tests for it. No architecture change; no change to
`nasiko-cloud-rs` or `HITL-POC`.

**Bugs found and fixed:**

| File | Bug | Fix |
| --- | --- | --- |
| `src/gateway/routes.rs` (`get_connector_status`) | An unregistered/never-configured connector name was checked against the local `CredentialStore` exactly like a real one, which has no row for it and answers "not connected" — the agent paused on a misleading `AuthRequired` for a connector that was never wired up at all. | Reject with `404 unknown connector '...'` (via the already-existing `gateway::protocol::connector_known`, now `pub(crate)`) before consulting any credential store or Composio. |
| `src/agent/routes.rs` (`act`) | A non-object `arguments` value (a string, number, array) was indistinguishable from "every required field is missing" in `SchemaValidator::missing_required_fields`, producing a misleading `InputRequired` pause instead of a validation error. Separately, an omitted `arguments` field defaulted to `Value::Null`, which — if the call paused — could never actually complete an `input` resume (merging into a stored `Null` has nothing to merge into). | Added `normalize_arguments`: non-null, non-object is rejected as a clean `{"status":"error",...}` validation error; `Null` (omitted) is normalized to `{}`. |
| `src/agent/checkpoint.rs` (`PauseReason::resume_from`) | An `InputRequired` checkpoint resumed straight at `Dispatch`, so a partial `input` response (a human supplying only some of the missing fields) dispatched an incomplete call instead of being caught. | `InputRequired` now resumes at `SchemaCheck` (re-runs the diff against the merged arguments), mirroring the pattern already used for `AuthRequired`. |

**Not a bug, verified and documented instead:** whether `ApprovalRequired`'s resume re-evaluates
policy. It doesn't — `resume_from` still points at `CredentialCheck`, not `Decide`, so an approved
call's original `ask` decision stands even if `policy.toml` changes before the human responds. This
is intentional and unchanged; what *is* still re-enforced regardless is the gateway's own
unconditional `Block` backstop on every dispatch. See `HITL_POC.md` §16 and README.md §12 for the
full reasoning, and `gateway_block_backstop_wins_even_with_preflight_approved_header` for the test
proving the `Block` half.

**Error message clarity:** `agent::gateway_client::GatewayClient`'s `list_tools`/`policy`/
`connector_status`/`store_credential` now all wrap a pure connection failure as `"gateway
unreachable while ..."` (previously a bare, harder-to-read `reqwest::Error` via `?`), matching the
style `call_tool` already used. No message anywhere echoes a credential token or API key (verified
by `gateway::composio::tests::is_connected_surfaces_api_failure_distinctly_from_not_connected`).

**Tests added:** 12 new end-to-end tests in `tests/hitl_flow.rs` (gateway unreachable, downstream
connector unreachable, unknown connector, unknown tool, non-object `arguments`, omitted
`arguments`, malformed JSON body, partial `input` re-pause, unverified `authenticate` re-pause,
denied/failed checkpoints rejecting a further response, `Block` backstop with a claimed
pre-flight-approved header), 2 new unit tests in `gateway::composio` (API-failure-vs-not-connected
distinction, against a fake in-process HTTP server — no live Composio credentials needed), and 1
new unit test in `agent::checkpoint` pinning down the `resume_from` mapping for all three pause
reasons. Test count: 34 → 49 (13 unit → 16 unit, 21 end-to-end → 33 end-to-end).

**Manually verified against the real compiled binary** (not just `cargo test`): stopped/never-
started services can't be isolated from a running `cargo run` without killing the whole process
(gateway, agent, and both mocks are `tokio::spawn`ed inside one binary — see `src/main.rs`), so the
truly network-level scenarios (gateway down, connector down) are covered by the automated tests
above (which spin up services on independently controllable ports) rather than by killing a live
process. What *is* directly checkable against the real binary was: the full `./demo/demo.sh` happy
path; a live nonexistent-tool call (`{"status":"failed","error":"unknown tool '...'"}`); a live
nonexistent-connector call (`{"status":"error","error":"...unknown connector '...'"}`, not
`auth_required`); a live non-object-`arguments` call (`{"status":"error","error":"'arguments' must
be a JSON object, got string"}`); `POST /hitl/{fake-id}/respond` (`404`); and, with the real
Composio credentials this environment already has configured, an invalid-API-key restart (fails
fast at startup with Composio's own `401 Invalid API key` message — no stray process left running,
`.env` restored immediately afterward) plus a safe, read-only `GET /connectors/composio_github/
status` against the real account (`connected: true`, no write or destructive action taken).

**Final verified behavior, one line each** (full detail + real captured output: `README.md`
§21.1, `DEMO_GUIDE.md` "Error handling / failure scenarios"):

- Gateway unreachable → `{"status":"error",...}`, no checkpoint.
- Downstream connector unreachable → `{"status":"failed",...}`, never `AuthRequired`.
- Composio API failure → a hard error distinct in kind from a genuine "not connected" fact.
- Unknown tool → `{"status":"failed","error":"unknown tool '...'"}`, no checkpoint.
- Unknown connector → `{"status":"error","error":"...unknown connector..."}`, never `AuthRequired`.
- Malformed `arguments` → a validation error, never `InputRequired`.
- Partial `input` response → re-checks schema, re-pauses `InputRequired` on what's still missing.
- Invalid action for a checkpoint's pause reason → `400`, checkpoint stays `pending`.
- Duplicate/concurrent response → exactly one `200`, the other `409`.
- Response to a `denied`/`resolved`/`failed` checkpoint → `409`, never re-executes.
- Approved tool that fails downstream → checkpoint finalizes `failed`, never a false success.
- `authenticate` with no real credential behind it → re-pauses `AuthRequired`, never trusts the claim.
- Policy change between pause and resume → not re-evaluated for `ApprovalRequired` (approve resumes
  at `CredentialCheck`, not `Decide`); `Block` alone is re-enforced unconditionally at every
  dispatch regardless of this history.

## Commands

```sh
cargo fmt --check
cargo build
cargo test
cargo clippy --all-targets   # zero warnings
cargo run                    # boots agent :8090, gateway :8080, mock github :8081, mock notion :8082
./demo/demo.sh               # full walkthrough against a running instance, through the agent
./demo/demo_composio.sh      # same, against a live Composio connector (skips cleanly without credentials)

# to see a real Composio write actually execute (not just pause at InputRequired):
COMPOSIO_ASK_OWNER=<your-github-username> \
COMPOSIO_ASK_REPO=<a-repo-you-own-with-issues-enabled> \
./demo/demo_composio.sh
```

## Current status

Complete and passing, now including live verification, not just mocks: all 49 tests green, zero
clippy warnings, `cargo fmt --check` clean. `demo/demo.sh` verified end-to-end against the real
running agent + gateway (not just `cargo test`'s in-process harness), including both backstop
checks confirming `GET /hitl/pending` is unaffected by a direct gateway bypass attempt.
`demo/demo_composio.sh` has now actually been run against a live Composio account with a real
GitHub connection — the read tool, the `AuthRequired` pause with a real OAuth `auth_url`, the
`ApprovalRequired`/`InputRequired` pauses against Composio's real live tool schema, and a real
`GITHUB_CREATE_AN_ISSUE` dispatch were all verified with real responses from Composio and GitHub,
not mocked or assumed. See "This cleanup pass" above for the exact result and `DEMO_GUIDE.md` for
the full walkthrough.

## Known limitations

- All four sub-servers (gateway, agent, both mock connectors) run as `tokio::spawn`ed tasks inside
  one `cargo run` process (`src/main.rs`) — there's no way to stop just one of them from outside a
  running instance without killing all of them. The operational-failure scenarios that would
  otherwise need that (gateway down, one connector down while the rest of the system stays up) are
  instead covered deterministically by `tests/hitl_flow.rs`, which spins up each service as its own
  axum app on an independently controllable port — see "This error-handling pass" above.
- No checkpoint-expiry sweep is implemented (`expires_at`/`'expired'` exist in the schema but
  nothing sets or acts on them yet) — see `HITL_POC.md` §14.
- `demo/demo.sh`'s JSON field extraction falls back to a fragile `python3 -c eval(...)` path when
  `jq` isn't installed; install `jq` for reliable output. (`demo/demo_composio.sh` shares this same
  fallback.)
- The audit log has no pagination — fine at POC scale, would need one before any real volume.
- The Composio REST calls in `gateway/composio.rs` still target the `v3` API
  (`https://backend.composio.dev/api/v3/...`), which Composio's own docs mark `mcp/servers/generate`
  as "deprecated but functional" in favor of a `v3.1` base URL. Left alone deliberately — v3 still
  works (verified live in this pass), and this cleanup pass's brief was explicitly not to chase a
  newer API. Worth a look before this POC's lifetime outlasts v3's actual sunset.
- `x-agent-preflight: passed` remains a plaintext, unsigned trust-boundary header (see
  `HITL_POC.md` §14) — unchanged by this pass, still the right POC simplification, still not
  production-safe as-is.
- Credentials remain scoped per-connector (not per-`(user, connector)`) for the mock/local path
  only — the Composio path was already per-`user_id` and remains so; unchanged by this pass.

## Production considerations

Beyond what `HITL_POC.md` §15 already covers (the agent gaining its own pre-flight step against a
real gateway that has the two advisory endpoints, `policy.toml`'s per-agent-config production
analog, folding `x-agent-preflight` into a signed delegation JWT, persisting `inputSchema`, and
deciding what "resume the caller" means once MCP is in the real orchestrator's tool loop) — this
pass adds one more concrete lesson: **a production connector-status check must always distinguish
"checked, not connected" from "the check failed,"** exactly as fixed here. In a real deployment
this matters even more than in a demo — a flaky upstream (Composio, or any real OAuth provider)
returning a transient 5xx must never look identical to "this user hasn't authenticated yet," or an
operator debugging a real outage will be pointed at exactly the wrong place (send affected users
an auth prompt instead of noticing the connector is down). The fix applied here — treat a non-2xx
status as a hard error at the HTTP boundary, never infer a business fact from a failed call — is
directly portable to the real gateway's connector-status logic as-is.

## Remaining TODOs (not required by the brief, listed for completeness)

- Checkpoint-expiry sweep (port `HITL-POC`'s `sweep_due_checkpoints` pattern).
- Fold `x-agent-preflight` into a real signed delegation token instead of a plaintext header.
- Per-`(user, connector)` credential scoping instead of per-connector, for the mock/local path.
- Pagination on `GET /hitl/pending` and `GET /audit/{call_id}`.
- Revisit `gateway/composio.rs`'s `v3` API usage before Composio actually sunsets it (currently
  "deprecated but functional," not yet removed).

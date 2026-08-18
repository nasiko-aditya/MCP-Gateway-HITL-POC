# MCP Gateway HITL POC

A standalone, independently runnable proof-of-concept showing **Human-in-the-Loop (HITL) owned by
the agent, not the MCP gateway** — with a mechanism proven twice: once generically across two mock
connectors, and once for real against a live Composio-backed GitHub connector (real OAuth, real
connected account, a real GitHub issue created through the full chain).

This repo does not modify, and does not depend on being able to run, `nasiko-cloud-rs/` or
`HITL-POC/`. It is not integrated into either — see [Production mapping](#22-production-mapping--future-nasiko-integration).

## 1. What this is

| | |
| --- | --- |
| **Stack** | Rust, Axum, SQLite (`sqlx`), Tokio |
| **Run as** | One binary, one `cargo run`, two HTTP services (agent + gateway) + two mock downstream MCP servers + one optional real Composio connector |
| **Tests** | 49 (16 unit + 33 end-to-end), zero `cargo clippy` warnings |
| **Verified live** | Real Composio OAuth, real connected GitHub account, real `GITHUB_CREATE_AN_ISSUE` dispatch |

## 2. Why this exists

Nasiko's real MCP gateway today has three tool-call outcomes: `ALLOW` (execute), `BLOCK` (reject),
and `ASK` — which is *also* a reject. There is no way to approve one specific call, no persisted
memory of why a call was rejected, and a `FlowEvent::ToolApprovalRequired` is published to a
broadcast channel with zero subscribers anywhere in the real codebase (see
[`HITL_INVESTIGATION.md`](./HITL_INVESTIGATION.md), the investigation this POC implements).

This POC turns `ASK` into a real **pause**: a paused call is persisted as a resumable `Checkpoint`,
a human resolves it through a small HTTP API, and the *same* call then resumes to actual dispatch.
Critically, it also answers a second question the brief raised: *who decides a pause is needed* —
and lands firmly on **the agent**, not the gateway. An earlier iteration of this POC put that
decision inside the gateway's `tools/call` handler; that's the exact pattern this version removes.

## 3. What this POC demonstrates

- All five tool-call outcomes — `ALLOW`, `BLOCK`, `ApprovalRequired`, `InputRequired`,
  `AuthRequired` — generically, with zero connector-specific branching in the decision code.
- The trigger for HITL lives entirely in the **agent's own pre-flight loop**; the gateway never
  independently creates a checkpoint (two tests exist specifically to prove this).
- A **Checkpoint** + **Human Action API** (`approve` / `deny` / `input` / `authenticate`) that
  resumes a paused call exactly where it stopped — no in-memory state, a database row is enough.
- The same mechanism proven against **two independent mock connectors** (GitHub-shaped,
  Notion-shaped) *and* **one real Composio-backed GitHub connector** — real OAuth, a real
  connected account, and a real GitHub issue created through the full pause/resume chain.
- Zero LLM involvement anywhere a pause/resume decision is made — see [§7](#7-agent-side-hitl-pre-flight-flow).

## 4. Final architecture

```
User
  │
  ▼
Agent  (axum, :8090 — POST /agent/act)
  │
  ▼
Agent-side pre-flight
  ├── Policy         (Decide: GET /policy/{connector}/{tool} → allow | ask | block)
  ├── Credential check (CredentialCheck: GET /connectors/{connector}/status)
  └── Schema check     (SchemaCheck: tools/list inputSchema vs supplied arguments)
  │
  ▼
HITL if required  (ApprovalRequired | AuthRequired | InputRequired — persisted as a Checkpoint)
  │
  ▼
Human response  (POST /hitl/{id}/respond — approve / deny / input / authenticate)
  │
  ▼
Agent resumes  (re-enters the pre-flight loop at the step right after the one that paused)
  │
  ▼
MCP Gateway  (axum, :8080 — POST /mcp tools/call)
  │
  ▼
Composio / MCP tool  (two mock connectors, or the real Composio-backed GitHub connector)
```

Four invariants this architecture enforces, all independently tested:

1. **HITL is triggered and owned by the agent.** `agent::preflight::execute` is the one and only
   place a pause is decided.
2. **The agent owns the checkpoint and the Human Action API.** `Checkpoint`, `PauseReason`,
   `CheckpointStore`, and every `/hitl/*` route live under `src/agent/`.
3. **The MCP Gateway remains the execution/communication layer.** It exposes the read-only advisory endpoints the agent needs, performs MCP `tools/call` dispatch, and enforces the `Block/Ask-bypass` backstops. It does not own HITL checkpoints or human interaction.
4. **The gateway never independently creates a HITL checkpoint.** It still hard-enforces `BLOCK`
   (defense in depth) and rejects a bypassed `ASK` with a distinct error telling the caller to go
   through the agent — but neither of those pauses anything or asks a human.

## 5. Repository / code structure

```
MCP-Gateway-HITL-POC/
├── Cargo.toml
├── policy.toml                    # allow/ask/block rules — POC stand-in for Nasiko's real per-agent config
├── .env.example                   # Composio env vars, all optional
├── migrations/0001_init.sql       # checkpoints, credentials, audit_log
├── src/
│   ├── main.rs                    # boots gateway + agent (2 routers, 2 ports) + 2 mocks + optional Composio
│   ├── lib.rs, config.rs, db.rs, types.rs
│   ├── policy.rs                  # PermissionPolicy::decide — glob match, block > ask > allow
│   ├── credentials.rs             # CredentialStore — per-connector token + expiry (mock/local path)
│   ├── schema_validator.rs        # required-field diff vs cached inputSchema
│   ├── provider.rs                # outbound JSON-RPC client + ConnectorRegistry
│   ├── audit.rs                   # shared audit log — both services write to the same table
│   ├── state.rs                   # GatewayState — policy/credentials/registry/composio/audit
│   ├── gateway/
│   │   ├── protocol.rs            # POST /mcp dispatch: Block backstop, reject bypassed Ask, dispatch
│   │   ├── routes.rs              # POST /mcp, GET /policy/.., GET/POST /connectors/..
│   │   └── composio.rs            # real Composio REST + MCP client
│   ├── agent/
│   │   ├── state.rs               # AgentState — checkpoints/schema-cache/audit/GatewayClient
│   │   ├── gateway_client.rs      # the agent's ONLY channel to the gateway (HTTP only)
│   │   ├── preflight.rs           # Decide → CredentialCheck → SchemaCheck → Dispatch
│   │   ├── checkpoint.rs          # Checkpoint / PauseReason / ToolOutcome / HumanAction
│   │   ├── routes.rs              # POST /agent/act, GET /agent/result/{call_id}
│   │   └── hitl/routes.rs         # GET /hitl/pending, GET /hitl/{id}, POST /hitl/{id}/respond, GET /audit/{call_id}
│   └── mock_connectors/{github,notion}.rs
├── tests/hitl_flow.rs              # 33 end-to-end scenarios
├── demo/demo.sh                    # walkthrough against the mocks
├── demo/demo_composio.sh           # same, against the live Composio connector
└── HITL_POC.md, HITL_HANDOFF.md, DEMO_GUIDE.md, HITL_INVESTIGATION.md
```

## 6. Core components and responsibilities

| Component | Owns | Never does |
| --- | --- | --- |
| `agent::preflight` | The four-step decision loop; the only place a pause is decided | Talk to a downstream connector directly, or import a gateway Rust type |
| `agent::checkpoint` | `Checkpoint`, `PauseReason`, `ToolOutcome`, `HumanAction`, the pause/resume state machine | Know what a connector's tool actually does |
| `agent::gateway_client::GatewayClient` | The agent's one HTTP channel to the gateway | Share a Rust struct with gateway code |
| `gateway::protocol` | `tools/list`/`tools/call` dispatch, the `Block` backstop, the `Ask`-bypass guard | Create a `Checkpoint`, ask a human anything |
| `gateway::routes` | `GET /policy/...`, `GET /connectors/.../status` — advisory, read-only | Decide whether HITL is needed |
| `policy.rs` | `PermissionPolicy::decide` — glob match, `block > ask > allow`, default `allow` | Call an LLM, know a connector's name in its logic |
| `credentials.rs` | `CredentialStore` — mock/local connector token + expiry | Anything Composio-related |
| `schema_validator.rs` | Cached `inputSchema` per `(connector, tool)`, `required`-field diff | Know a tool's field names in advance — reads them from the schema |
| `gateway::composio` | Real Composio REST client + minimal MCP-over-HTTP client | Get called from tests (only `demo/demo_composio.sh` touches it) |
| `provider.rs` | `ConnectorRegistry` + generic outbound JSON-RPC client for the mocks | Branch on which mock connector it's talking to |
| `audit.rs` | Append-only `audit_log`, shared by both services | Store a raw credential token |

## 7. Agent-side HITL pre-flight flow

`agent::preflight::execute` (`src/agent/preflight.rs`) is a fixed, four-step loop that never
branches on a connector or tool name:

```
Decide            → GET /policy/{connector}/{tool}
   │ Block  → Rejected (terminal, no checkpoint — not a HITL concern, no human involved either way)
   │ Ask    → Paused(ApprovalRequired)
   │ Allow  ↓
CredentialCheck   → GET /connectors/{connector}/status
   │ not connected → Paused(AuthRequired)
   ↓
SchemaCheck       → tools/list (cached) + schema_validator
   │ missing required field → Paused(InputRequired)
   ↓
Dispatch          → POST /mcp tools/call (the one step that crosses the process boundary)
   → Done(Success | Failed)
```

A fresh task starts at `Decide`. A resumed task starts at `PauseReason::resume_from()` — whichever
step comes right after the one that paused it (see [§12](#12-checkpoint--pauseresume-model)).
Nothing in this loop has a concept of "fresh" vs. "resumed"; it's the same function either way.

**No LLM is involved anywhere in this loop.** All three triggers below read deterministic data —
a policy table row, a connector's connection status, a JSON Schema's `required` array — never a
model's output. This matters specifically because a human approval gate must not be something a
model can be talked out of.

## 8. ALLOW / BLOCK / ASK policy behavior

`policy.rs`'s `PermissionPolicy::decide(connector, tool_name)` glob-matches `policy.toml` rules
(case-insensitive, `*`/`?` wildcards) with priority **`block > ask > allow`**; a tool matched by no
rule defaults to **`allow`**:

```toml
[[rules]]
connector = "github"
pattern = "delete_repo"
stance = "ask"
```

`policy.toml` is a **POC stand-in for what would really be per-agent MCP tool configuration set
through Nasiko's own UI/backend** (Allow/Ask/Block per connector, per agent). This POC reads that
shape of data from a file instead of a database-backed admin UI because building that UI isn't the
point of the exercise — production reads the same shape of data from wherever Nasiko already
stores it; nothing about the decision logic changes.

- **ALLOW** clears `Decide` and continues to `CredentialCheck` — it is *not* an unconditional
  execute; a connector still not connected pauses on `AuthRequired` regardless of policy stance.
- **BLOCK** is a hard, terminal rejection at the very first step — never checkpointed, never
  dispatched, and enforced a second time by the gateway itself as defense in depth
  ([§13](#13-mcp-gateway-responsibilities-and-backstop-behavior)).
- **ASK** → see [§11](#11-approvalrequired).

## 9. InputRequired

**Mechanism: the tool's own JSON Schema, not an LLM.** Every MCP `tools/list` response carries each
tool's `inputSchema`, including a `required` array. `schema_validator.rs`'s `SchemaValidator`
caches that schema per `(connector, tool_name)` — populated lazily from the gateway's namespaced
`tools/list` the first time a tool is called — and diffs `required` against the keys present in
`arguments`. A missing key becomes a `MissingField { name, field_type, description }`, read
straight from the schema's own `properties`, so the human-facing question is legible without any
tool-specific string ever hardcoded in this repo. Resuming an `InputRequired` checkpoint merges the
human's supplied fields into the original arguments and continues straight to `Dispatch`.

This is the exact mechanism verified live against Composio's real GitHub schema in
[§15](#15-real-end-to-end-example) — the `owner`/`repo`/`title` fields it asks for come from
GitHub's actual API schema via Composio, not from anything written in this repo.

## 10. AuthRequired

**Mechanism: connector connection status, not an LLM — generic across two genuinely different
connector kinds.** `CredentialCheck` asks `GET /connectors/{connector}/status`; the gateway answers
one of two ways depending on *connector type*, never a per-connector-name branch:

- **Mock/local connector** (`credentials.rs`): does a row exist in the `credentials` table, and if
  it has an expiry, has it passed? `{"action":"authenticate","token":"..."}` is this POC's
  deliberately simplified stand-in for a real OAuth exchange.
- **Composio connector** (`gateway/composio.rs`): real connected-account status
  (`GET /api/v3/connected_accounts`), and a real OAuth `auth_url`
  (`POST /api/v3/connected_accounts/link`) when not connected. There is no token to submit — the
  human completes OAuth out-of-band in a browser, and `{"action":"authenticate"}` (token omitted)
  means "re-check now."

`PauseReason::AuthRequired::resume_from()` points back at `CredentialCheck` (not past it) — a
tokenless Composio "I completed OAuth" claim is always re-verified against the real connection
status, never trusted blindly. This is applied unconditionally to both connector kinds, so the
resume path never branches on connector type.

## 11. ApprovalRequired

**Mechanism: the deterministic policy from [§8](#8-allow--block--ask-policy-behavior), unchanged
in its own logic — only who consumes the answer changed.** `GET /policy/{connector}/{tool}` just
answers the stance; the *agent*'s `Decide` step is what turns an `ask` answer into
`PauseReason::ApprovalRequired{summary}`. Resuming on `approve` continues to `CredentialCheck`
(not straight to dispatch — see [§12](#12-checkpoint--pauseresume-model) for why); `deny` is
terminal and universal (valid against any pending checkpoint regardless of its actual pause
reason).

## 12. Checkpoint + pause/resume model

A `Checkpoint` (`src/agent/checkpoint.rs`, table `checkpoints` in
`migrations/0001_init.sql`) stores everything needed to resume a paused call with **no in-memory
state**: `id`, `call_id`, `user_id`, `agent_id`, `connector`, `tool_name`, `tool_arguments`,
`reason` (the tagged `PauseReason` JSON), `resume_from`, `status`, `human_response`, `result`,
`error`, timestamps.

| `status` | Meaning |
| --- | --- |
| `pending` | Waiting on a human |
| `processing` | Transient — claimed by `claim_pending`, mid-resume |
| `denied` | Terminal — human declined, never dispatched |
| `resolved` | Terminal — resumed and the downstream call succeeded |
| `failed` | Terminal — resumed but the downstream dispatch itself failed |
| `expired` | Reserved in the schema; nothing currently sweeps into it — see [§23](#23-limitations) |

`PauseReason::resume_from()` is the single place that knows what resuming each pause reason means:

| Paused at | `resume_from` | Why |
| --- | --- | --- |
| `ApprovalRequired` | `CredentialCheck` | Approving doesn't skip the remaining checks — a not-yet-connected connector still pauses again on `AuthRequired` |
| `AuthRequired` | `CredentialCheck` (itself) | Re-verifies the real connection rather than trusting the human's claim |
| `InputRequired` | `SchemaCheck` (itself) | Re-validates the merged arguments before dispatch — an `input` response isn't guaranteed to have supplied every field that was missing (a human can submit a partial `fields` object), so this re-runs the same diff rather than trusting the response and dispatching straight away |

**What "approve" does *not* re-check:** resuming an `ApprovalRequired` checkpoint on `approve` does not re-run `Decide` — the policy decision that produced this specific pause stands for this specific call, even if `policy.toml` changes before a human responds. This is a deliberate, documented POC semantic (not re-evaluating policy on every resume), not an oversight: **`Block` is still re-enforced unconditionally on every dispatch** by the gateway's own backstop (`gateway::protocol::handle_tools_call`, [§13](#13-mcp-gateway-responsibilities-and-backstop-behavior)) regardless of the agent's own decision history, so a tool reclassified to `Block` between pause and resume still can't execute — only an `Ask`→`Ask` (different reason) or `Ask`→`Allow` change in the interim goes undetected. `tests/hitl_flow.rs`'s `gateway_block_backstop_wins_even_with_preflight_approved_header` proves the `Block` half of this directly. For a POC this is an acceptable trade-off (re-running `Decide` on every resume would mean an approval could be silently downgraded to a *different* pending reason without the human who approved ever finding out); a production version should decide deliberately rather than inherit this default.

A paused task does **not** hold the HTTP connection open — `POST /agent/act` returns immediately
with `{"status":"pending","call_id",...,"checkpoint_id","reason","question"}`. Resuming is a
brand-new `POST /hitl/{id}/respond` request that reloads the checkpoint from the database and
re-enters `agent::preflight::execute` at `checkpoint.resume_from`.

## 13. MCP Gateway responsibilities and backstop behavior

`gateway::protocol::handle_tools_call` (`src/gateway/protocol.rs`) is deliberately thin and
deliberately not where HITL is decided. Every outcome is synchronous — the gateway never returns a
`"status":"pending"` result. It enforces exactly two hard, human-free checks before dispatch:

1. **`Block`** — always enforced here too, as defense in depth. Not a HITL decision (no human is
   ever asked either way), so refusing it here doesn't make the gateway a HITL orchestrator.
   Returns JSON-RPC error `-32000` (`codes::TOOL_BLOCKED`).
2. **`Ask` reached without `x-agent-preflight: passed`** — refused with a distinct error
   (`-32001`, `codes::TOOL_ASK`) telling the caller to route through the agent instead. This is a
   **bypass guard**, not the gateway independently deciding to pause: it never creates a
   `Checkpoint`, never asks a human, never runs a credential/schema check.

`x-agent-preflight: passed` is the header the agent's `GatewayClient` sets on every dispatch it has
already pre-flighted — a **plaintext POC convention, not a signed security boundary** (see
[§23](#23-limitations)). Two tests in `tests/hitl_flow.rs`
(`gateway_block_backstop_rejects_direct_call_without_agent`,
`gateway_rejects_ask_tool_reached_directly_without_agent_preflight`) call the gateway directly and
assert `GET /hitl/pending` is completely unaffected — proof the gateway never creates a checkpoint
on its own.

## 14. Composio integration

`gateway/composio.rs` speaks Composio's `v3` REST API directly (no SDK dependency) — the same
`ComposioConnector`/`ComposioRestClient` shape ported from `HITL-POC/src/tools/composio/*` (a
read-only reference, not modified):

| Call | Endpoint | Purpose |
| --- | --- | --- |
| `is_connected` | `GET /api/v3/connected_accounts` | Real connected-account status for a `user_id` |
| `initiate_link` | `POST /api/v3/connected_accounts/link` | Start real OAuth, get a real `redirect_url` |
| `get_or_create_mcp_server` | `GET`/`POST /api/v3/mcp/servers` | One shared MCP server per toolkit, created once at boot |
| `generate_user_mcp_url` | `POST /api/v3/mcp/servers/generate` | Per-user MCP endpoint URL |
| `list_tools`/`call_tool` | JSON-RPC over that MCP URL | `tools/list`/`tools/call` against the real Composio-hosted server |
| `disconnect` | `GET` then `DELETE /api/v3/connected_accounts/{id}?revoke_on_delete=true` | Logs a `user_id` out — deletes every active connected account and best-effort revokes the upstream OAuth grant (exposed as `DELETE /connectors/{connector}/credentials`, see [§16](#16-api--endpoint-surface)) |

Registered into `GatewayState` only when `COMPOSIO_API_KEY`/`COMPOSIO_AUTH_CONFIG_ID` are set —
absent otherwise, so `cargo test` and a plain `cargo run` never require live credentials. Modeled
as just another `ConnectorRegistry`-shaped entry from the gateway dispatch code's point of view: one
two-arm match (`is this the configured Composio connector name, or look it up in the registry`),
never a per-tool special case.

**Verified live, not just compiled:** a real Composio account, a real GitHub OAuth connection
(via a real `https://connect.composio.dev/link/...` URL), and real dispatches of
`GITHUB_LIST_REPOSITORIES_FOR_THE_AUTHENTICATED_USER` (read) and `GITHUB_CREATE_AN_ISSUE` (ask-gated
write, per `policy.toml`'s `composio_github` rule) — the write produced a real, checkable GitHub
issue. See [§15](#15-real-end-to-end-example) and `HITL_HANDOFF.md` for the exact captured result.
`disconnect` was also verified live: it correctly reports `accounts_removed: 0` as a safe no-op
when nothing is currently connected, and a real earlier connected account was successfully deleted
and revoked the same way during this same verification effort.

## 15. Real end-to-end example

The single strongest proof this mechanism is real, not mocked — the ask-gated Composio write,
start to finish:

```
Agent calls composio_github / GITHUB_CREATE_AN_ISSUE
      │
      ▼
policy.toml: composio_github/GITHUB_CREATE_AN_ISSUE → ask
      │
      ▼
ApprovalRequired checkpoint created  ({"status":"pending","reason":{"kind":"approval_required"}})
      │
      ▼
Human: POST /hitl/{id}/respond {"action":"approve"}
      │
      ▼
Resumes into CredentialCheck (already connected) → SchemaCheck
      │
      ▼
InputRequired  (owner/repo/title — from Composio's real, live GitHub tool schema)
      │
      ▼
Human: POST /hitl/{id}/respond {"action":"input","fields":{"owner":"...","repo":"...","title":"..."}}
      │
      ▼
Agent resumes → MCP Gateway (POST /mcp tools/call) → real Composio MCP server → real GitHub API
      │
      ▼
Real result: html_url, a real issue number, mercury_last_http_status_code: 201, a real Composio log_id
```

A verified real result from exactly this flow: `https://github.com/nasiko-aditya/HITL-MCP-POC/issues/4`
— HTTP 201, a real issue number, a real Composio `log_id`, real timestamps (full capture in
`HITL_HANDOFF.md` and `DEMO_GUIDE.md`).

**Why this proves the mechanism is real:** every step above is the identical code path a mock
demo exercises — `agent::preflight::execute`, the same `Checkpoint`/`PauseReason` types, the same
`/hitl/{id}/respond` handler, the same gateway `POST /mcp` dispatch. Nothing about Composio's
presence changes the decision logic; only the last hop (`dispatch_tool` in
`gateway::protocol`) resolves to a real HTTP call instead of a mock's. If Composio or GitHub were
unreachable, or the API key lacked a permission, this is now surfaced as a clear, distinct error
rather than silently mislabeled as "needs authentication" (see [§20](#20-concurrency--duplicate-resume--idempotency)
and `HITL_HANDOFF.md`'s "Architecture decisions").

## 16. API / endpoint surface

**Agent (`:8090`) — owns HITL:**

| Route | Purpose |
| --- | --- |
| `POST /agent/act` | `{"connector","tool_name","arguments"}` — task intake; where the agent decides whether HITL is needed |
| `GET /hitl/pending` | List every pending checkpoint |
| `GET /hitl/{id}` | Inspect one checkpoint (question, reason, arguments, status, history) |
| `POST /hitl/{id}/respond` | `{"action":"approve"}` / `{"action":"deny"}` / `{"action":"input","fields":{...}}` / `{"action":"authenticate","token":"..."}` (token omitted for a tokenless Composio re-check) |
| `GET /agent/result/{call_id}` | Poll for how a paused call was eventually resolved |
| `GET /audit/{call_id}` | Full cross-service audit trail for one call |

**Gateway (`:8080`) — execution + advisory, never creates a checkpoint:**

| Route | Purpose |
| --- | --- |
| `POST /mcp` | `initialize` / `tools/list` / `tools/call` — the actual MCP execution layer |
| `GET /policy/{connector}/{tool}` | Read-only: what does `policy.toml` say the stance is? |
| `GET /connectors/{connector}/status` | Read-only: is this connector connected, and if not, what's the `auth_url`? Returns a hard error (not `connected: false`) if the check itself fails |
| `POST /connectors/{connector}/credentials` | Write a token for a mock/local connector (refused for the Composio connector) |
| `DELETE /connectors/{connector}/credentials` | Log out: deletes the local credential row for a mock connector, or deletes + revokes the real connected account for the Composio one. Not a HITL concern — only changes what the *next* `CredentialCheck` finds |

Every response carries a `call_id` only on its *first* appearance (`POST /agent/act`) —
`/hitl/{id}/respond` responses return `checkpoint_id` only; recover `call_id` via
`GET /hitl/{id}` if needed for the audit endpoint.

## 17. Persistence / database

SQLite via `sqlx`, file-backed by default (`./hitl_poc.db`), in-memory for tests — **one pool
shared by both services** (`GatewayState.pool` and `AgentState.pool` are clones of the same
`SqlitePool`). At POC scale (single machine, one `cargo run`) there's no reason to split storage;
SQLite serializes writers itself, and sharing the pool is what makes `GET /audit/{call_id}` show
one coherent trail spanning both services. Three tables (`migrations/0001_init.sql`):
`checkpoints`, `credentials` (mock/local path only), `audit_log`.

## 18. Audit trail

`audit.rs`'s `AuditLog`, shared and unchanged by both services, writes an append-only row at every
decision point and human action: `call_received`, `blocked`, `paused`, `success`, `failed`,
`approved`, `denied`, `authenticated`, `input_provided`, `re_paused`, `resumed_success`,
`resumed_failed`, `rejected_requires_agent`. Each row records who (`user_id`, `agent_id`), what
(`connector`, `tool_name`), why (`detail`), and when. Because the agent forwards its own
`call_id` to the gateway on every dispatch (`x-call-id` header), `GET /audit/{call_id}` shows one
chronologically ordered trail spanning both services. Credential tokens are never written to
`detail` — `authenticate` responses are redacted to `{"token_provided": true/false}`.

## 19. Important sequence/flow diagrams

**One full pause/resume cycle** (any connector, any pause reason):

```
Client            Agent (:8090)              Gateway (:8080)          Connector
  │  POST /agent/act │                            │                       │
  ├─────────────────►│  Decide (GET /policy)       │                       │
  │                  ├────────────────────────────►│                       │
  │                  │◄────────────────────────────┤ stance                │
  │                  │  [Ask] → pause                                      │
  │  {"status":"pending","checkpoint_id":...}                              │
  │◄─────────────────┤                                                     │
  │  POST /hitl/{id}/respond {"action":"approve"}                          │
  ├─────────────────►│  resume from CredentialCheck                        │
  │                  ├────────────────────────────►│ GET /connectors/status│
  │                  │◄────────────────────────────┤                       │
  │                  │  SchemaCheck (cached)                                │
  │                  │  Dispatch                                           │
  │                  ├────────────────────────────►│ POST /mcp tools/call  │
  │                  │                              ├──────────────────────►│
  │                  │                              │◄──────────────────────┤
  │                  │◄────────────────────────────┤ result                │
  │  {"status":"resolved","result":...}                                    │
  │◄─────────────────┤                                                     │
```

See [§15](#15-real-end-to-end-example) for the same shape against real Composio, and `DEMO_GUIDE.md`
for every outcome with real captured request/response bodies.

## 20. Concurrency / duplicate-resume / idempotency

`CheckpointStore::claim_pending` (`src/agent/checkpoint.rs`) is the entire duplicate-response /
double-resume guard: an atomic `UPDATE checkpoints SET status='processing' WHERE id=? AND
status='pending'`. Only one concurrent request can flip a given row from `pending` to
`processing` — a second concurrent `/hitl/{id}/respond` for the same checkpoint sees
`rows_affected() == 0` and is rejected with `409 Conflict` rather than racing. This is the SQLite
equivalent of a Postgres `SELECT ... FOR UPDATE` row lock (SQLite serializes writers itself, so a
plain conditional `UPDATE` is enough at this scale). `Processing` is never a resting state — every
code path that reaches it must follow up with either `finalize` (terminal) or `re_pause` (back to
`pending` under a new reason) within the same request. Two tests
(`duplicate_response_is_rejected_not_double_applied`,
`completed_checkpoint_cannot_be_resumed_again`) exercise this directly.

Separately: `gateway::routes::get_connector_status` and `agent::gateway_client::GatewayClient::
connector_status` distinguish a genuine "not connected" fact from the status check itself failing
(a real Composio API error or DB error) — the latter now surfaces as a hard HTTP error rather than
being silently reported as `connected: false`, so a real outage can never look identical to a
routine `AuthRequired` pause. Verified by
`connector_status_surfaces_real_store_failure_not_false` and
`credential_store_failure_surfaces_as_agent_error_not_auth_required`.

## 21. Testing and verification

```sh
cargo test                   # 16 unit tests + 33 end-to-end tests — all pass
cargo clippy --all-targets   # zero warnings
cargo fmt --check            # clean
```

The 33 end-to-end tests in `tests/hitl_flow.rs` spin up the real gateway, the real agent, and both
real mock connectors on ephemeral ports with an isolated in-memory DB shared by the two services,
driving everything over real HTTP. Covers every outcome, deny/approve, input-required/resume,
auth-required/resume, duplicate-response rejection, resumed-checkpoint-can't-resume-again,
downstream dispatch failure after a successful resume, genericity across both mock connectors, two
tests proving the gateway never independently triggers HITL, two tests proving a real
backing-store failure surfaces as an error rather than a false `AuthRequired`, two tests proving
`DELETE /connectors/{connector}/credentials` (log out) makes a connected connector require auth
again and is a safe no-op when nothing was connected, and the operational-failure/HITL-state suite
described in [§21.1](#211-error-handling--failure-scenarios) below.

**Beyond `cargo test`: live-verified, not just compiled.** `./demo/demo.sh` was run against the
real running binary (not just the in-process test harness). `./demo/demo_composio.sh` was run
against a real Composio account and a real GitHub repository — real OAuth completed in a browser,
real `GITHUB_LIST_REPOSITORIES_FOR_THE_AUTHENTICATED_USER` and `GITHUB_CREATE_AN_ISSUE` dispatches,
a real GitHub issue created. See `HITL_HANDOFF.md`'s "This cleanup pass" section for the exact
verified result and the two real bugs that verification pass found and fixed.

### 21.1 Error handling / failure scenarios

A later pass audited what happens when something *outside* a normal ALLOW/BLOCK/ASK/
InputRequired/AuthRequired decision goes wrong — the gateway is unreachable, a downstream
connector is down, Composio's API fails, a tool or connector name doesn't exist, or a human's HITL
response is malformed, stale, or a duplicate. The rule enforced throughout: **an operational
failure (a connection error, a 5xx, a timeout) is never allowed to look like a HITL condition.**
`AuthRequired` means "checked, and this connector genuinely isn't connected yet" — never "the
check itself couldn't run." Concretely:

| Scenario | Behavior |
| --- | --- |
| Gateway unreachable | `POST /agent/act` returns `{"status":"error",...}` with a `"gateway unreachable while ..."` message identifying which call failed — never `AuthRequired`/`ApprovalRequired`/`InputRequired`, never a checkpoint |
| Downstream connector (mock or Composio) unreachable | Surfaces as `{"status":"failed","error":"...downstream MCP server..."}` at `Dispatch` — a `Failed` outcome, not a false success, not `AuthRequired` |
| Composio API failure (bad key, 5xx, network error) | `GET /connectors/{connector}/status` returns a hard `502`/error, distinct in *kind* (`Err`, not a `connected: false` fact) from Composio genuinely reporting no active connected account |
| Unknown connector | `GET /connectors/{connector}/status` returns `404 unknown connector '...'` *before* consulting any credential store — fixed in this pass; previously an unregistered connector name looked exactly like an ordinary not-yet-connected one and paused on a misleading `AuthRequired` |
| Unknown tool | Dispatch fails with the downstream connector's own `"unknown tool '...'"` message — a `Failed` outcome, never a checkpoint |
| Malformed `arguments` (wrong JSON type) | Rejected as `{"status":"error","error":"'arguments' must be a JSON object, got ..."}` — fixed in this pass; previously a non-object `arguments` value was silently treated as "every required field is missing" and mispaused on `InputRequired` |
| `POST /hitl/{id}/respond` for an unknown checkpoint | `404` |
| Wrong action for a checkpoint's pause reason (e.g. `approve` on `InputRequired`) | `400`, checkpoint stays `pending`, nothing executes |
| Duplicate / concurrent response to the same checkpoint | Exactly one wins (`200`); the other gets `409` — see `CheckpointStore::claim_pending` |
| Responding again to a `denied`/`resolved`/`failed` checkpoint | `409` on every terminal status, not just `resolved` |
| A partial `input` response (doesn't actually supply every missing field) | Fixed in this pass — re-pauses on `InputRequired` again with whatever's still missing, instead of dispatching with an incomplete call (`InputRequired`'s `resume_from` now points back at `SchemaCheck`, mirroring how `AuthRequired` already re-verified rather than trusting the human's claim) |
| `authenticate` with no real credential behind it | Re-pauses on `AuthRequired` again — the connection status is always re-checked on resume, never inferred from the human's claim |
| Approved tool fails downstream after resume | Checkpoint finalizes `failed` with the real error — an approval is never mistaken for a successful execution |
| Policy (`policy.toml`) changes between pause and resume | Not re-evaluated for an `ApprovalRequired` checkpoint — `approve` resumes at `CredentialCheck`, not `Decide`, so the original `ask` decision for that specific call stands. The one exception: a tool reclassified to `Block` in the interim is still caught, because the gateway's `Block` backstop is enforced unconditionally on every dispatch regardless of the agent's own decision history. Documented trade-off, not a bug — see [§12](#12-checkpoint--pauseresume-model) |

Every case above is covered by an automated test in `tests/hitl_flow.rs` (or, for the Composio-API-
failure distinction specifically, a unit test in `gateway::composio` against a fake HTTP server, so
it's deterministic and needs no live credentials). See `DEMO_GUIDE.md`'s "Error handling / failure
scenarios" section for the same cases run by hand against a live `cargo run` instance, with real
captured output.

## 22. Production mapping / future Nasiko integration

| | This POC | Future Nasiko |
| --- | --- | --- |
| Tool permission config | `policy.toml`, a static file | Nasiko's existing per-agent UI/backend (Allow/Ask/Block per connector, per agent) — same shape of data, different storage |
| HITL trigger | Standalone agent process/router in this repo | The real agent (wherever Nasiko's agent loop lives) gains this same pre-flight step |
| Gateway advisory endpoints | `GET /policy/...`, `GET /connectors/.../status` in this repo's gateway | Nasiko's real gateway (`oss/mcp-gateway`) gains equivalents — it has neither today |
| Persistence | Shared SQLite file | Nasiko's real Postgres |
| Trust boundary | `x-agent-preflight: passed`, a plaintext header | Folded into the existing signed delegation JWT (`oss/auth/src/jwt.rs`) |
| Credentials | Per-connector (mock path), per-`user_id` (Composio path) | Per-`(user, connector)`, encrypted at rest |
| Composio | One real, demo-scoped connector | Not applicable — Composio is this POC's stand-in for "a real external MCP tool," not a production dependency |

**This POC is not integrated into `nasiko-cloud-rs`, and none of its code has been merged there.**
It proves the mechanism twice — once as the pause/resume model, once as the agent-owns-the-decision
boundary — so that bringing it into the real stack is wiring, not invention. Full detail:
`HITL_POC.md` §15.

## 23. Limitations

- No checkpoint-expiry sweep — `expires_at`/`'expired'` exist in the schema but nothing sets or
  acts on them yet.
- **Policy is not re-evaluated on resume for `ApprovalRequired`.** Approving a checkpoint resumes
  at `CredentialCheck`, not `Decide` — the original `ask` decision for that specific call stands
  even if `policy.toml` changes before a human responds. Only `Block` is re-enforced
  unconditionally, by the gateway's own dispatch-time backstop, regardless of this history. This is
  a deliberate, documented trade-off (see [§12](#12-checkpoint--pauseresume-model) and
  `HITL_POC.md` §16), not an oversight — re-running `Decide` on every resume was considered and
  rejected because it would let an approval be silently redirected to a different pending state
  without the approving human ever being told.
- **The gateway, agent, and both mock connectors run as tasks inside one process** (`src/main.rs`)
  — there is no way to stop just one of them from outside a running `cargo run` without killing all
  of them. The operational-failure scenarios that would otherwise need that (gateway down, one
  connector down while the rest stays up) are instead covered deterministically by
  `tests/hitl_flow.rs`, which spins up each service as its own axum app on an independently
  controllable port.
- `demo/demo.sh` and `demo/demo_composio.sh` fall back to a fragile `python3 -c eval(...)` JSON
  extraction when `jq` isn't installed.
- The audit log has no pagination — fine at POC scale.
- `gateway/composio.rs` targets Composio's `v3` REST API, which Composio's own docs mark
  `mcp/servers/generate` as "deprecated but functional" in favor of a `v3.1` base URL — left alone
  deliberately since v3 still works (verified live) and chasing a newer API wasn't the brief.
- `x-agent-preflight: passed` is a plaintext, unsigned trust-boundary header, not a production
  security control.
- Credentials are scoped per-connector (not per-`(user, connector)`) for the mock/local path only;
  the Composio path is already per-`user_id`.

## 24. Explicitly out of scope

- No LLM anywhere in the HITL decision path (deliberate — see [§7](#7-agent-side-hitl-pre-flight-flow)).
- No changes to, or dependency on running, `nasiko-cloud-rs/` or `HITL-POC/`.
- No production authentication/authorization — identity is a plain `x-user-id`/`x-agent-id` header.
- No multi-instance or production-grade persistence — one shared SQLite file.
- No checkpoint-expiry sweep, no audit-log pagination (see [§23](#23-limitations)).
- Building Nasiko's real per-agent permission UI — `policy.toml` is a deliberate stand-in, not a
  reimplementation.

## 25. Setup / prerequisites / environment variables

**Prerequisites:** Rust (stable), `cargo`. No external database, Docker, or network access
required for the mock path. Optional: `jq` (demo scripts fall back to raw JSON without it).

```sh
cd MCP-Gateway-HITL-POC
cargo build
```

All environment variables are optional — every field has a working default (`src/config.rs`),
loaded best-effort from `.env` via `dotenvy`. Copy `.env.example` to `.env` and fill in only the
values needed; **never commit a real `.env`** (already gitignored).

| Variable | Default | Purpose |
| --- | --- | --- |
| `GATEWAY_PORT` | `8080` | Gateway listener |
| `AGENT_PORT` | `8090` | Agent listener |
| `GITHUB_MOCK_PORT` | `8081` | Mock GitHub connector |
| `NOTION_MOCK_PORT` | `8082` | Mock Notion connector |
| `DATABASE_URL` | `sqlite://./hitl_poc.db` | Shared by both services |
| `POLICY_PATH` | `policy.toml` | Path to the policy file |
| `COMPOSIO_API_KEY` | *(unset)* | From the Composio dashboard — required, with the one below, to enable the live connector |
| `COMPOSIO_AUTH_CONFIG_ID` | *(unset)* | Auth config id for the configured toolkit in Composio |
| `COMPOSIO_BASE_URL` | `https://backend.composio.dev` | Composio API base |
| `COMPOSIO_CONNECTOR_NAME` | `composio_github` | Connector name used for routing + `policy.toml` rules |
| `COMPOSIO_SERVER_NAME` | `mcp-gateway-hitl-poc-demo` | MCP server name to get-or-create at boot |
| `COMPOSIO_ALLOWED_TOOLS` | `GITHUB_LIST_REPOSITORIES_FOR_THE_AUTHENTICATED_USER,GITHUB_CREATE_AN_ISSUE` | Tool slugs exposed through that MCP server |

Leaving `COMPOSIO_API_KEY`/`COMPOSIO_AUTH_CONFIG_ID` unset runs with the two mocks only — this is
what `cargo test` and a plain `cargo run` always do.

## 26. Quick demo / commands

```sh
cargo build
cargo test
cargo clippy --all-targets
cargo run                    # boots agent :8090, gateway :8080, mock github :8081, mock notion :8082

./demo/demo.sh                # every outcome, against the mocks, through the agent
./demo/demo_composio.sh       # same, against a live Composio connector (skips cleanly without credentials)

# to see a real Composio write actually execute:
COMPOSIO_ASK_OWNER=<your-github-username> \
COMPOSIO_ASK_REPO=<a-repo-you-own-with-issues-enabled> \
./demo/demo_composio.sh
```

## 27. Links to detailed docs

| Doc | What's in it |
| --- | --- |
| [`HITL_POC.md`](./HITL_POC.md) | Full design writeup — problem, architecture rationale, genericity proof, limitations, production considerations, section-by-section |
| [`HITL_HANDOFF.md`](./HITL_HANDOFF.md) | Engineer-to-engineer handoff — file map, architecture decisions, test list, verified Composio result, known issues |
| [`DEMO_GUIDE.md`](./DEMO_GUIDE.md) | Every demo command with real captured output, a team-lead talk track, and the live Composio walkthrough |


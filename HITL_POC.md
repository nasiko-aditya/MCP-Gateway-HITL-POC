# HITL at the MCP Gateway — Design Writeup

This document explains the design behind the standalone POC in this repo: the problem, what
exists today, what this POC adds, and how each piece works. 

## 1. Problem

An MCP gateway sits between an agent and a set of downstream tool-providing connectors
(GitHub, Slack, Notion, ...). Every `tools/call` is either safe to run immediately, or isn't —
because it needs a human's explicit go-ahead, because the connector isn't authenticated yet, or
because the call is simply missing information only a human can supply. A gateway with no
mechanism for that second case has exactly one option: reject the call outright and make the
*agent* (or its surrounding application) figure out what to do next, with no persisted memory of
why. That's a dead end for anything resembling human-in-the-loop control.

## 2. The existing Allow/Block/Ask model

Nasiko's real MCP gateway already has a deterministic, config-driven, non-LLM permission
decision: `ALLOW`, `BLOCK`, or `ASK`, computed by glob-matching a tool name against per-connector
rules with priority `block > ask > allow` (`HITL_INVESTIGATION.md` §2). `ALLOW` and `BLOCK` behave
as expected. `ASK`, however, is a dead end in production today: it returns a
JSON-RPC error telling the agent to go change a static permission setting, the downstream tool is
never contacted, and no state survives the request — there's no way to approve *this specific
call*. A `FlowEvent::ToolApprovalRequired` is published to a broadcast channel with zero
subscribers anywhere in the codebase.

This POC's `src/policy.rs` is a direct, standalone reimplementation of that same decision (same
priority rule, same glob semantics, same default-allow). What it changes is what happens on
`Ask` — see §7. Production framing for `policy.toml` itself: it stands in for what would really be
per-agent MCP tool configuration set through Nasiko's existing UI/backend
(Allow/Ask/Block per connector, per agent) — this POC reads that shape of data from a file instead
of a database-backed admin UI, because building that UI isn't the point of the exercise; see §15.

## 3. The HITL extension

The extension is narrow and precise: `Ask` no longer means "reject" — it means "pause." A paused
call is persisted as a `Checkpoint` that carries everything needed to resume it later, and a small
Human Action API lets a human resolve that checkpoint (approve/deny/supply input/authenticate).
Resuming re-enters the exact same pre-flight loop a fresh call would go through, just starting
partway in. Two more pause reasons — missing required input, missing credential — are added
alongside `Ask`'s "needs approval," because they're the same *shape* of problem (pause, ask a
human, resume) even though today's real gateway doesn't detect them as such.

## 4. Architecture — the trigger lives in the agent, not the gateway

**This is the one thing that changed since the first iteration of this POC**, and it's worth
stating precisely: an earlier version ran `Decide → CredentialCheck → SchemaCheck → Dispatch`
entirely inside the gateway's `tools/call` handler, and the gateway itself created the
`Checkpoint`. That made the gateway a HITL orchestrator, not just an MCP execution layer — exactly
the shape ruled out. This iteration splits the single process into two logically independent
Axum services (still one `cargo run`, still one shared SQLite file, but two ports and no shared
Rust structs across the boundary — see the file table in `HITL_HANDOFF.md`):

```
Agent (agent::preflight, :8090)                      Gateway (gateway::protocol, :8080)
     |
     |  Decide -----------------> GET /policy/{connector}/{tool}
     |     | Block -> Rejected (terminal, no checkpoint)
     |     | Allow/Ask
     |     | Ask -> Paused(ApprovalRequired)
     v
  CredentialCheck --------------> GET /connectors/{connector}/status
     |     | not connected -> Paused(AuthRequired)
     v
  SchemaCheck -------------------> tools/list (cached) + schema_validator, missing field -> Paused(InputRequired)
     v
  Dispatch ----------------------> POST /mcp tools/call  ---------->  policy re-check (Block backstop,
                                                                       reject bypassed Ask), then the
                                                                       actual downstream MCP call
```

`Paused{reason}` is persisted by the agent (`agent::routes::render_outcome`, and
`agent::hitl::routes::finish_resume`'s re-pause branch) as a `Checkpoint`
(`src/agent/checkpoint.rs`) and returned to the caller as `{"status":"pending",...}`.
`POST /hitl/{id}/respond` (`src/agent/hitl/routes.rs`) is the only way a paused checkpoint moves
forward: it validates the human's action against the pause reason, then calls
`agent::preflight::resume`, which runs the *same* `execute` function starting at whichever step
comes right after the one that paused (`PauseReason::resume_from`). Nothing in the pre-flight loop
itself has a concept of "fresh" vs. "resumed" — it's the same four-step function either way, with
`Decide` skipped on resume because the approval/block decision for this call already happened.

**What the gateway still enforces on its own, and why it doesn't count as HITL:**
`gateway::protocol::handle_tools_call` re-checks policy once more before dispatch, but only acts
on two outcomes, both hard and human-free:

- `Block` → always rejected, even bypassing the agent — defense in depth, not a decision to ask a
  human (there's no human involved either way).
- `Ask`, reached *without* the `x-agent-preflight: passed` header the agent's `GatewayClient` sets
  on every dispatch it has already pre-flighted → rejected with a distinct error
  (`codes::TOOL_ASK`, "route this call through the agent's HITL flow instead"). This is a bypass
  guard, not the gateway deciding to pause — it never creates a `Checkpoint`, never asks a human,
  and the two tests in `tests/hitl_flow.rs` named `gateway_*_backstop*` prove exactly that: calling
  the gateway directly for an `ask`-gated tool changes nothing about `GET /hitl/pending`.

## 5. InputRequired

**Mechanism: the tool's own JSON Schema, not an LLM.** Every MCP `tools/list` response carries
each tool's `inputSchema`, including a `required` array. `src/schema_validator.rs` (a pure,
connector-agnostic module shared by the agent) caches that schema — now sourced from the
*gateway's* aggregated, namespaced `tools/list` rather than a connector directly, since the agent
doesn't have connector base URLs — keyed by `(connector, tool_name)`, and on every task diffs
`required` against the keys actually present in `arguments`. Missing keys become
`MissingField { name, field_type, description }` — pulled straight from the schema's
`properties`, so the human-facing question is legible without any tool-specific string ever
written into either service. Resuming an `InputRequired` checkpoint merges the human's supplied
fields into the original arguments and re-runs `SchemaCheck` against the merged result before
continuing to `Dispatch` — not a straight skip to `Dispatch`. This is a deliberate fix from an
error-handling review pass: an `input` response isn't guaranteed to have actually supplied every
field that was originally missing (a human can submit a partial `fields` object), so re-running
the same diff means an incomplete resume re-pauses on whatever's still missing instead of
dispatching a call the tool's own schema says is invalid. Mirrors the reasoning already applied to
`AuthRequired` below — resume back into the check that produced the pause, not past it.

## 6. AuthRequired

**Mechanism: connector connection status, not an LLM — but now generic across two genuinely
different kinds of connector.** The agent asks `GET /connectors/{connector}/status`; the gateway
answers it one of two ways depending on *connector type*, never a per-connector-name branch:

- **Local/mock connector** (`src/credentials.rs::is_valid`): does a row exist in the `credentials`
  table, and if it has an expiry, has it passed? `POST /hitl/{id}/respond` with
  `{"action":"authenticate","token":"..."}` is the POC's deliberately simplified stand-in for a
  real OAuth exchange — the agent forwards the token to the gateway's
  `POST /connectors/{connector}/credentials`, which writes it into the credential store.
- **Composio connector** (`src/gateway/composio.rs`): real Composio connected-account status
  (`GET /api/v3/connected_accounts`), and a real OAuth `auth_url`
  (`POST /api/v3/connected_accounts/link`) when not connected. There is no token to submit — the
  human completes OAuth out-of-band in a browser, and `{"action":"authenticate"}` (token omitted)
  just means "re-check now."

Both cases resume identically: `PauseReason::AuthRequired::resume_from()` points back at
`CredentialCheck` (not past it), so resuming always re-verifies the real connection status rather
than blindly trusting the human's claim — see `src/agent/checkpoint.rs`'s doc comment on
`resume_from` for why this one field changed from the first iteration. Neither the "forward a
token" branch nor the "just re-check" branch in `agent::hitl::routes::respond` ever matches on a
connector name — the difference is entirely in what the human happened to submit.

**POC abstraction, clearly marked:** the local/mock credential path is scoped per-connector only,
not per `(user, connector)` as production would need, and its token is stored in plaintext (there
is no secret to protect in a demo credential). Composio's status/auth-url calls *are* real, scoped
per Composio `user_id` (this POC uses the same `x-user-id` header already threaded through
everything else). See §14.

## 7. ApprovalRequired

**Mechanism: the existing deterministic policy, completely unchanged in its own logic — only who
consumes the answer changed.** `policy.toml` maps `(connector, tool pattern) -> allow | ask |
block`, exactly like Nasiko's real per-agent stance table, with the same `block > ask > allow`
priority and default-allow — and exactly like production, this stands in for tool permissions an
agent would really get from Nasiko's own UI/backend configuration, not from a model. The one
change from the first iteration: the gateway no longer *acts* on an `Ask` verdict by pausing —
`GET /policy/{connector}/{tool}` just answers the question, and the *agent* is the one that turns
`ask` into `PauseReason::ApprovalRequired{summary}`, which resumes on `approve` (continuing to
`CredentialCheck`) or terminates on `deny`. There is no LLM anywhere in this decision, by design —
the brief for this POC was explicit that a human approval gate must not be something a model can
be talked out of.

## 8. Checkpoint

A `Checkpoint` (`src/agent/checkpoint.rs`, table `checkpoints` in `migrations/0001_init.sql`,
owned entirely by the agent) stores: `id`, `call_id`, `user_id`, `agent_id`, `connector`,
`tool_name`, `tool_arguments`, `reason` (the tagged `PauseReason` JSON), `resume_from` (which
pre-flight step to continue at), `status` (`pending` / `processing` / `denied` / `resolved` /
`failed` / `expired`), `human_response`, `result`, `error`, and timestamps — everything needed to
resume the agent's pending tool operation, with nothing about it referencing gateway-internal
state. This is unchanged in shape from the first iteration (itself a close port of
`HITL-POC/src/services/checkpoint.rs`'s `Checkpoint`/`PauseReason` model) except for one field:
`HumanAction::Authenticate.token` widened from `String` to `Option<String>` to cover the Composio
tokenless case (§6).

## 9. Pause/resume

A paused task does **not** hold the HTTP connection open. `POST /agent/act` returns immediately
with:

```json
{"status":"pending","call_id":"...","checkpoint_id":"...","reason":{...},"question":"..."}
```

Resuming is a **brand-new** `POST /hitl/{id}/respond` request, handled entirely by the agent. It
reloads the checkpoint from the database, atomically claims it (`CheckpointStore::claim_pending`,
an `UPDATE ... WHERE status='pending'` that flips the row to a transient `processing` state),
validates the human's action against the stored `PauseReason`, and then re-enters
`agent::preflight::execute` starting at `checkpoint.resume_from`. The one step that crosses the
process boundary is `Dispatch`, which the agent reaches by calling the gateway's `POST /mcp`
`tools/call` over HTTP (`GatewayClient::call_tool`) — every other step is answered by a gateway
GET endpoint and interpreted entirely on the agent side.

## 10. Persistence

SQLite (via `sqlx`), file-backed by default (`./hitl_poc.db`), in-memory for tests — **shared by
both services**, since at POC scale (single machine, one `cargo run`) there's no reason to split
it, and SQLite serializes writers itself. This is a POC simplification of Nasiko's Postgres — the
schema (`migrations/0001_init.sql`) is small enough that the choice of database engine doesn't
change any of the pre-flight logic. `claim_pending`'s conditional `UPDATE` gives the same "exactly
one request wins a given checkpoint" guarantee a Postgres `SELECT ... FOR UPDATE` row lock would.

## 11. Audit

`src/audit.rs` (shared, unchanged, used by both services against the same `audit_log` table)
writes an append-only row at every decision point and every human action:
`call_received`, `blocked`, `paused`, `success`, `failed` (agent- or gateway-recorded depending on
which step produced them), `approved`, `denied`, `authenticated`, `input_provided`, `re_paused`,
`resumed_success`, `resumed_failed`, `rejected_requires_agent` (the gateway's bypass-guard
rejection). Each row records who (`user_id`, `agent_id`), what (`connector`, `tool_name`), why
(`detail`, e.g. the `PauseReason`), and when. Because the agent forwards its own generated
`call_id` to the gateway on every dispatch (`x-call-id` header), `GET /audit/{call_id}` on the
agent shows one coherent, chronologically ordered trail spanning both services for a single task
— see the example in `DEMO_GUIDE.md` §6. Credential tokens are never written to `detail` —
`authenticate` responses are redacted to `{"token_provided": true/false}` before being recorded
(`agent::hitl::routes::redacted_response_json`).

## 12. Genericity

No file in `src/agent/preflight.rs`, `src/agent/checkpoint.rs`, `src/policy.rs`,
`src/credentials.rs`, `src/schema_validator.rs`, or `src/gateway/protocol.rs` contains the string
`"github"`, `"notion"`, or a specific Composio tool slug in a branch condition — every decision
reads generic data (a policy table row, a connector's connection status, a JSON Schema's
`required` array). The only connector-aware code is `src/provider.rs`'s `ConnectorRegistry` (a
name → base-URL map, populated once at startup) and `src/gateway/composio.rs`'s single
`ComposioSlot` (a connector name → real client mapping, also populated once at startup and
entirely config-driven — see `config::ComposioConfig`), plus the two mock connector definitions
themselves, which exist purely to give the pre-flight loop something real to call over HTTP.
`tests/hitl_flow.rs` exercises the mock connectors through the identical code path as direct
proof (`auth_required_is_generic_across_connectors`), and two more tests
(`gateway_block_backstop_rejects_direct_call_without_agent`,
`gateway_rejects_ask_tool_reached_directly_without_agent_preflight`) prove the trigger lives in
the agent, not the gateway.

## 13. GitHub demo

The mock GitHub connector (`src/mock_connectors/github.rs`) exposes four tools chosen to hit each
outcome once `policy.toml`'s rules are applied: `list_repos` (plain allow, but AuthRequired until
authenticated), `wipe_org` (blocked), `delete_repo` (ask-gated, requires `repository`),
`get_latest_pr` (allow, requires `repository` — the InputRequired demo). `demo/demo.sh` runs
through the same five outcomes against the agent's `/agent/act`, then against the *Notion* mock
connector to prove the mechanism generalizes, then both gateway backstop checks. `demo/demo_composio.sh`
runs the equivalent flow against one real Composio-backed connector — see `DEMO_GUIDE.md`.

## 14. Limitations

- **Non-standard protocol extension.** A real MCP client expects a synchronous `tools/call`
  response. This POC's `{"status":"pending",...}` result (and `GET /agent/result/{call_id}`
  polling) is a deliberate, documented deviation from the spec, now scoped to the agent's own
  task-intake API rather than MCP's `tools/call` itself — the gateway's `POST /mcp` is fully
  synchronous again, which is arguably closer to spec than the first iteration was.
- **Nasiko's real `ASK` path doesn't give this POC a working foundation to inherit** — it's a hard
  rejection today with no persisted state and a dead event bus (`HITL_INVESTIGATION.md` §10).
  Bringing this mechanism into the real product means adding the DB tables, the error/event
  vocabulary, and wiring MCP into whatever eventually becomes the orchestrator's agent loop (MCP
  isn't in it today) — this POC only proves the mechanism works, it inherits nothing "for free."
- **The agent/gateway trust boundary is a plaintext header, not a signed token.**
  `x-agent-preflight: passed` is a POC-level convention, not a security control — anything on the
  loopback network could set it. Production would fold this into the same signed delegation JWT
  the real gateway already uses for agent identity (`oss/auth/src/jwt.rs`) rather than a second,
  separate, unauthenticated header. This is a deliberate, documented simplification, not an
  oversight — see `gateway/routes.rs`'s module doc comment.
- **Credentials are a POC abstraction** for the mock/local path specifically: per-connector (not
  per `(user, connector)`), a plaintext demo token, no encryption at rest. The Composio path is
  real (real OAuth, real connected-account status), which is why it exists — see §6 and §9 of the
  original brief on "use real Composio auth behavior for the demo if practical."
- **Schema caching is process-local and unbounded** on the agent side now — fine for a POC, but a
  production version needs the same explicit persisted schema cache Nasiko's real gateway doesn't
  have today (`HITL_INVESTIGATION.md` §10).
- **No checkpoint expiry sweep.** The `checkpoints.expires_at` column and `'expired'` status exist
  in the schema but nothing currently sweeps overdue checkpoints — every checkpoint in this POC
  waits indefinitely for a human. `HITL-POC/src/services/task_service.rs::sweep_due_checkpoints` is
  the reference pattern for adding this (a periodic scan + row-locked wake), directly portable if
  needed.
- **Single-machine credential/audit visibility.** Everything lives in one SQLite file shared by
  both services; there is no multi-instance concern to solve here (unlike Nasiko's real
  Postgres/Redis-backed gateway).
- **Policy is not re-evaluated on resume for `ApprovalRequired`.** A documented trade-off, not a
  bug — see §16's "What does *not* get re-checked on resume, by design" for the full reasoning and
  why only `Block` is still enforced unconditionally at gateway dispatch.
- **All four sub-servers (gateway, agent, both mock connectors) run inside one process.** There's
  no way to stop just one from outside a running `cargo run` without killing all of them — the
  operational-failure scenarios that would otherwise need that are instead covered deterministically
  by `tests/hitl_flow.rs`, which spins up each service as its own axum app on an independently
  controllable port (see §16).

## 15. Production considerations

Bringing this mechanism into the real Nasiko stack would mean: (1) the *agent* (wherever Nasiko's
real agent loop lives) gaining its own pre-flight step that queries the gateway's policy/connector-
status endpoints and owns a `checkpoints`-shaped table, exactly mirroring this split; (2) those
policy/connector-status endpoints existing on the real gateway at all — today's real gateway
(`oss/mcp-gateway/src/permissions.rs`) has no endpoint an agent identity can call pre-flight, only
a session-JWT-gated management API (`HITL_INVESTIGATION.md` §11); (3) adding a real `McpError`
variant for "needs auth" distinct from "connector disabled" (today they're conflated —
`HITL_INVESTIGATION.md` §3); (4) persisting `inputSchema` instead of discarding it after
`tools/list`, so `InputRequired` can be detected the same way this POC does; (5) folding the
`x-agent-preflight` convention into the existing signed delegation JWT rather than a second header;
(6) deciding what a paused call means for whichever caller made it — since MCP isn't in the
orchestrator's LLM tool loop today, "resume the original agent conversation" has no target to
resume into, and the pragmatic answer (matching this POC, and `HITL-POC`'s own precedent) is: the
calling agent polls for the result, since it's already an active HTTP client by virtue of having
called the agent's task-intake endpoint at all. **`policy.toml` itself needs no production
analog beyond what already exists** — it's explicitly a stand-in for the per-agent MCP tool
configuration Nasiko's existing UI/backend already owns; the production version reads that
database instead of a TOML file, nothing about the pre-flight logic changes.

## 16. Error handling and operational failures

A later review pass (after the mechanism above was already working end to end) asked a narrower
question: what happens when something goes wrong that *isn't* one of the three HITL conditions —
the gateway is down, a downstream connector or Composio is unreachable, a tool or connector name
doesn't exist, or a human's HITL response is malformed, stale, or racing another one? The governing
rule, stated precisely because it's easy to get backwards: **`AuthRequired` means "checked, and
this connector genuinely isn't connected yet," never "the check itself couldn't run."** A
connectivity failure and a real "not authenticated" fact must never collapse into the same
response, or an operator debugging a real outage gets pointed at exactly the wrong place (told to
send users an auth prompt instead of noticing a dependency is down).

Two real gaps this pass found and fixed, both instances of the same anti-pattern — treating "the
question I asked couldn't be answered" as if it were the answer:

- **An unregistered connector name looked exactly like an ordinary not-yet-connected one.**
  `GET /connectors/{connector}/status` asked the local `CredentialStore` about *any* connector
  name, including one that was never registered in `ConnectorRegistry` and isn't the configured
  Composio connector either — the store has no concept of "this connector doesn't exist," so it
  just answered "no row, not connected," and the agent dutifully paused on `AuthRequired`, asking a
  human to authenticate something that was never wired up. Fixed by checking
  `protocol::connector_known` first and returning `404 unknown connector '...'` before ever
  consulting a credential store or Composio for it — the same existence check `handle_tools_call`
  already used at dispatch time, now also applied at the advisory status endpoint the agent's
  `CredentialCheck` actually depends on.
- **A malformed (non-object) `arguments` value looked exactly like "every required field is
  missing."** `SchemaValidator::missing_required_fields` diffs `required` against
  `arguments.as_object()`, which is `None` for anything that isn't a JSON object — including a
  genuinely malformed request (a string, a number, an array) sent where an object belongs. That
  produced a legible-looking `InputRequired` pause asking for fields that were never actually
  "missing" in any meaningful sense; the real problem was a malformed request, not a partial one.
  Fixed at the entry point (`agent::routes::act`): a non-null, non-object `arguments` is now
  rejected as a clean validation error before it ever reaches the pre-flight loop. The one
  legitimate `Null` case — `arguments` omitted entirely — is normalized to `{}` rather than left as
  `Null`, which also fixed a latent second bug: a checkpoint created from such a call previously
  could never actually resume its `input` action, because merging fields into a stored `Null`
  has no JSON object to merge into.

**A related, non-obvious fix in the same spirit:** an `InputRequired` checkpoint's `resume_from`
used to point straight at `Dispatch`, skipping `SchemaCheck` on the theory that "schema was the
only thing missing, nothing else to re-check." That's true only if the human's `input` response is
guaranteed to have supplied *every* field that was missing — which it isn't; a partial `fields`
object is a legitimate, unremarkable thing for a human to submit by mistake. `resume_from` for
`InputRequired` now points back at `SchemaCheck` itself, mirroring `AuthRequired`'s existing
"resume back into the check, not past it" pattern, so an incomplete `input` response re-pauses on
whatever's still missing instead of quietly dispatching a call the tool's own schema says is
invalid.

**What does *not* get re-checked on resume, by design:** an `ApprovalRequired` checkpoint's
`resume_from` still points at `CredentialCheck`, not `Decide` — approving does not re-run the
policy lookup. If `policy.toml` changes between pause and resume, the original `ask` decision for
that specific call stands; only the gateway's own unconditional `Block` backstop
(`gateway::protocol::handle_tools_call`, §4) still catches a tool reclassified to `Block` in the
interim, because that check runs again on every dispatch regardless of any earlier agent-side
decision. A tool that changed from `ask` to a *different* `ask` reason, or from `ask` to `allow`,
in the interim goes undetected by design — re-running `Decide` on every resume was considered and
rejected for this POC, because it would let an approval be silently redirected to a different
pending state without the human who approved it ever being told. This is a documented trade-off,
not an oversight; a production system should make this choice deliberately (e.g. re-decide and
notify the approver if the outcome changed) rather than inherit this POC's default.

**Final verified behavior, the complete list** (full detail + real captured output: `README.md`
§21.1, `DEMO_GUIDE.md`'s "Error handling / failure scenarios"):

- **Gateway unavailable** → `{"status":"error",...}` identifying the unreachable call, never a
  HITL condition, never a checkpoint.
- **Downstream connector unavailable** → `{"status":"failed",...}` at `Dispatch`, never
  `AuthRequired`, never a false success.
- **Composio API failure** (bad key, 5xx, network error) → surfaces as a hard error, distinct in
  *kind* from Composio genuinely reporting no connected account.
- **Unknown tool** → the downstream connector's own `"unknown tool '...'"` message, a `Failed`
  outcome, never a checkpoint.
- **Unknown connector** → `404 unknown connector '...'`, never a misleading `AuthRequired` (fixed
  in this pass, see above).
- **Malformed arguments** → a clean validation error, never `InputRequired` (fixed in this pass).
- **Partial HITL input** → re-runs `SchemaCheck` and re-pauses `InputRequired` on whatever's still
  missing, rather than dispatching an incomplete call (fixed in this pass).
- **Invalid HITL action** for a checkpoint's pause reason → rejected, checkpoint stays `pending`.
- **Duplicate HITL response** → exactly one wins; the race loser is rejected
  (`CheckpointStore::claim_pending`'s atomic `UPDATE`).
- **Terminal checkpoint** (`denied`/`resolved`/`failed`) → cannot be resumed again.
- **Approved tool that fails downstream** → the checkpoint finalizes `failed` with the real error,
  never mistaken for a successful execution.
- **Auth claim re-verified** → `authenticate` always re-checks the real connection status on
  resume rather than trusting the human's claim.
- **Policy on resume** → not re-evaluated for `ApprovalRequired` (documented trade-off, above);
  `Block` is the one thing still enforced unconditionally at gateway dispatch.

**Classification enforced throughout:** every operational failure (gateway unreachable, downstream
connector unreachable, a Composio API error, a malformed request, an unknown tool or connector)
surfaces as `{"status":"error"}` or `{"status":"failed"}` — never `{"status":"pending",...}` — and
never persists a `Checkpoint`. Only a genuine policy/credential/schema fact ever produces a pause.
See `README.md`'s "Error handling / failure scenarios" section for the full scenario-by-scenario
table and `DEMO_GUIDE.md` for the same scenarios run by hand with real captured output.

This pass grew the suite from 34 to **49 tests (16 unit + 33 end-to-end)** — `cargo test` all
green, `cargo clippy --all-targets` zero warnings, `cargo fmt --check` clean — without changing the
agent-owns-HITL architecture described in §4, and without touching `nasiko-cloud-rs` or
`HITL-POC`.

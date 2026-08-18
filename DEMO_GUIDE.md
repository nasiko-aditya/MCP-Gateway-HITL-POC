# Demo Guide — MCP Gateway HITL POC

Runbook for demonstrating this POC end to end: commands, expected output, and behavior notes.
Every command has been run against the compiled binary in this repo, including a real
Composio-backed GitHub write (see section 9, "Real Composio flow").

`cargo test` passes 49 tests (16 unit + 33 end-to-end). `cargo clippy --all-targets` reports zero
warnings. `cargo fmt --check` is clean.

## 1. Prerequisites

- Rust (stable), `cargo`.
- `jq` (used to format the JSON output in every command below).
- No external database or network access required for the mock-connector path.
- Optional: real Composio credentials in `.env` (`COMPOSIO_API_KEY`, `COMPOSIO_AUTH_CONFIG_ID`)
  for section 9.

Commands below assume `AGENT=http://localhost:8090`, `GATEWAY=http://localhost:8080`.

## 2. Start the POC

```sh
cd MCP-Gateway-HITL-POC
rm -f hitl_poc.db
cat .env
cargo run
```

`rm -f hitl_poc.db` starts from a clean database. `cat .env` confirms whether
`COMPOSIO_API_KEY`/`COMPOSIO_AUTH_CONFIG_ID` are set.

Startup logs five lines: mock GitHub on `:8081`, mock Notion on `:8082`, `composio connector
ready` (or `COMPOSIO_API_KEY not set — running with mock connectors only` with no Composio
credentials), the gateway on `:8080`, and the agent on `:8090`. Run the server in one terminal
and the commands below in a second terminal.

### Architecture

```
                     User
                      │
                      ▼
                    Agent  (axum, :8090 — POST /agent/act)
                      │
                      ▼
          Agent-side pre-flight (Decide → CredentialCheck → SchemaCheck)
                      │
        ┌─────────────┴─────────────┐
        │                           │
   nothing needed              HITL if required
   (Allow, connected,          (ApprovalRequired /
    all fields present)         AuthRequired /
        │                       InputRequired)
        │                           │
        │                    human resolves via
        │                    POST /hitl/{id}/respond
        │                           │
        │                    Agent resumes
        │                    (same pre-flight loop,
        │                     picks up right after
        │                     the step that paused)
        └─────────────┬─────────────┘
                       ▼
                MCP Gateway  (axum, :8080 — POST /mcp)
              (Block backstop, reject bypassed
               Ask, else dispatch)
                       │
        ┌──────────────┼──────────────┐
        ▼              ▼              ▼
   Mock GitHub    Mock Notion    Composio (real,
      :8081          :8082        if configured)
                                       │
                                       ▼
                                Real GitHub API
```

Only the agent creates a `Checkpoint`. The gateway answers two read-only advisory questions
(`GET /policy/...`, `GET /connectors/.../status`), executes `tools/call`, and enforces two hard,
human-free backstops (`Block`, and refusing a bypassed `Ask`) — see section 8, "Gateway
backstop."

## 3. ALLOW

On a freshly reset database, `notion` has no stored credential, so calling it directly returns
`AuthRequired` (section 7). Seed a demo credential first — the mock/local connector's stand-in
for a completed OAuth flow:

### Command
```sh
curl -s -X POST localhost:8080/connectors/notion/credentials -H 'content-type: application/json' \
  -d '{"token":"demo-notion-token"}' | jq
```
```sh
curl -s localhost:8090/agent/act -H 'content-type: application/json' \
  -d '{"connector":"notion","tool_name":"search_pages","arguments":{}}' | jq
```

### Expected result
```json
{
  "call_id": "8a52fded-34d1-4140-b513-1e26ca7986ca",
  "result": { "arguments": {}, "connector": "notion", "status": "ok", "tool": "search_pages" },
  "status": "success"
}
```

### Behavior
A tool with no policy rule, a connected credential, and no missing arguments runs immediately —
no pause, no checkpoint. Skipping the credential-seed step produces `AuthRequired` instead
(section 7's mechanism), since `notion` and `github` go through the identical `CredentialCheck`.

## 4. BLOCK

### Command
```sh
curl -s localhost:8090/agent/act -H 'content-type: application/json' \
  -d '{"connector":"github","tool_name":"wipe_org","arguments":{}}' | jq
```

### Expected result
```json
{
  "call_id": "b74dfce7-e6ff-4a62-b9db-5a0bb87206bb",
  "reason": "Tool 'wipe_org' on connector 'github' is blocked by policy.",
  "status": "blocked"
}
```
No `checkpoint_id`. The call never appears in `GET /hitl/pending`.

### Behavior
`Block` is a terminal rejection decided at `Decide`, the first pre-flight step. The tool is
never dispatched and no human is asked, since a blocked tool has no decision for a human to
make.

## 5. ApprovalRequired

### Command
```sh
RESP=$(curl -s localhost:8090/agent/act -H 'content-type: application/json' \
  -d '{"connector":"github","tool_name":"delete_repo","arguments":{"repository":"acme/widgets"}}')
echo "$RESP" | jq
CP=$(echo "$RESP" | jq -r .checkpoint_id)
```

### Expected result
```json
{
  "call_id": "81afc367-9b1e-46d6-b508-464547ad0a76",
  "checkpoint_id": "d72ce3aa-f896-44fd-87ee-bb5598b1e0d8",
  "question": "Approval required: agent 'demo-agent' wants to call 'delete_repo' on connector 'github'. Approve or deny this tool call?",
  "reason": { "kind": "approval_required", "summary": "agent 'demo-agent' wants to call 'delete_repo' on connector 'github'" },
  "status": "pending"
}
```

Deny it — the tool must not execute:
```sh
curl -s -X POST localhost:8090/hitl/$CP/respond -H 'content-type: application/json' -d '{"action":"deny"}' | jq
curl -s localhost:8090/hitl/$CP | jq '.status, .result'
```
Result: `"denied"` and `null`.

Repeat and approve instead:
```sh
RESP=$(curl -s localhost:8090/agent/act -H 'content-type: application/json' \
  -d '{"connector":"github","tool_name":"delete_repo","arguments":{"repository":"acme/widgets"}}')
CP=$(echo "$RESP" | jq -r .checkpoint_id)
curl -s -X POST localhost:8090/hitl/$CP/respond -H 'content-type: application/json' -d '{"action":"approve"}' | jq
```

### Expected result
On a fresh database, `github` has not been authenticated either, so approving resumes into
`CredentialCheck` (`PauseReason::resume_from` for `ApprovalRequired` points there, not past it),
and the same checkpoint pauses again:
```json
{
  "checkpoint_id": "e9241794-62de-4a39-8522-5ad50f50237a",
  "question": "Authentication required for 'github'.",
  "reason": { "auth_url": null, "connector": "github", "kind": "auth_required" },
  "status": "pending"
}
```
Resolve that pause in the same terminal:
```sh
curl -s -X POST localhost:8090/hitl/$CP/respond -H 'content-type: application/json' \
  -d '{"action":"authenticate","token":"demo-token"}' | jq
```
```json
{
  "checkpoint_id": "e9241794-62de-4a39-8522-5ad50f50237a",
  "result": { "arguments": { "repository": "acme/widgets" }, "connector": "github", "status": "ok", "tool": "delete_repo" },
  "status": "resolved"
}
```

### Behavior
`policy.toml`'s `ask` stance pauses the call rather than rejecting it. Deny terminates it.
Approve resumes the pre-flight loop from the step right after `Decide` — it does not skip the
remaining checks, which is why approving here led into `CredentialCheck` rather than dispatching
directly. `policy.toml` stands in for per-agent tool configuration that in production comes from
Nasiko's own UI/backend.

This sequence also leaves `github` authenticated for the rest of this walkthrough — section 6
relies on it. Section 7 resets the database to demonstrate `AuthRequired` in isolation.

## 6. InputRequired

### Command
```sh
RESP=$(curl -s localhost:8090/agent/act -H 'content-type: application/json' \
  -d '{"connector":"github","tool_name":"get_latest_pr","arguments":{}}')
echo "$RESP" | jq
CP=$(echo "$RESP" | jq -r .checkpoint_id)
```
```sh
curl -s localhost:8090/hitl/$CP | jq
```

### Expected result
```json
{
  "call_id": "4caf2988-5f79-43e6-a16f-4cd7880fb0f8",
  "checkpoint_id": "c45f1aa7-e651-47b7-bf2f-3df36de20612",
  "question": "Missing required input: repository",
  "reason": {
    "kind": "input_required",
    "missing": [ { "description": "owner/repo, e.g. Nasiko-Labs/nasiko-cloud-rs", "field_type": "string", "name": "repository" } ]
  },
  "status": "pending"
}
```
`missing` is read from the tool's own `inputSchema`, fetched from the gateway's `tools/list` —
not hardcoded.

Provide the missing input:
```sh
curl -s -X POST localhost:8090/hitl/$CP/respond -H 'content-type: application/json' \
  -d '{"action":"input","fields":{"repository":"acme/widgets"}}' | jq
```
```json
{
  "checkpoint_id": "c45f1aa7-e651-47b7-bf2f-3df36de20612",
  "result": { "arguments": { "repository": "acme/widgets" }, "connector": "github", "status": "ok", "tool": "get_latest_pr" },
  "status": "resolved"
}
```
`result.arguments.repository` shows the human-supplied value merged into the original call
before dispatch.

### Behavior
Missing-input detection reads the tool's JSON Schema — the same mechanism used against
Composio's live GitHub schema in section 9. This call reaches `InputRequired` directly (not
`AuthRequired` first) because `github` was already authenticated in section 5; `CredentialCheck`
still runs before `SchemaCheck` on every call, it is just already satisfied here.

## 7. AuthRequired

By this point both mock connectors are authenticated (`notion` from section 3, `github` from
section 5), so calling either again succeeds immediately rather than pausing on `AuthRequired`.
To demonstrate the pause in isolation, reset the database:

In the terminal running the server, press Ctrl-C, then:
```sh
rm -f hitl_poc.db
cargo run
```

### Command
```sh
RESP=$(curl -s localhost:8090/agent/act -H 'content-type: application/json' \
  -d '{"connector":"github","tool_name":"list_repos","arguments":{}}')
echo "$RESP" | jq
CP=$(echo "$RESP" | jq -r .checkpoint_id)
```

### Expected result
```json
{
  "call_id": "a49bb387-b18d-4d07-a43f-c60e88191e9f",
  "checkpoint_id": "bcd789d3-1516-4b5c-9211-6bacb6a5c645",
  "question": "Authentication required for 'github'.",
  "reason": { "auth_url": null, "connector": "github", "kind": "auth_required" },
  "status": "pending"
}
```
`auth_url` is `null` because the mock/local connector authenticates with a plain demo token
rather than OAuth (section 9 covers the real `auth_url` case).

Resume it:
```sh
curl -s -X POST localhost:8090/hitl/$CP/respond -H 'content-type: application/json' \
  -d '{"action":"authenticate","token":"demo-token"}' | jq
```
```json
{
  "checkpoint_id": "bcd789d3-1516-4b5c-9211-6bacb6a5c645",
  "result": { "arguments": {}, "connector": "github", "status": "ok", "tool": "list_repos" },
  "status": "resolved"
}
```

### Behavior
Missing-credential detection is a status check (`GET /connectors/{connector}/status`), not an
LLM guess, and is generic across connector kinds — section 9 exercises the identical check
against a real Composio connection.

## 8. Gateway backstop

Confirms the gateway never independently triggers HITL, by calling `/mcp` directly and
bypassing the agent.

### Command
```sh
curl -s localhost:8080/mcp -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"github__wipe_org","arguments":{}}}' | jq
```

### Expected result
`{"error":{"code":-32000,...}}` — `Block` is enforced directly at the gateway, defense in
depth.

### Command
```sh
curl -s localhost:8080/mcp -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"github__delete_repo","arguments":{"repository":"a/b"}}}' | jq
```

### Expected result
`{"error":{"code":-32001,"message":"... route this call through the agent's HITL flow ..."}}`
— a distinct error from `Block`, refusing an ask-gated tool reached without going through the
agent.

Confirm no checkpoint was created:
```sh
curl -s localhost:8090/hitl/pending | jq
```
Unchanged from before the call.

### Behavior
The gateway stays a pure MCP execution layer. It refuses a blocked tool and an ask-gated tool
reached directly, but neither creates a checkpoint or asks a human — only `/agent/act` creates a
checkpoint.

## 9. Real Composio flow

Everything above also runs against a real Composio-backed GitHub connector — real OAuth, real
GitHub API calls. `./demo/demo_composio.sh` runs this in one shot; the commands below break it
into the same steps as sections 3–7.

### Read tool — first run, no connected account

```sh
RESP=$(curl -s localhost:8090/agent/act -H 'content-type: application/json' -d \
  '{"connector":"composio_github","tool_name":"GITHUB_LIST_REPOSITORIES_FOR_THE_AUTHENTICATED_USER","arguments":{}}')
echo "$RESP" | jq
CP=$(echo "$RESP" | jq -r .checkpoint_id)
```
`$CP` is only meaningful if `.status` above is `"pending"`. In interactive `zsh`, a trailing
`# comment` on a line that assigns a variable is parsed as a separate command named `#`, leaving
the real variable unset — keep notes on their own line, as done here.

### Expected result (first-ever run)
```json
{
  "checkpoint_id": "90fffb3f-bf84-4315-b329-41c959b16285",
  "question": "Authentication required for 'composio_github'. Complete auth at: https://connect.composio.dev/link/lk_lkO6fgVap8bf",
  "reason": {
    "auth_url": "https://connect.composio.dev/link/lk_lkO6fgVap8bf",
    "connector": "composio_github",
    "kind": "auth_required"
  },
  "status": "pending"
}
```
`auth_url` is a real OAuth URL, not `null`. Complete GitHub's consent screen at that URL, then
resume — tokenless, since this re-verifies the real connection rather than trusting a claim:
```sh
curl -s -X POST localhost:8090/hitl/$CP/respond -H 'content-type: application/json' \
  -d '{"action":"authenticate"}' | jq
```
```json
{
  "checkpoint_id": "a792c83e-b664-4837-a616-7df98834a8b9",
  "result": {
    "content": [{ "text": "{\"successful\":true,\"data\":{\"repositories\":[]},\"mercury_last_http_status_code\":200,\"log_id\":\"log_TCeOJ2EPzyv3\"}", "type": "text" }],
    "isError": false
  },
  "status": "resolved"
}
```
On later runs, the account is already connected, and the same command goes straight to
`"status":"success"`.

### Logout / re-trigger AuthRequired

`DELETE /connectors/{connector}/credentials` logs a connector out. For `composio_github` it
deletes the real connected account and best-effort revokes the GitHub OAuth grant
(`revoke_on_delete=true`); for a mock connector it deletes the local credential row.

```sh
curl -s -X DELETE localhost:8080/connectors/composio_github/credentials | jq
```
```json
{ "connector": "composio_github", "disconnected": true, "accounts_removed": 1 }
```
`accounts_removed: 0` is a valid, non-error result — it means the account was already logged
out. The next `/agent/act` call for `composio_github` pauses on `AuthRequired` with a fresh
`auth_url`. Same command for a mock connector, without an `accounts_removed` field:
```sh
curl -s -X DELETE localhost:8080/connectors/notion/credentials | jq
```

### Ask-gated write — `GITHUB_CREATE_AN_ISSUE`

```sh
RESP=$(curl -s localhost:8090/agent/act -H 'content-type: application/json' \
  -d '{"connector":"composio_github","tool_name":"GITHUB_CREATE_AN_ISSUE","arguments":{}}')
echo "$RESP" | jq
CP=$(echo "$RESP" | jq -r .checkpoint_id)
```
```json
{
  "checkpoint_id": "92064b66-41b6-44ae-bdea-77219b57c19c",
  "question": "Approval required: agent 'demo-agent' wants to call 'GITHUB_CREATE_AN_ISSUE' on connector 'composio_github'. Approve or deny this tool call?",
  "reason": { "kind": "approval_required", "summary": "agent 'demo-agent' wants to call 'GITHUB_CREATE_AN_ISSUE' on connector 'composio_github'" },
  "status": "pending"
}
```
Approve:
```sh
curl -s -X POST localhost:8090/hitl/$CP/respond -H 'content-type: application/json' -d '{"action":"approve"}' | jq
```
```json
{
  "checkpoint_id": "92064b66-41b6-44ae-bdea-77219b57c19c",
  "question": "Missing required input: repo, owner, title",
  "reason": {
    "kind": "input_required",
    "missing": [
      { "name": "repo",  "field_type": "string", "description": "...must exist, be accessible, and have issues enabled." },
      { "name": "owner", "field_type": "string", "description": "...must exist and be accessible to the authenticated user." },
      { "name": "title", "field_type": "string", "description": "The title for the new issue." }
    ]
  },
  "status": "pending"
}
```
This schema comes from Composio's live `tools/list` — real GitHub field names, not hardcoded in
this repo. `missing` only lists the tool's `required` fields (`owner`, `repo`, `title`); the same
schema also declares optional fields (`body`, `labels`, `assignees`, `milestone`, ...), which are
never flagged as missing but can be supplied the same way. Supply values for a repository under
the caller's control, including the issue body:
```sh
curl -s -X POST localhost:8090/hitl/$CP/respond -H 'content-type: application/json' -d \
  '{"action":"input","fields":{"owner":"<your-github-username>","repo":"<a-repo-you-own-with-issues-enabled>","title":"Live HITL demo","body":"Detailed description of the issue."}}' | jq
```

### Expected result (a real GitHub issue, created live)
```json
{
  "checkpoint_id": "92064b66-41b6-44ae-bdea-77219b57c19c",
  "result": {
    "content": [{ "text": "{\"successful\":true,\"data\":{\"html_url\":\"https://github.com/nasiko-aditya/HITL-MCP-POC/issues/4\",\"number\":4,...},\"mercury_last_http_status_code\":201,\"log_id\":\"log_VTybFhDvl5lN\"}", "type": "text" }],
    "isError": false
  },
  "status": "resolved"
}
```
`https://github.com/nasiko-aditya/HITL-MCP-POC/issues/4` is real — HTTP 201, a real issue
number, a real Composio `log_id`. Each run creates a new issue (numbers increment).

This response carries no `call_id` — only `/agent/act`'s first response does. To recover it for
the audit endpoint (section 11): `curl -s localhost:8090/hitl/$CP | jq -r .call_id`.

Run all of the above in one script, including the fallback message printed when the target repo
variables are unset:
```sh
COMPOSIO_ASK_OWNER=<your-github-username> \
COMPOSIO_ASK_REPO=<a-repo-you-own-with-issues-enabled> \
COMPOSIO_ASK_TITLE="Live HITL demo" \
./demo/demo_composio.sh
```

### Behavior
This is not a simulated OAuth flow or a fake tool response — it is the same agent → HITL →
resume → gateway code path as the mock demos, terminating in a real external system. If
Composio or GitHub is unreachable, or the API key lacks a permission, this surfaces as a
distinct error rather than being mislabeled as `AuthRequired` (see section 10).

### Composio diagnostics (raw API, for debugging)

These call Composio's API directly, bypassing this POC's gateway — useful for checking the
actual state Composio has when a pause doesn't behave as expected. All of them need
`COMPOSIO_API_KEY`/`COMPOSIO_AUTH_CONFIG_ID` loaded into the shell (`set -a && source .env &&
set +a`).

List every connected account for the demo user, any status:
```sh
curl -s "https://backend.composio.dev/api/v3/connected_accounts?user_ids=demo-user&auth_config_ids=$COMPOSIO_AUTH_CONFIG_ID" \
  -H "x-api-key: $COMPOSIO_API_KEY" | jq '.items[] | {id, status, status_reason, created_at, updated_at}'
```

Currently-connected accounts only (`ACTIVE`) — this is what `is_connected`, and therefore
`AuthRequired`'s trigger, checks:
```sh
curl -s "https://backend.composio.dev/api/v3/connected_accounts?user_ids=demo-user&auth_config_ids=$COMPOSIO_AUTH_CONFIG_ID" \
  -H "x-api-key: $COMPOSIO_API_KEY" | jq '.items[] | select(.status=="ACTIVE")'
```

One connected account by id:
```sh
curl -s "https://backend.composio.dev/api/v3/connected_accounts/<id>" \
  -H "x-api-key: $COMPOSIO_API_KEY" | jq
```

Auth config settings — toolkit, OAuth scheme, managed vs. custom, scopes:
```sh
curl -s "https://backend.composio.dev/api/v3/auth_configs/$COMPOSIO_AUTH_CONFIG_ID" \
  -H "x-api-key: $COMPOSIO_API_KEY" | jq
```

MCP servers created by this API key (confirms the one `ComposioConnector::bootstrap`
gets-or-creates at startup, `COMPOSIO_SERVER_NAME` by default):
```sh
curl -s "https://backend.composio.dev/api/v3/mcp/servers" \
  -H "x-api-key: $COMPOSIO_API_KEY" | jq '.items[] | {id, name, toolkits, allowed_tools}'
```

Verify a tool slug exists before pointing `COMPOSIO_ALLOWED_TOOLS`/`COMPOSIO_ASK_TOOL`/
`COMPOSIO_READ_TOOL` at it (HTTP 200 + full schema if real):
```sh
curl -s "https://backend.composio.dev/api/v3/tools/GITHUB_CREATE_AN_ISSUE" \
  -H "x-api-key: $COMPOSIO_API_KEY" | jq '{slug, name, input_parameters}'
```

Search tool slugs by keyword (fuzzy — confirm the exact slug with the command above):
```sh
curl -s "https://backend.composio.dev/api/v3/tools?toolkit_slug=github&search=issue" \
  -H "x-api-key: $COMPOSIO_API_KEY" | jq '.items[] | .slug'
```

Disconnect and revoke a connected account directly (the same call `DELETE
/connectors/composio_github/credentials` makes internally — use this form only to target a
specific account id):
```sh
curl -s -X DELETE "https://backend.composio.dev/api/v3/connected_accounts/<id>?revoke_on_delete=true" \
  -H "x-api-key: $COMPOSIO_API_KEY"
```

## 10. Error handling / failure scenarios

Sections 3–9 cover the happy path and the three HITL pauses. This section covers what happens
when something operational goes wrong — a bad tool/connector name, a malformed request, a stale
or duplicate HITL response — and confirms none of it is treated as a HITL condition. Run these
against the same live instance from section 2; none are destructive.

`github` needs a stored credential first (skip this if sections 4/5 already ran against this
instance), or the unknown-tool call below pauses on `AuthRequired` before reaching the
unknown-tool check:
```sh
curl -s -X POST localhost:8080/connectors/github/credentials -H 'content-type: application/json' \
  -d '{"token":"demo-token"}' | jq
```

### Unknown tool

```sh
curl -s -X POST localhost:8090/agent/act -H 'content-type: application/json' \
  -d '{"connector":"github","tool_name":"totally_bogus_tool","arguments":{}}' | jq
```
Result: `{"status":"failed","error":"unknown tool 'totally_bogus_tool'", ...}` — a `failed`
result, not a checkpoint, not `AuthRequired`.

### Unknown connector

```sh
curl -s -X POST localhost:8090/agent/act -H 'content-type: application/json' \
  -d '{"connector":"no_such_connector","tool_name":"x","arguments":{}}' | jq
```
Result: `{"status":"error","error":"...unknown connector 'no_such_connector'", ...}`. An
unregistered connector name previously behaved identically to an ordinary not-yet-connected
connector and paused on a misleading `AuthRequired`.

### Malformed arguments

```sh
curl -s -X POST localhost:8090/agent/act -H 'content-type: application/json' \
  -d '{"connector":"github","tool_name":"get_latest_pr","arguments":"oops-a-string"}' | jq
```
Result: `{"status":"error","error":"'arguments' must be a JSON object, got string"}` — a
validation error, not an `InputRequired` pause for a field that was never actually missing.

### Nonexistent checkpoint

```sh
curl -s -o /dev/stdout -w '\nHTTP %{http_code}\n' -X POST \
  localhost:8090/hitl/00000000-0000-0000-0000-000000000000/respond \
  -H 'content-type: application/json' -d '{"action":"approve"}'
```
Result: `HTTP 404` and `{"error":"no checkpoint '00000000-...'"}`.

### Wrong action for a checkpoint's pause reason

Pause on `delete_repo` (`ApprovalRequired`), then send `input` instead of `approve`/`deny`:
```sh
RESP=$(curl -s -X POST localhost:8090/agent/act -H 'content-type: application/json' \
  -d '{"connector":"github","tool_name":"delete_repo","arguments":{"repository":"a/b"}}')
CP=$(echo "$RESP" | jq -r .checkpoint_id)
curl -s -o /dev/stdout -w '\nHTTP %{http_code}\n' -X POST localhost:8090/hitl/$CP/respond \
  -H 'content-type: application/json' -d '{"action":"input","fields":{}}'
```
Result: `HTTP 400`, checkpoint remains `pending` (`curl -s localhost:8090/hitl/$CP | jq
.status`). Clean up: `curl -s -X POST localhost:8090/hitl/$CP/respond -d
'{"action":"deny"}'`.

### Duplicate response

Respond twice to the same checkpoint:
```sh
RESP=$(curl -s -X POST localhost:8090/agent/act -H 'content-type: application/json' \
  -d '{"connector":"github","tool_name":"delete_repo","arguments":{"repository":"a/b"}}')
CP=$(echo "$RESP" | jq -r .checkpoint_id)
curl -s -X POST localhost:8090/hitl/$CP/respond -d '{"action":"deny"}' | jq
curl -s -o /dev/stdout -w '\nHTTP %{http_code}\n' -X POST localhost:8090/hitl/$CP/respond \
  -d '{"action":"deny"}'
```
Result: first response `200`/`"denied"`, second `409` — exactly one response wins.

### Gateway / downstream connector unreachable

The gateway, agent, and both mock connectors run as tasks inside the one `cargo run` process
(`src/main.rs`), so there is no way to stop just one of them from outside without killing all of
them — this scenario is not hand-demoable against a running instance. `tests/hitl_flow.rs`
covers it deterministically (`gateway_unavailable_surfaces_clean_error_not_a_false_checkpoint`,
`downstream_connector_unavailable_surfaces_clean_failure_not_success_or_auth`), spinning up the
gateway/agent/connectors as independent axum apps with one deliberately pointed at a port
nothing listens on. `cargo test` takes one filter string at a time; `unavailable` matches both
test names and no others:
```sh
cargo test unavailable -- --nocapture
```

### Composio API failure

With real credentials configured, restart with a deliberately invalid `COMPOSIO_API_KEY`. The
connector's startup bootstrap (`ComposioConnector::bootstrap`) calls Composio's API immediately,
so this fails at process start with Composio's own error message (e.g. `composio 401
Unauthorized: {"error":{"message":"Invalid API key: sk-**...", "slug":"APIKey_InvalidAPIKey",
...}}`) rather than booting into a half-working state. Restore the real key in `.env` and
restart afterward. `gateway::composio`'s unit tests cover the distinction between this kind of
failure and a genuine "no connected account yet" deterministically, without live credentials:
```sh
cargo test --lib composio
```

### Other verified behaviors

Not separately hand-demoable against the shipped `policy.toml` without editing it mid-demo;
covered by the automated tests named below.

- A terminal checkpoint (`denied`/`resolved`/`failed`) cannot be resumed again — the
  duplicate-response case above uses `deny`, and the same `409` applies after `resolved` or
  `failed` (`completed_checkpoint_cannot_be_resumed_again`,
  `failed_checkpoint_cannot_be_resumed_again`).
- An approved tool can fail downstream; the checkpoint becomes `failed`, never a false success.
  Sections 5 and 9 both dispatch against tools that exist, so this repo has no ask-gated
  nonexistent tool to demonstrate it against directly —
  `dispatch_failure_after_resume_marks_checkpoint_failed` proves it: an ask-gated tool that
  doesn't exist on the mock connector pauses normally, and its approved resume finalizes
  `failed` with the real downstream error.
- `authenticate` always re-verifies the real connection rather than trusting the claim.
  Sections 7 and 9 supply a real token / complete real OAuth, so they resolve; calling
  `authenticate` with no token against a connector that is genuinely unauthenticated re-pauses
  on `AuthRequired` (`authenticate_without_a_real_credential_repauses_auth_required`).
- Policy changes between pause and resume are not re-evaluated for `ApprovalRequired`. Approving
  `delete_repo` in section 5 resumes at `CredentialCheck`, not back through `Decide` — if
  `policy.toml` changed that tool's stance in the interim, the original `ask` decision for that
  call stands. `Block` alone is enforced unconditionally on every dispatch, demonstrated
  directly in section 8. This is a documented trade-off, not a bug — see `HITL_POC.md` §16 and
  `README.md` §12, and `gateway_block_backstop_wins_even_with_preflight_approved_header`.

None of the cases in this section produce `{"status":"pending",...}` or a `Checkpoint` row
unless the condition is a genuine HITL pause. Operational failures surface as `error`/`failed`;
HITL conditions surface as `pending`. See `README.md`'s "Error handling / failure scenarios"
table for the full list and `HITL_POC.md` §16 for the reasoning.

## 11. Audit / inspection

The first `/agent/act` response for a task includes its `call_id`; capture it from `$RESP`
alongside `$CP`. `POST /hitl/{id}/respond` responses return `checkpoint_id` only — recover
`call_id` from the checkpoint if needed:

```sh
CALL_ID=$(curl -s localhost:8090/hitl/$CP | jq -r .call_id)
```

Full cross-service trail:
```sh
curl -s localhost:8090/audit/$CALL_ID | jq
```

Expected shape: `call_received` (agent) → `paused` (agent) → (`approved` / `authenticated` /
`input_provided`, agent) → `call_received` (gateway, on the resumed dispatch) → `success`
(gateway) → `resumed_success` (agent) — one chronologically ordered trail spanning both services
under one `call_id`. If an `authenticate` action was recorded, `detail` never contains the raw
token — it is `{"token_provided": true}`.

## 12. Cleanup

Stop the running instance:
```sh
pkill -f target/debug/mcp-gateway-hitl-poc
```
Reset state:
```sh
rm -f hitl_poc.db
```

`.env` holds real Composio credentials and is gitignored; do not commit it. Re-running
`demo_composio.sh` after a database reset re-triggers `AuthRequired` only if the underlying
Composio connected account was revoked — the database reset does not disconnect the real GitHub
OAuth grant, which lives in Composio, not `hitl_poc.db`.

## 13. Demo notes

- Nasiko's real MCP gateway today has three outcomes for `tools/call`: `ALLOW`, `BLOCK`, and
  `ASK` — where `ASK` is also a rejection. There is no way to approve one specific call, and
  nothing about the rejection is persisted.
- The first iteration of this POC fixed that but put the decision in the wrong place — the
  gateway itself decided when to pause. The current iteration moves that decision into the
  agent: the agent runs its own pre-flight (policy check, credential check, schema check), and
  only the agent creates a checkpoint. The gateway answers two factual questions (tool stance,
  connector status) and executes the call. It still refuses a blocked tool and an ask-gated tool
  reached without going through the agent, but neither is the gateway deciding to ask a human —
  section 8 demonstrates that boundary directly.
- All three HITL triggers are deterministic, not model-driven. Approval comes from
  `policy.toml`'s glob rules with `block > ask > allow` priority, standing in for per-agent tool
  configuration in Nasiko's own UI/backend. Missing-input detection reads the tool's own JSON
  Schema `required` array. Missing-auth detection is either a credential-row check (mock
  connector) or Composio's real connected-account status (live connector).
- A paused call is a `Checkpoint` — a database row with everything needed to resume it. A human
  resolves it through the HTTP API (`approve`/`deny`/`input`/`authenticate`), and the same task
  resumes exactly where it paused — no in-memory state, no LLM anywhere in the decision.
- This mechanism is not only proven against mocks — it is wired to a real, live Composio-backed
  GitHub connector: real OAuth, a real connected account, and a real GitHub issue created
  through this exact chain.
- Bringing this into the real Nasiko stack is wiring, not invention: the real agent gains this
  same pre-flight step, and the real gateway gains the two advisory endpoints it does not have
  today.

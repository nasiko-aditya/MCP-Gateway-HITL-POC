#!/usr/bin/env bash
# End-to-end demo of every HITL outcome against a running instance
# (`cargo run` in the repo root, default ports) — this time driven through
# the *agent's* task-intake endpoint (`POST /agent/act`), never the
# gateway's `/mcp` directly, proving the agent is what decides HITL is
# needed. Exercises, in order: AuthRequired -> authenticate -> resume,
# InputRequired -> input -> resume, ApprovalRequired -> deny, then approve,
# BLOCK (never even pauses), a second connector (Notion) to prove
# genericity, and finally the two gateway backstop checks that prove the
# gateway itself never independently creates a checkpoint.
#
# Usage: ./demo/demo.sh [agent_base] [gateway_base]
#   defaults: http://localhost:8090   http://localhost:8080

set -euo pipefail

AGENT="${1:-http://localhost:8090}"
GATEWAY="${2:-http://localhost:8080}"

if command -v jq >/dev/null 2>&1; then
  PP() { jq .; }
  FIELD() { jq -r "$1"; }
else
  PP() { cat; }
  FIELD() { python3 -c "import sys,json; d=json.load(sys.stdin); print(eval('d'+sys.argv[1]))" "$(echo "$1" | sed -E 's/\.([a-zA-Z_]+)/["\1"]/g')"; }
fi

step() { printf '\n\033[1;36m### %s\033[0m\n' "$1"; }
act() { curl -s -X POST "$AGENT/agent/act" -H 'content-type: application/json' -d "$1"; }
respond() { curl -s -X POST "$AGENT/hitl/$1/respond" -H 'content-type: application/json' -d "$2"; }
gateway_call() { curl -s -X POST "$GATEWAY/mcp" -H 'content-type: application/json' -d "$1"; }

step "tools/list — read directly from the gateway (a listing is not a HITL concern)"
gateway_call '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' | PP

step "Demo 3a — ApprovalRequired: agent decides github/delete_repo is ask-gated"
RESP=$(act '{"connector":"github","tool_name":"delete_repo","arguments":{"repository":"Nasiko-Labs/nasiko-cloud-rs"}}')
echo "$RESP" | PP
CP_APPROVE=$(echo "$RESP" | FIELD '.checkpoint_id')

step "Human denies the first attempt (tool must never execute)"
respond "$CP_APPROVE" '{"action":"deny"}' | PP

step "Demo 2 — AuthRequired: github/list_repos, no credential yet"
RESP=$(act '{"connector":"github","tool_name":"list_repos","arguments":{}}')
echo "$RESP" | PP
CP_AUTH=$(echo "$RESP" | FIELD '.checkpoint_id')

step "Human authenticates -> agent resumes -> gateway dispatches"
respond "$CP_AUTH" '{"action":"authenticate","token":"demo-token"}' | PP

step "Demo 1 — InputRequired: github/get_latest_pr, repository omitted (github is now authenticated)"
RESP=$(act '{"connector":"github","tool_name":"get_latest_pr","arguments":{}}')
echo "$RESP" | PP
CP_INPUT=$(echo "$RESP" | FIELD '.checkpoint_id')
CALL_ID_INPUT=$(echo "$RESP" | FIELD '.call_id')

step "Human supplies the missing repository -> resume -> dispatch"
respond "$CP_INPUT" '{"action":"input","fields":{"repository":"Nasiko-Labs/nasiko-cloud-rs"}}' | PP

step "Demo 3b — ApprovalRequired again, this time approved (github already authenticated)"
RESP=$(act '{"connector":"github","tool_name":"delete_repo","arguments":{"repository":"Nasiko-Labs/nasiko-cloud-rs"}}')
echo "$RESP" | PP
CP_APPROVE2=$(echo "$RESP" | FIELD '.checkpoint_id')
respond "$CP_APPROVE2" '{"action":"approve"}' | PP

step "BLOCK: github/wipe_org — rejected outright, never a checkpoint, never dispatched"
act '{"connector":"github","tool_name":"wipe_org","arguments":{}}' | PP

step "Genericity check — second connector (Notion), no GitHub-specific code involved: AuthRequired"
RESP=$(act '{"connector":"notion","tool_name":"search_pages","arguments":{}}')
echo "$RESP" | PP
CP_NOTION=$(echo "$RESP" | FIELD '.checkpoint_id')
step "Authenticate Notion -> resume -> dispatch"
respond "$CP_NOTION" '{"action":"authenticate","token":"demo-notion-token"}' | PP

step "Full audit trail for the InputRequired call above (agent + gateway entries, one call_id)"
curl -s "$AGENT/audit/$CALL_ID_INPUT" | PP

step "Pending checkpoints remaining (should be empty — every pause above was resolved)"
curl -s "$AGENT/hitl/pending" | PP

step "Backstop 1/2 — gateway still hard-blocks directly, defense in depth"
gateway_call '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"github__wipe_org","arguments":{}}}' | PP

step "Backstop 2/2 — gateway refuses an ask-gated tool reached directly, WITHOUT creating a checkpoint of its own"
gateway_call '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"github__delete_repo","arguments":{"repository":"a/b"}}}' | PP
echo "(compare: /hitl/pending above and after this call should be identically empty)"
curl -s "$AGENT/hitl/pending" | PP

echo -e "\nDemo complete."

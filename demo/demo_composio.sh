#!/usr/bin/env bash
# Live Composio demo — same agent -> HITL -> gateway flow as demo.sh, but
# against the one real, Composio-backed connector (see gateway/composio.rs)
# instead of the mocks. Requires a running instance (`cargo run`) that was
# itself started with COMPOSIO_API_KEY/COMPOSIO_AUTH_CONFIG_ID set (see
# .env.example) — this script only checks its *own* environment as a sanity
# check and cannot verify the server's, but the two should always be the
# same shell/`.env`.
#
# Usage: ./demo/demo_composio.sh [agent_base] [connector_name]
#   defaults: http://localhost:8090   composio_github
#
# The ask-gated write tool (GITHUB_CREATE_AN_ISSUE by default) always needs
# a real owner/repo/title — there's no generic value to guess, so set
# COMPOSIO_ASK_OWNER/COMPOSIO_ASK_REPO/COMPOSIO_ASK_TITLE to a repo you
# actually control to see the full approve -> input -> real dispatch flow.
# Without them the script still shows the real InputRequired pause (proving
# the schema came from Composio's live tool, not a mock) but says so
# honestly instead of implying a write happened.

set -euo pipefail

if [[ -z "${COMPOSIO_API_KEY:-}" || -z "${COMPOSIO_AUTH_CONFIG_ID:-}" ]]; then
  echo "Skipping: COMPOSIO_API_KEY / COMPOSIO_AUTH_CONFIG_ID are not set in this shell." >&2
  echo "Copy .env.example to .env, fill these in, restart 'cargo run', then re-run this script." >&2
  exit 0
fi

AGENT="${1:-http://localhost:8090}"
CONNECTOR="${2:-composio_github}"
READ_TOOL="${COMPOSIO_READ_TOOL:-GITHUB_LIST_REPOSITORIES_FOR_THE_AUTHENTICATED_USER}"
ASK_TOOL="${COMPOSIO_ASK_TOOL:-GITHUB_CREATE_AN_ISSUE}"

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

step "Composio demo — connector '$CONNECTOR', read tool '$READ_TOOL'"
RESP=$(act "$(printf '{"connector":"%s","tool_name":"%s","arguments":{}}' "$CONNECTOR" "$READ_TOOL")")
echo "$RESP" | PP

if [[ "$(echo "$RESP" | FIELD '.status')" == "pending" ]]; then
  REASON_KIND=$(echo "$RESP" | FIELD '.reason.kind')
  if [[ "$REASON_KIND" == "auth_required" ]]; then
    AUTH_URL=$(echo "$RESP" | FIELD '.reason.auth_url')
    CP_ID=$(echo "$RESP" | FIELD '.checkpoint_id')
    step "AuthRequired — this account has no live Composio connection yet"
    echo "Open this URL in a browser and complete the OAuth flow:"
    echo "  $AUTH_URL"
    if [[ -t 0 ]]; then
      echo "Press Enter once you've authorized it."
      read -r _
    else
      echo "Not running in an interactive terminal, so I can't wait for you here." >&2
      echo "Complete the OAuth flow above, then re-run this script — the account" >&2
      echo "will already be connected and this step will be skipped." >&2
      exit 0
    fi
    step "Re-checking status (tokenless 'authenticate' — see checkpoint.rs docs on AuthRequired's generic resume)"
    respond "$CP_ID" '{"action":"authenticate"}' | PP
  fi
fi

step "Composio demo — ask-gated write tool '$ASK_TOOL' (requires policy.toml's composio_github rule)"
RESP=$(act "$(printf '{"connector":"%s","tool_name":"%s","arguments":{}}' "$CONNECTOR" "$ASK_TOOL")")
echo "$RESP" | PP

WRITE_COMPLETED=0
if [[ "$(echo "$RESP" | FIELD '.status')" == "pending" ]]; then
  CP_ID=$(echo "$RESP" | FIELD '.checkpoint_id')
  step "Approving the real Composio-backed write action"
  RESP=$(respond "$CP_ID" '{"action":"approve"}')
  echo "$RESP" | PP

  if [[ "$(echo "$RESP" | FIELD '.status')" == "pending" ]]; then
    # ApprovalRequired resumed straight into InputRequired — the write
    # tool's own schema (fetched from Composio's live tools/list) needs
    # fields no generic default can supply, e.g. a repository to file an
    # issue against.
    if [[ -n "${COMPOSIO_ASK_OWNER:-}" && -n "${COMPOSIO_ASK_REPO:-}" ]]; then
      TITLE="${COMPOSIO_ASK_TITLE:-HITL MCP Gateway POC — live Composio demo issue}"
      step "Supplying the missing input (COMPOSIO_ASK_OWNER/COMPOSIO_ASK_REPO/COMPOSIO_ASK_TITLE) and dispatching for real"
      RESP=$(respond "$CP_ID" "$(printf '{"action":"input","fields":{"owner":"%s","repo":"%s","title":"%s"}}' \
        "$COMPOSIO_ASK_OWNER" "$COMPOSIO_ASK_REPO" "$TITLE")")
      echo "$RESP" | PP
      if [[ "$(echo "$RESP" | FIELD '.status')" == "resolved" ]]; then
        WRITE_COMPLETED=1
      fi
    else
      echo ""
      echo "Stopped here on purpose: '$ASK_TOOL' needs a real owner/repo/title —" >&2
      echo "there's no generic value to guess. Set COMPOSIO_ASK_OWNER and" >&2
      echo "COMPOSIO_ASK_REPO (a repo you control, with issues enabled) and" >&2
      echo "optionally COMPOSIO_ASK_TITLE, then re-run to see the real dispatch." >&2
      echo "Checkpoint '$CP_ID' is still pending — resolve it directly with:" >&2
      echo "  curl -s -X POST $AGENT/hitl/$CP_ID/respond -H 'content-type: application/json' \\" >&2
      echo "    -d '{\"action\":\"input\",\"fields\":{\"owner\":\"...\",\"repo\":\"...\",\"title\":\"...\"}}'" >&2
    fi
  elif [[ "$(echo "$RESP" | FIELD '.status')" == "resolved" ]]; then
    WRITE_COMPLETED=1
  fi
fi

echo ""
if [[ "$WRITE_COMPLETED" == "1" ]]; then
  echo "Composio demo complete — a real Composio-backed write actually executed."
else
  echo "Composio demo complete for the read path and the ApprovalRequired/"
  echo "InputRequired pauses. No real write was dispatched this run — see above"
  echo "for exactly why, and what to set to complete one."
fi

-- MCP Gateway HITL POC schema (SQLite).
--
-- A single `checkpoints` row is created the moment a `tools/call` pauses for
-- any reason (approval, missing auth, missing input) and is enough on its
-- own to resume the call later: it stores exactly the tool call that was
-- paused (`tool_name` + `tool_arguments`), why it paused (`reason`), and
-- which pipeline step to resume at (`resume_from`). No in-memory state is
-- required to resume — see src/pipeline.rs.

CREATE TABLE checkpoints (
    id              TEXT PRIMARY KEY,
    call_id         TEXT NOT NULL,
    user_id         TEXT NOT NULL,
    agent_id        TEXT NOT NULL,
    connector       TEXT NOT NULL,
    tool_name       TEXT NOT NULL,
    tool_arguments  TEXT NOT NULL, -- JSON
    reason          TEXT NOT NULL, -- JSON: {"kind": "approval_required"|"auth_required"|"input_required", ...}
    resume_from     TEXT NOT NULL, -- pipeline step to resume at: credential_check | schema_check | dispatch
    -- 'processing' is a short-lived transitional state: claim_pending() in
    -- src/checkpoint.rs flips a row from 'pending' to 'processing'
    -- atomically (an UPDATE ... WHERE status='pending'), which is what
    -- makes a duplicate human response or a second concurrent resume a
    -- safe no-op instead of a race — see CheckpointStore::claim_pending.
    status          TEXT NOT NULL CHECK (
                        status IN ('pending', 'processing', 'denied', 'resolved', 'failed', 'expired')
                    ),
    human_response  TEXT,          -- JSON, set once a human has acted
    result          TEXT,          -- JSON, set once the downstream tool call actually completes
    error           TEXT,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    expires_at      TEXT
);

CREATE INDEX idx_checkpoints_status ON checkpoints (status);
CREATE INDEX idx_checkpoints_call_id ON checkpoints (call_id);

-- Simulated credential store — a deliberate POC abstraction. Scoped per
-- connector only (not per user/agent as the real Nasiko does), and stores a
-- plaintext demo token rather than an encrypted OAuth credential. See
-- HITL_POC.md #10 "AuthRequired" for what a production version would need.
CREATE TABLE credentials (
    connector   TEXT PRIMARY KEY,
    token       TEXT NOT NULL,
    expires_at  TEXT,
    created_at  TEXT NOT NULL
);

-- Append-only audit trail. Never stores secrets — `detail` is a small JSON
-- object of non-sensitive fields only (see src/audit.rs).
CREATE TABLE audit_log (
    id             TEXT PRIMARY KEY,
    checkpoint_id  TEXT,
    call_id        TEXT NOT NULL,
    user_id        TEXT NOT NULL,
    agent_id       TEXT NOT NULL,
    connector      TEXT NOT NULL,
    tool_name      TEXT NOT NULL,
    action         TEXT NOT NULL,
    detail         TEXT NOT NULL, -- JSON
    created_at     TEXT NOT NULL
);

CREATE INDEX idx_audit_call_id ON audit_log (call_id);
CREATE INDEX idx_audit_checkpoint_id ON audit_log (checkpoint_id);

-- MCP submit_reply channel: per-turn arming plus buffered submissions.
-- armed_turns holds the one live arm per group: the executor writes it just
-- before a turn starts and the MCP server checks the presented token against
-- it on every submit_reply call, so a late or cross-turn submission can never
-- be committed. turn_submissions is the append-only record of every call;
-- the executor commits the last accepted row when the turn ends.
CREATE TABLE armed_turns (
    group_id TEXT PRIMARY KEY,
    turn_token TEXT NOT NULL,
    batch_id TEXT NOT NULL DEFAULT '',
    armed_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
);

CREATE TABLE turn_submissions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    group_id TEXT NOT NULL,
    turn_token TEXT NOT NULL,
    seq INTEGER NOT NULL,
    payload TEXT NOT NULL,
    verdict TEXT NOT NULL,
    reason TEXT NOT NULL DEFAULT '',
    created_at INTEGER NOT NULL
);

CREATE INDEX idx_turn_submissions_token ON turn_submissions (turn_token, seq);

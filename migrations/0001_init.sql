-- Initial schema for the Gemini QQ bot.
-- All QQ identifiers are decimal strings: JavaScript/JSON number precision must never
-- be part of the storage contract.

CREATE TABLE IF NOT EXISTS groups (
    bot_id              TEXT    NOT NULL,
    group_id            TEXT    NOT NULL,
    enabled             INTEGER NOT NULL DEFAULT 1,
    paused              INTEGER NOT NULL DEFAULT 0,
    agy_conversation_id TEXT,
    prompt_version      INTEGER NOT NULL DEFAULT 1,
    memory_revision     INTEGER NOT NULL DEFAULT 0,
    created_at          INTEGER NOT NULL,
    updated_at          INTEGER NOT NULL,
    PRIMARY KEY (bot_id, group_id)
);

CREATE TABLE IF NOT EXISTS group_members (
    bot_id        TEXT    NOT NULL,
    group_id      TEXT    NOT NULL,
    user_id       TEXT    NOT NULL,
    speaker_ref   TEXT    NOT NULL,
    first_seen_at INTEGER NOT NULL,
    last_seen_at  INTEGER NOT NULL,
    PRIMARY KEY (bot_id, group_id, user_id),
    UNIQUE (bot_id, group_id, speaker_ref),
    FOREIGN KEY (bot_id, group_id) REFERENCES groups (bot_id, group_id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS pending_events (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    bot_id             TEXT    NOT NULL,
    group_id           TEXT    NOT NULL,
    message_id         TEXT    NOT NULL,
    sender_id          TEXT    NOT NULL,
    received_at        INTEGER NOT NULL,
    normalized_payload TEXT    NOT NULL,
    image_refs         TEXT    NOT NULL DEFAULT '[]',
    batch_id           TEXT,
    UNIQUE (bot_id, message_id),
    FOREIGN KEY (bot_id, group_id) REFERENCES groups (bot_id, group_id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS seen_messages (
    bot_id     TEXT    NOT NULL,
    message_id TEXT    NOT NULL,
    seen_at    INTEGER NOT NULL,
    PRIMARY KEY (bot_id, message_id)
);

CREATE TABLE IF NOT EXISTS group_state (
    bot_id      TEXT    NOT NULL,
    group_id    TEXT    NOT NULL,
    mood_json   TEXT,
    topic_state TEXT,
    updated_at  INTEGER NOT NULL,
    PRIMARY KEY (bot_id, group_id),
    FOREIGN KEY (bot_id, group_id) REFERENCES groups (bot_id, group_id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS memories (
    memory_ref         TEXT    PRIMARY KEY,
    bot_id             TEXT    NOT NULL,
    group_id           TEXT    NOT NULL,
    subject_user_id    TEXT,
    content            TEXT    NOT NULL,
    kind               TEXT    NOT NULL,
    source_message_ids TEXT    NOT NULL DEFAULT '[]',
    confidence         REAL    NOT NULL DEFAULT 0.5,
    revision           INTEGER NOT NULL DEFAULT 1,
    created_at         INTEGER NOT NULL,
    updated_at         INTEGER NOT NULL,
    FOREIGN KEY (bot_id, group_id) REFERENCES groups (bot_id, group_id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS memories_group_idx ON memories (bot_id, group_id, updated_at DESC);

CREATE TABLE IF NOT EXISTS group_memory_summaries (
    bot_id          TEXT    NOT NULL,
    group_id        TEXT    NOT NULL,
    summary_text    TEXT    NOT NULL,
    source_revision INTEGER NOT NULL,
    model_id        TEXT    NOT NULL,
    updated_at      INTEGER NOT NULL,
    PRIMARY KEY (bot_id, group_id),
    FOREIGN KEY (bot_id, group_id) REFERENCES groups (bot_id, group_id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS memory_compaction_jobs (
    id              TEXT    PRIMARY KEY,
    bot_id          TEXT    NOT NULL,
    group_id        TEXT    NOT NULL,
    source_revision INTEGER NOT NULL,
    state           TEXT    NOT NULL,
    attempt_count   INTEGER NOT NULL DEFAULT 0,
    requested_by    TEXT    NOT NULL,
    created_at      INTEGER NOT NULL,
    started_at      INTEGER,
    finished_at     INTEGER,
    error_class     TEXT,
    FOREIGN KEY (bot_id, group_id) REFERENCES groups (bot_id, group_id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS compaction_jobs_group_idx
    ON memory_compaction_jobs (bot_id, group_id, created_at DESC);

CREATE TABLE IF NOT EXISTS agent_calls (
    id            TEXT    PRIMARY KEY,
    bot_id        TEXT    NOT NULL,
    group_id      TEXT    NOT NULL,
    batch_id      TEXT    NOT NULL,
    state         TEXT    NOT NULL,
    attempt_count INTEGER NOT NULL DEFAULT 0,
    started_at    INTEGER NOT NULL,
    finished_at   INTEGER,
    error_class   TEXT,
    FOREIGN KEY (bot_id, group_id) REFERENCES groups (bot_id, group_id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS agent_calls_group_idx ON agent_calls (bot_id, group_id, started_at DESC);

CREATE TABLE IF NOT EXISTS outbox (
    id         TEXT    PRIMARY KEY,
    bot_id     TEXT    NOT NULL,
    group_id   TEXT    NOT NULL,
    batch_id   TEXT    NOT NULL,
    text       TEXT    NOT NULL,
    state      TEXT    NOT NULL,
    message_id TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    FOREIGN KEY (bot_id, group_id) REFERENCES groups (bot_id, group_id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS outbox_state_idx ON outbox (state, created_at);

CREATE TABLE IF NOT EXISTS audit_events (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    actor_kind TEXT    NOT NULL,
    action     TEXT    NOT NULL,
    bot_id     TEXT,
    group_id   TEXT,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS persona_config (
    id         TEXT PRIMARY KEY,
    text       TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);

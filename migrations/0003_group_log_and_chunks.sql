-- L1 raw group log and L2 compacted chunks.
--
-- group_messages keeps every group message the bot saw, independent of the batch
-- that consumed it: pending_events is deleted after each turn, seen_messages only
-- keeps ids. Nothing here is ever deleted by the bot; archiving only flips a flag.

CREATE TABLE IF NOT EXISTS group_messages (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    bot_id       TEXT    NOT NULL,
    group_id     TEXT    NOT NULL,
    message_id   TEXT    NOT NULL,
    sender_id    TEXT    NOT NULL,
    display_name TEXT,
    speaker_role TEXT    NOT NULL,
    text         TEXT    NOT NULL,
    has_image    INTEGER NOT NULL DEFAULT 0,
    sticker_hash TEXT,
    created_at   INTEGER NOT NULL,
    archived     INTEGER NOT NULL DEFAULT 0,
    FOREIGN KEY (bot_id, group_id) REFERENCES groups (bot_id, group_id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS group_messages_recent_idx
    ON group_messages (bot_id, group_id, id DESC);

CREATE UNIQUE INDEX IF NOT EXISTS group_messages_dedup_idx
    ON group_messages (bot_id, message_id);

CREATE INDEX IF NOT EXISTS group_messages_unarchived_idx
    ON group_messages (bot_id, group_id, archived, id);

CREATE TABLE IF NOT EXISTS memory_chunks (
    chunk_id    TEXT    PRIMARY KEY,
    bot_id      TEXT    NOT NULL,
    group_id    TEXT    NOT NULL,
    seq         INTEGER NOT NULL,
    from_msg_id INTEGER NOT NULL,
    to_msg_id   INTEGER NOT NULL,
    summary     TEXT    NOT NULL,
    model_id    TEXT    NOT NULL,
    created_at  INTEGER NOT NULL,
    FOREIGN KEY (bot_id, group_id) REFERENCES groups (bot_id, group_id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS memory_chunks_group_idx
    ON memory_chunks (bot_id, group_id, seq DESC);

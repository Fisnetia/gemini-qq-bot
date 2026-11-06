-- Multi-part outbox rows and model-proposed nicknames.
-- part_index orders messages inside one batch. A later failure seals every
-- still-pending row in that batch; sent rows stay sent and are not resent.

ALTER TABLE outbox ADD COLUMN part_index INTEGER NOT NULL DEFAULT 0;
ALTER TABLE outbox ADD COLUMN part_count INTEGER NOT NULL DEFAULT 1;

CREATE TABLE IF NOT EXISTS speaker_aliases (
    bot_id     TEXT    NOT NULL,
    group_id   TEXT    NOT NULL,
    user_id    TEXT    NOT NULL,
    alias      TEXT    NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (bot_id, group_id, user_id),
    FOREIGN KEY (bot_id, group_id) REFERENCES groups (bot_id, group_id) ON DELETE CASCADE
);

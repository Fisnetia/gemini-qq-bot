-- Rolling handover notes: the shift-change note one conversation leaves for
-- its successor at the 35-turn rotation (and for resident rebuilds after a
-- crash). Refreshed in the background by the fold loop from the group's
-- recent messages; consumed by the seed brief so a fresh session opens
-- mid-conversation instead of cold. Deliberately separate from
-- group_memory_summaries, which is the L3 compaction product with a
-- different lifetime and audience.
CREATE TABLE group_handover_notes (
    bot_id TEXT NOT NULL,
    group_id TEXT NOT NULL,
    note_text TEXT NOT NULL,
    source_msg_id INTEGER NOT NULL DEFAULT 0,
    model_id TEXT NOT NULL DEFAULT '',
    updated_at INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (bot_id, group_id)
);

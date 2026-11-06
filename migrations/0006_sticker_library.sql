-- Two separate libraries that happen to share one table.
--
-- `stickers` is the RECOGNITION CACHE: one row per distinct image, globally shared, so a
-- repost never pays the vision model twice. It is keyed by the sha256 of the bytes and is
-- scoped to the bot, not to a group - the same picture posted in five groups is one row.
--
-- `stickers.kept` is HER LIBRARY: the subset she decided she likes and would actually
-- send. Only `kept = 1` rows are ever searchable by her; the rest exist purely so the
-- tagger can be skipped.
--
--   1  kept    - in her library, retrievable, sendable
--   0  unknown - tagged, she has not judged it yet, not retrievable
--  -1  refused - she looked at it and said no, never offer it again
--
-- `group_id` keeps the group where the image was first seen. It is provenance for the
-- panel, NOT a scope: her taste does not change per group.

ALTER TABLE stickers ADD COLUMN kept INTEGER NOT NULL DEFAULT 0;

CREATE INDEX IF NOT EXISTS stickers_kept_idx
    ON stickers (bot_id, kept, enabled);

-- Group discovery metadata (stage 3b).
--
-- Discovery itself already happens on the first message (the row is
-- created disabled and a single group.discovered audit row is written).
-- This migration adds the descriptive fields the console shows for a
-- discovered group — name and member count, fetched from OneBot at
-- discovery time (first message or group_increase notice) — plus the
-- departure marker set when the bot is removed from a group
-- (group_decrease). Nothing here changes reply behaviour: a departed
-- group's data is kept, only the marker is set.
--
-- Reversible: `ALTER TABLE groups DROP COLUMN departed_at` (and likewise
-- for member_count / name) restores the previous schema (SQLite >= 3.35).

ALTER TABLE groups ADD COLUMN name TEXT;
ALTER TABLE groups ADD COLUMN member_count INTEGER;
ALTER TABLE groups ADD COLUMN departed_at INTEGER;

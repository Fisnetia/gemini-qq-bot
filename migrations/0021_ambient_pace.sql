-- Per-group ambient pace (2026-10-29).
--
-- How eagerly the bot joins chatter that is not addressed to her is a
-- per-group operator choice, not a global constant: a lively group and
-- a quiet one want different thresholds for butting in. The panel sets
-- it through the admin API (`ambient_pace` on the group PATCH); intake
-- reads the column every tick, so a change takes effect without a
-- restart. Values: addressed_only | quiet | normal | lively. The
-- relevance gate that consumes the tier ships separately; this column
-- only carries the setting.
--
-- Reversible: `ALTER TABLE groups DROP COLUMN ambient_pace` restores
-- the previous schema (SQLite >= 3.35).

ALTER TABLE groups ADD COLUMN ambient_pace TEXT NOT NULL DEFAULT 'normal';

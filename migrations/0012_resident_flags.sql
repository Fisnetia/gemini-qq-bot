-- P4 resident sessions: per-group opt-in plus per-turn step accounting.
--
-- Both flags default to 0 so an upgraded deploy behaves byte-for-byte like the
-- previous release until an operator PATCHes a group. There is no migration path
-- that turns the feature on: enabling it is a deliberate, auditable act.

ALTER TABLE groups ADD COLUMN resident_enabled INTEGER NOT NULL DEFAULT 0;
ALTER TABLE groups ADD COLUMN resident_insert_enabled INTEGER NOT NULL DEFAULT 0;

-- Residency makes one agent call span several batches, so the call row must be
-- able to say how much step stream that covered. All three are nullable: rows
-- written by the print-mode path have no step stream to count.
ALTER TABLE agent_calls ADD COLUMN steps_count INTEGER;
ALTER TABLE agent_calls ADD COLUMN insert_count INTEGER;
ALTER TABLE agent_calls ADD COLUMN draft_count INTEGER;

-- Provenance and soft-retirement for memories.
--
-- `origin` records which writer produced the row: the per-turn agent contract
-- (`agy`), the operator panel (`panel`), or the fold-time extraction pass
-- (`hy3`). Every pre-existing row was written by the per-turn agent, so the
-- default is not a placeholder here: it is the accurate backfill.
--
-- `archived_at` is nullable and NULL means active. An archived row is kept for
-- the audit trail but is excluded from retrieval and from `memory_search`, and
-- no writer may update it. Nothing sets it yet; the column exists so the
-- retrieval and write paths can honour it.
ALTER TABLE memories ADD COLUMN origin TEXT NOT NULL DEFAULT 'agy';

ALTER TABLE memories ADD COLUMN archived_at INTEGER;

CREATE INDEX IF NOT EXISTS memories_active_idx
    ON memories (bot_id, group_id, archived_at, updated_at DESC);

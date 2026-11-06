-- Seed quarantine for upstream content-filter rejections (2026-10-28).
--
-- When the upstream model refuses a submitted prompt ("The prompt could not
-- be submitted. The prompt contains sensitive words…"), the resident session
-- that carried the exchange keeps getting refused on later rounds, and a
-- fresh session born with the same seed brief walks into the same wall: in
-- group 943 six minutes saw four sessions retired with the refusal misread
-- as a malformed contract. The refusal is not the session's fault, so it
-- gets its own error class — and the seed it was born with is quarantined
-- here by hash, so the next spawn degrades its seed down a ladder (drop the
-- newest folded segment, then a facts-only minimal seed) instead of
-- resubmitting the rejected bytes verbatim. Ten consecutive delivered
-- rounds earn the full seed back. Persisted because a deploy restart must
-- not hand the group its poisoned seed again.
CREATE TABLE IF NOT EXISTS seed_quarantine (
    bot_id TEXT NOT NULL,
    group_id TEXT NOT NULL,
    last_seed_hash TEXT,
    rejected_hash TEXT,
    tier INTEGER NOT NULL DEFAULT 0,
    clean_rounds INTEGER NOT NULL DEFAULT 0,
    updated_at INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (bot_id, group_id)
);

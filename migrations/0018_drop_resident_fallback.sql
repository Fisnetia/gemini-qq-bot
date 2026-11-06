-- Retire the resident fallback cooling-off columns.
--
-- 0013 made a fallback a cooling-off period backed by two `groups` columns: a
-- deadline and a consecutive count. In practice the deadline punished a healthy
-- group for one bad afternoon and, worse, the *existence* of a fallback path in
-- the turn loop meant a retired group quietly became a print group — the failure
-- mode this whole change removes.
--
-- The state now lives in memory in the supervisor (`respawn_backoff`), keyed by
-- group, with a short ladder (1m/5m/15m/30m) that the next success clears. A
-- restart forgetting the ladder is the correct behaviour: a fresh process has no
-- evidence against the group.
--
-- Both columns are dropped rather than left orphaned so nothing can start reading
-- them again. A nullable column keeps `INSERT ... DEFAULT` semantics elsewhere, so
-- no data rewrite is needed. `DROP COLUMN` needs SQLite >= 3.35.0; the toolchain
-- this ships with is 3.46.0.

ALTER TABLE groups DROP COLUMN resident_fallback_until;
ALTER TABLE groups DROP COLUMN resident_fallback_count;

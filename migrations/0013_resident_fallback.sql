-- P4 follow-up: a fallback is a cooling-off period, not a life sentence.
--
-- `resident_enabled` used to be one-way: the probe fell a group back and nothing
-- ever re-enabled it, so the only way back was an operator PATCH. That is too
-- blunt — a bad afternoon (proxy flap, a model outage, one bloated window) took
-- residency away until someone noticed. Now the fallback writes a deadline and a
-- count, and the group re-enters residency by itself once the deadline passes.
--
-- `resident_fallback_until` is nullable on purpose: NULL means "no fallback in
-- effect", which is also the state of every row that predates this migration, so
-- nothing needs rewriting and an upgraded deploy keeps behaving as before.
--
-- `resident_fallback_count` records how many consecutive fallbacks the group has
-- taken. The cooldown doubles with it (6h, 12h, 24h, capped at 48h), so a group
-- that keeps failing after every re-entry backs off instead of flapping.

ALTER TABLE groups ADD COLUMN resident_fallback_until INTEGER;
ALTER TABLE groups ADD COLUMN resident_fallback_count INTEGER NOT NULL DEFAULT 0;

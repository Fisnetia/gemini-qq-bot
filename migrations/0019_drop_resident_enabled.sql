-- Retire the per-group resident switch.
--
-- 0012 added `resident_enabled` as an opt-in: a group with it on ran the resident
-- PTY session, a group with it off ran `agy --print` once per batch. That second
-- lane is now gone entirely, so the column has no meaning: there is nothing to opt
-- into and nothing to opt out of, and keeping it would leave a switch that reads as
-- authoritative while changing nothing.
--
-- The engine failing is now a group-level silence by design, softened only by the
-- in-memory respawn ladder. `resident_insert_enabled` is a different flag (it lets a
-- *running* resident turn absorb newly claimed batches) and stays.
--
-- `DROP COLUMN` needs SQLite >= 3.35.0; the toolchain this ships with is 3.46.0.

ALTER TABLE groups DROP COLUMN resident_enabled;

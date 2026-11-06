-- Count successful Agy turns per group so the conversation can be rotated
-- before the remote session grows without bound. Resetting the conversation
-- id also zeroes this counter.
ALTER TABLE groups ADD COLUMN agy_turn_count INTEGER NOT NULL DEFAULT 0;

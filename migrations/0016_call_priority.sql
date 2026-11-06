-- Budget split: mark calls whose batch carried a direct @-mention, so the
-- hourly fuse can hold a reserve for addressed batches (see
-- BUDGET_PRIORITY_RESERVE in app/intake.rs). Existing rows default to 0
-- (ambient), which only ever tightens the ambient side for one window.
ALTER TABLE agent_calls ADD COLUMN priority INTEGER NOT NULL DEFAULT 0;

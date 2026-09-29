-- dagq-schema: compatible
-- The provider each headless job ran on (goal 73, task 1062), with the
-- values of a run's `requested_provider` / `actual_provider`. Every job
-- before this migration ran on Claude, so the rows written before it read
-- as `claude`, and so does a row an older binary inserts without naming
-- the column. An addition with a default only: an older binary never names
-- the column.
ALTER TABLE headless_jobs ADD COLUMN provider TEXT NOT NULL DEFAULT 'claude';

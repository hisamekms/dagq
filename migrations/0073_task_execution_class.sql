-- dagq-schema: compatible
-- A task's execution class (ADR-t1487-1 decision 1): `implementation` or
-- `spike` (`add --execution-class`, `edit --execution-class`), independent
-- of its change. Every task before this migration implements. No CHECK: a
-- binary reads a value it does not know as `implementation`. An addition
-- with a default only: an older binary never names the column, and its
-- inserts leave it `implementation`.
ALTER TABLE tasks ADD COLUMN execution_class TEXT NOT NULL DEFAULT 'implementation';

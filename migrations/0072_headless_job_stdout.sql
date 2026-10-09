-- dagq-schema: compatible
-- Where each headless job's stdout goes, so that a supervisor that stops
-- the job of a gone one reads the Execution of its agent from it
-- (ADR-t1486-1). Null for a row written before this migration and for one
-- an older binary inserts without naming the column: such a job's
-- Execution is recorded as not measured. An addition only.
ALTER TABLE headless_jobs ADD COLUMN stdout TEXT;

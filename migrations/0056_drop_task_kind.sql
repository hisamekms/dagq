-- dagq-schema: breaking
-- The task's kind is gone (ADR-t980-1 decision 1): the runs are grouped by
-- the change a task declares (0053) and the areas of what it landed, and
-- nothing reads `tasks.kind` any more. Dropping a column is not an
-- addition, so the migration is breaking: a binary from before it inserts
-- and updates the column by name and would fail on this schema. No
-- trigger, index or view names the column, so it drops in place.
ALTER TABLE tasks DROP COLUMN kind;

-- dagq-schema: breaking
-- The goal's priority is the source and a task inherits it (ADR-t1639-1).
-- goals.priority holds the five levels like tasks did (low=0 …
-- interrupt=4); every goal starts as normal (decision 1).
--
-- tasks.priority becomes the task's own setting, NULL when it inherits
-- its goal's (decision 2). SQLite cannot drop NOT NULL in place, so the
-- values move to a new nullable column that takes the old name. A normal
-- task reads as inheriting and any other level as its own setting
-- (decision 5): every goal is normal now, so no task's priority, nor its
-- effective priority, changes. No trigger, index or view names the
-- column. Breaking: a binary from before it reads the column as NOT NULL.
ALTER TABLE goals ADD COLUMN priority INTEGER NOT NULL DEFAULT 1;
ALTER TABLE tasks ADD COLUMN priority_own INTEGER;
UPDATE tasks SET priority_own = NULLIF(priority, 1);
ALTER TABLE tasks DROP COLUMN priority;
ALTER TABLE tasks RENAME COLUMN priority_own TO priority;

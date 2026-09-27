-- dagq-schema: compatible
-- Who wrote each event (ADR-t728-1 decision 4, task 730): the actor's role
-- (`actor_role`, a name of ADR-t728-1 decision 2 such as `user`, `worker`
-- or `supervisor`) and id (`actor_id`, such as `worker:<run>` or
-- `supervisor:<pid>`), and, when the supervisor applies the verdict of one
-- of its headless jobs, the id of that job (`requested_by`, such as
-- `review-job:<run>:<attempt>`). Rows written before this migration, and
-- by an older binary afterwards, leave them NULL. Additions only: an older
-- binary reads the table by column name and never writes these.
ALTER TABLE run_events ADD COLUMN actor_role TEXT;
ALTER TABLE run_events ADD COLUMN actor_id TEXT;
ALTER TABLE run_events ADD COLUMN requested_by TEXT;

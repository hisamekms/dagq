-- The two reads of the visibility of a small read of run_events; same
-- contract as SQLite's ledger_visibility_indexes migration, whose comments
-- name the read each index serves. Neither has a JSON expression.
CREATE INDEX ledger_rows_open_tasks ON ledger_rows(kind)
  WHERE kind = 'task' AND ended_at IS NULL;
CREATE INDEX ledger_rows_by_version ON ledger_rows(ledger_v)
  WHERE legacy = FALSE AND kind IN ('run', 'task', 'session', 'queue');

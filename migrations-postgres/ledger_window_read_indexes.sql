-- The indexes the range reads of the ledger's window look up; same contract
-- as SQLite's ledger_window_read_indexes migration, whose comments name the
-- read each index serves. Each expression is the Postgres dialect's
-- (SqlDialect::json_field, src/infrastructure/sql_dialect.rs) for the same
-- path and type a read writes, so a read built by the dialect hits it: the
-- text of a field is what SQLite's json_extract reads of a string, and the
-- times stay text in the form records write, compared as text.
-- `parallel` of a start is indexed as an integer, as SQLite's json_extract
-- reads it, so a read binds the same integer in both stores.

-- (1) The recorded marks by the time they take effect.
CREATE INDEX events_by_mark_at
  ON run_events(((jsonb_path_query_first((payload)::jsonb, ('$.at')::text::jsonpath) #>> '{}')), id)
  WHERE kind = 'mark_recorded';
CREATE INDEX events_by_pruned_heartbeat
  ON run_events(((jsonb_path_query_first((payload)::jsonb, ('$.last_heartbeat_at')::text::jsonpath) #>> '{}')::BIGINT), id)
  WHERE kind = 'supervisor_stopped';

-- (2) The retractions of a mark by the mark they name.
CREATE INDEX events_by_retracted_mark
  ON run_events(kind, ((jsonb_path_query_first((payload)::jsonb, ('$.mark')::text::jsonpath) #>> '{}')::BIGINT), id)
  WHERE kind = 'mark_retracted';

-- (3) The claims that recorded each derived attribute.
CREATE INDEX claims_with_dagq_version ON run_events(id)
  WHERE kind = 'run_claimed' AND (jsonb_path_query_first((payload)::jsonb, ('$.dagq_version')::text::jsonpath) #>> '{}') IS NOT NULL;
CREATE INDEX claims_with_claude_version ON run_events(id)
  WHERE kind = 'run_claimed' AND (jsonb_path_query_first((payload)::jsonb, ('$.claude_version')::text::jsonpath) #>> '{}') IS NOT NULL;
CREATE INDEX claims_with_codex_version ON run_events(id)
  WHERE kind = 'run_claimed' AND (jsonb_path_query_first((payload)::jsonb, ('$.codex_version')::text::jsonpath) #>> '{}') IS NOT NULL;
CREATE INDEX claims_with_parallel ON run_events(id)
  WHERE kind = 'run_claimed' AND (jsonb_path_query_first((payload)::jsonb, ('$.parallel')::text::jsonpath) #>> '{}') IS NOT NULL;
CREATE INDEX claims_with_toolchain ON run_events(id)
  WHERE kind = 'run_claimed' AND (jsonb_path_query_first((payload)::jsonb, ('$.rustc_release')::text::jsonpath) #>> '{}') IS NOT NULL
    AND (jsonb_path_query_first((payload)::jsonb, ('$.rustc_host')::text::jsonpath) #>> '{}') IS NOT NULL;

-- (4) The starts by the value they name.
CREATE INDEX starts_by_dagq_version
  ON run_events(((jsonb_path_query_first((payload)::jsonb, ('$.dagq_version')::text::jsonpath) #>> '{}')), id)
  WHERE kind = 'supervisor_started';
CREATE INDEX starts_by_parallel
  ON run_events(((jsonb_path_query_first((payload)::jsonb, ('$.parallel')::text::jsonpath) #>> '{}')::BIGINT), id)
  WHERE kind = 'supervisor_started';

-- (5) The order of time, one partial index per kind, and the events written
-- off the record's form, by the regular expression of the same form as
-- SQLite's GLOB (case-sensitive, the whole text).
CREATE INDEX starts_by_time ON run_events(created_at, id)
  WHERE kind = 'supervisor_started';
CREATE INDEX stops_by_time ON run_events(created_at, id)
  WHERE kind = 'supervisor_stopped';
CREATE INDEX run_env_changes_by_time ON run_events(created_at, id)
  WHERE kind = 'run_env_changed';
CREATE INDEX marks_by_time ON run_events(created_at, id)
  WHERE kind = 'mark_recorded';
CREATE INDEX retractions_by_time ON run_events(created_at, id)
  WHERE kind = 'mark_retracted';
CREATE INDEX task_ends_by_time ON run_events(created_at, id)
  WHERE kind = 'task_status_changed'
    AND (jsonb_path_query_first((payload)::jsonb, ('$.to')::text::jsonpath) #>> '{}') IN ('completed', 'canceled');
CREATE INDEX goal_ends_by_time ON run_events(created_at, id)
  WHERE kind = 'goal_closed';
CREATE INDEX events_by_time ON run_events(created_at, id);
CREATE INDEX events_off_the_record_form ON run_events(id)
  WHERE created_at !~ '^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}[.][0-9]{3}Z$';

-- (6) The ends of a forecast's target.
CREATE INDEX task_ends_by_task ON run_events(task_id, id)
  WHERE kind = 'task_status_changed'
    AND (jsonb_path_query_first((payload)::jsonb, ('$.to')::text::jsonpath) #>> '{}') IN ('completed', 'canceled');
CREATE INDEX goal_ends_by_goal ON run_events(goal_id, id)
  WHERE kind = 'goal_closed';

-- (7) The events that end a run (stats::window::ends_run).
CREATE INDEX run_ends_by_time ON run_events(created_at, id)
  WHERE run_id IS NOT NULL AND task_id IS NOT NULL
    AND (kind IN ('run_integrated', 'run_satisfied_closed')
         OR (jsonb_path_query_first((payload)::jsonb, ('$.status')::text::jsonpath) #>> '{}') IN ('failed', 'interrupted'));
CREATE INDEX run_ends_by_run ON run_events(run_id, id)
  WHERE run_id IS NOT NULL AND task_id IS NOT NULL
    AND (kind IN ('run_integrated', 'run_satisfied_closed')
         OR (jsonb_path_query_first((payload)::jsonb, ('$.status')::text::jsonpath) #>> '{}') IN ('failed', 'interrupted'));

-- (8) The rows of the ledger by the events of their inputs_through.
CREATE INDEX ledger_rows_by_inputs_through
  ON ledger_rows(kind, ((jsonb_path_query_first((inputs_through)::jsonb, ('$.events')::text::jsonpath) #>> '{}')::BIGINT));

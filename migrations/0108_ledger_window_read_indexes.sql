-- dagq-schema: compatible
-- The indexes the range reads of the ledger's window look up
-- (docs/plans/ledger-window-read-preconditions-spike.md, "読みの約束"): with
-- them a read of the range (S, E] reads the same events as a read of every
-- event, however the clocks went back or drifted, and no more than the range
-- and one look up per event of it, whatever the settled history outside it.
-- Indexes only: no event, no row of the ledger and no ledger_v changes
-- (ADR-t1662-2 decisions 1, 7 and 8). An older binary ignores them.
--
-- Each expression is the one the dialect writes (SqlDialect::json_field,
-- src/infrastructure/sql_dialect.rs) for the path the domain reads, and a
-- query hits an index only when it writes the same expression, the same
-- partial condition (`kind = '<literal>'`, not a bound kind) and compares
-- the expression with a bound value. The query each index serves is spelled
-- in src/infrastructure/runtime_store/ledger/window_read_plan_tests.rs, the
-- form the reads take; the row of the Spike's table it is for is named by
-- each index.

-- (1) The recorded marks by the time they take effect, three indexes whose
-- union is "範囲に効く記録の mark": the time a person's mark names
-- (domain::marks reads `at` of mark_recorded)...
CREATE INDEX events_by_mark_at
  ON run_events(json_extract(payload, '$.at'), id)
  WHERE kind = 'mark_recorded';
-- ...the last heartbeat a pruned stop stands at, in Unix seconds as written:
-- the effective time is it times 1000 as a time's text, so a read rounds its
-- range out to whole seconds and takes a little more, and the index needs no
-- date function...
CREATE INDEX events_by_pruned_heartbeat
  ON run_events(json_extract(payload, '$.last_heartbeat_at'), id)
  WHERE kind = 'supervisor_stopped';
-- ...and each recorded kind (domain::marks::RECORDED_KINDS) by the time it
-- was written, the effective time of every recorded mark but those two, one
-- index per kind below in (5).

-- (2) "その中の撤回できる mark の撤回": the retractions of a mark by the
-- mark they name, the first by id.
CREATE INDEX events_by_retracted_mark
  ON run_events(kind, json_extract(payload, '$.mark'), id)
  WHERE kind = 'mark_retracted';

-- (3) "属性ごとの前の claim": for each claim of the range, the last claim
-- before it in id that recorded the attribute, one index per attribute of
-- domain::marks::DERIVED. A claim records one when its value is there and
-- not JSON null (claim_value), the same as json_extract IS NOT NULL; the
-- toolchain is rustc_release and rustc_host together.
CREATE INDEX claims_with_dagq_version ON run_events(id)
  WHERE kind = 'run_claimed' AND json_extract(payload, '$.dagq_version') IS NOT NULL;
CREATE INDEX claims_with_claude_version ON run_events(id)
  WHERE kind = 'run_claimed' AND json_extract(payload, '$.claude_version') IS NOT NULL;
CREATE INDEX claims_with_codex_version ON run_events(id)
  WHERE kind = 'run_claimed' AND json_extract(payload, '$.codex_version') IS NOT NULL;
CREATE INDEX claims_with_parallel ON run_events(id)
  WHERE kind = 'run_claimed' AND json_extract(payload, '$.parallel') IS NOT NULL;
CREATE INDEX claims_with_toolchain ON run_events(id)
  WHERE kind = 'run_claimed' AND json_extract(payload, '$.rustc_release') IS NOT NULL
    AND json_extract(payload, '$.rustc_host') IS NOT NULL;

-- (4) "前の claim とその claim の間で新しい値を名乗る起動": a start between
-- two claims in id that names the claim's new value, for the two attributes
-- a start names. `parallel` is an integer (JsonType::Integer, the integer
-- the supervisor writes) in both stores: a read binds the claim's value as
-- an integer, not as the text marks::claim_value compares.
CREATE INDEX starts_by_dagq_version
  ON run_events(json_extract(payload, '$.dagq_version'), id)
  WHERE kind = 'supervisor_started';
CREATE INDEX starts_by_parallel
  ON run_events(json_extract(payload, '$.parallel'), id)
  WHERE kind = 'supervisor_started';

-- (5) The order of time. By kind, an index on (created_at, id) for each
-- kind a read takes by the time it was written: the recorded kinds of (1)
-- ("範囲に効く記録の mark"), the last supervisor_started at S or before by
-- (created_at, id) ("routine の直前の起動"), and the ends a forecast scores
-- written in the range ("forecast の終わり": a task completed or canceled, a
-- goal closed). One partial index per kind, not one on (kind, created_at,
-- id): that one would be the planner's pick for every `kind=?` read, and
-- takes it off events_by_run and events_by_kind; a partial index serves only
-- a read that writes its kind as a literal.
--
-- Of every kind, events_by_time: "event の id の cursor の時刻", the search
-- that walks (created_at, id) back K events, and the aggregate
-- MAX(created_at) of the events up to an id, which walks the index's
-- entries and none of the payloads.
--
-- A time's text orders as its time only in the form records write: digits
-- where the form has them, an upper-case T and Z (SqlDialect::off_the_record_form,
-- stricter than domain::ledger::window::RECORD_TIME_FORM, whose LIKE ignores
-- the case here and takes any character for a digit). Whether an event was
-- written off it is the read of events_off_the_record_form: the index holds
-- only those events, so `SELECT EXISTS (SELECT 1 FROM run_events WHERE
-- created_at NOT GLOB '<the pattern below>')`, written with the same
-- condition, reads them and nothing else, none on a queue whose clocks
-- always wrote the form, however long its history. With one, a read by time
-- goes back to every event. GLOB is case-sensitive and matches the whole
-- text, so 'T…z', 't', a space for the T, a missing millisecond and a
-- trailing character are all off the form.
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
    AND json_extract(payload, '$.to') IN ('completed', 'canceled');
CREATE INDEX goal_ends_by_time ON run_events(created_at, id)
  WHERE kind = 'goal_closed';
CREATE INDEX events_by_time ON run_events(created_at, id);
CREATE INDEX events_off_the_record_form ON run_events(id)
  WHERE created_at NOT GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9].[0-9][0-9][0-9]Z';

-- (6) "forecast の前の終わりと target の最初の event": for each end in the
-- range, the end of the same target before it in id (a task completed or
-- canceled, a goal closed).
CREATE INDEX task_ends_by_task ON run_events(task_id, id)
  WHERE kind = 'task_status_changed'
    AND json_extract(payload, '$.to') IN ('completed', 'canceled');
CREATE INDEX goal_ends_by_goal ON run_events(goal_id, id)
  WHERE kind = 'goal_closed';

-- (7) "run の終わり": the events that end a run as stats::window::ends_run
-- reads them, by time for the ends of the range and by run for the run's
-- first end.
CREATE INDEX run_ends_by_time ON run_events(created_at, id)
  WHERE run_id IS NOT NULL AND task_id IS NOT NULL
    AND (kind IN ('run_integrated', 'run_satisfied_closed')
         OR json_extract(payload, '$.status') IN ('failed', 'interrupted'));
CREATE INDEX run_ends_by_run ON run_events(run_id, id)
  WHERE run_id IS NOT NULL AND task_id IS NOT NULL
    AND (kind IN ('run_integrated', 'run_satisfied_closed')
         OR json_extract(payload, '$.status') IN ('failed', 'interrupted'));

-- (8) The rows of the ledger written after a mark: the session and queue
-- rows whose inputs reach past W or R, by the events of their
-- inputs_through, for the read's visibility.
CREATE INDEX ledger_rows_by_inputs_through
  ON ledger_rows(kind, json_extract(inputs_through, '$.events'));

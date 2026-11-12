-- The views of the queue rows and the indexes of run_events their seeks take,
-- as the SQLite migration of the same name makes them; each expression is
-- the Postgres dialect's (SqlDialect::ledger_index_key) for the same key.
CREATE TABLE ledger_snapshot_targets (
    snapshot_event_id BIGINT NOT NULL,
    task_id BIGINT NOT NULL,
    PRIMARY KEY (snapshot_event_id, task_id)
);
CREATE INDEX snapshot_targets_by_task
  ON ledger_snapshot_targets(task_id, snapshot_event_id);
CREATE TABLE ledger_main_commits (
    event_id BIGINT NOT NULL,
    sha TEXT NOT NULL,
    reverts TEXT,
    PRIMARY KEY (event_id, sha)
);
CREATE INDEX main_commits_by_sha ON ledger_main_commits(sha, event_id);
CREATE INDEX main_commits_by_reverts ON ledger_main_commits(reverts, event_id)
  WHERE reverts IS NOT NULL;
CREATE TABLE ledger_main_summaries (
    through BIGINT PRIMARY KEY,
    at_ms BIGINT,
    summary TEXT NOT NULL
);
CREATE INDEX main_summaries_by_time ON ledger_main_summaries(at_ms, through);
CREATE TABLE ledger_answered_asks (
    key TEXT NOT NULL,
    what TEXT NOT NULL,
    PRIMARY KEY (key, what)
);
CREATE INDEX answered_asks_by_ask ON ledger_answered_asks(what, key);
CREATE TABLE ledger_queue_views_filled (
    through TEXT,
    generation BIGINT
);

CREATE INDEX ledger_rows_queue_views ON ledger_rows(inputs_from)
  WHERE kind = 'queue' AND subject_kind IN ('forecast_recorded', 'main_commits_recorded', 'main_reverts_imported', 'main_paths_recorded', 'main_observed', 'main_read_failed', 'main_read_recovered', 'main_rewritten', 'ci_checked', 'ask_opened', 'ask_answered');

CREATE INDEX events_by_kind_supervisor
  ON run_events(kind, (payload::jsonb ->> 'supervisor'), id)
  WHERE (payload::jsonb ->> 'supervisor') IS NOT NULL;
CREATE INDEX asks_by_id ON run_events((payload::jsonb ->> 'ask_id'), id)
  WHERE kind = 'ask_opened';
CREATE INDEX ci_reds_by_sha ON run_events((payload::jsonb ->> 'sha'), id)
  WHERE kind = 'ci_turned_red';
CREATE INDEX finding_changes_by_proposal
  ON run_events((payload::jsonb ->> 'proposal_id'), id)
  WHERE kind = 'finding_status_changed';
CREATE INDEX attribute_baseline_marks ON run_events(id)
  WHERE kind = 'attributes_baseline' AND task_id IS NULL AND goal_id IS NULL;
CREATE INDEX finding_baseline_marks ON run_events(id)
  WHERE kind = 'finding_baseline' AND (payload::jsonb ->> 'finding') IS NULL;

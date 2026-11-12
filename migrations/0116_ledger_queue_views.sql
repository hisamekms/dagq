-- dagq-schema: compatible
-- The views of the queue rows a read from a time looks up instead of walking
-- the settled history (domain::ledger::queue_views;
-- docs/plans/ledger-window-earlier-visibility-spike.md, "追試 (ii) の残り"
-- "判定"), and the indexes of run_events its seeks take. Each table is
-- written with the queue row it is made of, in the row's transaction:
-- ledger_snapshot_targets the tasks each forecast_recorded names,
-- ledger_main_commits each commit a main_commits_recorded records with the
-- commit it reverts, ledger_main_summaries main's record and CI's checks up
-- to an event (`through`; `at_ms` its time), one kept every
-- queue_views::SUMMARY_EVERY events and the last, ledger_answered_asks the
-- key of the ask (AskRef::key, as domain::ledger::open::Left::text writes
-- it) each run-less ask_answered row answers. The run-less asks no answer
-- closed go to ledger_open as the queue rows' entries.
-- ledger_queue_views_filled is the mark of how far the queue rows written
-- before the tables were taken in, kept through the write leader's
-- generations as ledger_open_filled is: an older binary that writes a row
-- without the views leaves the mark behind, and a reader reads the whole
-- pages until the filling took every row in again. Views: rebuilt of the
-- rows alone, they change no row of the ledger and no ledger_v
-- (ADR-t1662-2 decisions 1, 7 and 8). Additions only.
CREATE TABLE ledger_snapshot_targets (
    snapshot_event_id INTEGER NOT NULL,
    task_id INTEGER NOT NULL,
    PRIMARY KEY (snapshot_event_id, task_id)
);
CREATE INDEX snapshot_targets_by_task
  ON ledger_snapshot_targets(task_id, snapshot_event_id);
CREATE TABLE ledger_main_commits (
    event_id INTEGER NOT NULL,
    sha TEXT NOT NULL,
    reverts TEXT,
    PRIMARY KEY (event_id, sha)
);
CREATE INDEX main_commits_by_sha ON ledger_main_commits(sha, event_id);
CREATE INDEX main_commits_by_reverts ON ledger_main_commits(reverts, event_id)
  WHERE reverts IS NOT NULL;
CREATE TABLE ledger_main_summaries (
    through INTEGER PRIMARY KEY,
    at_ms INTEGER,
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
    generation INTEGER
);

-- The queue rows the views are made of, by their event, for the filling.
CREATE INDEX ledger_rows_queue_views ON ledger_rows(inputs_from)
  WHERE kind = 'queue' AND subject_kind IN ('forecast_recorded', 'main_commits_recorded', 'main_reverts_imported', 'main_paths_recorded', 'main_observed', 'main_read_failed', 'main_read_recovered', 'main_rewritten', 'ci_checked', 'ask_opened', 'ask_answered');

-- The seeks of run_events, each expression SqlDialect::ledger_index_key's.
-- A supervisor's last event of a kind before S, walked back in id: the
-- events naming a supervisor (their lives, heartbeats and waits).
CREATE INDEX events_by_kind_supervisor
  ON run_events(kind, (json_extract(payload,'$.supervisor')), id)
  WHERE json_extract(payload,'$.supervisor') IS NOT NULL;
-- The ask an answer of the range names.
CREATE INDEX asks_by_id ON run_events((json_extract(payload,'$.ask_id')), id)
  WHERE kind = 'ask_opened';
-- The turns to red of CI on a landed commit.
CREATE INDEX ci_reds_by_sha ON run_events((json_extract(payload,'$.sha')), id)
  WHERE kind = 'ci_turned_red';
-- The findings that link a proposal the range names.
CREATE INDEX finding_changes_by_proposal
  ON run_events((json_extract(payload,'$.proposal_id')), id)
  WHERE kind = 'finding_status_changed';
-- The latest mark of each baseline before S, walked back in id: the marks
-- alone, not the baselines' records.
CREATE INDEX attribute_baseline_marks ON run_events(id)
  WHERE kind = 'attributes_baseline' AND task_id IS NULL AND goal_id IS NULL;
CREATE INDEX finding_baseline_marks ON run_events(id)
  WHERE kind = 'finding_baseline' AND json_extract(payload,'$.finding') IS NULL;

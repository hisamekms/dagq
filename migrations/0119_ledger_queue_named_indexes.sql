-- dagq-schema: compatible
-- The seeks a read from a time makes of what the range names of the queue
-- rows read by the views (src/infrastructure/runtime_store/ledger/queue_named.rs;
-- docs/plans/ledger-window-earlier-visibility-spike.md, "追試 (ii) の残り"
-- "判定" (E) and (F)): a finding's row by the finding, read whole when a
-- finding links a proposal the range names, and a goal's submissions down
-- the ids, the proposal that holds a goal named among them, and a goal's
-- closes, which a forecast's score reads of a goal named. Indexes only:
-- ledger rows and ledger_v keep their shape (ADR-t1662-2 decisions 1, 7,
-- 8); older binaries may ignore them.
CREATE INDEX ledger_rows_findings ON ledger_rows(subject_id)
  WHERE kind = 'queue' AND subject_kind = 'finding';
CREATE INDEX goal_submissions ON run_events(goal_id, id)
  WHERE kind = 'goal_submitted';
CREATE INDEX goal_closes ON run_events(goal_id, id)
  WHERE kind = 'goal_closed';

-- A late CI check never makes a reader walk older main summaries. Only
-- branches whose summary reaches past S need their first/last check at S;
-- each queue row holds one event and ended_at is its parsed time.
CREATE INDEX ledger_ci_by_branch_time ON ledger_rows(
  (json_extract(records,'$[0].payload.branch')), ended_at, inputs_from)
  WHERE kind = 'queue' AND subject_kind = 'ci_checked';

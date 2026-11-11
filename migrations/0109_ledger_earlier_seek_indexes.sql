-- dagq-schema: compatible
-- The earlier seeks of docs/plans/ledger-window-earlier-visibility-spike.md:
-- "判定" (i), "読みの約束" 6, and "追試 (i) earlier" / "判定".
-- Indexes only: events, ledger rows and ledger_v keep their shape
-- (ADR-t1662-2 decisions 1, 7, 8); older binaries may ignore the indexes.
-- JSON expressions match SqlDialect::ledger_index_key(column, key).
-- Bind IDs as native integers in SQLite and as text in Postgres, without
-- casting the indexed expression in the query (ADR-t3982-1).

-- (i-b) member_end: each task's creation, baseline, latest goal/status
-- change, and reopening after that status, by task_id and kind in id order.
CREATE INDEX events_by_task_kind ON run_events(task_id, kind, id);

-- (i-d) goal -> tasks: creation and attributes_baseline restore goal_id
-- (ledger::attributes::apply_task); task_goal_changed stores its goal in to.
CREATE INDEX events_created_by_goal ON run_events((json_extract(payload,'$.goal_id')))
  WHERE kind = 'task_created';
CREATE INDEX events_baseline_by_goal ON run_events((json_extract(payload,'$.goal_id')))
  WHERE kind = 'attributes_baseline';
CREATE INDEX events_changed_by_goal ON run_events((json_extract(payload,'$.to')))
  WHERE kind = 'task_goal_changed';

-- (i) task records and improvement_proposals membership; (i-d) proposal -> goal.
CREATE INDEX events_submitted_by_proposal ON run_events((json_extract(payload,'$.proposal_id')))
  WHERE kind = 'task_submitted';
CREATE INDEX events_reopened_by_proposal ON run_events((json_extract(payload,'$.proposal_id')))
  WHERE kind = 'task_reopened';
CREATE INDEX events_goals_by_proposal ON run_events((json_extract(payload,'$.proposal_id')))
  WHERE kind = 'goal_submitted';
-- (i) task records naming a plan review, including task_created.
CREATE INDEX events_by_plan_review ON run_events((json_extract(payload,'$.plan_review_id')));

-- (i) follow_up_registered names a task in payload, not the event task_id.
CREATE INDEX follow_ups_by_task ON run_events((json_extract(payload,'$.task_id')))
  WHERE kind = 'follow_up_registered';

-- Promise 6: the last queue row of a subject_kind ending before S.
CREATE INDEX ledger_rows_by_subject_end ON ledger_rows(kind, subject_kind, ended_at);

-- (i-a) session keys: kind = 'session' AND key >= 'runtime_planner/<id>/'
-- AND key < 'runtime_planner/<id>0' (and plan_review likewise).
-- The existing PRIMARY KEY (kind, key) serves these prefixes; no new index.

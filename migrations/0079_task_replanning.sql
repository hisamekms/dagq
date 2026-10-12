-- dagq-schema: compatible
-- The replanning of a long-running task's run (ADR-t1521-1, ADR-t1521-2).
--
-- replan_operations: one row per operation on a run, from a person's
-- request (`origin` manual), a worker's proposal on its own run (worker)
-- or the runtime's detection (runtime). `status` is where it is
-- (domain::task_replan::ReplanStatus) and `status_reason` why;
-- `generation` rises at the pause's fence; `return_phase` is the step a
-- `blocked` one goes back to. `conditions` is the JSON copy of the task's
-- and its goal's conditions as it began, `task_version` their SHA-256 and
-- `goal_acceptance_version` the goal's acceptance version then.
-- `diagnosis` is the JSON of its evidence and decision. `snapshot_id`,
-- `request_id` and `proposal_id` are the saved work, the planning request
-- and the replacement proposal. One open operation per run: each write
-- transaction that opens one inserts it only when the run has none open,
-- so none opens two.
--
-- replan_mappings: how each item of the task's acceptance (`ordinal`,
-- `hash`) went to the replacement: the child task and its item, or none
-- with the authority of a deletion.
--
-- replan_replacements: the tasks an applied operation replaced its task
-- with.
--
-- plan_requests.replan_operation_id: the operation a planning request was
-- recorded for; NULL for every other request.
--
-- Additions only: an older binary ignores the tables and never names the
-- column. No REFERENCES and no unique index, which a compatible migration
-- may not add.
CREATE TABLE replan_operations (
    id INTEGER PRIMARY KEY,
    task_id INTEGER NOT NULL,
    run_id TEXT NOT NULL,
    goal_id INTEGER,
    origin TEXT NOT NULL,
    status TEXT NOT NULL,
    status_reason TEXT,
    generation INTEGER NOT NULL DEFAULT 1,
    return_phase TEXT,
    reason TEXT NOT NULL,
    conditions TEXT NOT NULL,
    task_version TEXT NOT NULL,
    goal_acceptance_version INTEGER,
    diagnosis TEXT NOT NULL DEFAULT '{}',
    snapshot_id TEXT,
    request_id INTEGER,
    proposal_id INTEGER,
    requested_by TEXT NOT NULL,
    requested_by_id TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    closed_at INTEGER
);
CREATE INDEX replan_operations_by_task ON replan_operations(task_id, closed_at);
CREATE INDEX replan_operations_by_run ON replan_operations(run_id, closed_at);
CREATE TABLE replan_mappings (
    operation_id INTEGER NOT NULL,
    ordinal INTEGER NOT NULL,
    hash TEXT NOT NULL,
    child_task_id INTEGER,
    child_ordinal INTEGER,
    disposition TEXT NOT NULL,
    authority TEXT,
    PRIMARY KEY (operation_id, ordinal, child_task_id)
);
CREATE TABLE replan_replacements (
    operation_id INTEGER NOT NULL,
    from_task_id INTEGER NOT NULL,
    to_task_id INTEGER NOT NULL,
    PRIMARY KEY (operation_id, to_task_id)
);
CREATE INDEX replan_replacements_by_from ON replan_replacements(from_task_id);
ALTER TABLE plan_requests ADD COLUMN replan_operation_id INTEGER;
CREATE INDEX plan_requests_by_replan ON plan_requests(replan_operation_id);

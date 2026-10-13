-- dagq-schema: compatible
-- The hold of a task (ADR-t1879-1): a person's or the inbox's mark that
-- keeps the runtime from claiming the task, resuming its run or starting
-- a recovery job of it while something outside the plan is fixed. It is
-- not the task's status, which the hold leaves as it is.
--
-- task_holds: one row per hold. At most one is open per task (`released_at`
-- NULL): each write transaction that opens one inserts it only when the
-- task has none open. `run_id` is the run a hold named (a hold on a run is
-- its task's), `reason` why, `until` when to remind a person (never a
-- release), `by_role` / `by_id` who held it and `created_at` when.
-- `settled_at` is when the task stopped moving under the hold (the safe
-- wait). `released_at`, `released_by_role`, `released_by_id` and
-- `continuation` record its release. The row and its `task_held` or
-- `task_released` event are written in one transaction.
--
-- Additions only: an older binary ignores the table. No REFERENCES and no
-- unique index, which a compatible migration may not add.
CREATE TABLE task_holds (
    id INTEGER PRIMARY KEY,
    task_id INTEGER NOT NULL,
    run_id TEXT,
    reason TEXT NOT NULL,
    until INTEGER,
    by_role TEXT NOT NULL,
    by_id TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    settled_at INTEGER,
    released_at INTEGER,
    released_by_role TEXT,
    released_by_id TEXT,
    continuation TEXT
);
CREATE INDEX task_holds_by_task ON task_holds(task_id, released_at);

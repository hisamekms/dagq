-- dagq-schema: compatible
-- A draft's revisit time (ADR-t1540-1): when it comes, the draft is a
-- target of the runtime's draft planners again, past a `keep_draft` answer
-- and without an origin of its own (`revisit`), and the planner's prompt
-- carries the last decision about it.
--
-- draft_revisits: one row per draft. `revisit_at` is the time in Unix
-- seconds; `note` what to look at then; `set_by` the role that set it
-- (`planner`, `user`, `inbox`) and `set_by_id` its actor id. `opened_at`
-- and `planner_id` are null while it waits, and set when the time came and
-- the runtime opened that planner (the time is used once). Setting it again
-- replaces the row; clearing it deletes it.
--
-- An addition only: an older binary never names the table. No
-- REFERENCES, which a compatible migration may not add: the runtime writes
-- only the ID of a task it just read.
CREATE TABLE draft_revisits (
    task_id INTEGER PRIMARY KEY,
    revisit_at INTEGER NOT NULL,
    note TEXT,
    set_by TEXT NOT NULL,
    set_by_id TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    opened_at INTEGER,
    planner_id INTEGER
);
CREATE INDEX draft_revisits_by_time ON draft_revisits(opened_at, revisit_at);

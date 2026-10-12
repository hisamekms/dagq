-- dagq-schema: compatible
-- The replanning of a Spike's result and the investigations
-- (ADR-t1487-1 decisions 5, 7 and 8).
--
-- plan_requests.spike_task_id: the Spike task whose completion the runtime
-- recorded the request for; NULL for a request a person's words made. One
-- request per Spike: each write transaction that records one (the
-- landing's, the supervisor's reconciling pass) inserts it only when the
-- Spike has none, so none records two.
--
-- spike_cycles: one row per investigation (a goal's, or a Spike's without
-- a goal), opened when the first of its Spikes is claimed and closed by
-- the supervisor once no Spike of it is unfinished and no request of its
-- Spikes is `open`. A goal's rows count its cycles. The claim's transaction
-- opens one only when its goal (or its goalless Spike) has none open.
--
-- Additions only: an older binary ignores the table and never names the
-- column. No REFERENCES and no unique index, which a compatible migration
-- may not add.
ALTER TABLE plan_requests ADD COLUMN spike_task_id INTEGER;
CREATE INDEX plan_requests_by_spike ON plan_requests(spike_task_id);
CREATE TABLE spike_cycles (
    id INTEGER PRIMARY KEY,
    goal_id INTEGER,
    task_id INTEGER,
    opened_at INTEGER NOT NULL,
    closed_at INTEGER
);
CREATE INDEX spike_cycles_by_goal ON spike_cycles(goal_id, closed_at);
CREATE INDEX spike_cycles_by_task ON spike_cycles(task_id, closed_at);

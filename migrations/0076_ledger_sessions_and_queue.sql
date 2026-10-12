-- dagq-schema: compatible
-- The ledger's session and queue rows (ADR-t1662-1 decisions 10 and 12,
-- ADR-t1662-2): `kind` takes `session` (a session or a job of no run) and
-- `queue` (an interval or an event of the queue as a whole) beside `run`
-- and `task`, in the same table and the same shape.
--
-- inputs_from is the first run_events id a row was folded from, beside
-- inputs_through's last: a window is matched by that range and by
-- started_at / ended_at. records holds the events themselves (JSON array),
-- so that the statistics are made again from the rows alone. A legacy
-- row's values stay frozen, but its records and their range follow the
-- events, so that every event stays some row's.
--
-- The subject of a session row: subject_kind and subject_id the kind and
-- id of what it is of (a planner, a plan review, a goal review, a
-- throughput review, the observer, the inbox) and round which of its
-- sessions or jobs it is; route, provider and model what it ran on. A
-- queue row's subject_kind is the kind of interval or event and its
-- subject_id what it is about (a supervisor, a finding, a goal). Run and
-- task rows leave them null.
--
-- Additions with defaults only: an older binary never names the columns.
ALTER TABLE ledger_rows ADD COLUMN inputs_from INTEGER NOT NULL DEFAULT 0;
ALTER TABLE ledger_rows ADD COLUMN subject_kind TEXT;
ALTER TABLE ledger_rows ADD COLUMN subject_id TEXT;
ALTER TABLE ledger_rows ADD COLUMN round INTEGER;
ALTER TABLE ledger_rows ADD COLUMN route TEXT;
ALTER TABLE ledger_rows ADD COLUMN provider TEXT;
ALTER TABLE ledger_rows ADD COLUMN model TEXT;
ALTER TABLE ledger_rows ADD COLUMN records TEXT NOT NULL DEFAULT '[]';

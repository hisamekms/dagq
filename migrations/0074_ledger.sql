-- dagq-schema: compatible
-- The ledger (ADR-t1662-1 decisions 10, 17 and 18; ADR-t1662-2 is the
-- exception to "no new table" of ADR-0049 decision 5): one row per
-- measured object, folded from run_events alone by domain::ledger. A view:
-- every row but a legacy one may be dropped and built again. Measurement
-- only: no decision of the runtime reads it.
--
-- kind is the kind of object (`run`, `task`; a new kind is a new value),
-- key its id. ledger_v is the version of the fold that built the row, and
-- inputs_through a JSON object of how far into each input it was folded
-- (`events`: the last run_events id; later inputs add their own key).
-- final is the mark of a settled row. legacy marks a run from before
-- run_phase_changed (and a task with such a run): legacy_values holds the
-- values stats gave it, written once and never rebuilt.
--
-- The sums are columns in milliseconds: the whole, the slot time, the
-- unattributed time, per blocker and per holds; attempts counts the
-- attempts. What changes shape with the flow is JSON: totals (per blocker,
-- holds and attempt, each with its slot time), intervals, the claim's
-- attributes (whatever their keys), after_claim and the kind's detail.
-- Additions only: an older binary ignores the table.
CREATE TABLE ledger_rows (
    kind TEXT NOT NULL,
    key TEXT NOT NULL,
    task_id INTEGER,
    ledger_v INTEGER NOT NULL,
    built_at TEXT NOT NULL,
    inputs_through TEXT NOT NULL,
    final INTEGER NOT NULL DEFAULT 0,
    legacy INTEGER NOT NULL DEFAULT 0,
    legacy_values TEXT,
    started_at INTEGER,
    ended_at INTEGER,
    outcome TEXT,
    duration_ms INTEGER NOT NULL DEFAULT 0,
    slot_ms INTEGER NOT NULL DEFAULT 0,
    unattributed_ms INTEGER NOT NULL DEFAULT 0,
    attempts INTEGER NOT NULL DEFAULT 0,
    blocker_queue_ms INTEGER NOT NULL DEFAULT 0,
    blocker_ai_ms INTEGER NOT NULL DEFAULT 0,
    blocker_compute_ms INTEGER NOT NULL DEFAULT 0,
    blocker_human_ms INTEGER NOT NULL DEFAULT 0,
    blocker_runtime_ms INTEGER NOT NULL DEFAULT 0,
    blocker_external_ms INTEGER NOT NULL DEFAULT 0,
    blocker_infra_ms INTEGER NOT NULL DEFAULT 0,
    holds_worker_slot_ms INTEGER NOT NULL DEFAULT 0,
    holds_landing_slot_ms INTEGER NOT NULL DEFAULT 0,
    holds_none_ms INTEGER NOT NULL DEFAULT 0,
    totals TEXT NOT NULL DEFAULT '{}',
    intervals TEXT NOT NULL DEFAULT '[]',
    attributes TEXT NOT NULL DEFAULT '{}',
    after_claim TEXT,
    detail TEXT NOT NULL DEFAULT '{}',
    PRIMARY KEY (kind, key)
);

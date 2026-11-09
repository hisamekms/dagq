-- dagq-schema: compatible
-- The points of the coverage of the queue's events the ledger does not copy
-- to queue rows (ADR-t4644-1 decision 2, domain::ledger::session::written::
-- omitted): one row per kind and position `at` (an event id), with the
-- latest time (max_ms), the last event (last) and the last timed event
-- (timed) of the kind's omitted events up to it. Each only rises with `at`,
-- so a reader seeks a kind's points by position (the key) or by time (the
-- index). Written with the ledger's marks in their transaction, never
-- removed. A view: measurement only, no decision of the runtime reads it.
-- Additions only: an older binary ignores the table.
CREATE TABLE ledger_omitted_points (
    kind TEXT NOT NULL,
    at INTEGER NOT NULL,
    max_ms INTEGER,
    last INTEGER,
    timed INTEGER,
    PRIMARY KEY (kind, at)
);
CREATE INDEX ledger_omitted_points_by_ms ON ledger_omitted_points(kind, max_ms, at);

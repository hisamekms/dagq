-- The points of the coverage of the queue's events the ledger does not copy
-- to queue rows, as the SQLite migration of the same name makes them.
CREATE TABLE ledger_omitted_points (
    kind TEXT NOT NULL,
    at BIGINT NOT NULL,
    max_ms BIGINT,
    last BIGINT,
    timed BIGINT,
    PRIMARY KEY (kind, at)
);
CREATE INDEX ledger_omitted_points_by_ms ON ledger_omitted_points(kind, max_ms, at);

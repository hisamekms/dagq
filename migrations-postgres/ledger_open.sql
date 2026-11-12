-- What each run row leaves open at its end, and the mark of how far the run
-- rows written before the table were taken in with the last generation of
-- the write leader it is kept through, as the SQLite migration of the same
-- name makes them.
CREATE TABLE ledger_open (
    kind TEXT NOT NULL,
    key TEXT NOT NULL,
    what TEXT NOT NULL,
    PRIMARY KEY (kind, key, what)
);
CREATE TABLE ledger_open_filled (
    through TEXT,
    generation BIGINT
);

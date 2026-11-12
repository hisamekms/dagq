-- dagq-schema: compatible
-- What each run row leaves open at its end (domain::ledger::open::left_open):
-- one row per row kind, key and thing left open (`what`: the slot, an ask by
-- its key, a session by its opened event, the landing's wait), so a read from
-- a time finds the rows it leaves out that it still reads whole by the key,
-- without walking the settled history. Written with the run row in its
-- transaction each time the row is written, which removes what a later event
-- closed. ledger_open_filled is the mark of how far the run rows written
-- before the table were taken in: no row, none yet; a row with `through` the
-- last key taken in; a row with NULL, all of them, the table is that of the
-- rows. Its `generation` is the last generation of the write leader the mark
-- is kept through: a writer that keeps the table carries it on from the
-- generation just before its own with its first write of the ledger, and an
-- older binary ignores the tables, so the generation it takes over leaves the
-- mark behind, it counts as no mark and the filling takes every run row in
-- again. A view: rebuilt of the rows alone, it changes no row of the ledger
-- and no ledger_v (ADR-t1662-2 decisions 1, 7 and 8), and no decision of the
-- runtime reads it. Additions only.
CREATE TABLE ledger_open (
    kind TEXT NOT NULL,
    key TEXT NOT NULL,
    what TEXT NOT NULL,
    PRIMARY KEY (kind, key, what)
);
CREATE TABLE ledger_open_filled (
    through TEXT,
    generation INTEGER
);

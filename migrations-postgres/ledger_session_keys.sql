-- The read of the session rows a read from a time leaves out, by the range
-- of their keys' prefix (`runtime_planner/<id>/` to `runtime_planner/<id>0`,
-- src/infrastructure/runtime_store/ledger/earlier.rs) compares the keys by
-- their bytes (SqlDialect::by_bytes), whatever the database's collation, and
-- seeks this index: the primary key (kind, key) is of that collation, which
-- need not order '/' before '0'. An index only: ledger rows and ledger_v keep
-- their shape (ADR-t1662-2 decisions 1, 7, 8); older binaries may ignore it.
CREATE INDEX ledger_rows_session_keys ON ledger_rows (key COLLATE "C")
  WHERE kind = 'session';

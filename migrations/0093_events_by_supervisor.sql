-- dagq-schema: compatible
-- The last event naming a supervisor up to a position
-- (LedgerStore::last_event_naming_supervisor): what builds the ledger cuts
-- a row a supervisor's life reaches back to a position by it. Without an
-- index the read walks every event back from the position, as many as
-- that supervisor was silent for. The index is on the expression and the
-- id, partial to the events naming a supervisor; a query hits it only
-- when it writes the same expression and compares it with a bound value.
CREATE INDEX events_by_supervisor
  ON run_events(json_extract(payload, '$.supervisor'), id)
  WHERE json_extract(payload, '$.supervisor') IS NOT NULL;

-- dagq-schema: compatible
-- Seek the landing start preceding a range by (time, id), without walking
-- settled events of other kinds. No row or ledger_v changes.
CREATE INDEX landing_starts_by_time ON run_events(created_at, id)
  WHERE kind = 'integration_started';

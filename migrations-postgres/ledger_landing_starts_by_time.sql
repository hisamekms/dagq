-- Seek the same landing boundary in both stores.
CREATE INDEX landing_starts_by_time ON run_events(created_at, id)
  WHERE kind = 'integration_started';

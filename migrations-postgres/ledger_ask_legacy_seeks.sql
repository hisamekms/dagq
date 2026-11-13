-- The same AskRef legacy and explicit-null partitions as SQLite.
CREATE INDEX asks_by_legacy_id ON run_events((payload::jsonb ->> 'id'), id)
  WHERE kind = 'ask_opened' AND payload::jsonb ->> 'ask_id' IS NULL;
CREATE INDEX asks_by_explicit_null ON run_events(id)
  WHERE kind = 'ask_opened' AND payload::jsonb -> 'ask_id' = 'null'::jsonb;

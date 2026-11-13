-- dagq-schema: compatible
-- AskRef also accepts the older id key and an explicit JSON null. Keep
-- those seeks outside the ask_id index's null partition, which can hold
-- unrelated settled legacy asks. These are event indexes, not new views.
CREATE INDEX asks_by_legacy_id ON run_events((json_extract(payload,'$.id')), id)
  WHERE kind = 'ask_opened' AND json_extract(payload,'$.ask_id') IS NULL;
CREATE INDEX asks_by_explicit_null ON run_events(id)
  WHERE kind = 'ask_opened' AND json_type(payload,'$.ask_id') = 'null';

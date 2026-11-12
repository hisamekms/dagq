-- Same candidate seek as SQLite's ledger_omitted_removal migration: the
-- single record's event id, not the row's input position.
CREATE INDEX ledger_rows_omitted_removal
  ON ledger_rows(kind, subject_kind, ledger_v,
    ((jsonb_path_query_first((records)::jsonb, ('$[0].id')::text::jsonpath) #>> '{}')::BIGINT), key)
  WHERE final = TRUE AND jsonb_array_length((records)::jsonb) = 1;

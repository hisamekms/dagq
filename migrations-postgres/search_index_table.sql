-- ADR-t4258-1: LIKE candidates with application bm25, shared across kinds.
-- The base already provides the ordinary search_index table, without triggers.
ALTER TABLE search_index RENAME COLUMN rowid TO doc_order;
ALTER TABLE search_index ADD COLUMN trigram_count bigint NOT NULL DEFAULT 0;
ALTER TABLE search_index ADD COLUMN doc text GENERATED ALWAYS AS
 (coalesce(title, '') || E'\n' || coalesce(description, '') || E'\n' ||
  coalesce(acceptance, '') || E'\n' || coalesce(context, '') || E'\n' || coalesce(text, '')) STORED;
ALTER TABLE search_index ADD COLUMN td text GENERATED ALWAYS AS
 (coalesce(title, '') || E'\n' || coalesce(description, '')) STORED;
DO $$
DECLARE ops text;
BEGIN
  BEGIN
    CREATE EXTENSION IF NOT EXISTS pg_bigm;
    ops := 'gin_bigm_ops';
  EXCEPTION WHEN undefined_file OR feature_not_supported OR insufficient_privilege THEN
    CREATE EXTENSION IF NOT EXISTS pg_trgm;
    ops := 'gin_trgm_ops';
  END;
  EXECUTE format('CREATE INDEX search_index_doc ON search_index USING gin (lower(doc) %s)', ops);
  EXECUTE format('CREATE INDEX search_index_td ON search_index USING gin (lower(td) %s)', ops);
END $$;

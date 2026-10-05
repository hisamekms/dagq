-- dagq-schema: compatible
-- A goal's tags (ADR-t1639-1 decision 6; `goal add --tag`, `goal edit
-- --tag` / `--no-tags`): a JSON array of distinct lowercase slugs, '[]'
-- for a goal without one, every goal before this migration included. No
-- CHECK: the runtime checks the form and the repository's set (`[goals]
-- tags` of dagq.toml) when it writes. An addition with a default only: an
-- older binary never names the column, and its inserts leave it '[]'.
ALTER TABLE goals ADD COLUMN tags TEXT NOT NULL DEFAULT '[]';

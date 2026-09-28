-- dagq-schema: compatible
-- The change a task declares (ADR-t980-1): the kind of change it makes, a
-- lowercase label of the repository's own (`add --change`, `edit
-- --change`), from the set `[tasks] changes` of dagq.toml when the
-- repository names one. NULL for a task registered without one; the tasks
-- before this migration are not filled in. The column has no CHECK: the
-- runtime holds no set of values, and a binary reads a value it cannot
-- parse as none. A nullable addition only: an older binary never names the
-- column, and its inserts leave it NULL.
ALTER TABLE tasks ADD COLUMN change TEXT;

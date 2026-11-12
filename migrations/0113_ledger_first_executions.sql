-- dagq-schema: compatible
-- A rebuildable ledger view, independent of ledger_v. All run rows contribute,
-- including legacy rows' unfrozen execution detail. Times compare as integers,
-- never in run order. Absence of the ready row means readers must use the
-- ledger_rows GROUP BY instead; publish readiness only after the backfill.
CREATE TABLE ledger_first_executions (
    actor TEXT NOT NULL,
    provider TEXT NOT NULL,
    at BIGINT NOT NULL,
    PRIMARY KEY (actor, provider)
);
CREATE TABLE ledger_first_executions_ready (
    singleton INTEGER PRIMARY KEY
);
INSERT INTO ledger_first_executions(actor, provider, at)
SELECT json_extract(j.value, '$.actor'), json_extract(j.value, '$.provider'),
       MIN(CAST(json_extract(j.value, '$.at') AS INTEGER))
  FROM ledger_rows r, json_each(r.detail, '$.executions') j
 WHERE r.kind = 'run'
   AND json_extract(j.value, '$.actor') IS NOT NULL
   AND json_extract(j.value, '$.provider') IS NOT NULL
   AND json_extract(j.value, '$.at') IS NOT NULL
 GROUP BY json_extract(j.value, '$.actor'), json_extract(j.value, '$.provider');
INSERT INTO ledger_first_executions_ready(singleton) VALUES (1);

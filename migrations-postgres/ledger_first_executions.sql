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
SELECT (j.value::jsonb ->> 'actor'), (j.value::jsonb ->> 'provider'),
       MIN(CAST((j.value::jsonb ->> 'at') AS BIGINT))
  FROM ledger_rows r, jsonb_array_elements(r.detail::jsonb -> 'executions') AS j(value)
 WHERE r.kind = 'run'
   AND (j.value::jsonb ->> 'actor') IS NOT NULL
   AND (j.value::jsonb ->> 'provider') IS NOT NULL
   AND (j.value::jsonb ->> 'at') IS NOT NULL
 GROUP BY (j.value::jsonb ->> 'actor'), (j.value::jsonb ->> 'provider');
INSERT INTO ledger_first_executions_ready(singleton) VALUES (1);

-- dagq-schema: compatible
-- The session_closed and session_turns of a span are found by the
-- session_opened they name, json_extract(payload,'$.opened_event_id')
-- (task 1333). With only events_by_kind(kind, id), each lookup walked every
-- row of its kind, and the open spans' NOT EXISTS did so once per span:
-- quadratic in the sessions, under the write lock. One index on the kind
-- and the expression serves both kinds; a query hits it only when it
-- writes the same expression and compares it with no affinity applied
-- (a bound value, or `+o.id` rather than the column `o.id`).
CREATE INDEX events_by_opened_event
  ON run_events(kind, json_extract(payload, '$.opened_event_id'));

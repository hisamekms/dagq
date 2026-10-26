-- dagq-schema: compatible
-- The identities of the results' records the queue service took as events
-- (ADR-t3102-3 decision 5): a record sent again, however late and from
-- whichever supervisor, is the event named here, and two results alike
-- sent as two records are two events. A binary before it neither reads
-- nor writes the table.
CREATE TABLE event_records (
  record TEXT PRIMARY KEY,
  event_id INTEGER NOT NULL
) WITHOUT ROWID;

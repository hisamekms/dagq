-- dagq-schema: compatible
-- The write leader's row (ADR-t3986-1 decisions 2, 4 and 5): the one
-- process of the queue service whose write transactions may commit, and
-- the generation that fences an older leader out. Exactly one row, which
-- this migration inserts; nothing deletes it or inserts another.
--
-- singleton       the key of the one row, always 1.
-- holder          the leader's identifier: a random value its process
--                 makes at each start (no pid or host name), NULL while
--                 no process has ever led.
-- generation      the fencing number, advanced by 1 at each takeover;
--                 0 means no process has led yet.
-- lease_until_ms  the lease's end, in Unix milliseconds of the DB's clock;
--                 0 when released (and before the first takeover). It only
--                 says when another process may take over: a leader whose
--                 lease ran out still writes until one does.
-- acquired_at_ms  when the generation was taken, in Unix milliseconds of
--                 the DB's clock, NULL before the first takeover; for
--                 observation only.
--
-- The primary key alone lets a second row in under another singleton
-- value, so a unique index on a constant expression holds the table to
-- one row whatever the value (as one_integrating_run_per_queue does for
-- the integrating run). No CHECK (ADR-t876-1). An older binary does not
-- know the table and never writes it, so the migration is compatible.
CREATE TABLE write_leader (
  singleton INTEGER PRIMARY KEY,
  holder TEXT,
  generation INTEGER NOT NULL,
  lease_until_ms INTEGER NOT NULL,
  acquired_at_ms INTEGER
);
CREATE UNIQUE INDEX write_leader_one_row ON write_leader((1));
INSERT INTO write_leader (singleton, generation, lease_until_ms) VALUES (1, 0, 0);

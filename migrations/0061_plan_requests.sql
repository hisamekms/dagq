-- dagq-schema: compatible
-- Planning requests (ADR-t1394-1): a person's words the inbox records for
-- a planner of the runtime's, which the supervisor opens for each `open`
-- request.
--
-- plan_requests: one row per request. `text` is the person's own words
-- and `note` what the inbox adds apart from them; `refs` is a JSON array
-- of what it refers to (`{"kind": "ask", "id": 3}`, ...). `requested_by` is
-- the role that recorded it (`inbox`, `user`) and `requested_by_id` its
-- actor id. `status` is open, proposed, declined or exhausted, with
-- `status_reason` for the last two. The words never change once recorded.
--
-- plan_request_proposals: the proposals its planners submitted, each once.
--
-- planners.request_id: the request a planner of the runtime's was opened
-- for; asks.request_id: the request a `planner_question` is about. NULL
-- for every other planner and ask. Additions only: an older binary ignores
-- the tables and never names the columns. No REFERENCES, which a
-- compatible migration may not add: the runtime writes only the ID of a
-- request it just read.
CREATE TABLE plan_requests (
    id INTEGER PRIMARY KEY,
    text TEXT NOT NULL,
    note TEXT,
    refs TEXT NOT NULL DEFAULT '[]',
    requested_by TEXT NOT NULL,
    requested_by_id TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'open',
    status_reason TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE INDEX plan_requests_by_status ON plan_requests(status, id);
CREATE TABLE plan_request_proposals (
    request_id INTEGER NOT NULL,
    proposal_id INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (request_id, proposal_id)
);
CREATE INDEX plan_request_proposals_by_proposal ON plan_request_proposals(proposal_id);
ALTER TABLE planners ADD COLUMN request_id INTEGER;
CREATE INDEX planners_by_request ON planners(request_id);
ALTER TABLE asks ADD COLUMN request_id INTEGER;
CREATE INDEX asks_by_request ON asks(request_id);

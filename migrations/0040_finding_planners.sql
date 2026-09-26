-- dagq-schema: compatible
-- The finding a planner of the runtime's was opened for (ADR-0044
-- decision 19, carried by ADR-0047): a finding marked for a proposal (by
-- the observer's `--propose`, or a person's `propose` answer) gets one
-- planner at a time, and at most three for each mark. NULL for every other
-- planner. A nullable addition only: an older binary never names it and
-- takes such a planner for one opened for nothing in particular. No
-- REFERENCES, which a compatible migration may not add: the runtime
-- writes only the ID of a finding it just read.
ALTER TABLE planners ADD COLUMN finding_id INTEGER;
CREATE INDEX planners_by_finding ON planners(finding_id);

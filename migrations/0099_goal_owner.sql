-- dagq-schema: compatible
-- The owner of a goal and of a task without one (ADR-t3989-1 decisions 1-2),
-- the person's (issuer, subject) of the members registry. Each pair is both
-- NULL or both set. Both stay NULL where no OIDC issuer is configured: the
-- host's user owns everything. A task in a goal keeps its pair NULL and reads
-- its goal's; moving a task out of a goal writes that goal's owner to it.
ALTER TABLE goals ADD COLUMN owner_issuer TEXT;
ALTER TABLE goals ADD COLUMN owner_subject TEXT;
ALTER TABLE tasks ADD COLUMN owner_issuer TEXT;
ALTER TABLE tasks ADD COLUMN owner_subject TEXT;

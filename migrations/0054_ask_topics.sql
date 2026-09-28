-- dagq-schema: compatible
-- What a worker_question left undecided (ADR-t947-2): its topic codes, a
-- JSON array with the primary code first (`dagq ask --topic`). NULL for
-- every other kind and for the asks before this migration, which are not
-- filled in and count as `unlabeled`. The column has no CHECK: the codes
-- are labels the design lists, and a code outside the list is kept as it
-- was given. A nullable addition only: an older binary never names the
-- column, and its inserts leave it NULL.
ALTER TABLE asks ADD COLUMN topics TEXT;

-- dagq-schema: compatible
-- The AI that opens an ask may recommend one of its options and say how
-- sure it is (ADR-t451-1 decision 1): `recommendation` is the option's
-- text, `confidence` is 'high' or 'low'. Both are NULL without them, and
-- for every ask opened before this.
ALTER TABLE asks ADD COLUMN recommendation TEXT;
ALTER TABLE asks ADD COLUMN confidence TEXT;

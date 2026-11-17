-- dagq-schema: compatible
-- Per-pass display state; overwritten without an event and removed with its registration.
ALTER TABLE supervisors ADD COLUMN disk_status TEXT;

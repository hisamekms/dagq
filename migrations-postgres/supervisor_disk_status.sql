-- Per-pass display state, with the lifetime of the supervisor registration.
ALTER TABLE supervisors ADD COLUMN disk_status TEXT;

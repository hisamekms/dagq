-- dagq-schema: breaking
-- Rejecting triggers require a breaking declaration under the migration
-- compatibility policy, even on a newly created table.
-- Primary audit chain, independent of operational events and never backfilled.
-- seq is admission order; hash and prev_hash link JCS records. record is the
-- complete versioned JSON, including these indexed fields. Sender identity is
-- assigned by the service; sender_seq makes retries idempotent per instance.
-- Inserts belong to the same transaction as the state they attest.
CREATE TABLE audit_records (
    seq INTEGER PRIMARY KEY,
    sender_id TEXT NOT NULL,
    sender_seq INTEGER NOT NULL,
    hash TEXT NOT NULL,
    prev_hash TEXT NOT NULL,
    received_at TEXT NOT NULL,
    record TEXT NOT NULL,
    UNIQUE (sender_id, sender_seq)
);
CREATE TRIGGER audit_records_no_update BEFORE UPDATE ON audit_records
BEGIN SELECT RAISE(ABORT, 'audit records are append only'); END;
CREATE TRIGGER audit_records_no_delete BEFORE DELETE ON audit_records
BEGIN SELECT RAISE(ABORT, 'audit records are append only'); END;

-- Primary audit chain schema. The stage-zero Postgres adapter writes no audit.
-- seq orders admissions; the sender key deduplicates retries; record contains
-- the complete versioned JSON. hash and prev_hash link canonical records.
-- received_at is the service's admission time. No triggers on this store.
CREATE TABLE audit_records (
    seq BIGINT PRIMARY KEY,
    sender_id TEXT NOT NULL,
    sender_seq BIGINT NOT NULL,
    hash TEXT NOT NULL,
    prev_hash TEXT NOT NULL,
    received_at TEXT NOT NULL,
    record TEXT NOT NULL,
    UNIQUE (sender_id, sender_seq)
);

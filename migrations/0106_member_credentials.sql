-- dagq-schema: compatible
-- A registered human's own Claude credential choice, one row per registry
-- entry by the same pair. No row reads as offering nothing. No foreign key,
-- as other compatible migrations keep; the store writes a row only while
-- the member is active, in the transaction that checks it.
CREATE TABLE member_credentials (
    issuer TEXT NOT NULL,
    subject TEXT NOT NULL,
    offer TEXT NOT NULL,
    prefer TEXT,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (issuer, subject)
);

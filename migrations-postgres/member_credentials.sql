-- A registered human's own Claude credential choice; same contract as SQLite.
CREATE TABLE member_credentials (
    issuer TEXT NOT NULL,
    subject TEXT NOT NULL,
    offer TEXT NOT NULL,
    prefer TEXT,
    updated_at BIGINT NOT NULL,
    PRIMARY KEY (issuer, subject)
);

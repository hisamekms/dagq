-- The people who answer for an owner, one row per owner and delegate; same
-- contract as SQLite's delegates migration.
CREATE TABLE delegates (
    owner_issuer TEXT NOT NULL,
    owner_subject TEXT NOT NULL,
    delegate_issuer TEXT NOT NULL,
    delegate_subject TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    PRIMARY KEY (owner_issuer, owner_subject, delegate_issuer, delegate_subject)
);

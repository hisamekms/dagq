-- Registered humans; same identity and role contract as SQLite.
CREATE TABLE members (
    issuer TEXT NOT NULL,
    subject TEXT NOT NULL,
    role TEXT NOT NULL,
    on_call BIGINT NOT NULL,
    display_name TEXT,
    created_at BIGINT NOT NULL,
    disabled_at BIGINT,
    PRIMARY KEY (issuer, subject)
);

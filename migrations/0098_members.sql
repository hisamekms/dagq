-- dagq-schema: compatible
-- Registered humans; the pair, never the display name, determines identity.
CREATE TABLE members (
    issuer TEXT NOT NULL,
    subject TEXT NOT NULL,
    role TEXT NOT NULL,
    on_call INTEGER NOT NULL,
    display_name TEXT,
    created_at INTEGER NOT NULL,
    disabled_at INTEGER,
    PRIMARY KEY (issuer, subject)
);

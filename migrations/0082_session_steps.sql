-- dagq-schema: compatible
-- The session steps (ADR-t1662-2 decisions 1 and 4, ADR-t1662-3
-- decisions 8, 10 and 12, SessionStepStore): one row per step of a
-- session. Not events: a session's many steps stay out of run_events.
--
-- id is the order the rows came in (AUTOINCREMENT: never reused after the
-- retention removes rows, so a cursor past it holds). target_kind and
-- target_id name the one target the session is of (`run` and the run's
-- id, or a target of no run's kind and id). session_id and seq are the
-- step's session and its number in it from 1, kept once: a step sent
-- again adds no row, and the missing seqs are the session's gaps, read
-- from the rows. kind is `model_turn`, `tool_call`, `subagent` or
-- `wait`; offset_ms and duration_ms place the step from the session's
-- start, and received_at is when it came in (unix ms). model, tool,
-- command_shape (JSON of the command's generic shape), exit_code,
-- background (0 or 1) and output_bytes are what the step ran, null when
-- it has none. No column holds a prompt, an output, a command line or
-- tokens (ADR-t1662-3 decision 8).
--
-- The retention removes each step that came in more than 90 days before.
-- session_step_removed keeps, for a session it removed some steps of and
-- kept others, the seqs it removed as runs from_seq..=to_seq of
-- consecutive seqs: they are no gap, unlike a seq that never came in, and
-- a step sent again within them adds no row. A session with no step left
-- has no row there.
-- Additions only: an older binary ignores the tables.
CREATE TABLE session_steps (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    target_kind TEXT NOT NULL,
    target_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    kind TEXT NOT NULL,
    offset_ms INTEGER NOT NULL,
    duration_ms INTEGER NOT NULL,
    model TEXT,
    tool TEXT,
    command_shape TEXT,
    exit_code INTEGER,
    background INTEGER,
    output_bytes INTEGER,
    received_at INTEGER NOT NULL,
    UNIQUE (session_id, seq)
);

CREATE INDEX session_steps_target ON session_steps(target_kind, target_id);

CREATE TABLE session_step_removed (
    session_id TEXT NOT NULL,
    from_seq INTEGER NOT NULL,
    to_seq INTEGER NOT NULL,
    PRIMARY KEY (session_id, from_seq)
);

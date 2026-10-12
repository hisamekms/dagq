//! Whether a session, queue or node row has ended, is dirty and may be
//! marked settled (`final`), for what builds the ledger
//! (docs/design/measurement.md "台帳を作る係"). A session ends when it
//! closes (a job at its end); a queue interval at its end event, or the
//! end of its supervisor's life; a queue event ends as it happens; a node
//! row when its hour has passed. A row is settled only once it has
//! ended and its grace has passed with no later input folded: the records
//! a closed session may still get (an interactive session's cut at its
//! close comes up to a week later, [`TOKEN_CUT_CLOSED_WINDOW_MS`]) and the
//! end a silent supervisor's later stop moves. A row not ended is never
//! settled. Pure: the row, its last input and the time are all.

use super::{LEDGER_V, LedgerKind, LedgerRow, session};
use crate::domain::{
    EventId, RunEvent,
    sessions::{TOKEN_CUT_CLOSED_WINDOW_MS, TOKEN_CUT_KINDS},
    supervisor_life::supervisor_of,
};

/// For a row its supervisor's life ended, the last event of `events`
/// (ascending id) naming that supervisor: past what the fold read
/// (`inputs_through.supervisor`), the end may have moved (a stop recorded
/// after a silence, or more of its life). `None` for any other row.
pub fn life_input(row: &LedgerRow, events: &[RunEvent]) -> Option<EventId> {
    row.inputs_through.supervisor?;
    let supervisor = row.subject.as_ref()?.id.as_str();
    events
        .iter()
        .rev()
        .find(|event| supervisor_of(event) == Some(supervisor))
        .map(|event| event.id)
}

/// How long after a session or a queue interval ended its row waits for
/// later records before it is settled, but an interactive session's.
pub const SETTLE_GRACE_MS: i64 = 15 * 60 * 1000;

/// What [`settlement`] says of a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settlement {
    /// Its session closed, its job or interval ended, its event happened.
    pub ended: bool,
    /// It is to be folded again: of an older [`LEDGER_V`], or an input
    /// after its `inputs_through`.
    pub dirty: bool,
    /// It may be marked `final`: ended, its grace passed, not dirty.
    pub settles: bool,
}

/// The grace of `row` after it ended.
fn grace(row: &LedgerRow) -> i64 {
    let interactive = row
        .subject
        .as_ref()
        .is_some_and(|subject| TOKEN_CUT_KINDS.contains(&subject.kind.as_str()));
    if row.kind == LedgerKind::Session && interactive && session::is_span(row) {
        TOKEN_CUT_CLOSED_WINDOW_MS
    } else {
        SETTLE_GRACE_MS
    }
}

/// The settlement of the session or queue row `row` at `now_ms`, the last
/// input of what it is of being `last_input`: its own last event, and for
/// a row its supervisor's life ended the later of that and
/// [`life_input`]; for a row not ended, which any later event may end, the
/// queue's last event. A run or task row is never settled here:
/// [`super::rebuilds`] and its own rules hold it.
pub fn settlement(row: &LedgerRow, last_input: EventId, now_ms: i64) -> Settlement {
    let ended =
        matches!(row.kind, LedgerKind::Session | LedgerKind::Queue) && row.ended_at.is_some();
    let dirty = row.ledger_v != LEDGER_V || last_input > row.inputs_through.last();
    let settles = !dirty
        && row
            .ended_at
            .is_some_and(|end| ended && now_ms - end >= grace(row));
    Settlement {
        ended,
        dirty,
        settles,
    }
}

/// The settlement of the node row `row` at `now_ms`, the largest
/// NodeSampleStore id of its node's samples in its bucket being
/// `last_sample` (none when the store has none left): ended once its
/// hour has passed; dirty when of an older [`LEDGER_V`] or a sample came
/// in after what it folded (taken late, or imported from a CSV file after
/// its hour ended); settled once ended, past [`SETTLE_GRACE_MS`] after its
/// hour, and not dirty. A settled row a later sample reaches is dirty
/// again.
pub fn node_settlement(row: &LedgerRow, last_sample: Option<i64>, now_ms: i64) -> Settlement {
    let ended = row.kind == LedgerKind::Node && row.ended_at.is_some_and(|end| now_ms >= end);
    let dirty = row.ledger_v != LEDGER_V || last_sample > row.inputs_through.node_samples;
    let settles = !dirty
        && ended
        && row
            .ended_at
            .is_some_and(|end| now_ms - end >= SETTLE_GRACE_MS);
    Settlement {
        ended,
        dirty,
        settles,
    }
}

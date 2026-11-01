//! Today's names of a run's times (`work`, `validate`, `wait_to_land`,
//! `land_phases`, `startup`, and `first_commit`, the same as `startup`)
//! as `stats` and `kpi` take them from the ledger (ADR-t1662-1 decisions
//! 18 and 19): each run's from its row by the ledger's one read
//! ([`crate::domain::ledger::read`]), a legacy row's frozen values as they
//! are, never made again of the events. A run with no row before the
//! switch has none of them: its values are null and it is counted in
//! `ledger_missing`.

use std::collections::{BTreeMap, HashMap};

use super::landing::LandPhases;
use crate::domain::{RunId, TaskId};

/// One run's times under today's names, from its ledger row.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RowTimes {
    /// A legacy row: the values are those it froze before the switch.
    pub legacy: bool,
    pub work: Option<i64>,
    pub validate: Option<i64>,
    pub wait_to_land: Option<i64>,
    pub land_phases: Option<LandPhases>,
    pub startup: Option<i64>,
    /// The phase of `land_phases` its wait to land spent most in so far
    /// (to its row's last interval for one still waiting), for the
    /// `awaiting_integration` alert.
    pub waiting_phase: Option<&'static str>,
}

/// The runs' times a window of the ledger gives.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LedgerTimes {
    /// Per run with a row.
    pub runs: HashMap<RunId, RowTimes>,
    /// The runs before the switch its task rows name that have no row of
    /// their own (none of the ledger's records is theirs), with the task:
    /// their values are not made again of the events.
    pub missing: BTreeMap<RunId, TaskId>,
}

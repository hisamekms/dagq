//! The readers of every event of the queue, stopped until the ledger's
//! readers take their place (ADR-t3745-1): each entry refuses before it
//! reads, naming why and what brings it back. The counting functions
//! behind the entries stay, as the values the ledger's readers are checked
//! against.

/// An entry that would read every event of the queue, refused while it is
/// stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoppedRead {
    /// `stats`, and a job's read of it.
    Stats,
    /// `stats --ledger`, the parallel run of the ledger beside `stats`.
    StatsLedger,
    /// `stats_inputs`: what the client of `stats` and `kpi` reads Git for,
    /// and the observer's read of it.
    StatsInputs,
    /// `kpi` (`--compare` too), a job's read of it and the observer's KPI
    /// input.
    Kpi,
    /// `report` and `report_inputs`.
    Report,
    /// `marks`.
    Marks,
    /// `mark` and `mark --retract`.
    Mark,
    /// The supervisor's use case's `all_events`.
    AllEvents,
}

impl StoppedRead {
    /// The entry's name as its caller knows it.
    pub fn entry(self) -> &'static str {
        match self {
            Self::Stats => "stats",
            Self::StatsLedger => "stats --ledger",
            Self::StatsInputs => "stats_inputs",
            Self::Kpi => "kpi",
            Self::Report => "report",
            Self::Marks => "marks",
            Self::Mark => "mark",
            Self::AllEvents => "all_events",
        }
    }

    /// What ends the stop of the entry, in plain words (the runtime's text
    /// cites none of dagq's own tasks). For maintainers: the ledger's
    /// reader of stats, kpi and report is task 1669, the marks' read the marks task of goal 235, and task 1671
    /// removes `stats --ledger`.
    pub fn restored_by(self) -> &'static str {
        match self {
            Self::Stats | Self::StatsInputs | Self::Kpi | Self::Report => {
                "the ledger's reader of stats, kpi and report brings it back"
            }
            Self::Marks | Self::Mark => {
                "a read of the marks that does not read every event brings it back"
            }
            Self::StatsLedger => "it is removed once stats reads the ledger",
            Self::AllEvents => {
                "no reader is given it again: the ledger's readers read no whole log"
            }
        }
    }
}

/// The refusal of a [`StoppedRead`]: the entry, why it is stopped and what
/// brings it back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadStopped(pub StoppedRead);

/// What every refusal says after its entry's name, by which a caller that
/// gets it as text (across the queue service) tells it from another error.
const STOPPED: &str = "is stopped: it reads every event of the queue";

impl std::fmt::Display for ReadStopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} {STOPPED}, which holds too much in memory, so it stays stopped until the \
             ledger's readers take its place; {}",
            self.0.entry(),
            self.0.restored_by()
        )
    }
}

/// Whether `error`, as its text reads, is the refusal of a stopped entry
/// ([`ReadStopped`]), and not another failure of it.
pub fn is_refusal(error: &str) -> bool {
    error.contains(STOPPED)
}

impl std::error::Error for ReadStopped {}

/// The refusal of `read`, in place of what the entry would have read: an
/// entry calls it before it reads anything, and where the wiring behind it
/// read every event, so the wiring stays for the ledger's reader to take.
pub fn refuse<T>(read: StoppedRead) -> Result<T, ReadStopped> {
    Err(ReadStopped(read))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refusal_names_the_entry_why_and_what_brings_it_back() {
        let cases = [
            (
                StoppedRead::Stats,
                "stats is stopped",
                "reader of stats, kpi and report",
            ),
            (
                StoppedRead::StatsInputs,
                "stats_inputs is stopped",
                "reader of stats, kpi and report",
            ),
            (
                StoppedRead::Kpi,
                "kpi is stopped",
                "reader of stats, kpi and report",
            ),
            (
                StoppedRead::Report,
                "report is stopped",
                "reader of stats, kpi and report",
            ),
            (
                StoppedRead::Marks,
                "marks is stopped",
                "a read of the marks",
            ),
            (StoppedRead::Mark, "mark is stopped", "a read of the marks"),
            (
                StoppedRead::StatsLedger,
                "stats --ledger is stopped",
                "removed once stats reads the ledger",
            ),
            (
                StoppedRead::AllEvents,
                "all_events is stopped",
                "read no whole log",
            ),
        ];
        for (read, entry, restored) in cases {
            let text = ReadStopped(read).to_string();
            assert!(text.starts_with(entry), "{text}");
            assert!(
                text.contains("until the ledger's readers take its place"),
                "{text}"
            );
            assert!(text.contains(restored), "{text}");
            assert!(
                is_refusal(&format!("the service answered: {text}")),
                "{text}"
            );
        }
        assert!(!is_refusal("stats: the queue is busy"));
    }
}

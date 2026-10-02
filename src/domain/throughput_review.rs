//! The throughput review (ADR-t996-1): which period a review of each
//! cadence covers, the rules that decide whether an hour is worth a review,
//! and what the job's output says. The supervisor starts the job; the
//! runtime counts the landings and judges the hour, so a quiet hour starts
//! no agent. The thresholds are the initial values a person set on
//! 2026-09-28; `docs/design/supervisor-lifecycle/throughput-review.md`
//! holds them.

use serde::{Deserialize, Serialize};

use super::{
    DomainError, RunEvent,
    event_kind::{THROUGHPUT_REVIEW_FINISHED, THROUGHPUT_REVIEW_STARTED},
    kpi::{DAY_MS, Period},
    stats::timestamp_millis,
};

/// One hour in milliseconds.
pub const HOUR_MS: i64 = 60 * 60 * 1000;
/// Hours the moving average an hour is compared with covers: the hours
/// before it.
pub const BASELINE_HOURS: usize = 6;
/// How far from that average an hour must be, as a share of it ...
pub const DEVIATION_RATIO: f64 = 0.5;
/// ... and in landings.
pub const DEVIATION_MIN: f64 = 3.0;
/// The short and the long moving average of the sustained drop.
pub const SHORT_HOURS: usize = 3;
pub const LONG_HOURS: usize = 24;
/// How far below the long average the short one must be ...
pub const DROP_RATIO: f64 = 0.3;
/// ... for this many hours in a row.
pub const DROP_HOURS: usize = 3;
/// The hours of landings before the judged one that its judgment reads:
/// the long average of the first hour of the drop.
pub const LOOKBACK_HOURS: usize = LONG_HOURS + DROP_HOURS - 2;
/// The most lines of a conclusion the inbox shows.
pub const MAX_CONCLUSION_LINES: usize = 5;
/// The `throughput_review_finished` events the supervisor reads back for
/// the periods reviewed: more than a week of hours.
pub const HISTORY_EVENTS: usize = 400;
/// How long a started review counts as running without its finish: the
/// job's timeout and a margin. A review a handoff left running, or one on
/// another supervisor, is not started again meanwhile.
pub const RUNNING_MS: i64 = 35 * 60 * 1000;
/// The fence the weekly job puts its next move in.
pub const NEXT_MOVE_FENCE: &str = "next_move";

string_enum!(ReviewMode {
    Hourly => "hourly",
    Daily => "daily",
    Weekly => "weekly",
});

impl ReviewMode {
    pub const ALL: [Self; 3] = [Self::Hourly, Self::Daily, Self::Weekly];
}

/// The period a review covers: `[start_ms, end_ms)`, labelled in the
/// host's time zone (`2026-09-29T13`, `2026-09-28`, `2026-W39`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Window {
    pub mode: ReviewMode,
    pub label: String,
    pub start_ms: i64,
    pub end_ms: i64,
}

/// The latest finished period of `mode` at `now_ms`, `offset_ms` east of
/// UTC: the last whole local hour, yesterday, or the ISO week before this
/// one.
pub fn window(mode: ReviewMode, now_ms: i64, offset_ms: i64) -> Window {
    let (start_ms, end_ms) = match mode {
        ReviewMode::Hourly => {
            let end = (now_ms + offset_ms).div_euclid(HOUR_MS) * HOUR_MS - offset_ms;
            (end - HOUR_MS, end)
        }
        ReviewMode::Daily => {
            let end = Period::Day.start(now_ms, offset_ms);
            (end - DAY_MS, end)
        }
        ReviewMode::Weekly => {
            let end = Period::Week.start(now_ms, offset_ms);
            (end - 7 * DAY_MS, end)
        }
    };
    let label = match mode {
        ReviewMode::Hourly => {
            let hour = (start_ms + offset_ms).rem_euclid(DAY_MS) / HOUR_MS;
            format!(
                "{}T{hour:02}",
                Period::Day.label(Period::Day.start(start_ms, offset_ms), offset_ms)
            )
        }
        ReviewMode::Daily => Period::Day.label(start_ms, offset_ms),
        ReviewMode::Weekly => Period::Week.label(start_ms, offset_ms),
    };
    Window {
        mode,
        label,
        start_ms,
        end_ms,
    }
}

/// Whether a review of `mode` for `period` finished already among `events`
/// (the newest `throughput_review_finished`, whatever its outcome: a failed
/// review is not tried again for its period), but for one whose provider
/// could not be used (`provider_unusable`), which is started again on the
/// other provider (ADR-t1063-1 decision 4, task 1220).
pub fn reviewed(events: &[RunEvent], mode: ReviewMode, period: &str) -> bool {
    events.iter().any(|event| {
        event.kind == THROUGHPUT_REVIEW_FINISHED
            && event.payload["mode"] == mode.as_str()
            && event.payload["period"] == period
            && event.payload.get("provider_unusable").is_none()
    })
}

/// Whether a review of `mode` for `period` started less than
/// [`RUNNING_MS`] before `now_ms` among `started` (the newest
/// `throughput_review_started`) and has no finish of its directory among
/// `finished`: it is still running somewhere.
pub fn running(
    started: &[RunEvent],
    finished: &[RunEvent],
    mode: ReviewMode,
    period: &str,
    now_ms: i64,
) -> bool {
    started.iter().any(|event| {
        event.kind == THROUGHPUT_REVIEW_STARTED
            && event.payload["mode"] == mode.as_str()
            && event.payload["period"] == period
            && timestamp_millis(&event.created_at).is_some_and(|at| now_ms - at < RUNNING_MS)
            && !finished.iter().any(|end| {
                end.kind == THROUGHPUT_REVIEW_FINISHED
                    && event.payload["dir"].is_string()
                    && end.payload["dir"] == event.payload["dir"]
            })
    })
}

/// The pids of the reviews among `finished` (the newest
/// `throughput_review_finished`) that ran as children of `parent_pid` and
/// finished at `since_ms` or later: a supervisor that exec'd while they ran
/// is still their parent and reaps them by these.
pub fn children_finished(finished: &[RunEvent], parent_pid: u32, since_ms: i64) -> Vec<u32> {
    finished
        .iter()
        .filter(|event| {
            event.kind == THROUGHPUT_REVIEW_FINISHED
                && event.payload["parent_pid"].as_u64() == Some(u64::from(parent_pid))
                && timestamp_millis(&event.created_at).is_some_and(|at| at >= since_ms)
        })
        .filter_map(|event| event.payload["pid"].as_u64())
        .filter_map(|pid| u32::try_from(pid).ok())
        .collect()
}

/// The landings (unix milliseconds) in each of the `buckets` spans of
/// `bucket_ms` that end at `end_ms`, oldest first.
pub fn bucket_counts(
    landings_ms: &[i64],
    end_ms: i64,
    bucket_ms: i64,
    buckets: usize,
) -> Vec<usize> {
    let mut counts = vec![0; buckets];
    let span = bucket_ms * i64::try_from(buckets).unwrap_or(i64::MAX);
    let start = end_ms.saturating_sub(span);
    for &at in landings_ms {
        if at >= start && at < end_ms {
            let index = usize::try_from((at - start) / bucket_ms).unwrap_or(0);
            counts[index.min(buckets - 1)] += 1;
        }
    }
    counts
}

string_enum!(HourlyReason {
    Deviation => "deviation",
    SustainedDrop => "sustained_drop",
    NoLanding => "no_landing",
});

/// What the rules made of the last whole hour.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HourlyJudgment {
    /// The landings of the judged hour.
    pub landings: usize,
    /// The moving averages the rules read at the judged hour.
    pub mean_baseline: f64,
    pub mean_short: f64,
    pub mean_long: f64,
    /// The rules the judged hour met.
    pub reasons: Vec<HourlyReason>,
    /// Whether the job starts: the hour met a rule (ADR-t996-1 decision 2).
    /// A state that goes on is reviewed each hour it lasts.
    pub triggered: bool,
}

fn mean(counts: &[usize]) -> f64 {
    if counts.is_empty() {
        return 0.0;
    }
    #[allow(clippy::cast_precision_loss)]
    let (sum, n) = (counts.iter().sum::<usize>() as f64, counts.len() as f64);
    sum / n
}

/// The mean of the `hours` hours that end with `at`.
fn mean_to(counts: &[usize], at: usize, hours: usize) -> f64 {
    mean(&counts[(at + 1).saturating_sub(hours)..=at])
}

/// The rules the hour at `at` of `counts` meets: its landings are off the
/// mean of the hours before it by [`DEVIATION_RATIO`] of that mean and by
/// [`DEVIATION_MIN`] landings or more; the short mean was
/// [`DROP_RATIO`] or more below the long mean at each of the
/// [`DROP_HOURS`] hours that end with it; or it landed nothing.
fn reasons_at(counts: &[usize], at: usize) -> Vec<HourlyReason> {
    let mut reasons = Vec::new();
    #[allow(clippy::cast_precision_loss)]
    let landed = counts[at] as f64;
    let baseline = mean(&counts[at.saturating_sub(BASELINE_HOURS)..at]);
    let off = (landed - baseline).abs();
    if off >= DEVIATION_MIN && off >= DEVIATION_RATIO * baseline {
        reasons.push(HourlyReason::Deviation);
    }
    let dropped = (0..DROP_HOURS).all(|back| {
        let hour = at - back;
        let long = mean_to(counts, hour, LONG_HOURS);
        long > 0.0 && mean_to(counts, hour, SHORT_HOURS) <= (1.0 - DROP_RATIO) * long
    });
    if dropped {
        reasons.push(HourlyReason::SustainedDrop);
    }
    if counts[at] == 0 {
        reasons.push(HourlyReason::NoLanding);
    }
    reasons
}

/// Judge the last hour of `counts` (the landings of each of
/// [`LOOKBACK_HOURS`] + 1 hours, oldest first).
pub fn judge_hourly(counts: &[usize]) -> Result<HourlyJudgment, DomainError> {
    if counts.len() != LOOKBACK_HOURS + 1 {
        return Err(DomainError::UnknownValue {
            kind: "hourly landing counts",
            value: counts.len().to_string(),
        });
    }
    let at = counts.len() - 1;
    let reasons = reasons_at(counts, at);
    let triggered = !reasons.is_empty();
    Ok(HourlyJudgment {
        landings: counts[at],
        mean_baseline: mean(&counts[at - BASELINE_HOURS..at]),
        mean_short: mean_to(counts, at, SHORT_HOURS),
        mean_long: mean_to(counts, at, LONG_HOURS),
        reasons,
        triggered,
    })
}

/// The one change the weekly job proposes (step 5 of the review), which the
/// supervisor records as a finding marked for a proposal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NextMove {
    pub summary: String,
    #[serde(default)]
    pub detail: Option<String>,
    pub why: String,
}

/// What the job printed: the conclusion the inbox shows, the whole text,
/// and the weekly job's next move.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReviewOutput {
    pub conclusion: Vec<String>,
    pub text: String,
    pub next_move: Option<NextMove>,
    /// Why a next move was left out: its block did not parse.
    pub next_move_error: Option<String>,
}

/// Read the job's output: the lines under its `## Conclusion` heading up to
/// the next heading or fence (or, without that heading, its first lines), at most
/// [`MAX_CONCLUSION_LINES`]; the text without the next move's block; and
/// the JSON of the block fenced as [`NEXT_MOVE_FENCE`], when there is one.
pub fn parse_output(output: &str) -> ReviewOutput {
    let mut text = Vec::new();
    let mut block: Option<Vec<&str>> = None;
    let mut next_move_text = None;
    for line in output.lines() {
        match &mut block {
            Some(lines) if line.trim_start().starts_with("```") => {
                next_move_text = Some(lines.join("\n"));
                block = None;
            }
            Some(lines) => lines.push(line),
            None if line.trim() == format!("```{NEXT_MOVE_FENCE}") => block = Some(Vec::new()),
            None => text.push(line),
        }
    }
    // A block never closed is text, and no next move.
    let unclosed = block.is_some();
    if let Some(lines) = block {
        text.push("```next_move");
        text.extend(lines);
    }
    let text = text.join("\n").trim().to_owned();
    let lines: Vec<&str> = text.lines().collect();
    let heading = lines.iter().position(|line| {
        line.trim_start_matches('#')
            .trim()
            .eq_ignore_ascii_case("conclusion")
            && line.starts_with('#')
    });
    let body = match heading {
        Some(at) => lines[at + 1..]
            .iter()
            .take_while(|line| !line.starts_with('#') && !line.trim_start().starts_with("```"))
            .copied()
            .collect::<Vec<_>>(),
        None => lines,
    };
    let conclusion = body
        .iter()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .take(MAX_CONCLUSION_LINES)
        .map(str::to_owned)
        .collect();
    let (next_move, next_move_error) = match next_move_text {
        None if unclosed => (None, Some("the next move's block is not closed".to_owned())),
        None => (None, None),
        Some(json) => match serde_json::from_str::<NextMove>(&json) {
            Ok(next) if !next.summary.trim().is_empty() => (Some(next), None),
            Ok(_) => (None, Some("the next move has no summary".to_owned())),
            Err(error) => (
                None,
                Some(format!("the next move is not its JSON: {error}")),
            ),
        },
    };
    ReviewOutput {
        conclusion,
        text,
        next_move,
        next_move_error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const JST: i64 = 9 * HOUR_MS;
    // 2026-09-29T13:25:00+09:00 (a Tuesday).
    const NOW: i64 = 1_790_655_900_000;

    #[test]
    fn each_mode_covers_its_last_finished_local_period() {
        let hourly = window(ReviewMode::Hourly, NOW, JST);
        assert_eq!(hourly.label, "2026-09-29T12");
        assert_eq!(hourly.end_ms - hourly.start_ms, HOUR_MS);
        assert_eq!((hourly.end_ms + JST) % HOUR_MS, 0);
        assert!(hourly.end_ms <= NOW && NOW - hourly.end_ms < HOUR_MS);
        let daily = window(ReviewMode::Daily, NOW, JST);
        assert_eq!(daily.label, "2026-09-28");
        assert_eq!(daily.end_ms - daily.start_ms, DAY_MS);
        let weekly = window(ReviewMode::Weekly, NOW, JST);
        assert_eq!(weekly.label, "2026-W39");
        assert_eq!(weekly.end_ms - weekly.start_ms, 7 * DAY_MS);
        assert_eq!(weekly.end_ms, Period::Week.start(NOW, JST));
        // Midnight's hour is the day before's 23rd.
        let midnight = window(ReviewMode::Hourly, daily.end_ms + 60_000, JST);
        assert_eq!(midnight.label, "2026-09-28T23");
    }

    #[test]
    fn landings_are_counted_in_their_buckets_and_the_rest_left_out() {
        let end = 10 * HOUR_MS;
        let counts = bucket_counts(
            &[
                end - 1,
                end - HOUR_MS,
                end - HOUR_MS - 1,
                end,
                end - 3 * HOUR_MS - 1,
            ],
            end,
            HOUR_MS,
            3,
        );
        assert_eq!(counts, [0, 1, 2]);
    }

    fn hours(steady: usize, tail: &[usize]) -> Vec<usize> {
        let mut counts = vec![steady; LOOKBACK_HOURS + 1 - tail.len()];
        counts.extend_from_slice(tail);
        counts
    }

    #[test]
    fn a_steady_hour_starts_no_review() {
        let judged = judge_hourly(&hours(4, &[5])).unwrap();
        assert!(judged.reasons.is_empty());
        assert!(!judged.triggered);
        assert_eq!(judged.landings, 5);
        assert!((judged.mean_baseline - 4.0).abs() < 1e-9);
    }

    #[test]
    fn an_hour_off_its_average_by_half_and_three_landings_starts_one() {
        let rise = judge_hourly(&hours(4, &[10])).unwrap();
        assert_eq!(rise.reasons, [HourlyReason::Deviation]);
        assert!(rise.triggered);
        // Half off but fewer than three landings is noise.
        assert!(judge_hourly(&hours(2, &[4])).unwrap().reasons.is_empty());
        // Three off but less than half of a large average too.
        assert!(judge_hourly(&hours(10, &[13])).unwrap().reasons.is_empty());
        let fall = judge_hourly(&hours(8, &[1])).unwrap();
        assert_eq!(fall.reasons, [HourlyReason::Deviation]);
    }

    #[test]
    fn a_short_average_below_the_long_one_for_three_hours_starts_one_each_hour() {
        // 3h means at the last three hours: 2, 2, 2 against a long mean near 6.
        let first = judge_hourly(&hours(6, &[2, 2, 2, 2])).unwrap();
        assert!(
            first.reasons.contains(&HourlyReason::SustainedDrop),
            "{first:?}"
        );
        assert!(first.triggered);
        // An hour later it goes on, and is reviewed again (ADR-t996-1
        // decision 2: every hour that meets a rule).
        let later = judge_hourly(&hours(6, &[2, 2, 2, 2, 2])).unwrap();
        assert!(later.reasons.contains(&HourlyReason::SustainedDrop));
        assert!(later.triggered);
        // Two hours of it are not three.
        let short = judge_hourly(&hours(6, &[2, 2])).unwrap();
        assert!(!short.reasons.contains(&HourlyReason::SustainedDrop));
    }

    #[test]
    fn every_hour_without_a_landing_starts_one() {
        let first = judge_hourly(&hours(1, &[0])).unwrap();
        assert_eq!(first.reasons, [HourlyReason::NoLanding]);
        assert!(first.triggered);
        let again = judge_hourly(&hours(1, &[0, 0])).unwrap();
        assert!(again.reasons.contains(&HourlyReason::NoLanding));
        assert!(again.triggered);
        // An idle queue too.
        assert!(judge_hourly(&hours(0, &[])).unwrap().triggered);
    }

    #[test]
    fn the_judgment_needs_the_lookback_hours() {
        assert!(judge_hourly(&[1, 2, 3]).is_err());
    }

    #[test]
    fn the_output_gives_its_conclusion_text_and_next_move() {
        let output = "## Conclusion\n- landings fell to 1/h\n- the landing slot is the constraint\n\n## Details\nlong text\n```next_move\n{\"summary\": \"split the e2e\", \"why\": \"verify is 60%\"}\n```\n";
        let parsed = parse_output(output);
        assert_eq!(
            parsed.conclusion,
            [
                "- landings fell to 1/h",
                "- the landing slot is the constraint"
            ]
        );
        assert!(parsed.text.contains("long text"));
        assert!(!parsed.text.contains("split the e2e"));
        let next = parsed.next_move.unwrap();
        assert_eq!(next.summary, "split the e2e");
        assert_eq!(next.detail, None);
        assert_eq!(parsed.next_move_error, None);
    }

    #[test]
    fn an_output_without_a_heading_gives_its_first_lines_and_a_bad_move_its_error() {
        let many: String = (1..=8).map(|n| format!("line {n}\n")).collect();
        let parsed = parse_output(&format!("{many}```next_move\nnot json\n```\n"));
        assert_eq!(parsed.conclusion.len(), MAX_CONCLUSION_LINES);
        assert_eq!(parsed.conclusion[0], "line 1");
        assert!(parsed.next_move.is_none());
        assert!(parsed.next_move_error.unwrap().contains("not its JSON"));
        let blank = parse_output("```next_move\n{\"summary\": \" \", \"why\": \"x\"}\n```");
        assert_eq!(
            blank.next_move_error.as_deref(),
            Some("the next move has no summary")
        );
        assert!(blank.conclusion.is_empty());
    }

    #[test]
    fn a_period_is_reviewed_once_its_finish_is_recorded() {
        let event = |kind: &str, mode: &str, period: &str| RunEvent {
            id: super::super::EventId::new(1),
            run_id: None,
            task_id: None,
            goal_id: None,
            kind: kind.into(),
            payload: serde_json::json!({"mode": mode, "period": period}),
            created_at: String::new(),
            actor: None,
        };
        let events = [
            event(THROUGHPUT_REVIEW_FINISHED, "hourly", "2026-09-29T12"),
            event("throughput_review_started", "daily", "2026-09-28"),
        ];
        assert!(reviewed(&events, ReviewMode::Hourly, "2026-09-29T12"));
        assert!(!reviewed(&events, ReviewMode::Hourly, "2026-09-29T11"));
        assert!(!reviewed(&events, ReviewMode::Daily, "2026-09-28"));
        // One whose provider could not be used is reviewed again (task
        // 1220).
        let unusable = RunEvent {
            payload: serde_json::json!({"mode": "hourly", "period": "2026-09-29T11", "dir": "/r/a",
                "provider_unusable": {"provider": "codex", "reason": "usage_limit"}}),
            ..event(THROUGHPUT_REVIEW_FINISHED, "", "")
        };
        assert!(!reviewed(
            std::slice::from_ref(&unusable),
            ReviewMode::Hourly,
            "2026-09-29T11"
        ));
        let started = |created_at: &str| RunEvent {
            kind: THROUGHPUT_REVIEW_STARTED.into(),
            created_at: created_at.into(),
            payload: serde_json::json!({"mode": "daily", "period": "2026-09-28", "dir": "/r/d"}),
            ..event("", "daily", "2026-09-28")
        };
        let now = timestamp_millis("2026-09-29T01:00:00.000Z").unwrap();
        assert!(running(
            &[started("2026-09-29T00:40:00.000Z")],
            &[unusable],
            ReviewMode::Daily,
            "2026-09-28",
            now
        ));
        assert!(!running(
            &[started("2026-09-29T00:20:00.000Z")],
            &[],
            ReviewMode::Daily,
            "2026-09-28",
            now
        ));
        assert!(!running(
            &[started("2026-09-29T00:40:00.000Z")],
            &[],
            ReviewMode::Weekly,
            "2026-09-28",
            now
        ));
        // A start whose directory has a finish ran already.
        let ended = RunEvent {
            payload: serde_json::json!({"mode": "daily", "period": "2026-09-28", "dir": "/r/d"}),
            ..event(THROUGHPUT_REVIEW_FINISHED, "", "")
        };
        assert!(!running(
            &[started("2026-09-29T00:40:00.000Z")],
            &[ended],
            ReviewMode::Daily,
            "2026-09-28",
            now
        ));
    }

    #[test]
    fn the_children_of_a_supervisor_are_its_finished_reviews_since_a_time() {
        let finished = |pid: u32, parent: u32, created_at: &str| RunEvent {
            id: super::super::EventId::new(1),
            run_id: None,
            task_id: None,
            goal_id: None,
            kind: THROUGHPUT_REVIEW_FINISHED.into(),
            payload: serde_json::json!({"pid": pid, "parent_pid": parent}),
            created_at: created_at.into(),
            actor: None,
        };
        let since = timestamp_millis("2026-09-29T01:00:00.000Z").unwrap();
        let events = [
            finished(11, 7, "2026-09-29T01:10:00.000Z"),
            finished(12, 8, "2026-09-29T01:10:00.000Z"),
            finished(13, 7, "2026-09-29T00:50:00.000Z"),
        ];
        assert_eq!(children_finished(&events, 7, since), [11]);
        assert!(children_finished(&events, 9, since).is_empty());
    }

    #[test]
    fn an_unclosed_next_move_is_left_in_the_text() {
        let parsed = parse_output("## Conclusion\n- one\n```next_move\n{\"summary\": \"x\"\n");
        assert_eq!(parsed.conclusion, ["- one"]);
        assert!(parsed.next_move.is_none());
        assert!(parsed.text.contains("{\"summary\""));
        assert_eq!(
            parsed.next_move_error.as_deref(),
            Some("the next move's block is not closed")
        );
    }

    #[test]
    fn the_modes_parse_from_their_names() {
        for mode in ReviewMode::ALL {
            assert_eq!(mode.as_str().parse::<ReviewMode>().unwrap(), mode);
        }
    }
}

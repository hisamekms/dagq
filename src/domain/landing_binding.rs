//! What a landing's judgment is bound to, and the comparison of it with
//! what lands (ADR-t4840-2 decisions 7 and 8): every judgment that lets a
//! run land (a review's pass, an AI's `land` of a concern, the convergence
//! after the revise limit, a person's `land` answer to approve_landing and
//! a person's or the inbox's direct `integrate`) carries, in its event's
//! [`BINDING_KEY`], the set of the files the run's diff touches, each with
//! its blob hash after the change or [`BlobState::Removed`]. The landing
//! compares the set the merged result touches with the set of the run's
//! last such judgment ([`last_judgment`]) and lands only when they are the
//! same ([`compare`]); the normalized diff hash and the tree hash ride
//! along for the record and are compared with nothing.
//!
//! A comparison that differs (`integration_compared` with `matched:
//! false`) spends every judgment before it: the run lands again only after
//! a judgment of the path that gave the spent one ([`Reapproval`]).

use std::collections::BTreeMap;

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::audit::{BlobState, LandingFacts};
use super::{EventId, EventKind, RunEvent};

/// The key of a judgment event's payload that holds its [`LandingFacts`].
pub const BINDING_KEY: &str = "binding";

/// The files a diff touches, by path, each with its blob hash after the
/// change or its removal.
pub type BlobSet = BTreeMap<String, BlobState>;

/// Which judgment let a run land (ADR-t4840-2 decision 8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JudgmentPath {
    /// The review passed.
    ReviewPass,
    /// The review's job recommended landing a concern, and the runtime
    /// applied it (ADR-t451-1 decision 3).
    AiLand,
    /// Past the revise limit with no code finding (ADR-t2599-1).
    Converged,
    /// A person answered `land` to approve_landing.
    HumanLand,
    /// A person or the inbox ran `dagq integrate` on the run (ADR-0016
    /// decision 5).
    DirectIntegrate,
}

impl JudgmentPath {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReviewPass => "review_pass",
            Self::AiLand => "ai_land",
            Self::Converged => "converged",
            Self::HumanLand => "approve_landing",
            Self::DirectIntegrate => "integrate",
        }
    }

    /// Where a run whose content changed after a judgment of this path
    /// goes to be approved again.
    pub const fn reapproval(self) -> Reapproval {
        match self {
            Self::ReviewPass | Self::AiLand | Self::Converged => Reapproval::Review,
            Self::HumanLand => Reapproval::ApproveLanding,
            Self::DirectIntegrate => Reapproval::Integrate,
        }
    }
}

/// Where a run whose landed content differs from its judgment goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reapproval {
    /// The supervisor reviews it again.
    Review,
    /// The supervisor opens its approve_landing ask again.
    ApproveLanding,
    /// It waits for a person's or the inbox's `dagq integrate` again.
    Integrate,
}

impl Reapproval {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Review => "review",
            Self::ApproveLanding => "approve_landing",
            Self::Integrate => "integrate",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        [Self::Review, Self::ApproveLanding, Self::Integrate]
            .into_iter()
            .find(|r| r.as_str() == text)
    }
}

/// The judgment `event` records, if it lets the run land: a
/// `review_finished` whose verdict lets it land
/// ([`super::concern::lets_land`]), a converged `revise_limit_judged`, or
/// an `integration_approved` (an approve_landing answer when it names its
/// ask). An `integration_approved` the supervisor wrote of no ask is none:
/// a landing the supervisor starts after a judgment is no judgment of its
/// own.
pub fn judgment_of(event: &RunEvent) -> Option<JudgmentPath> {
    match event.kind.known()? {
        EventKind::ReviewFinished if super::concern::lets_land(&event.payload) => {
            Some(if event.payload["verdict"] == "pass" {
                JudgmentPath::ReviewPass
            } else {
                JudgmentPath::AiLand
            })
        }
        EventKind::ReviseLimitJudged if event.payload["converged"] == true => {
            Some(JudgmentPath::Converged)
        }
        EventKind::IntegrationApproved => {
            if event.payload.get("ask_id").is_some_and(|id| !id.is_null()) {
                Some(JudgmentPath::HumanLand)
            } else if event
                .actor
                .as_ref()
                .is_some_and(|actor| actor.role == "supervisor")
            {
                None
            } else {
                Some(JudgmentPath::DirectIntegrate)
            }
        }
        _ => None,
    }
}

/// The judgment a landing compares with: its event, its path and what it
/// was bound to (`None` for one recorded with no binding).
#[derive(Debug, Clone, PartialEq)]
pub struct Judgment<'a> {
    pub event: &'a RunEvent,
    pub path: JudgmentPath,
    pub binding: Option<LandingFacts>,
}

/// The binding a judgment event's payload holds, read leniently: a field
/// a later version adds to it is passed over, so its set is still
/// compared.
pub fn binding_of(payload: &Value) -> Option<LandingFacts> {
    facts_of(payload.get(BINDING_KEY)?)
}

/// [`binding_of`]'s reading of one binding's value.
fn facts_of(binding: &Value) -> Option<LandingFacts> {
    Some(LandingFacts {
        approved_blobs: serde_json::from_value(binding.get("approved_blobs")?.clone()).ok()?,
        diff_hash: binding["diff_hash"].as_str().map(str::to_owned),
        tree_hash: binding["tree_hash"].as_str().map(str::to_owned),
    })
}

/// The binding of the diff approve_landing ask `ask_id` of `events` was
/// opened on: the later, before its `ask_opened`, of the latest review's
/// start (its `review_started`'s binding, the diff the review showed) and
/// the latest comparison that differed (the set it found land, which the
/// ask opened again shows). A person's `land` of the ask is bound to it,
/// not to the diff when the answer is applied, so a change after the ask
/// opened goes back to it. `None` when its opening or what it showed is
/// not on record.
pub fn ask_binding(events: &[RunEvent], ask_id: i64) -> Option<LandingFacts> {
    let opened = events.iter().position(|e| {
        e.kind == EventKind::AskOpened && e.payload["ask_id"].as_i64() == Some(ask_id)
    })?;
    let shown = events[..opened]
        .iter()
        .rev()
        .find(|e| e.kind == EventKind::ReviewStarted || is_mismatch(e))?;
    if is_mismatch(shown) {
        facts_of(shown.payload.get("landed")?)
    } else {
        binding_of(&shown.payload)
    }
}

/// The person's re-approval (the approve_landing ask again, or a person's
/// `integrate` again) that the latest comparison sending the run to a
/// person found due, while no person has judged the run since: neither an
/// approval of theirs (a `land` answer or an `integrate`) nor an answer
/// that decided the landing otherwise (`send_back`, `cancel`). A review
/// that lets the run land after it (the run's session rewrote its receipt
/// for a commit it had not named and was reviewed again) does not stand
/// for the person's (ADR-t4840-2 decision 8). `None` when no comparison
/// sent the run to a person, or a person judged it since.
pub fn person_reapproval_due(events: &[RunEvent]) -> Option<Reapproval> {
    let (at, reapproval) = events.iter().enumerate().rev().find_map(|(at, e)| {
        if !is_mismatch(e) {
            return None;
        }
        match Reapproval::parse(e.payload["reapproval"].as_str()?)? {
            reapproval @ (Reapproval::ApproveLanding | Reapproval::Integrate) => {
                Some((at, reapproval))
            }
            Reapproval::Review => None,
        }
    })?;
    let judged = events[at + 1..].iter().any(|e| {
        e.kind == EventKind::IntegrationApproved
            && matches!(
                judgment_of(e),
                Some(JudgmentPath::HumanLand | JudgmentPath::DirectIntegrate)
            )
            || e.kind == EventKind::LandingDecided && e.payload["ask_id"].is_i64()
    });
    (!judged).then_some(reapproval)
}

/// The head the run's branch stood at when the landing's rebase or the
/// landing recheck found it conflicting with main, when that conflict is
/// what parked the run last: the resume that resolves it rewrites the
/// branch after its judgment, its own change for the audit (a
/// `conflict_resolution`, ADR-t4840-2 decision 7). `None` when the run was
/// parked for anything else, or its park names no head.
pub fn conflict_parked_head(events: &[RunEvent]) -> Option<super::CommitSha> {
    let park = events
        .iter()
        .rev()
        .find(|event| super::resume::parks(event))?;
    let conflict = matches!(
        park.kind.known(),
        Some(EventKind::IntegrationDeferred | EventKind::LandingRecheckFailed)
    ) && park.payload["code"] == super::ReasonCode::RebaseConflict.as_str();
    let head = park.payload["head"].as_str().filter(|_| conflict)?;
    super::CommitSha::parse(head, "head").ok()
}

/// What a resume that resolved the run (with a session, `resume_finished`,
/// or without one, `resume_skipped`) rewrote its branch from to `head`,
/// and how: from the head a rebase conflict parked it at
/// ([`conflict_parked_head`], a `conflict_resolution`), or else, once a
/// judgment that lets the run land is on record, from the commit its
/// receipt named before the resume (`named`, a `resume`: the session's fix
/// after its park). Each is its own change for the audit (ADR-t4840-2
/// decision 7). `None` when the head did not move, or nothing judged the
/// run before it.
pub fn resume_rewrite(
    events: &[RunEvent],
    named: Option<&super::CommitSha>,
    head: &super::CommitSha,
) -> Option<(super::CommitSha, &'static str)> {
    let (before, rewrite) = match conflict_parked_head(events) {
        Some(parked) => (parked, CONFLICT_RESOLUTION),
        None if events.iter().any(|e| judgment_of(e).is_some()) => (named?.clone(), RESUME),
        None => return None,
    };
    (before != *head).then_some((before, rewrite))
}

/// The rewrite of a resume that resolved the conflict its park was for.
pub const CONFLICT_RESOLUTION: &str = "conflict_resolution";
/// The rewrite of a resume that fixed what else parked the run.
pub const RESUME: &str = "resume";

/// Whether `event` is a comparison that found the landed content differ
/// from its judgment.
pub fn is_mismatch(event: &RunEvent) -> bool {
    event.kind == EventKind::IntegrationCompared && event.payload["matched"] == false
}

/// The events no comparison has spent: those after the latest
/// `integration_compared` that differed, all of them when none did.
pub fn unspent(events: &[RunEvent]) -> &[RunEvent] {
    let start = events.iter().rposition(is_mismatch).map_or(0, |at| at + 1);
    &events[start..]
}

/// The run's last judgment that lets it land, of `events` (oldest first,
/// already cut at the run's latest park at a turn's end): the later of its
/// latest review's judgment (the `review_finished` itself, or a converged
/// `revise_limit_judged` after it) and its latest `integration_approved`
/// that is a judgment ([`judgment_of`]). A comparison that differed spends
/// every judgment before it ([`unspent`]); a later review that does not
/// let the run land leaves only an approval.
pub fn last_judgment(events: &[RunEvent]) -> Option<Judgment<'_>> {
    let events = unspent(events);
    let review = events
        .iter()
        .rposition(|e| e.kind == EventKind::ReviewFinished)
        .and_then(|at| {
            let finished = &events[at];
            if judgment_of(finished).is_some() {
                Some(finished)
            } else {
                events[at + 1..]
                    .iter()
                    .rev()
                    .find(|e| judgment_of(e) == Some(JudgmentPath::Converged))
            }
        });
    let approval = events
        .iter()
        .rev()
        .find(|e| e.kind == EventKind::IntegrationApproved && judgment_of(e).is_some());
    let event = match (review, approval) {
        (Some(review), Some(approval)) => {
            if approval.id > review.id {
                approval
            } else {
                review
            }
        }
        (Some(one), None) | (None, Some(one)) => one,
        (None, None) => return None,
    };
    Some(Judgment {
        event,
        path: judgment_of(event)?,
        binding: binding_of(&event.payload),
    })
}

/// What a comparison found: the paths whose blob hash, or whose being in
/// the set at all, differs between the judged set and the landed one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comparison {
    pub differing: Vec<String>,
}

impl Comparison {
    pub fn matched(&self) -> bool {
        self.differing.is_empty()
    }
}

/// Compare the set a judgment was bound to (`judged`) with the set the
/// merged result touches (`landed`): the same paths, each with the same
/// blob hash or both removed. Only the sets are compared: neither the
/// diff hash nor the tree hash is an input (ADR-t4840-2 decision 7).
pub fn compare(judged: &BlobSet, landed: &BlobSet) -> Comparison {
    let mut differing: Vec<String> = judged
        .iter()
        .filter(|(path, state)| landed.get(*path) != Some(*state))
        .map(|(path, _)| path.clone())
        .collect();
    differing.extend(
        landed
            .keys()
            .filter(|path| !judged.contains_key(*path))
            .cloned(),
    );
    differing.sort();
    Comparison { differing }
}

/// What the landing does with `judgment` and the set `landed` touches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// The sets are the same: it lands.
    Land,
    /// They differ (or the judgment was bound to nothing): it does not
    /// land, and goes to `reapproval`.
    Reapprove {
        reapproval: Reapproval,
        differing: Vec<String>,
    },
}

/// [`compare`] of `judgment`'s set with `landed`, and where a run that
/// differs goes ([`JudgmentPath::reapproval`]). A judgment bound to
/// nothing (recorded before judgments were bound) holds nothing to compare
/// with: every landed path differs.
pub fn decide(judgment: &Judgment<'_>, landed: &BlobSet) -> Decision {
    let differing = match &judgment.binding {
        Some(binding) => compare(&binding.approved_blobs, landed).differing,
        None => landed.keys().cloned().collect(),
    };
    if judgment.binding.is_some() && differing.is_empty() {
        return Decision::Land;
    }
    Decision::Reapprove {
        reapproval: judgment.path.reapproval(),
        differing,
    }
}

/// The latest comparison that differed and its re-approval, when nothing
/// after it moved the run on (`moved_on` says whether an event after the
/// given one did): what the supervisor sends the run to.
pub fn pending_reapproval(
    events: &[RunEvent],
    moved_on: impl Fn(EventId) -> bool,
) -> Option<(EventId, Reapproval)> {
    let mismatch = events.iter().rev().find(|e| is_mismatch(e))?;
    let reapproval = Reapproval::parse(mismatch.payload["reapproval"].as_str()?)?;
    (!moved_on(mismatch.id)).then_some((mismatch.id, reapproval))
}

/// The hash of a diff as recorded and shown only, never compared: the
/// SHA-256 of the diff with its `index` lines dropped and each hunk header
/// (its line numbers and the function name after them) cut to `@@`, the
/// normalization of `git patch-id --verbatim` (whitespace kept, a line's
/// `\r` included), of the diff's bytes as Git wrote them (a file that is
/// not UTF-8 included).
pub fn diff_hash(diff: impl AsRef<[u8]>) -> String {
    let mut hasher = Sha256::new();
    // Split at `\n` alone: a line's `\r` is its content (`--verbatim`).
    for line in diff.as_ref().split_inclusive(|b| *b == b'\n') {
        let line = line.strip_suffix(b"\n").unwrap_or(line);
        if line.starts_with(b"index ") {
            continue;
        }
        if line.starts_with(b"@@ ") {
            // `@@ -a,b +c,d @@ function`: the whole header goes, its line
            // numbers and the function name after them alike.
            hasher.update(b"@@");
        } else {
            hasher.update(line);
        }
        hasher.update(b"\n");
    }
    hex(&hasher.finalize())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::EventActor;
    use serde_json::json;

    fn event(id: i64, kind: EventKind, payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.as_str().into(),
            payload,
            created_at: String::new(),
            actor: None,
        }
    }

    fn blobs(pairs: &[(&str, Option<&str>)]) -> BlobSet {
        pairs
            .iter()
            .map(|(path, blob)| {
                (
                    (*path).to_owned(),
                    blob.map_or(BlobState::Removed, |b| BlobState::Blob(b.to_owned())),
                )
            })
            .collect()
    }

    fn bound(pairs: &[(&str, Option<&str>)]) -> Value {
        serde_json::to_value(LandingFacts {
            approved_blobs: blobs(pairs),
            diff_hash: Some("d".into()),
            tree_hash: Some("t".into()),
        })
        .unwrap()
    }

    fn pass(id: i64, pairs: &[(&str, Option<&str>)]) -> RunEvent {
        event(
            id,
            EventKind::ReviewFinished,
            json!({"verdict": "pass", BINDING_KEY: bound(pairs)}),
        )
    }

    /// The judging events of every path are told apart, and what is no
    /// judgment (a revise, a converged-false judgment, an approval the
    /// supervisor wrote of no ask) is none.
    #[test]
    fn each_path_that_lets_a_run_land_is_a_judgment_and_a_supervisors_own_approval_is_not() {
        let concern_landed = json!({"verdict": "concern", "route": {"destination": "land"}});
        let cases = [
            (
                event(1, EventKind::ReviewFinished, json!({"verdict": "pass"})),
                Some(JudgmentPath::ReviewPass),
            ),
            (
                event(1, EventKind::ReviewFinished, concern_landed),
                Some(JudgmentPath::AiLand),
            ),
            (
                event(1, EventKind::ReviewFinished, json!({"verdict": "revise"})),
                None,
            ),
            (
                event(1, EventKind::ReviseLimitJudged, json!({"converged": true})),
                Some(JudgmentPath::Converged),
            ),
            (
                event(1, EventKind::ReviseLimitJudged, json!({"converged": false})),
                None,
            ),
            (
                event(1, EventKind::IntegrationApproved, json!({"ask_id": 4})),
                Some(JudgmentPath::HumanLand),
            ),
            (
                event(1, EventKind::IntegrationApproved, json!({"pid": 1})),
                Some(JudgmentPath::DirectIntegrate),
            ),
        ];
        for (event, path) in cases {
            assert_eq!(judgment_of(&event), path, "{event:?}");
        }
        let mut by_supervisor = event(1, EventKind::IntegrationApproved, json!({"pid": 1}));
        by_supervisor.actor = Some(EventActor {
            role: "supervisor".into(),
            id: "supervisor:1".into(),
            requested_by: None,
        });
        assert_eq!(judgment_of(&by_supervisor), None);
    }

    /// Each path goes back to its own re-approval.
    #[test]
    fn each_path_is_approved_again_where_it_was_judged() {
        use JudgmentPath::*;
        assert_eq!(ReviewPass.reapproval(), Reapproval::Review);
        assert_eq!(AiLand.reapproval(), Reapproval::Review);
        assert_eq!(Converged.reapproval(), Reapproval::Review);
        assert_eq!(HumanLand.reapproval(), Reapproval::ApproveLanding);
        assert_eq!(DirectIntegrate.reapproval(), Reapproval::Integrate);
        for r in [
            Reapproval::Review,
            Reapproval::ApproveLanding,
            Reapproval::Integrate,
        ] {
            assert_eq!(Reapproval::parse(r.as_str()), Some(r));
        }
    }

    /// The last judgment is the later of the latest review's and the
    /// latest approval; a later review that does not let the run land
    /// leaves the review no judgment; a mismatch spends everything before
    /// it.
    #[test]
    fn the_last_judgment_is_the_latest_unspent_one_of_any_path() {
        let a = &[("a", Some("1"))];
        let approval = event(
            3,
            EventKind::IntegrationApproved,
            json!({"ask_id": 9, BINDING_KEY: bound(a)}),
        );
        let events = [pass(1, a), approval.clone()];
        let judged = last_judgment(&events).unwrap();
        assert_eq!(
            (judged.event.id, judged.path),
            (EventId::new(3), JudgmentPath::HumanLand)
        );
        assert_eq!(judged.binding.unwrap().approved_blobs, blobs(a));

        let events = [
            pass(1, a),
            event(2, EventKind::ReviewFinished, json!({"verdict": "revise"})),
        ];
        assert!(last_judgment(&events).is_none());

        let events = [
            event(1, EventKind::ReviewFinished, json!({"verdict": "revise"})),
            event(
                2,
                EventKind::ReviseLimitJudged,
                json!({"converged": true, BINDING_KEY: bound(a)}),
            ),
        ];
        assert_eq!(
            last_judgment(&events).unwrap().path,
            JudgmentPath::Converged
        );

        let mismatch = event(
            4,
            EventKind::IntegrationCompared,
            json!({"matched": false, "reapproval": "review"}),
        );
        let events = [pass(1, a), approval, mismatch.clone()];
        assert!(
            last_judgment(&events).is_none(),
            "a mismatch spends the judgments before it"
        );
        let events = [pass(1, a), mismatch, pass(5, a)];
        assert_eq!(last_judgment(&events).unwrap().event.id, EventId::new(5));
    }

    /// A supervisor's integrate after a judgment makes none of its own:
    /// the judgment it compares with stays the review's, bound to the set
    /// the review saw, so content added after the review does not land.
    #[test]
    fn a_supervisors_landing_compares_with_the_review_before_it_not_with_itself() {
        let judged = &[("src/a.rs", Some("1"))];
        let mut own = event(
            2,
            EventKind::IntegrationApproved,
            json!({"pid": 1, BINDING_KEY: bound(&[("src/a.rs", Some("2"))])}),
        );
        own.actor = Some(EventActor {
            role: "supervisor".into(),
            id: "supervisor:1".into(),
            requested_by: None,
        });
        let events = [pass(1, judged), own];
        let judgment = last_judgment(&events).unwrap();
        assert_eq!(judgment.path, JudgmentPath::ReviewPass);
        let landed = blobs(&[("src/a.rs", Some("2"))]);
        assert_eq!(
            decide(&judgment, &landed),
            Decision::Reapprove {
                reapproval: Reapproval::Review,
                differing: vec!["src/a.rs".into()]
            }
        );
    }

    /// The same set lands; a changed blob, a removal, a file added or a
    /// file dropped after the judgment does not, naming the file.
    #[test]
    fn only_the_same_set_of_paths_and_blobs_matches() {
        let judged = blobs(&[("a", Some("1")), ("b", None)]);
        assert!(compare(&judged, &judged).matched());
        let cases = [
            (blobs(&[("a", Some("2")), ("b", None)]), vec!["a"]),
            (blobs(&[("a", None), ("b", None)]), vec!["a"]),
            (blobs(&[("a", Some("1")), ("b", Some("3"))]), vec!["b"]),
            (
                blobs(&[("a", Some("1")), ("b", None), ("c", Some("4"))]),
                vec!["c"],
            ),
            (blobs(&[("a", Some("1"))]), vec!["b"]),
        ];
        for (landed, differing) in cases {
            assert_eq!(compare(&judged, &landed).differing, differing, "{landed:?}");
        }
    }

    /// Swapping which of two same-shaped checks a diff removes keeps the
    /// normalized diff hash but not the file's blob: the comparison, which
    /// takes no diff hash, does not land it (ADR-t4840-2 decision 7, the
    /// test ask 1711 asked for). The file holds the same block twice; the
    /// approved diff removes the first block's check, the revise removes
    /// the second's instead. Independent references, made outside this code
    /// from these contents: each diff as `git diff` wrote it (the second's
    /// hunk header carries the function name as its suffix), each result's
    /// blob from `git hash-object`, the id `git patch-id --verbatim` gives
    /// both diffs, and the SHA-256 (`shasum -a 256`) of the diff
    /// with its `index` line dropped and its hunk header cut to `@@`.
    #[test]
    fn swapping_which_of_two_like_checks_is_removed_keeps_the_diff_hash_but_is_approved_again() {
        const BLOCK: &str = "fn a() {\n    let v = 1;\n    let w = 2;\n    check(x);\n    done(v);\n    done(w);\n}\n";
        const BEFORE_BLOB: &str = "746c285863491ae6cf31004c2a0cedd196f8b31c";
        const FIRST_REMOVED_BLOB: &str = "ba5143e410ae6c530f9442f54d0b950f0be8339a";
        const SECOND_REMOVED_BLOB: &str = "f26bff58d8ddb52551702375bfa94831961780dd";
        const NORMALIZED_SHA256: &str =
            "742524ff474908370ed74fa3ed19d3df12b253ad45f7189f445d4638cce60634";
        let before = BLOCK.repeat(2);
        let without_line = |n: usize| -> String {
            before
                .lines()
                .enumerate()
                .filter(|(at, _)| at + 1 != n)
                .map(|(_, line)| format!("{line}\n"))
                .collect()
        };
        // The two results come from the same file, each without one of its
        // two identical checks: unlike contents, so unlike blobs.
        let (first_removed, second_removed) = (without_line(4), without_line(11));
        assert_eq!(before.matches("    check(x);\n").count(), 2);
        assert_ne!(first_removed, second_removed);
        assert_eq!(first_removed.matches("    check(x);\n").count(), 1);
        assert_eq!(second_removed.matches("    check(x);\n").count(), 1);
        let body = " fn a() {\n     let v = 1;\n     let w = 2;\n-    check(x);\n     done(v);\n     done(w);\n }\n";
        let approved_diff = format!(
            "diff --git a/f b/f\nindex {}..{} 100644\n--- a/f\n+++ b/f\n@@ -1,7 +1,6 @@\n{body}",
            &BEFORE_BLOB[..7],
            &FIRST_REMOVED_BLOB[..7]
        );
        let revised_diff = format!(
            "diff --git a/f b/f\nindex {}..{} 100644\n--- a/f\n+++ b/f\n@@ -8,7 +8,6 @@ fn a() {{\n{body}",
            &BEFORE_BLOB[..7],
            &SECOND_REMOVED_BLOB[..7]
        );
        // `git patch-id --verbatim` gives both the one id
        // 33e8b350b53a46b3fdb8ca9efbf6c8a4f8c507d2: so does the hash.
        for diff in [&approved_diff, &revised_diff] {
            assert_eq!(diff_hash(diff), NORMALIZED_SHA256, "{diff}");
        }
        // Another diff (removing the block's `done(v)` instead) has another
        // hash: the hash still tells patches apart.
        assert_ne!(
            diff_hash(&approved_diff),
            diff_hash(approved_diff.replace(
                "-    check(x);\n     done(v);",
                "     check(x);\n-    done(v);"
            ))
        );
        let judgment_payload = json!({"verdict": "pass", BINDING_KEY: {
            "approved_blobs": {"f": {"blob": FIRST_REMOVED_BLOB}},
            "diff_hash": diff_hash(&approved_diff), "tree_hash": "t1",
        }});
        let events = [event(1, EventKind::ReviewFinished, judgment_payload)];
        let judgment = last_judgment(&events).unwrap();
        // The revise lands the same normalized diff with another blob.
        assert_eq!(
            judgment.binding.as_ref().unwrap().diff_hash.as_deref(),
            Some(diff_hash(&revised_diff).as_str())
        );
        assert_eq!(
            decide(&judgment, &blobs(&[("f", Some(SECOND_REMOVED_BLOB))])),
            Decision::Reapprove {
                reapproval: Reapproval::Review,
                differing: vec!["f".into()]
            }
        );
        assert_eq!(
            decide(&judgment, &blobs(&[("f", Some(FIRST_REMOVED_BLOB))])),
            Decision::Land
        );
    }

    /// A line's carriage return is its content, as `git patch-id
    /// --verbatim` keeps it: the same patch with CRLF lines and with LF
    /// lines hashes apart. Independent references: the SHA-256
    /// (`shasum -a 256`) of each normalized text written by hand; `git
    /// patch-id --verbatim` gives the two patches the ids
    /// ac2d3b0053c42c83a9f8f83a26f07cde88da0d4a and
    /// f4a8f40220e34bfc8e9dcfd1143b7ded327d7550.
    #[test]
    fn a_crlf_line_keeps_its_carriage_return_in_the_diff_hash() {
        let crlf = "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1 +1 @@\n-a\r\n+b\r\n";
        let lf = "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1 +1 @@\n-a\n+b\n";
        assert_eq!(
            diff_hash(crlf),
            "d51a649a9aaaa8eef530ad1906bb3aeb0313b7e4e66974c04d407b5984c1e30a"
        );
        assert_eq!(
            diff_hash(lf),
            "719e99dce93b395f05760f3e6e87c7acc82f25f88f6d02baef534664f101b8c1"
        );
    }

    /// A diff of a file that is not UTF-8 is hashed from its bytes: two
    /// invalid bytes do not hash alike. Independent references: the
    /// SHA-256 (`shasum -a 256`) of each normalized text written by hand.
    #[test]
    fn a_diff_that_is_not_utf8_is_hashed_from_its_bytes() {
        let diff = |byte: u8| {
            let mut diff = b"diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1 +1 @@\n-old".to_vec();
            diff.push(byte);
            diff.extend_from_slice(b"\n+new\n");
            diff
        };
        assert_eq!(
            diff_hash(diff(0xff)),
            "32968ab5a6656c4f4c1ec21120bce679cb9ca0b2fdea9a1890119180e798aeeb"
        );
        assert_eq!(
            diff_hash(diff(0xfe)),
            "2048bc9df381675241e3419b32a6669910102d4995437093dbd6a3ce3acaf096"
        );
    }

    /// A rebase over main's change of other files keeps the set and lands,
    /// though the tree hash differs; main's change of a file of the diff,
    /// a conflict's resolution or a revise changes that file's blob and
    /// goes back to the review, even with no conflict; a file the diff
    /// gains or loses does too.
    #[test]
    fn a_rebase_outside_the_diff_carries_the_judgment_and_any_change_of_its_files_does_not() {
        let judged = [("src/a.rs", Some("a1")), ("src/b.rs", None)];
        let payload = json!({"verdict": "pass", BINDING_KEY: {
            "approved_blobs": serde_json::to_value(blobs(&judged)).unwrap(), "tree_hash": "tree-before",
        }});
        let events = [event(1, EventKind::ReviewFinished, payload)];
        let judgment = last_judgment(&events).unwrap();
        // Main moved only outside the diff: the merged tree is another,
        // the set is the same.
        assert_eq!(decide(&judgment, &blobs(&judged)), Decision::Land);
        let back = |differing: &[&str]| Decision::Reapprove {
            reapproval: Reapproval::Review,
            differing: differing.iter().map(|p| (*p).to_owned()).collect(),
        };
        for (landed, differing) in [
            // Main changed src/a.rs too: merged without a conflict, its blob is another.
            (
                blobs(&[("src/a.rs", Some("a1+main")), ("src/b.rs", None)]),
                vec!["src/a.rs"],
            ),
            // A session resolved a conflict in src/a.rs, or a revise changed it.
            (
                blobs(&[("src/a.rs", Some("a2")), ("src/b.rs", None)]),
                vec!["src/a.rs"],
            ),
            // The diff touches one more file, or one fewer.
            (
                blobs(&[
                    ("src/a.rs", Some("a1")),
                    ("src/b.rs", None),
                    ("src/c.rs", Some("c")),
                ]),
                vec!["src/c.rs"],
            ),
            (blobs(&[("src/a.rs", Some("a1"))]), vec!["src/b.rs"]),
        ] {
            assert_eq!(decide(&judgment, &landed), back(&differing), "{landed:?}");
        }
    }

    /// A person's `land` is bound to what its ask showed: the latest
    /// review's start before the ask opened, or the set a differing
    /// comparison found when the ask was opened again after it; nothing
    /// after the opening (a resume's commit) moves it.
    #[test]
    fn an_asks_land_is_bound_to_the_diff_the_ask_was_opened_on() {
        let started = |id: i64, blob: &str| {
            event(
                id,
                EventKind::ReviewStarted,
                json!({"attempt": 1, BINDING_KEY: {"approved_blobs": {"f": {"blob": blob}}}}),
            )
        };
        let opened = |id: i64, ask: i64| {
            event(
                id,
                EventKind::AskOpened,
                json!({"ask_id": ask, "kind": "approve_landing"}),
            )
        };
        let differed = |id: i64, blob: &str| {
            event(
                id,
                EventKind::IntegrationCompared,
                json!({"matched": false, "reapproval": "approve_landing",
                "landed": {"approved_blobs": {"f": {"blob": blob}}}}),
            )
        };
        let set = |facts: Option<LandingFacts>| {
            facts.map(|f| serde_json::to_value(f.approved_blobs).unwrap())
        };
        let events = [
            started(1, "shown"),
            opened(2, 7),
            // A resume after the ask opened changed the file.
            started(3, "after"),
            differed(4, "merged"),
            opened(5, 8),
        ];
        assert_eq!(
            set(ask_binding(&events, 7)),
            Some(json!({"f": {"blob": "shown"}}))
        );
        assert_eq!(
            set(ask_binding(&events, 8)),
            Some(json!({"f": {"blob": "merged"}}))
        );
        assert_eq!(ask_binding(&events, 9), None);
        assert_eq!(ask_binding(&[opened(1, 7)], 7), None);
    }

    /// A person's re-approval stays due through the session's new receipt
    /// and the review of it: only a person's own judgment after the
    /// comparison that sent the run to them clears it.
    #[test]
    fn a_persons_reapproval_stays_due_until_a_person_judges_the_run() {
        let differed = |id: i64, reapproval: &str| {
            event(
                id,
                EventKind::IntegrationCompared,
                json!({"matched": false, "reapproval": reapproval}),
            )
        };
        let person = |id: i64, payload: Value| {
            let mut approved = event(id, EventKind::IntegrationApproved, payload);
            approved.actor = Some(EventActor {
                role: "user".into(),
                id: "u".into(),
                requested_by: None,
            });
            approved
        };
        let pass = event(3, EventKind::ReviewFinished, json!({"verdict": "pass"}));
        for (reapproval, due) in [
            ("approve_landing", Some(Reapproval::ApproveLanding)),
            ("integrate", Some(Reapproval::Integrate)),
            ("review", None),
        ] {
            let events = [
                differed(1, reapproval),
                event(2, EventKind::ResumeStarted, json!({})),
                pass.clone(),
            ];
            assert_eq!(person_reapproval_due(&events), due, "{reapproval}");
        }
        let base = [differed(1, "approve_landing"), pass.clone()];
        // A person's land or integrate, or an answer deciding otherwise.
        for after in [
            person(4, json!({"ask_id": 9})),
            person(4, json!({})),
            event(
                4,
                EventKind::LandingDecided,
                json!({"ask_id": 9, "answer": "send_back"}),
            ),
        ] {
            let events = [base[0].clone(), base[1].clone(), after];
            assert_eq!(person_reapproval_due(&events), None);
        }
        // The supervisor's approval of no ask, or a decision of no answer, is no person's.
        let mut supervisors = event(4, EventKind::IntegrationApproved, json!({}));
        supervisors.actor = Some(EventActor {
            role: "supervisor".into(),
            id: "s".into(),
            requested_by: None,
        });
        let events = [
            base[0].clone(),
            base[1].clone(),
            supervisors,
            event(
                5,
                EventKind::LandingDecided,
                json!({"reason": "commit_mismatch"}),
            ),
        ];
        assert_eq!(
            person_reapproval_due(&events),
            Some(Reapproval::ApproveLanding)
        );
        assert_eq!(person_reapproval_due(&[]), None);
    }

    /// A binding with a field a later version adds still reads its set.
    #[test]
    fn a_binding_with_a_field_of_a_later_version_still_reads_its_set() {
        let payload = json!({BINDING_KEY: {"approved_blobs": {"a": {"blob": "1"}}, "tree_hash": "t", "later": 1}});
        let binding = binding_of(&payload).unwrap();
        assert_eq!(binding.approved_blobs, blobs(&[("a", Some("1"))]));
        assert_eq!(binding.tree_hash.as_deref(), Some("t"));
        assert_eq!(binding_of(&json!({BINDING_KEY: {"tree_hash": "t"}})), None);
    }

    /// A judgment bound to nothing compares with nothing: it does not land
    /// and every landed path is named.
    #[test]
    fn a_judgment_bound_to_nothing_is_approved_again() {
        let events = [event(1, EventKind::IntegrationApproved, json!({"pid": 1}))];
        let judgment = last_judgment(&events).unwrap();
        assert_eq!(
            decide(&judgment, &blobs(&[("a", Some("1"))])),
            Decision::Reapprove {
                reapproval: Reapproval::Integrate,
                differing: vec!["a".into()]
            }
        );
    }

    /// The latest mismatch waits for its re-approval until something moves
    /// the run on.
    #[test]
    fn a_mismatch_waits_for_its_reapproval_until_the_run_moves_on() {
        let events = [
            pass(1, &[]),
            event(
                2,
                EventKind::IntegrationCompared,
                json!({"matched": false, "reapproval": "approve_landing"}),
            ),
        ];
        assert_eq!(
            pending_reapproval(&events, |_| false),
            Some((EventId::new(2), Reapproval::ApproveLanding))
        );
        assert_eq!(pending_reapproval(&events, |_| true), None);
        assert_eq!(pending_reapproval(&events[..1], |_| false), None);
    }

    /// A resume rewrites the branch from the head a rebase conflict parked
    /// it at, the landing's or its recheck's, while that conflict is the
    /// latest park: a resume for anything else is no conflict resolution.
    #[test]
    fn a_resume_resolves_the_conflict_of_the_head_its_latest_park_names() {
        let (conflicted, rechecked_head) = ("c".repeat(40), "b".repeat(40));
        let deferred = |id: i64, code: &str| {
            event(
                id,
                EventKind::IntegrationDeferred,
                json!({"code": code, "head": conflicted, "main": "m"}),
            )
        };
        let started = event(9, EventKind::ResumeStarted, json!({}));
        let head = conflict_parked_head(&[deferred(1, "rebase_conflict"), started.clone()]);
        assert_eq!(head.as_ref().map(|h| h.as_str()), Some(conflicted.as_str()));
        let rechecked = event(
            2,
            EventKind::LandingRecheckFailed,
            json!({"action": "resumed", "code": "rebase_conflict", "head": rechecked_head, "main": "m"}),
        );
        let head = conflict_parked_head(&[rechecked, started.clone()]);
        assert_eq!(
            head.as_ref().map(|h| h.as_str()),
            Some(rechecked_head.as_str())
        );
        // Another code, a later park of another kind, or a park with no
        // head is no conflict to resolve.
        assert_eq!(
            conflict_parked_head(&[deferred(1, "verification_failed")]),
            None
        );
        let held = event(3, EventKind::HoldParked, json!({}));
        assert_eq!(
            conflict_parked_head(&[deferred(1, "rebase_conflict"), held, started]),
            None
        );
        let headless = event(
            1,
            EventKind::IntegrationDeferred,
            json!({"code": "rebase_conflict"}),
        );
        assert_eq!(conflict_parked_head(&[headless]), None);
        assert_eq!(conflict_parked_head(&[]), None);
    }

    /// A resolved resume rewrote the branch from the conflicted head, or,
    /// once a judgment is on record, from the commit the receipt named:
    /// each with its kind. A head that did not move, or a run nothing
    /// judged, rewrote nothing.
    #[test]
    fn a_resolved_resume_rewrote_from_the_conflict_or_the_judged_receipt() {
        let sha = |c: &str| crate::domain::CommitSha::parse(c.repeat(40), "head").unwrap();
        let (conflicted, named, resolved) = (sha("c"), sha("a"), sha("d"));
        let deferred = event(
            1,
            EventKind::IntegrationDeferred,
            json!({"code": "rebase_conflict", "head": conflicted.as_str()}),
        );
        let started = event(2, EventKind::ResumeStarted, json!({}));
        assert_eq!(
            resume_rewrite(
                &[deferred.clone(), started.clone()],
                Some(&named),
                &resolved
            ),
            Some((conflicted.clone(), CONFLICT_RESOLUTION))
        );
        // A conflict left as it was is no rewrite.
        assert_eq!(
            resume_rewrite(&[deferred, started.clone()], Some(&named), &conflicted),
            None
        );
        let passed = pass(3, &[("f", Some("b"))]);
        let parked = event(4, EventKind::RunE2eFailed, json!({"code": "e2e_failed"}));
        let after_pass = [passed, parked.clone(), started.clone()];
        assert_eq!(
            resume_rewrite(&after_pass, Some(&named), &resolved),
            Some((named.clone(), RESUME))
        );
        assert_eq!(resume_rewrite(&after_pass, Some(&named), &named), None);
        assert_eq!(resume_rewrite(&after_pass, None, &resolved), None);
        // Nothing judged the run: its resume rewrites nothing approved.
        assert_eq!(
            resume_rewrite(&[parked, started], Some(&named), &resolved),
            None
        );
    }
}

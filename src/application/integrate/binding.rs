//! Read the change that a judgment binds and verify its Git provenance
//! against the contents that would land (ADR-t5283-2).

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::application::Repository;
use crate::domain::landing_binding::{
    BINDING_KEY, ChangeProof, Decision, Judgment, LandingFacts, Reapproval, RunChange,
    approval_carries, diff_hash,
};
use crate::domain::{CommitSha, EventKind, RunHistory, TaskRun};

/// Bind the precise range and both sides of every changed path. Keep the
/// normalized hash for display and the head tree for independent replay.
pub fn bind(repository: &dyn Repository, from: &str, to: &str) -> Result<LandingFacts> {
    Ok(LandingFacts {
        approved_blobs: repository
            .landing_blobs(from, to)
            .context("read the blob hashes of the files the diff touches")?,
        run_change: Some(RunChange {
            base: CommitSha::parse(from, "binding base")?,
            head: CommitSha::parse(to, "binding head")?,
            before_blobs: repository
                .landing_blobs(to, from)
                .context("read the blob hashes before the diff")?,
        }),
        diff_hash: Some(diff_hash(
            &repository
                .diff_text(from, to)
                .context("read the diff for its hash")?,
        )),
        tree_hash: Some(
            repository
                .tree_of(to)
                .context("read the tree hash of the diff's head")?,
        ),
    })
}

/// The binding of `run`'s diff as it stands: from where its own commits
/// start on the landing branch's current commit
/// ([`crate::application::review::range_base`]) to the head that lands
/// ([`landing_head`]).
pub fn of_run(
    repository: &dyn Repository,
    run: &TaskRun,
    events: &[crate::domain::RunEvent],
) -> Result<LandingFacts> {
    let head = landing_head(repository, run, events)?;
    let main = repository.main_head()?;
    let base = crate::application::review::range_base(repository, run, &main, &head)?;
    bind(repository, &base, &head)
}

/// The head of `run` that lands: its branch's head
/// ([`crate::application::run_git::head`]); for a run in a container, its
/// validated commit, or what the latest stopped landing rebased that
/// commit to in its landing worktree ([`RunHistory::landing_head`]), which
/// a re-approval approves.
pub fn landing_head(
    repository: &dyn Repository,
    run: &TaskRun,
    events: &[crate::domain::RunEvent],
) -> Result<String> {
    // A run in a container lands its validated commit, rebased in its
    // landing worktree, not the head its bundle has now (what its worker
    // wrote since is not what lands, ADR-t2179-1 decision 5).
    if run.in_container()
        && let Some(commit) = run.result_commit()
    {
        let commit = commit.to_string();
        return Ok(RunHistory::from_events(events)
            .landing_head(&commit)
            .unwrap_or(commit));
    }
    Ok(crate::application::run_git::head(repository, run)?.into_string())
}

/// The ref that keeps the head a landing of `run` rebased it to in its
/// landing worktree reachable once that worktree is made again: what a
/// re-approval of a run in a container reads and approves.
pub fn rewritten_ref(run: &TaskRun) -> String {
    format!("refs/dagq/landing/{}", run.id())
}

/// `payload`, the record of a change of the run's branch after its
/// judgment (a landing's rebase or a migration's renumbering) from
/// `before` to `after` on `main`, with the binding of each side
/// (`binding_before`: from where the run's own commits start on `main`
/// to `before`; `binding_after`: from `main` to `after`), their tree hashes
/// included: the change is recorded with what it changed, apart from the
/// comparison, even when the landing stops before comparing.
pub fn rewrite_record(
    repository: &dyn Repository,
    mut payload: Value,
    (main, before, after): (&CommitSha, &CommitSha, &CommitSha),
) -> Result<Value> {
    let base = repository
        .merge_base(main.as_str(), before.as_str())?
        .map_or_else(|| main.to_string(), CommitSha::into_string);
    payload["binding_before"] = serde_json::to_value(bind(repository, &base, before.as_str())?)?;
    payload["binding_after"] =
        serde_json::to_value(bind(repository, main.as_str(), after.as_str())?)?;
    Ok(payload)
}

/// `payload` with `facts` under [`BINDING_KEY`].
pub fn put(payload: &mut Value, facts: &LandingFacts) {
    if let Ok(value) = serde_json::to_value(facts) {
        payload[BINDING_KEY] = value;
    }
}

/// What [`compare_landing`] found.
#[derive(Debug, Clone, PartialEq)]
pub enum Compared {
    /// The landed change was approved, and `merged`
    /// is what its `run_integrated` records of the merge
    /// ([`merged_record`] adds main's commit after it).
    Matched { merged: Value },
    /// It differs: nothing lands, and the run goes to `reapproval`.
    Changed {
        reason: String,
        reapproval: Reapproval,
        differing: Vec<String>,
    },
}

/// How the run's branch came to `rebased` from the `head` the landing found:
/// whether the rebase moved it and whether a migration was renumbered.
#[derive(Debug, Clone, Copy)]
pub struct Rewrites<'a> {
    pub main: &'a CommitSha,
    pub head: &'a CommitSha,
    pub rebased: &'a CommitSha,
    pub moved_by_rebase: bool,
    pub renumbered: bool,
}

/// Prove that the worktree head is the approved commit or a commit made
/// from it by an earlier runtime rebase, and that the tree that would land
/// is the clean Git merge of the approved source with current main.
fn own_change_proof(
    repository: &dyn Repository,
    events: &[crate::domain::RunEvent],
    judgment: &Judgment<'_>,
    rewrites: Rewrites<'_>,
) -> ChangeProof {
    let Some(binding) = &judgment.binding else {
        return ChangeProof::Unproven;
    };
    let Some(change) = &binding.run_change else {
        return ChangeProof::LegacyBlobs;
    };

    // Follow only runtime rebases. The receipt's more permissive rewrite
    // chain also accepts renumberings, which do not preserve approval.
    let mut heads = std::collections::HashSet::from([change.head.as_str()]);
    for event in events {
        if event.kind == EventKind::IntegrationRebased
            && let Some(before) = event.payload["head_before"].as_str()
            && heads.contains(before)
            && let Some(after) = event.payload["head_after"].as_str()
        {
            heads.insert(after);
        }
    }
    if !heads.contains(rewrites.head.as_str()) {
        return ChangeProof::Unproven;
    }
    if *rewrites.main == change.base {
        return ChangeProof::UnchangedBase;
    }
    let main = rewrites.main.as_str();
    let source = change.head.as_str();
    if !matches!(repository.unique_merge_base(main, source), Ok(Some(base)) if base == change.base)
    {
        return ChangeProof::Unproven;
    }
    match repository.merged_tree(main, source) {
        Ok(Ok(tree)) => ChangeProof::CleanMerge { tree },
        Ok(Err(paths)) => ChangeProof::Conflict { paths },
        Err(_) => ChangeProof::Unproven,
    }
}

/// Compare the latest judgment with the final landing tree. Publish the
/// evidence, including the judged and landed ranges and any rewrite.
pub fn compare_landing(
    queue: &dyn crate::application::EventStore,
    repository: &dyn Repository,
    run: &TaskRun,
    rewrites: Rewrites<'_>,
    protected_paths: &[String],
) -> Result<Compared> {
    let events = queue.run_events(run.id())?;
    let history = RunHistory::from_events(&events);
    let judgment = history
        .last_judgment()
        .context("the run has no judgment that lets it land to compare with")?;
    let landed = bind(
        repository,
        rewrites.main.as_str(),
        rewrites.rebased.as_str(),
    )?;
    let conflict_resolved = events.iter().any(|event| {
        event.id > judgment.event.id
            && (event.kind == EventKind::ConflictResolved
                || matches!(
                    event.kind.known(),
                    Some(EventKind::ResumeFinished | EventKind::ResumeSkipped)
                ) && event.payload["rewrite"]
                    == crate::domain::landing_binding::CONFLICT_RESOLUTION)
    });
    let proof = own_change_proof(repository, &events, &judgment, rewrites);
    // A person's re-approval still due is not stood for by a review's
    // judgment after it (the session's new receipt reviewed again): the
    // landing goes back to the person (ADR-t4840-2 decision 8).
    let decision = match history.person_reapproval_due() {
        Some(reapproval)
            if !matches!(
                judgment.path,
                crate::domain::landing_binding::JudgmentPath::HumanLand
                    | crate::domain::landing_binding::JudgmentPath::DirectIntegrate
            ) =>
        {
            Decision::Reapprove {
                reapproval,
                differing: landed.approved_blobs.keys().cloned().collect(),
            }
        }
        _ => approval_carries(
            &judgment,
            &landed,
            &proof,
            rewrites.renumbered || conflict_resolved,
            protected_paths,
        ),
    };
    if decision == Decision::Land
        && matches!(proof, ChangeProof::CleanMerge { .. })
        && let Some(change) = judgment
            .binding
            .as_ref()
            .and_then(|b| b.run_change.as_ref())
    {
        // The source may otherwise become unreachable when the rebased
        // branch is removed. Retain it so the audit proof can be replayed.
        repository.update_ref(
            &format!(
                "refs/dagq/landing-approval/{}/{}",
                run.id(),
                judgment.event.id.as_i64()
            ),
            change.head.as_str(),
        )?;
    }
    let mut payload = json!({
        "judgment": judgment.event.id,
        "path": judgment.path.as_str(),
        "matched": decision == Decision::Land,
        "judged": judgment.binding,
        "landed": landed,
        "own_change": proof,
        "main": rewrites.main,
        "head_before": rewrites.head,
        "head_after": rewrites.rebased,
        "rebased": rewrites.moved_by_rebase,
        "renumbered": rewrites.renumbered,
        "conflict_resolved": conflict_resolved,
    });
    let compared = match decision {
        Decision::Land => {
            let mut merged = payload.clone();
            if let Some(merged) = merged.as_object_mut() {
                merged.remove("matched");
                merged.remove("judged");
                merged.remove("head_before");
                merged.remove("head_after");
                merged.insert("source_commit".into(), json!(rewrites.rebased));
            }
            Compared::Matched { merged }
        }
        Decision::Reapprove {
            reapproval,
            differing,
        } => {
            payload["differing"] = json!(differing);
            payload["reapproval"] = json!(reapproval.as_str());
            Compared::Changed {
                reason: changed_reason(&differing, reapproval),
                reapproval,
                differing,
            }
        }
    };
    queue.record_runtime_event(run.id(), EventKind::IntegrationCompared, payload)?;
    Ok(compared)
}

/// What `run_integrated` records of the merge a landing put on main as
/// `commit` on `parent` and stopped before it recorded it (task 1118): the
/// latest comparison of `events` that matched, as [`compare_landing`]
/// made it its merge's record, with main before and after it and the set
/// read from the landed commit itself (`landed`, when it could be read).
/// `None` when no comparison matched (a landing that compared nothing).
pub fn recovered_merged(
    events: &[crate::domain::RunEvent],
    landed: Option<&LandingFacts>,
    parent: &CommitSha,
    commit: &CommitSha,
) -> Option<Value> {
    let compared = events
        .iter()
        .rev()
        .find(|e| e.kind == EventKind::IntegrationCompared && e.payload["matched"] == true)?;
    let mut merged = compared.payload.clone();
    let object = merged.as_object_mut()?;
    let source = object.remove("head_after").unwrap_or(Value::Null);
    for field in ["matched", "judged", "head_before"] {
        object.remove(field);
    }
    object.insert("source_commit".into(), source);
    object.insert("main".into(), json!(parent));
    if let Some(landed) = landed.and_then(|facts| serde_json::to_value(facts).ok()) {
        object.insert("landed".into(), landed);
    }
    Some(merged_record(merged, commit))
}

/// Compare an unnamed commit before asking for a corrected receipt.
/// When it differs from the authorized change, the judgment is
/// spent ([`crate::domain::landing_binding::unspent`]) and the run's
/// re-approval is its path's, as for any comparison after a rebase.
/// A run with no judgment has nothing to spend.
pub fn spend_on_unnamed_commit(
    queue: &dyn crate::application::EventStore,
    repository: &dyn Repository,
    run: &TaskRun,
    main: &CommitSha,
    head: &CommitSha,
) -> Result<()> {
    let events = queue.run_events(run.id())?;
    let history = RunHistory::from_events(&events);
    let Some(judgment) = history.last_judgment() else {
        return Ok(());
    };
    let base = repository
        .merge_base(main.as_str(), head.as_str())?
        .map_or_else(|| main.to_string(), CommitSha::into_string);
    let landed = bind(repository, &base, head.as_str())?;
    let Decision::Reapprove {
        reapproval,
        differing,
    } = approval_carries(&judgment, &landed, &ChangeProof::UnchangedBase, false, &[])
    else {
        return Ok(());
    };
    queue.record_runtime_event(
        run.id(),
        EventKind::IntegrationCompared,
        json!({
            "judgment": judgment.event.id,
            "path": judgment.path.as_str(),
            "matched": false,
            "judged": judgment.binding,
            "landed": landed,
            "main": main,
            "head_after": head,
            "unnamed_commit": true,
            "own_change": ChangeProof::Unproven,
            "differing": differing,
            "reapproval": reapproval.as_str(),
        }),
    )
}

/// `merged` with main's commit the landing made.
pub fn merged_record(mut merged: Value, commit: &CommitSha) -> Value {
    merged["main_after"] = json!(commit);
    merged
}

/// Why a landing whose content differs from its judgment does not land,
/// and what approves it again.
fn changed_reason(differing: &[String], reapproval: Reapproval) -> String {
    let files = if differing.is_empty() {
        "the approved change could not be established".to_owned()
    } else {
        differing.join(", ")
    };
    let next = match reapproval {
        Reapproval::Review => "the supervisor reviews the run again on its current diff",
        Reapproval::ApproveLanding => {
            "the supervisor asks approve_landing again on its current diff"
        }
        Reapproval::Integrate => {
            "that approval is not used again: run `dagq integrate` again to approve the current diff as it is now"
        }
    };
    format!(
        "the landing could not carry over the approved change ({files}); nothing landed, and {next}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::EventId;

    fn event(id: i64, kind: EventKind, payload: Value) -> crate::domain::RunEvent {
        crate::domain::RunEvent {
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

    /// A landing that moved main and stopped before it recorded it is
    /// recorded with its merge: the latest comparison that matched, with
    /// main before and after and the set the landed commit touches.
    #[test]
    fn a_landing_found_on_main_is_recorded_with_its_matched_comparison() {
        let parent = CommitSha::parse("1".repeat(40), "parent").unwrap();
        let commit = CommitSha::parse("2".repeat(40), "commit").unwrap();
        let set = |blob: &str| json!({"f": {"blob": blob}});
        let compared = |id: i64, matched: bool, blob: &str| {
            event(
                id,
                EventKind::IntegrationCompared,
                json!({"judgment": 1, "path": "review_pass", "matched": matched,
                    "judged": {"approved_blobs": set(blob)}, "landed": {"approved_blobs": set(blob)},
                    "main": "0".repeat(40), "head_before": "h0", "head_after": format!("h-{blob}"),
                    "rebased": true, "renumbered": false, "conflict_resolved": false}),
            )
        };
        assert_eq!(recovered_merged(&[], None, &parent, &commit), None);
        assert_eq!(
            recovered_merged(&[compared(2, false, "a")], None, &parent, &commit),
            None
        );
        let events = [compared(2, false, "a"), compared(3, true, "b")];
        let landed = LandingFacts {
            approved_blobs: serde_json::from_value(set("b")).unwrap(),
            run_change: None,
            diff_hash: Some("d".into()),
            tree_hash: Some("t".into()),
        };
        let merged = recovered_merged(&events, Some(&landed), &parent, &commit).unwrap();
        assert_eq!(merged["judgment"], 1);
        assert_eq!(merged["path"], "review_pass");
        assert_eq!(merged["source_commit"], "h-b");
        assert_eq!(merged["main"], json!(parent));
        assert_eq!(merged["main_after"], json!(commit));
        assert_eq!(merged["rebased"], true);
        assert_eq!(merged["landed"]["approved_blobs"], set("b"));
        assert_eq!(merged["landed"]["tree_hash"], "t");
        for gone in ["matched", "judged", "head_before", "head_after"] {
            assert!(merged.get(gone).is_none(), "{gone}: {merged}");
        }
        // Without the landed commit's set, the comparison's stays.
        let merged = recovered_merged(&events, None, &parent, &commit).unwrap();
        assert_eq!(merged["landed"]["approved_blobs"], set("b"));
    }
}

# Paths and light verification

`integrate` runs a task's `--verify` commands once, serially, after its rebase, so a long check holds every landing behind it. Register a task with the checks its kind of change needs, and declare with `--paths` what it may change so the runtime catches a run that changed more.

## Globs

`--paths GLOB` (repeatable) is matched against the whole path from the repository root. `*` and `?` stay inside one directory; a segment that is exactly `**` spans any depth. `*.md` is Markdown at the root only, `docs/**` everything under `docs/`, `**/*.md` Markdown anywhere. Blank, absolute (`/...`) and `.` / `..` / empty segments are refused. Without `--paths` a run may change anything.

## Recommended combinations

The checks and paths each kind of change takes are the repository's own rules. Read them from the repository's instructions (`AGENTS.md`, or `CLAUDE.md` when there is none) and the documents they name; when those say nothing, from its README, CI and build settings; and when that still decides nothing, ask the person. A typical set of rows:

| What the task changes | `--paths` | `--verify` | `--evidence` |
| --- | --- | --- | --- |
| Docs only | the docs' globs | the cheapest check the repository names for docs (or none) | none |
| The plugin's or tool's own docs, when a test reads them | their globs plus the docs' | that test | none |
| Code | none (it may touch anything) | the repository's format, lint and full test or coverage gate (one command that runs every test, so no second whole test run) | none (the diff decides `e2e`, below) |
| Scripts and CI | as the repository says | as the repository says | none |

A task that changes several of these takes the heaviest row's verification. The row is chosen by what the task changes, not by its `--change`: the same code change may be a feature, a fix or a refactor.

`--change CHANGE` declares the kind of change the task makes (a feature, a fix, a test, a measurement; a lowercase label of the repository's own, one per task; the values and when to pick each are in the repository's rules). It decides no check: `stats` (`runs[].change`, `changes`), `kpi` (`change=` strata, `--change`, the `change_summary` of `--compare`, targets with `change = "..."`) and `forecast` (a distribution per change) group runs by it. When `dagq.toml` has `[tasks] changes = [...]`, only those values are accepted, and `lint` and `submit` refuse a task that declares none; without it any label or none is accepted. Change it with `edit TASK --change CHANGE` while the task is `draft` or `submitted`.

What a run changed (its areas) is not declared: `stats` and `kpi` read it from the landed commit's diff through the `[areas]` map of `dagq.toml` (`area=` strata, `--area`; `reference/kpi.md`). The task's `--kind` and the `tasks.kind` column were removed; tasks no longer show a `kind`.

The `--verify` commands are integrate's gate, not the worker's checklist: the worker prompt shows them as what integrate runs once after its rebase and tells the session to run the checks the repository's instructions ask of a worker (the `--verify` commands only when the instructions name none). Which tests a worker runs by hand, and whether it may skip a slow gate and leave it to integrate, are the repository's rules; the worker writes the range it ran into the receipt's `tests` evidence. A run resumed because integrate's verification failed may rerun the failing command to reproduce it. No worker runs e2e: it reports `e2e` `not_applicable` with a reason, and the runtime runs the e2e of a run that needs it after the review passes (below).

## E2E: the diff decides, the runtime runs it

Whether a run needs e2e comes from its diff, not from the task: `dagq.toml`'s `[e2e] paths` (globs in the `--paths` syntax) lists what only the real cmux, processes and Git check, and validating records in `validation_finished`'s `e2e_requirement` that the run needs e2e when its diff (merge-base to the receipt's commit) touches one of them. Give `--evidence e2e` only when a task must check the real cmux although its diff may stay outside `[e2e] paths`, and say why in the description; it then needs e2e whatever the diff.

No worker runs e2e, whatever its provider: the prompt says so in one line, the receipt's `e2e` is `not_applicable` with that reason, and validating asks the receipt for no `e2e` evidence. Nor does a Codex worker exclude any test: there are no exclusions. After the review passes (or a person answers `land` to `approve_landing`), before asking to land, the supervisor runs the whole e2e suite on the host in the run's worktree at the reviewed commit, one at a time per host (one lock shared with the auto-update and `install` gates; a run waiting for it keeps its slot, `run_e2e_waiting`). Events `run_e2e_started` and `run_e2e_finished`; the log is the run dir's `e2e-<attempt>.log`. A failed test is rerun once by name; one that passes then is recorded as flaky. When tests still fail and no mark holds them, the run parks `needs_session` (`run_e2e_failed`, code `e2e_failed`) and the resume hands the worker the failed tests and the log to fix (it may rerun a failed test by name to reproduce it, never the whole suite); the fixed run goes through validating and review back to this step. When the e2e cannot run (cmux not answering, past its time limit), the run is not sent back: `run_e2e_finished` `outcome: unavailable`, retried later, and after 3 in a row the inbox sees `check the e2e host`. A repository that is not dagq's source has no e2e the runtime knows (`outcome: not_configured`). Landing's rebase does not run it again and `integrate` never runs e2e, so a person who lands such a run by hand checks `run_e2e_finished` first.

The marks of `.config/e2e-quarantine.toml` hold for this step too, read from the landing branch's committed blob (only landed marks that passed plan review and the review job; no worker edits or uncommitted main checkout edits): a test failing its rerun passes under a mark in date (at most 3) unless the run's diff changes that test's file or the task is the one fixing it (`marks_left_out`). The whole suite also runs before the installed binary is replaced (auto-update and `install` from the source checkout) under the same marks; a failure replaces nothing: auto-update opens an `update_failed` ask (`stage: e2e`), `install` stops with an error naming the failed tests and the log, and a planner registers the fix.

## Name the files a task without paths mainly touches

A task that registers no `--paths` (code, per the repository's rules) has nothing that says which files it will change. Write them in its `--description`, e.g. "mainly touches `src/parser.rs` and `tests/parser.rs`". Two readers use them:

- plan review, to find duplicates and partial overlaps with other tasks; tasks that merely touch the same files, even the stats' conflict hotspots, get no dependency for it: the runtime's claim deferral holds a task back from hot files, and rebase or the landing resolves the rest;
- `related`, whose clues include file names in a task's text, so `related` ranks the right completed tasks higher, and their landed files are what the runtime forecasts this task will touch.

The list is a forecast, not a limit: the worker may change other files as the work needs, and nothing checks the list (`lint` does not require it). Do not write it as `--paths` instead: `--paths` declares what a run may change, and a path outside it parks the run (`scope_violation`); and the forecast does not need the list there. Of a task's declared `--paths`, only the concrete paths stand in for `related`'s forecast; a path with a wildcard (`*`, `**`, `?`) forecasts nothing, and when no concrete path is left, the runtime forecasts from the landed diffs of the most similar completed tasks `related` finds, as for a task without `--paths`.

## What happens outside the paths

- Validation compares the receipt's commit with where the branch forked from the current resolved landing branch (`git merge-base`; the base commit unless a resumed session rebased), so paths other tasks landed are never counted. A changed path no glob matches parks the run as `needs_session` with a `scope_violation` event (`paths`, `allowed`, `reason`); the supervisor resumes the session to restore those paths to their state at `git merge-base HEAD <branch>`.
- `integrate` checks the diff it would squash onto the resolved landing branch after its rebase the same way. Outside paths defer the run (`integration_deferred` with `scope_violation`) without moving the resolved landing branch or running the verification.
- If the task truly needs another path, the session writes a `failed` receipt naming it. Register the task again with wider `--paths` and the verification that path needs.

## Change the paths

```sh
"$DAGQ" set-paths TASK --paths 'docs/**' --paths '*.md'   # replace every glob of a draft, submitted or ready task
"$DAGQ" set-paths TASK --none                             # remove the limit
```

Like `set-goal`, only a `draft`, `submitted` or `ready` task can change; a claimed run is checked against the paths it started with. A change records `task_paths_changed` (`from`, `to`).

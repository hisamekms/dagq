# Paths and light verification

`integrate` runs a task's `--verify` commands once, serially, after its rebase, so a long check holds every landing behind it. Register a task with the checks its change needs, and declare with `--paths` what it may change (`"$DAGQ" add --help`).

## Globs

`--paths GLOB` (repeatable) is matched against the whole path from the repository root: `*` and `?` stay inside one directory, a `**` segment spans any depth. `*.md` is the root only, `**/*.md` anywhere. Without `--paths` a run may change anything.

## Recommended combinations

The checks and paths each kind of change takes are the repository's own rules. Read them from the repository's instructions (`AGENTS.md`, or `CLAUDE.md` when there is none) and the documents they name; when those say nothing, from its README, CI and build settings; and when that still decides nothing, ask the person. A typical set of rows:

| What the task changes | `--paths` | `--verify` | `--evidence` |
| --- | --- | --- | --- |
| Docs only | the docs' globs | the cheapest check the repository names for docs (or none) | none |
| The plugin's or tool's own docs, when a test reads them | their globs plus the docs' | that test | none |
| Code | none (it may touch anything) | the repository's format, lint and full test or coverage gate (one command that runs every test, so no second whole test run) | none (the diff decides `e2e`, below) |
| Scripts and CI | as the repository says | as the repository says | none |

A task that changes several of these takes the heaviest row's verification. The row is chosen by what the task changes, not by its `--change`: the same code change may be a feature, a fix or a refactor.

`--change` declares the kind of change (a label of the repository's own; the values and when to pick each are its rules). It decides no check: `stats`, `kpi` and `forecast` group runs by it. What a run changed (its areas) is not declared: `kpi` reads it from the landed diff through `dagq.toml`'s `[areas]` (`reference/kpi.md`).

The `--verify` commands are integrate's gate, not the worker's checklist: the worker runs the checks the repository's instructions ask of a worker (the `--verify` commands only when they name none) and writes the range it ran into the receipt's `tests` evidence. A run resumed because integrate's verification failed may rerun the failing command to reproduce it.

## E2E: the diff decides, the runtime runs it

No worker runs e2e: its receipt reports `e2e` `not_applicable`. A run needs e2e when its diff touches `dagq.toml`'s `[e2e] paths`, and the runtime then runs it on the host after the review passes, before the landing, and sends a failing run back to its session. Give `--evidence e2e` only when a task must check the real processes although its diff may stay outside `[e2e] paths`, and say why in the description. `integrate` never runs e2e, so a person who lands such a run by hand checks its e2e result first.

## Name the files a task without paths mainly touches

A task that registers no `--paths` (code, per the repository's rules) names in its `--description` the files it mainly touches. Plan review reads them for duplicates and overlaps (sharing files alone earns no dependency), and `related` ranks by them.

The list is a forecast, not a limit: the worker may change other files, and nothing checks it. Never write it as `--paths`: a path outside `--paths` parks the run (`scope_violation`).

## What happens outside the paths

- Validation compares the receipt's commit with where the branch forked from the landing branch, so paths other tasks landed never count. A changed path no glob matches parks the run (`scope_violation`), and the supervisor resumes the session to restore those paths.
- `integrate` checks the diff it would land after its rebase the same way and defers the run without landing it.
- If the task truly needs another path, the session writes a `failed` receipt naming it. Register the task again with wider `--paths` and the verification that path needs.

## Change the paths

```sh
"$DAGQ" set-paths TASK --paths '<glob>' --paths '<glob>'   # replace every glob of a draft, submitted or ready task
"$DAGQ" set-paths TASK --none                             # remove the limit
```

Only a `draft`, `submitted` or `ready` task can change; a claimed run is checked against the paths it started with.

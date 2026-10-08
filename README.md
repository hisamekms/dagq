# dagq

dagq is a dependency DAG queue: a Rust task orchestrator for running dependency-aware development tasks as background worker sessions in isolated Git worktrees, with a resident inbox session in cmux. The name is the shape of the work — tasks form a dependency DAG, and the queue runs the ones whose predecessors have landed. (It is unrelated to DAQ, data acquisition.)

You describe a problem to a planner session, which writes it down as a goal and tasks. A headless plan review checks the plan and makes the tasks ready. A resident supervisor runs each ready task as a Claude Code (or Codex) worker in its own Git worktree, has a headless review check the result, and lands it on your default branch as one squash commit. Everything that needs a person reaches one resident inbox session.

## Requirements

- **macOS on Apple Silicon (`aarch64-apple-darwin`).** No other platform is built, released, or tested.
- **cmux.** The inbox gets a cmux workspace. Workers and planners run in the background without one, and their output is read with `dagq run log` and `dagq planner log`.
- **Claude Code**, signed in, on PATH. The inbox, the planners and the headless jobs (plan review, run review, recovery, goal review, observer) are Claude Code sessions, and workers are too unless a task asks for Codex. The Codex CLI is optional (`up --codex`, `add --provider codex`).
- **Git.** The repository needs at least one commit on the branch dagq lands on ([Landing branch, remote and push](#landing-branch-remote-and-push)).
- **Rust and a C compiler** for `cargo install` (the crate's `rust-version`, now 1.98; the C compiler builds the bundled SQLite). The release update also runs `cargo install` ([Update](#update)).

## Install

**1. The binary.** dagq is published to [crates.io](https://crates.io/crates/dagq):

```sh
cargo install --locked dagq
command -v dagq   # expect ~/.cargo/bin/dagq
```

Keep one `dagq` on PATH. The plugin's launcher and `up` take the first one PATH resolves, and the resident supervisor runs the absolute path of the binary `up` was run as.

**2. The Claude Code plugin.** This repository is the plugin's marketplace. Install it with Claude Code's own plugin commands:

```sh
claude plugin marketplace add hisamekms/dagq
claude plugin install claude-dagq@dagq
```

The plugin holds the skills the inbox and planner sessions follow, the launcher `bin/dagq` the skills call, and hooks for the inbox and planner sessions (`SessionStart`, `Stop`, `SessionEnd`). It does not hold the binary. From release v0.4.0 the marketplace entry points at the latest release tag, so the plugin you install has the same `X.Y.Z` as the release on crates.io. Until then the marketplace serves the plugin on `main` ([ADR-t617-1](docs/adr/2026-09-27-t617-1-plugin-marketplace-pinned-to-release-tag.md)). When the skills resolve the binary (`bin/dagq --resolve`) and the plugin and binary differ in major.minor, the launcher writes one `{"warning": ...}` line to stderr and carries on. Update whichever is older ([Update](#update)).

`up` opens its sessions with the installed plugin. Do not pass `--plugin-dir`, which is only for developing the plugin itself ([ADR-t617-2](docs/adr/2026-09-27-t617-2-installed-plugin-by-default-plugin-dir-for-development.md)). Without `--plugin-dir`, `up` stops before opening a session if `claude plugin list --json` does not show `claude-dagq` installed and enabled, and it prints the two commands above.

**3. Trust the repository in Claude Code once.** Claude Code asks for folder trust by repository root, not by worktree. Run `claude` once in the repository root and accept the prompt; every run worktree then starts without asking. `up` checks this and stops with instructions while the repository is not trusted ([Trust prompt](docs/design/provider-lifecycle.md#trust-prompt)).

**4. Decide what workers may do, in Claude Code's settings.** dagq has no permission policy of its own for workers. It adds only a few deny rules to each run: no signalling processes by name (`pkill`, `killall`), no `dagq` commands outside the worker's role, and no setting or unsetting the variables that name the worker's role and queue (`DAGQ_ROLE` and the like). Everything else comes from Claude Code:

- A Claude worker runs headless by default, one `claude -p` call per turn, in Claude Code's auto mode (`--permission-mode auto`). Auto mode and your `permissions` rules (`allow` / `deny` in `~/.claude/settings.json` or the repository's `.claude/settings.json`) decide what a worker may do without a person. If the session does not start in auto mode (for example, a model that does not support it), dagq stops the turn as a launch failure instead of running it in another mode.
- `add` and `edit` refuse `--interactive`: the interactive worker was retired, every worker runs headless, and no run gets a cmux workspace. A task added with it before keeps that mark, but its runs are headless too. Read a run's session with `dagq run log RUN --follow`; a worker reaches you only through its asks, which you `dagq answer`.

Codex workers run in Codex's own sandbox ([provider lifecycle](docs/design/provider-lifecycle.md)). Set these up before the first run.

## Start

From anywhere inside the repository:

```sh
dagq init   # create the queue for this repository
dagq up     # start the supervisor and open the inbox
```

Each Git repository has one queue, at `~/.local/share/dagq/<hash>/queue.db` (`$XDG_DATA_HOME/dagq/<hash>/`). `<hash>` comes from the repository's Git common directory, so every worktree of the repository shares the queue. `dagq locate` shows where a directory's queue is without creating anything. `--db PATH` before the subcommand uses another queue file.

`up` is idempotent. It checks cmux, Claude Code, the plugin, folder trust, the landing branch, `dagq.toml`, and the programs `[run.env]` names. Then it starts one supervisor, resident as a launchd LaunchAgent that launchd restarts if it stops, and opens the inbox session in the cmux workspace `[<repo>]inbox`. Run it again and it reuses what is already running. It hands a supervisor of another build over to its own binary without stopping the runs in flight. `dagq down` stops the supervisor after it drains (`--wait` waits for that); it leaves the inbox and planner sessions open.

When you have something to plan, ask the inbox in your own words. It records them as a planning request, and the supervisor opens a planner for it in the background, which submits a proposal or declines the request with a reason. From a terminal without `DAGQ_ROLE` you can record one yourself:

```sh
dagq request add --text 'what you want planned'
dagq requests   # follow it
```

`dagq plan` no longer opens a planner; it refuses with this guidance ([ADR-t1394-1](docs/adr/2026-10-03-t1394-1-abolish-person-planners-and-route-planning-through-inbox-requests.md)).

The number of runs at once comes from `[supervisor] parallel` in `dagq.toml` (default 4). Give `up` `--parallel` only to override it.

## How work flows

1. **Plan.** Tell a planner session what you want done. It registers a goal (`goal add`) and draft tasks (`add`). Each task has a description, acceptance criteria, verification commands (`--verify`), dependencies (`--depends-on`), and optionally the paths it may change (`--paths`). The planner checks the plan with `lint` and submits it as one proposal with `submit`. A planner does not make tasks ready.
2. **Plan review.** The supervisor runs a headless plan review on each submitted proposal. `pass` makes its tasks ready. `revise` sends it back to its planner, which fixes it and submits it again with `submit --proposal ID`. A `concern` carries the review's recommendation (`ready`, `send_back` or `cancel`) and how sure it is. The runtime applies a `ready` or `send_back` recommendation itself when the review is sure (`high` confidence) and names no reason a person is needed; a `send_back` also needs the proposal to have a revise left. Otherwise you get an `approve_plan` ask in the inbox (`ready`, `send_back` or `cancel`). That covers a recommendation to cancel, low confidence, a scope or discard reason, a missing recommendation, and a `send_back` past the revise limit ([ADR-t451-1](docs/adr/2026-10-02-t451-1-ai-decides-recommendable-asks-and-escalates-only-the-undecidable.md)). Only a person makes a task ready without plan review: `dagq ready ID --bypass-review`.
3. **Run.** The supervisor claims ready tasks whose dependencies have landed, in priority order, up to `parallel` at once. Each run gets a branch `dagq/<run-id>`, a worktree, and a worker. The worker commits and writes a completion receipt. The supervisor validates the receipt against Git (and the declared `--paths`). It does not run the verification commands at this stage.
4. **Review.** A headless review checks each validated run. On `pass` the run lands. On `revise` the findings go back to the same worker session. A `concern` carries the review's recommendation (`land` or `send_back`) and how sure it is. The runtime lands the run, or sends the findings back, when the review is sure (`high` confidence) and names no reason a person is needed, while the run has a revise left for `send_back`. Otherwise you get an `approve_landing` ask in the inbox (`land`, `send_back` or `cancel`). That covers low confidence, a scope or discard reason, a missing recommendation, and a `send_back` past the revise limit.
5. **Land.** Landing rebases the run onto the landing branch and runs the task's verification commands once on the result. It squashes the run into one commit with `Dagq-Task` / `Dagq-Run` trailers, completes the task, and pushes. A conflict or a failing verification sends the run back to its worker to fix (`needs_session`), and the dependent tasks start from the new commit.
6. **Close the goal.** When every task of a goal has completed or been canceled, a headless goal review checks the receipts against the goal's acceptance. It closes the goal, adds draft tasks for what is missing, or asks you.
7. **Answer.** Questions from workers, planners and jobs reach the inbox as asks, together with every other thing that waits for a person: a stopped supervisor, a failed review or recovery, a failed push. The inbox shows each one, writes your answer with `answer`, and does only what you tell it. A failed or stuck run first goes to a headless recovery job; the inbox asks you only when that job cannot decide.

You can also do the planner's part by hand:

```sh
dagq goal add "Faster CI" --acceptance "CI finishes in under 10 minutes"
dagq add "Cache dependencies in CI" --goal 1 \
  --description "Cache the package manager's downloads between CI runs" \
  --acceptance "A second CI run downloads no dependencies" \
  --verify "make test" --paths '.github/**'
dagq lint 1
dagq submit --goal 1
```

## Configure the repository: `dagq.toml`

A `dagq.toml` at the repository root holds the settings committed with the repository. The runtime reads the file from the main checkout, not from a run's worktree, so a change takes effect once it is in the main checkout. With no file, every setting has its default. An unknown table or key is an error: `up` stops on it, and an older binary rejects tables it does not know yet. The values can appear in process arguments, so keep secrets out of the file. The full list of tables is in [Run environment](docs/design/supervisor-lifecycle/run-environment.md).

### Landing branch, remote and push

```toml
[repository]
branch = "master"    # default: the push remote's HEAD, else main, else master
remote = "upstream"  # default: origin
push = false         # default: true
```

Without `branch`, dagq picks the first local branch among the push remote's HEAD, `main` and `master`. It works this out each time and does not store the result. Without `remote`, dagq pushes to `origin`; if there is no `origin`, landing skips the push (`push_skipped`). A `remote` you name must exist. `push = false` never pushes. `up` and `doctor` show the branch, remote and push dagq resolved and where each came from ([Landing branch](docs/design/supervisor-lifecycle/landing-branch.md), [ADR-t615-1](docs/adr/2026-09-27-t615-1-landing-branch-and-push-remote-per-repository.md)). A failed push keeps the landing and reaches the inbox.

### Language

```toml
[language]
tag = "ja"   # a BCP 47 tag: "en", "ja", "pt-BR", ...
```

This sets the language the AI uses for text people read: replies, asks, task and goal texts, notes, receipt summaries, and landing commit messages. Code, identifiers and CLI flags are not translated. The repository's tag wins. Without it, dagq uses `[language] tag` from your own `~/.config/dagq/config.toml` (`$XDG_CONFIG_HOME/dagq/config.toml`). With neither, dagq gives no instruction. `doctor` shows the tag in effect and where it came from ([Language](docs/design/supervisor-lifecycle/language.md)).

### Run environment

```toml
[run.env]
RUST_LOG = 'info'
CACHE_DIR = '${DAGQ_QUEUE_DIR}/cache'
```

The supervisor passes these variables to every worker and to the verification commands landing runs. `${DAGQ_QUEUE_DIR}` expands to the queue directory and `${DAGQ_RUN_DIR}` to the run's directory; any other `$` is kept as written. Keys starting with `DAGQ_` are refused. A program a variable names (such as `RUSTC_WRAPPER`) must be found on the supervisor's PATH. While it is missing, `up` does not start the supervisor, claims and landings wait, and the inbox gets `install tool`.

### Landing recheck

```toml
[recheck]
command = "cargo check --locked --all-targets"
```

After each landing, the supervisor checks the runs still waiting to land against the new tip of the landing branch. Without `[recheck]` it checks only for Git conflicts (`git merge-tree`). With it, it also runs `command` on the merged tree in a scratch worktree under the queue directory. With `paths` (globs) in `[recheck]`, it runs `command` only on the runs whose merged tree differs from the tip in a path one of the globs matches. A run that conflicts or fails the command goes back to its worker before it tries to land ([Landing recheck](docs/design/supervisor-lifecycle/landing-recheck.md)).

### Supervisor and the rest

`[supervisor]` sets `parallel` (runs at once, default 4), `max_waiting` (runs waiting for a person's answer outside the slots, default 4), and `runtime_planners` (planners the runtime opens at once, default 1). The supervisor rereads them on every pass. Other tables cover task labels (`[tasks] changes`), areas for statistics (`[areas]`), KPI targets (`[kpi]`), which roles run on Codex (`[roles.<role>]`), and thresholds. See [Run environment](docs/design/supervisor-lifecycle/run-environment.md).

Settings for the host, not the repository, go in `host.toml`: the queue's `<queue dir>/host.toml`, or `~/.config/dagq/host.toml` for every queue on the host. These include the release update (`[update]`) and where reports are pushed (`[push]`).

## Commands

Every command prints JSON on stdout (except `graph --format d2|svg`, and `run log` / `planner log`, which print a background session's log as text); a runtime error prints JSON on stderr and exits nonzero. `dagq <command> --help` lists every flag. The plugin's skills run these commands for you; the table is for reading what they do.

| Area | Commands |
| --- | --- |
| Queue and runtime | `locate`, `init`, `migrate [--check]`, `up`, `down [--wait\|--force]`, `request add`, `requests`, `planners`, `install`, `rebind` |
| Goals | `goal add`, `goal list`, `goal show`, `goal edit`, `goal ready` (open a draft goal), `goal close --verdict achieved\|abandoned`, `goal review` |
| Tasks | `add`, `edit`, `list`, `show`, `search`, `related`, `draft`, `cancel`, `dependency add\|remove`, `set-goal`, `set-paths`, `set-priority`, `revisit` |
| Plans | `lint`, `submit`, `proposal list\|show\|withdraw`, `ready ID --bypass-review` (a person only) |
| Watching | `status [--role inbox]`, `doctor [--full]`, `watch [--role inbox] [--until-attention]`, `events`, `timeline RUN`, `candidates [--ignore-deferrals]` (the claim order less the tasks the supervisor last recorded as deferred, those apart in `deferred`, and what holds every claim or `no_supervisor` in `held`), `graph` (its `candidates` and `deferred` the same), `run send\|log`, `planner screen\|send\|log` (`run screen` is refused: a run's session has no screen, so read it with `run log`; likewise `planner screen` shows no screen and `planner send` is refused, so read a planner with `planner log`) |
| Asks and notes | `asks [--open]`, `ask`, `answer ID --text TEXT`, `ask close`, `note`, `notes`, `findings`, `finding` |
| Landing and recovery | `review ID`, `integrate ID\|--next`, `recover RUN_ID` (`run close-workspaces` is refused: the runtime opens no workspace for a run and stops a run's background wrapper itself) |
| Measuring | `stats`, `kpi`, `forecast`, `report`, `mark`, `marks` |

The supervisor and its jobs run some of these for you (`supervise`, `observe`, `throughput-review`); you do not need to run them by hand.

## Update

**Automatic, with your answer.** A supervisor running a release build (`dagq --version` prints a plain `X.Y.Z`) checks crates.io about once a day, and when it starts as a new build ([ADR-t618-1](docs/adr/2026-09-27-t618-1-release-update-by-ask-from-crates-io.md), [Release update](docs/design/supervisor-lifecycle/release-update.md)). When there is a newer release, the inbox gets an `approve_release` ask. It names the new version and the version the supervisor runs now. Answer `install` to update, or `skip` to skip that version. When the binary already runs the newest release but the installed plugin is older, the same ask asks about the plugin, and `install` runs only the two plugin commands below.

On `install`, the supervisor runs `cargo install --locked dagq@<version>` into the queue directory. It checks the new binary's `--version` and that it starts on a throwaway queue, and applies compatible migrations. It then replaces the running binary by a rename (the old one stays as `dagq.previous`) and hands the supervisor over without stopping the runs in flight. After that it updates the installed plugin to the same release with `claude plugin marketplace update dagq` and `claude plugin update claude-dagq@dagq` ([ADR-t618-2](docs/adr/2026-09-27-t618-2-plugin-follows-the-release-update.md)). The open inbox and planner sessions load the new plugin when you restart them. If a check fails, nothing is replaced (or the old binary is put back), and the inbox gets an `update_failed` ask (`retry` / `skip`). A release with a breaking migration is never installed automatically. It opens an `approve_update` ask instead, because the supervisor must drain first. The ask names the `dagq install ... --allow-breaking` command for the release it built. That command waits for the runs in flight, backs up the queue, migrates it, and starts the supervisor again. Run it only after you have answered the open asks.

Choose the behavior in `host.toml`:

```toml
[update]
release = "ask"   # "ask" (default), "auto" (no ask; breaking migrations still ask), or "off"
```

A failed check, for example while offline, is recorded as an event only and is tried again later. `dagq status` shows the last check under `release_update`, and `dagq doctor` shows the settings in effect. If no supervisor is running when you answer `install`, nothing applies the answer: update by hand as below.

**By hand.**

```sh
dagq install --release          # the newest release (or --release 0.4.0); same checks, swap and handoff
dagq install --rollback         # put dagq.previous back
claude plugin marketplace update dagq
claude plugin update claude-dagq@dagq   # install --release does not update the plugin
```

Reopen the inbox and planner sessions after the plugin is updated; they keep the plugin they started with.

`cargo install --locked dagq` followed by `dagq up` also works: `up` hands the supervisor over to the new binary. It skips the checks and keeps no `.previous`. Update the plugin with `claude plugin update claude-dagq@dagq` either way.

Do not copy a new binary over the running one: on macOS that can kill the processes running it.

## When something goes wrong

Ask the inbox; the `dagq-recover` skill has the procedures (`up` / `down`, `doctor` and `recover` when no supervisor serves the queue, reviewing a run by hand, pushing after a failed push). The design documents explain each state: [Recover](docs/design/supervisor-lifecycle/recover.md), [Needs session](docs/design/supervisor-lifecycle/needs-session.md), [Integrate](docs/design/supervisor-lifecycle/integrate.md), [Rebind](docs/design/supervisor-lifecycle/rebind.md) (after moving the repository).

## Developing dagq

This section is for working on dagq itself. [AGENTS.md](AGENTS.md) has the rules for this repository: the fixed binary in `~/.local/bin`, `dagq install` from the source checkout, `up --auto-update`, and the checks every change passes. This repository also runs its own development through dagq.

```sh
cargo build --locked   # target/debug/dagq; rust-toolchain.toml pins the toolchain
cargo fmt --all --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

To use the plugin from the checkout instead of the installed one, pass `--plugin-dir <repository>/plugins/claude-dagq` to `up` (or `claude --plugin-dir`). It overrides an installed plugin of the same name.

### Release

Releases are cut by a tag, following [`.claude/skills/release/SKILL.md`](.claude/skills/release/SKILL.md). [`.github/workflows/release.yml`](.github/workflows/release.yml) builds the GitHub Release and publishes the crates to crates.io. The plugin's marketplace is pinned to the release tag ([ADR-t617-1](docs/adr/2026-09-27-t617-1-plugin-marketplace-pinned-to-release-tag.md)):

1. The release change drops `-dev` from the version in `Cargo.toml`, `crates/*/Cargo.toml`, `plugins/claude-dagq/.claude-plugin/plugin.json` and `Cargo.lock`. The same change points the `claude-dagq` entry of `.claude-plugin/marketplace.json` at the tag to come: `"source": {"source": "git-subdir", "url": "hisamekms/dagq", "path": "plugins/claude-dagq", "ref": "vX.Y.Z"}`, with no `version` in the entry. `sh scripts/check-plugin-version.sh` checks both.
2. As soon as that change lands, push the tag `vX.Y.Z`. Until then the marketplace on main points at a tag that does not exist, and `claude plugin install` and `claude plugin update` fail. `release.yml` runs `scripts/check-plugin-version.sh --tag` and stops if the tag's marketplace entry does not point at the tag itself.
3. After `release.yml` succeeds, check the plugin with a throwaway `HOME`: run `claude plugin marketplace add hisamekms/dagq` and `claude plugin install claude-dagq@dagq`. The installed plugin's version must be `X.Y.Z`.
4. The next change bumps main to the next `-dev` version. It does not touch the marketplace `ref`, so users keep getting the released plugin until the next release.

## Documentation

- [Documentation guide](docs/README.md)
- [Current design](docs/design/overview.md)
- [Architecture decisions](docs/adr/README.md)
- [Plugin integration](docs/design/plugin-integration.md)

## License

dagq is released under the [MIT License](LICENSE); see that file for the full text.

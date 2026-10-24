# Setting up the review

Read this after `init`, when `doctor` names built-in agents without project rules, or when the project's rules for the review changed.

## What a run's review reads

A run's review takes the built-in agents and programs (`builtin:<name>`) from the dagq binary. Everything else it reads from the landing branch's commit, never from a worktree: `dagq.toml`'s review settings and each built-in agent's project rules in `.dagq/review/builtin-<name>.md`. So a change to them reaches the review only once it is committed and landed. `init` recommends the settings and points to `setup-review` (`"$DAGQ" init --help`).

## The toolchain profiles' proposals

`init` also prints what the toolchain profiles built into dagq recommend for the repository: those `dagq.toml` declares, or, when it declares none, those the repository's files suggest. Each one is a proposal for a person to write and commit; nothing of it works until then.

- The person writes and commits the `[toolchains.<name>]` declaration; `setup-review` (below) never writes it or the `--verify` commands, and writes the review programs that fit for the person to commit.
- The verification commands are for a task's `--verify` when it is registered, not a key of `dagq.toml`.
- The container's settings are for running workers in containers: the person writes the proposed Containerfile and tables, and keeps a registry host only for the reason its comment gives. `setup-review` never writes them.
- An error there says `dagq.toml` cannot be read or names a profile dagq does not have: fix that first.
- The proposals assume the tool runs at the repository's root: where the profile applies to a directory below it, adapt the commands and scripts to it.

## The commands' classification

The measurement classifies the commands a session ran (build, test, lint and so on) by rules: a declared profile's own rules work by the declaration alone, and the repository's `dagq.toml` adds to or overrides them. Write there only the rules the project needs on top (its own test targets, a tool no profile knows), never a profile's again. `init` proposes templates for the tools no profile knows yet; nothing of it works until a person writes and commits it. `"$DAGQ" doctor` shows the rules in effect, each with its source. Add these settings only once the binary that runs the queue reads them.

## Generating the settings: `setup-review`

`dagq setup-review` has a coding agent read the project once and write the review's settings, its rules files and fast review programs in a temporary checkout, then shows the diff and writes it to the working tree only at the person's approval. What it may write, what it refuses and the flags are in `"$DAGQ" setup-review --help`.

1. Commit or set aside every change in `dagq.toml` and `.dagq/review/` first: it refuses a file it would change that has changes not committed, ignored files included.
2. Run it. On a refusal fix the cause it names or run it again; nothing was written.
3. The person reads the diff and approves it at the prompt. Without approval nothing is written.
4. It commits and pushes nothing and changes nothing in the queue: the person reads the written files and commits them; they reach the review once they land.
5. Run it again to update the settings: the committed ones are its input.

## Who runs it

Only the person and the inbox. The inbox runs it without `--yes`, on the person's word, to show the person the diff; it is refused `--yes`. Writing the files is the person's: they run it in their own terminal without `DAGQ_ROLE` and answer the prompt, or pass `--yes` there. A planner, a worker and every job are refused (`reference/authority.md`).

# Setting up the review

Read this after `init`, when `doctor`'s `agents` names built-ins `without_rules`, or when the project's rules for the review changed.

## What a run's review reads

A run's review takes the built-in agents' definitions and the built-in programs (`builtin:<name>`) from the dagq binary, never from the repository. The rest it reads from the landing branch's commit, never from a worktree: `dagq.toml`'s settings (which built-ins are on, their paths and provider, and the project's own agents and programs), and each built-in agent's project rules in `.dagq/review/builtin-<name>.md` (the whole review's in `.dagq/review/builtin-acceptance.md`). The rules files are read whether or not `dagq.toml` exists: without it, the built-ins that are on by default run, each with the rules file the commit has, or with none. `init` prints them under `recommended` (`enabled_by_default`), with the command below; it writes no `dagq.toml` and starts no agent.

## Generating the settings: `setup-review`

`dagq setup-review` has a coding agent read the project once (its instructions, documents, tests, languages and tools) in a temporary checkout of the main checkout's HEAD (written from the commit's objects, so no hook or filter of the repository runs), and write there `dagq.toml`'s built-in and program tables, `.dagq/review/builtin-<name>.md` and `.dagq/review/programs/<name>.sh` (fast checks that run no build or test). The settings the HEAD commits are its input: run it again to update them. Its job runs on `[roles.setup_review]`'s provider (Claude by default) and may only read the queue.

Before anyone sees the diff, it refuses (an error, nothing written):

- a change to any other file, or to any other table of `dagq.toml`;
- a `dagq.toml` the runtime cannot read;
- a program script that does not take its root from the working directory's Git work tree (`git rev-parse --show-toplevel`, word for word, with no other option or argument, and the variable's value or `cd`'s argument that output alone, nothing like `/..` joined to it);
- a program script that only names that command in a comment, a string (one over lines too) or a here-document: a command must assign its output to a variable or `cd` to it;
- a file it changes that has changes not committed in the working tree, ignored files included (commit or set them aside, then run it again), or a HEAD that moved while it ran. The working file is compared byte for byte with the commit, and the index's entry with HEAD's, without `git status`: an edit the index's skip-worktree or assume-unchanged flag hides counts, no filter of the repository runs, and a file a filter or a line-ending conversion changes counts as changed;
- a target that is, or lies under, a symbolic link in the checkout.

Files the repository ignores count as changes too. The changes are read once when the job ends, and those bytes are what is checked, shown in the diff and written: nothing the temporary checkout gets later reaches the working tree, and no filter, attribute or diff setting of the repository runs on them. On Claude the job loads no setting sources (no hook or permission of the repository's or the person's settings); on Codex it does not load the repository's project layer (`.codex/`), and its sandbox writes the temporary checkout only, with no network, whatever the person's Codex settings add. Only the paths written are checked for changes not committed, so other files in an ignored directory do not stop it.

Then it prints the diff, its control characters (escape sequences, carriage returns) written out as escapes, and asks on the terminal whether to write it to the working tree; `--yes` approves beforehand, and only the person may pass it. The written files are 0755 when the job made them executable and 0644 otherwise. After the approval it checks HEAD and the working tree again, so a commit or an edit made while the diff was read is not overwritten. Without approval (and with no terminal and no `--yes`) nothing is written, and the output's `approved` is `false`. Either way the temporary checkout is removed, also when the command is interrupted (Ctrl-C while the job runs or at the prompt). It commits and pushes nothing and changes nothing in the queue: the person reads the written files, commits them, and they reach the review once they land.

## Who runs it

Only the person and the inbox. The inbox runs it without `--yes`, on the person's word, to show the person the diff (the output's `diff`); it is refused `--yes`. Writing the files is the person's: they run it in their own terminal without `DAGQ_ROLE` and answer the prompt, or pass `--yes` there. A planner, a worker and every job are refused (`reference/authority.md`).

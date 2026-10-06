# The resource broker: modes, a trial, status and doctor

The resource broker (`dagq-broker`) is a long-lived container in dagq's own Podman machine (`dagq`) that does a worker's file, command, git and package operations with a per-run token, publishing on 127.0.0.1 only. The worker stays a host process: the broker is a contract (token, confinement to the run's worktree, limits, audit), **not a sandbox**. What a person does when it fails: `${CLAUDE_PLUGIN_ROOT}/skills/dagq-recover/reference/broker.md`.

## The modes

`[broker] mode` in the repository's `dagq.toml` (the main checkout's working file), read when the supervisor starts (a change takes effect only in a supervisor started after it):

| mode | what the supervisor does |
| --- | --- |
| `disabled` (the default; also no `[broker]` at all) | Nothing: it never looks for or calls podman. A queue that was on another mode has its leftover tokens revoked (`broker_token_revoked`, `reason: mode_disabled`). |
| `preferred` | Makes the broker ready in the background and hands each Claude Code worker (and resume) a token and the broker's MCP tools when it can (a Codex worker gets none: `broker_unavailable`). The built-in tools stay, and the prompt asks the worker to prefer the broker's. When the broker cannot be used the run records `broker_unavailable` and the worker runs without the tools; claims never wait for it. |
| `required` | The worker's and resume's built-in file tools are denied, Bash runs only `dagq`, and the turn reads only the broker's MCP server; the receipt is written with the broker's `write_receipt`. The control side (`dagq ask` and the queue commands a worker may run, the receipt) stays outside the broker. When no worker can be given the tools, the supervisor claims and resumes nothing and tells the inbox (`broker_claims_held`); it never falls back to the built-in tools. |

- Even `required` is a guardrail, not enforcement: the deny list and the permission mode are Claude Code's settings, and the worker is not isolated.
- Under `required` only a Claude worker in headless turns can be given the tools; any other run (a Codex worker) is refused with `broker_required: ...` instead of running without them.
- `[broker.package]` names the only commands `package_install` may run (none by default); `exec_allow` names the programs `exec` may start (none by default).
- `host.toml`'s `[broker] mode` can only lower a host to `"disabled"`; any other value there is a warning (`up`'s `broker.warnings`) and changes nothing. It also holds the host's podman path and the machine's and container's resources.
- `up` refuses to start a supervisor whose mode is not `disabled` when podman cannot be found on its PATH.

Changing a queue's mode, and adding `[broker]` to a repository's `dagq.toml` at all, is the repository's decision; follow its own rules. A planner or the inbox never changes it on its own.

## Try it in a throwaway repository

Never try a mode on a queue that does real work. The person (or the inbox on their word) does this in a terminal without `DAGQ_ROLE`:

1. Podman is installed on the host (`brew install podman`). Do not `podman machine init` by hand: dagq makes and starts its own machine `dagq` with the fewest resources and never touches another machine.
2. Make a throwaway Git repository with a commit, `user.name` and `user.email` (the token needs a committer), and a `dagq.toml` with `[broker]` and `mode = "preferred"` (or `"required"`). Run `dagq init` in it.
3. `dagq broker start` (idempotent; the first one builds the image from the material inside the dagq binary, which takes a while), then `dagq broker status`: `state: running`, `build_matches: true`, `client.matches: true`.
4. `up`, register a small task and let it run. `dagq show ID`'s `runs[0].broker_tool_use` counts the operations done through the broker (`brokered`) and the built-in tools used directly (`direct`); `dagq broker audit --task ID` lists the broker's own record.
5. `down --wait` stops the container and the machine once no queue uses it (`dagq broker stop` does the same by hand). Remove the repository afterwards.

## Reading status and doctor

- `actors` keeps `backend: host`, `enforcement: advisory` and `sandboxed: false` for every AI actor in every mode: the broker never makes a worker isolated. Never describe it as a sandbox.
- `status`'s `broker` (no role, or `--role inbox`): `mode` (after `host.toml`; `{"error"}` when unreadable), `health` (`{state, reason, at}` from the latest broker event: `healthy`, `unhealthy` with its reason, `stopped`, or `unknown` when none), `active_tokens` (runs holding a token now), `state` and `port` (the last known state), `build`, `image`, `running_image`, `image_matches` and `client` (`path`, `build`, `matches`, `error`). It only reads records: it calls neither podman nor the broker.
- `doctor`'s `broker`: the same `mode`, `health`, `active_tokens`, `build`, `image` and `client`, plus `podman` (the path found, or `null` with `podman_missing`), `machine` and `recorded`. It calls no podman either.
- `dagq broker status` is the one that asks podman and the broker: `state` (`running`, `stopped`, `unhealthy`, `machine_missing`, `machine_stopped`, `machine_busy` or a failure code), `image_present`, `container_status`, `health`, `build_matches` and `client`. `dagq broker logs` prints the container's log tail; `dagq broker audit [--run] [--task] [--since] [--until] [--limit]` the broker's audit lines. All three change nothing.
- With mode `disabled`, `health` is `unknown` (or the last state an earlier mode left) and nothing about it needs attention.

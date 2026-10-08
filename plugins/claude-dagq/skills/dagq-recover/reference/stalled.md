# A `stalled` ask: `wait`, `stop`, `propose`, and an instruction

Read this to carry out, on the person's word, the answer to a `stalled` ask (the `dagq-recover` skill, section 7).

## Where the ask comes from

Every run's session is headless and runs in the background: it has no screen, takes no keys and nobody steps into it. Its `stalled` ask, the reasons it opens for and its question are in "A headless run's `stalled` ask" below. A past `stalled` ask with `reason: idle_without_receipt` or `send_unconfirmed` came from the retired interactive session (a nudge or a text typed into its terminal); it is read as a record, and nothing is carried out for it.

While it is open the run usually holds no `--parallel` slot (`status`'s `waiting`).

## The options and who applies them

- **`wait`**: leave the session alone. While a supervisor holds the run (`running`, a fresh lease), the attention is `applying the answer of ask <id> (runtime)`: the supervisor closes the ask itself (`stall_resolved`, `outcome: answered_wait`) and counts the idle again from then. With no supervisor holding the run the attention is `read the answer of ask <id> and close it`: `"$DAGQ" ask close <id>`; the next supervisor counts again from the close.
- **`propose`** (or `propose: <why>`): the runtime records a finding for the stall and has a planner of its own propose a remedy; the session is left alone as for `wait`. It shows `read the answer of ask <id> and close it` until the supervisor closes it at its next look; with no supervisor holding the run, `ask close <id>` (read as `wait`).
- **`stop`**, **an instruction** (any other text) and **`intervene`**: the supervisor applies them as "A headless run's `stalled` ask" below says. Nothing is typed into the session by hand.

The supervisor also closes the ask itself (`the session moved on; closed by the runtime`) once the session ends a new turn after it was opened, the receipt arrives or a `worker_question` opens, with `the session exited; closed by the runtime` once the session exits, and with `the run was triaged; closed by the runtime` when its recovery job takes the ended run. An unanswered ask it closes this way makes `answer` report it is not open. `recover` does not close it.

## A headless run's `stalled` ask

A run on the headless route (`worker_mode: headless` in `show`, `skills/dagq/reference/provider.md`) has no screen and takes no keys: it is a background process with no workspace that runs only the session wrapper, so none of the `run screen` / `run send` steps above apply to it (`run screen` names its `turns/` instead, `run send` refuses it). Its ask comes from a turn that ended without a receipt or a `worker_question` after the runtime's nudges (`reason: turn_without_receipt`), or from a turn whose tools were refused three times or more (`permission_denied`), once the recovery job could not fix it, or from a recovery job's escalation of an `idle_process` alert (an ask escalated for `long_background` reads the same, but the runtime no longer raises that alert); the question ends with the last turns instead of a screen.

Its options are `wait`, `stop` and `propose` (and any the job added), with no `intervene`: nobody can step into a headless session. Before answering, read what the turns did (the question's last turns, `"$DAGQ" run log RUN` for a session in the background, `"$DAGQ" timeline RUN`, `"$DAGQ" events --run RUN --full` for `turn_finished` and `permission_denials`, the turns' output under the run dir's `turns/`) and bring it to the person; then answer with one of these or an instruction.

- **`wait`** and **`propose`**: as above; no new ask until the next turn ends.
- **An instruction** (any text other than the options): the supervisor sends it as `answer to ask <id>: <text>` for the next turn and closes the ask itself (`stall_resolved`, `answered_instruction`); nothing is typed by hand. A run waiting outside its slot gets it once it is back in one.
- **`stop`** (offered only on a headless run's ask): the supervisor writes the session's exit request once, as after a review's pass (the wrapper stops a running turn and exits), closes the ask itself (`stall_resolved`, `answered_stop`) and never sends `stop` as a turn. The run then ends without a receipt: validating fails it and its recovery job (alert `failed`) takes it, as for any ended session (section 1 to 4 of the skill). A run waiting outside its slot gets it once it is back in one. Nobody ends the wrapper, touches `turns/` or signals a process by hand.
- **`intervene` that still arrives** (an answer to an ask opened before it was taken off, or typed as text): the supervisor sends nothing, closes the ask itself (`stall_resolved`, `answered_intervene`) and at once opens a new `stalled` ask for the run with the options above (its question keeps what the previous one said of the session and its alert); show that one to the person and answer it with `stop`, an instruction, `wait` or `propose`. Do not `ask close` it by hand.

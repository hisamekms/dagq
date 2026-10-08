# A run's session: its turns, undelivered answers and past asks

Read this to carry out, on the person's word, an answer that has to reach a run's session (the `dagq-recover` skill, section 7). Every run's worker is on the headless route, Claude's and Codex's: `add` and `edit` refuse `--interactive`, and a run `show` gives as `worker_mode: interactive`, recorded before that route was retired, is claimed and resumed headless. Its session runs in the background with no workspace (`background` in `show`): each turn is one non-interactive call, and the runtime hands it everything as the next turn's request in the run directory's `turns/` (a worker's answer, a send-back, a resume's request, the exit request). The route and fallbacks are `skills/dagq/reference/provider.md`.

Nothing reaches a session by hand: no command types into it or reads a screen, and the inbox's settings refuse `cmux`. A run's session works only in its own worktree; never edit that worktree, merge or push for it, never touch its `turns/` and never signal its processes.

Take the run `id`, `worktree_path` and `last_error` from `"$DAGQ" show ID` (add `--full` when `last_error` is cut at 300 characters); the commands take the run id (or the task id, for its latest run).

## The commands

```sh
"$DAGQ" run log RUN [--lines N] [--follow]   # the [dagq] summary of each turn, also of an ended run
"$DAGQ" timeline RUN                         # the run's events and the gaps between them
"$DAGQ" events --run RUN --full              # turn_finished, permission_denials, ask_delivered, ...
"$DAGQ" planner log ID [--follow]            # the same for a planner
```

`run screen` and `run send` refuse every run with the reason (no screen, no input; `run screen` names the run's `turns/`), and `run close-workspaces` is refused: the runtime opens no workspace for a run and stops a run's background wrapper itself. A workspace a run opened before is closed by the person in their own terminal. What the runtime's resume sends and when it ends: `reference/resume.md`.

## An undelivered worker answer

A worker's `worker_question` is answered by the person in the inbox; once the session's turn has ended, the supervisor writes `answer to ask <id>: <answer>` as its next turn, closes the ask and records `ask_delivered` (`delivering the answer of ask <id> (runtime)` meanwhile). Do not deliver it yourself while its entry shows `returning`: a run waiting outside its slot gets it once it is back in one.

Attention `send the answer of ask <id> to the worker and close it` (`kind` `ask_delivery_failed`, or `ask_answered` on a run whose session no longer takes answers: not `running`, and not in a review's send-back or a resume): the supervisor could not write the turn (it tries once and does not try again), or the session is gone. No command delivers it by hand (`run send` refuses). Tell the person what the answer was and where the run stands (`show`, `run log`), then on their word `"$DAGQ" ask close <id>`: a session still at work asks again if it still needs the answer, and a run at rest has nobody to reach.

## A `stalled` ask

A headless session whose turn ended without a receipt after the supervisor's nudges and its recovery job (or again after a `wait`) (`turn_without_receipt`), whose tools were refused too often (`permission_denied`), or an `idle_process` alert its recovery job escalated. It has no screen and takes no keys: read what its turns did and answer `wait`, `stop`, `propose` (or an option the job added), or the person's instruction as the answer's text, which the supervisor sends as the session's next turn. The supervisor applies every answer and closes the ask itself; an answer starting with `intervene` is not sent, and the supervisor opens the ask again without it. The steps: `reference/stalled.md`, "A headless run's `stalled` ask".

## A live run's recovery job failed

When the recovery job for a live run's alert (`idle_process`, `stalled`) could not start, exited non-zero, timed out or printed no verdict, the supervisor opens the alert's `stalled` ask with its usual options, with `reason_category` `recovery_failed` and `the recovery job failed (<error>)` in the question. Answer it as above; no job runs for the alert while it is open, and it closes as that kind does.

## Past asks and attentions of the interactive session

The retired interactive session had a screen, and its asks are read as history only; nothing is carried out for them:

- **`answer_prompt`**: a dialog on its screen (`prompt_waiting`), or a resumed session's input box that did not get ready for the resolution request (`input_not_ready`). The supervisor closes one still open when its session exits or the run is triaged; an answered one is closed with `"$DAGQ" ask close <id>`.
- **`stuck_exit`**: its `/exit` did not end the session in time: `reference/stuck-exit.md`.
- **`stalled` with `reason: idle_without_receipt` or `send_unconfirmed`**: a nudge, or a text the supervisor typed that the session did not take: `reference/stalled.md`.

Attention `recover by hand` (`kind` `recovery_failed`) is a live session's recovery job failure an older runtime recorded, from before a failed recovery job opened an ask: no ask opened and the job does not run again for that alert; the session is left as it is. Show the person `last_error` and the job's files in the run directory (`recovery-<alert>-<attempt>.prompt.txt`, `.out`, `.err`) with the run's turns (`run log RUN`), and do what they say. The attention clears once the session exits.

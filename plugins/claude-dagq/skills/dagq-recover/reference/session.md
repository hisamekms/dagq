# A run's session: its turns, undelivered answers and past asks

Read this to carry out, on the person's word, an answer that has to reach a run's session (the `dagq-recover` skill, section 7). Every run's session is headless in the background, and the runtime hands it everything as its next turn. Nothing reaches a session by hand: never edit its worktree, merge or push for it, touch its run directory or signal its processes. Provider routes: `skills/dagq/reference/provider.md`.

## The commands

```sh
"$DAGQ" run log RUN              # what each turn did, also of an ended run
"$DAGQ" timeline RUN             # where the run's time went
"$DAGQ" events --run RUN --full
"$DAGQ" planner log ID
```

Take the run from `"$DAGQ" show ID --full`. Flags: each command's `--help`. A workspace an older run opened is closed by the person in their own terminal. The runtime's resume: `reference/resume.md`.

## An undelivered worker answer

The supervisor delivers an answered `worker_question` and closes the ask; do nothing while it shows `delivering the answer of ask <id> (runtime)`.

Attention `send the answer of ask <id> to the worker and close it`: the supervisor could not write the turn, or the session no longer takes answers. No command delivers it by hand. Tell the person what the answer was and where the run stands (`show`, `run log`), then on their word `"$DAGQ" ask close <id>`: a session still at work asks again if it needs it.

## An undelivered planner answer about a draft

Attention `plan task <N> with the answer: request add --ref task:<N> --ref ask:<id>, then close ask <id>`: no runtime planner carries the answer, and no worker works on a draft. Record the person's answer as a planning request, in their words: `"$DAGQ" request add --text '<the answer>' --ref task:<N> --ref ask:<id>` (the `dagq-inbox` skill's `reference/requests.md`), then `"$DAGQ" ask close <id>`. Never send it to a worker or a planner. An answered `planner_question` about anything else shows `read the answer of ask <id> and close it`: tell the person and close it.

## A `stalled` ask

Read the turns and answer with the person's choice; the supervisor applies every answer and closes the ask itself: `reference/stalled.md`, "A headless run's `stalled` ask". One whose question says `the recovery job failed` is answered the same way.

## Past asks and attentions of the interactive session

The retired interactive session's asks are history; nothing is carried out for them:

- **`answer_prompt`**: an answered one still open is closed with `"$DAGQ" ask close <id>`.
- **`stuck_exit`**: `reference/stuck-exit.md`.
- **`stalled` with `reason: idle_without_receipt` or `send_unconfirmed`**: a record only (`reference/stalled.md`).
- **`recover by hand`**: show the person the run's error (`show ID --full`), the job's files in the run directory and the run's turns (`run log RUN`), and do what they say. It clears once the session exits.

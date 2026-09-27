# Triage by hand, and recover by hand

Read this when the attention `triage by hand` or `recover by hand` reaches the person (the `dagq-recover` skill, section 4). Act only on the person's word.

## `triage by hand`

Attention `triage by hand` (`kind` `triage_failed`): the recovery job of a `failed` / `interrupted` (or `resume_exhausted`) run could not start, timed out, printed no verdict, or its verdict could not be applied; the run stays as it is and is not tried again. Read `last_error` and the run directory's `recovery-<alert>-<attempt>.prompt.txt`, `.out`, `.err`, and bring the choice to the person. On their answer: `"$DAGQ" ready ID` runs the task again as a new run (unchanged: no plan review; to change it, use a planner), `cancel ID` drops it. Its workspaces stay open until then, so read the screen first; close one by hand only while no supervisor runs.

## `recover by hand`

Attention `recover by hand` (`kind` `recovery_failed`): a live run's recovery job failed under a runtime from before ADR-t609-1 (`last_error` its error); the session is untouched and the job does not retry that alert. The runtime now opens the alert's own ask instead (`stuck_exit`, `answer_prompt`, `stalled`, `reason_category` `recovery_failed`), carried out as that kind's answer. Read the screen with the person and carry out what they decide as in the skill's section 7 (`reference/session.md`, `reference/stalled.md`, `reference/stuck-exit.md`). It clears once the session exits.

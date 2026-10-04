# Goal review by hand

Read this when the attention `goal review by hand` reaches the person (the `dagq-recover` skill, section 8). Act only on the person's word.

A goal whose tasks are all `completed` or `canceled` (no draft left, and no follow-up from it left unjudged, `undecided`, judged before its acceptance changed, or `required` outside it: `skills/dagq/reference/goal-close.md`) is judged by the supervisor's headless goal review job, not by a planner (ADR-0047 decision 43; `skills/dagq/reference/goal-close.md`). Attention `goal review by hand` (`kind` `goal_review_failed`, on the goal's first task, `reason_category` `recovery_failed`, `last_error` `goal <ID>: <error>`): the job could not start, exited non-zero, timed out or printed no valid verdict. The goal stays open and is not reviewed again by itself until its input changes: its tasks' statuses, its acceptance, a follow-up registered from it, or a membership judgement recorded or corrected. Which failures of the provider itself skip this attention depends on `[roles.goal_review]` in `dagq.toml`:

- **With a `provider` there**: a provider that cannot be used (its executable missing, its launch failed, a login or a usage limit) is held, and the review is retried on the other provider by itself (it waits while both are held; Claude's login or limit also opens the `queue_hold` ask). None of these shows here.
- **Without one** (the review runs on Claude): only Claude's login or usage limit is spared. The review joins the `queue_hold` ask, whose `done` answer reviews it again, and it is no attention meanwhile. A missing executable or a failed launch is a failure like any other: it shows here and needs `goal review ID` once fixed.

Read `last_error`, the job's files under `<queue dir>/goal-reviews/<goal review id>/` (`prompt.txt`, `review.out`, `review.err`; the ID is the job's, from `goal_review_failed`'s `goal_review_id`), `"$DAGQ" goal show ID --full` and `"$DAGQ" events --goal ID`, and bring the choice to the person:

- **Review it again**: once the cause `last_error` names is gone (the executable, the host, a timeout), on the person's word:

  ```sh
  "$DAGQ" goal review ID   # records goal_review_rearmed; the next supervisor pass starts the job
  ```

  The same command rearms a goal whose `approve_goal` ask was answered `keep_open`, when the person wants it looked at again before its input changes. A planner is refused it; it is the inbox's (on the person's word) or the person's. It is refused for a goal that is not open.
- **Close it**: the person decides it themselves: `goal close ID --verdict achieved` or `abandoned` (the rules, refusals and released `dependents` are in `skills/dagq/reference/goal-close.md`).
- **Add work**: on the person's word the inbox records a planning request that names the goal (`"$DAGQ" request add --text '<their words>' --ref goal:N`, `skills/dagq-inbox/reference/requests.md`), whose runtime planner adds and submits the missing tasks on it; once they finish, its tasks have changed and the review starts again by itself.

The `approve_goal` ask (the job's question, or a fourth `gaps` in a row) is no attention of its own: the inbox shows it as an ask (`skills/dagq-inbox/reference/asks.md`).

---
id: design-supervisor-lifecycle-run-workspaces
type: design
title: "Run workspaces"
status: current
created: 2026-09-26
updated: 2026-09-27
last_verified: 2026-09-27
scope: runtime
related:
  - design-supervisor-lifecycle
---

# Run workspaces

runが開いたworkspaceは、最初のsession（`task_runs.workspace_id`と`workspace_created`）と、resumeごとのworkspace（`start_resume`がcreateの直後に記録する`workspace_created`（`workspace_id`、`resume_attempt`）。これより前のresumeは`resume_finished`の`workspace_id`）で、すべてrun_eventsに残る。閉じたかどうかはworkspaceごとで、`workspace_closed`がそのworkspace_idを名指すか、resumeの`resume_finished`が`workspace_closed: true`か、最初のsessionなら`workspace_closed_at`がnullでないとき閉じている（`domain::run::run_workspaces`）。`workspace_closed_at`は最初のsessionの分だけを表す。reviseは生きているsessionに送るので新しいworkspaceを開かない。

**記録の無いworkspace**（task 806）: cmuxのcreateが失敗を返しても（createの時間切れなど）workspaceが実際には作られていることがあり、そのUUIDはどこにも記録されないので、下の経路のどれも閉じない。次の3つで残さない。

- **createの失敗**: `ActorExecutor`（`close_unrecorded`）が、runの`create` / `create_resume`が失敗したら`WorkspaceBackend::workspaces_described`でそのworkspaceに付けたdescription（最初のsessionは`dagq role=worker queue=<queue hash> run=<run-id> task=<task-id>`、resumeは`run <run-id> resume`）が一致するworkspaceを全windowの一覧から探して閉じ、閉じたか・閉じるのに失敗したかをcreateのerrorに足す（一覧が取れなければerrorはそのまま）。resumeのdescriptionは同じrunの前の試行とも同じだが、resumeは前のsessionが終わってから開き、前の試行のworkspaceは`close_left_resume_workspaces`が先に閉じているので、残っているものは閉じてよい
- **記録の失敗**: createの後に`workspace_created`（最初のsessionは`TaskRun::workspace_id`、resumeはrun_eventの`workspace_created`）を書けなければ、そのworkspaceを閉じてからerrorを返す
- **断られたwrapper**: runのwrapper（`session`、`--resume`を含む）は、supervisorが最初のsessionのworkspaceを記録するのを45秒待って来なかったとき（`workspace registration timed out`）と、`register_wrapper` / `register_resume_wrapper`が断ったとき、断られたことをlog（`<queue dir>/logs/session-*.jsonl`の`warn`）に書き、自分のworkspace（`CMUX_WORKSPACE_ID`）がrunの記録（`domain::run::run_workspaces`）に無ければ閉じてからerrorで終わる（`application::session::wrapper_refused`）。一覧の後に遅れて作られたworkspaceはこれで閉じる。記録にあるworkspaceは上の経路に任せて閉じず、記録が読めなければ閉じない。閉じるcmuxはhiddenの`--cmux`（既定`cmux`、PATHで解決）。testは`tests/it/runtime_session.rs`の`a_refused_run_wrapper_closes_its_workspace_the_run_does_not_record`と、`src/application/actor_executor.rs`の`a_workspace_cmux_made_although_its_create_failed_is_closed`

runが終わる経路ごとの close（task 180）:

- **着地**: supervisorのslotが`integrated`（か`succeeded`）で終わったら、閉じた記録の無いworkspaceのうちcmuxが list しているものを閉じる（`workspace_closed`、`by: supervisor`、`reason: ended`）。sessionのworkspaceはその前に`/exit`とcloseで閉じているので、残るのは時間切れで手放したresumeのworkspaceなど
- **failed / interrupted**: triageが閉じる（上の6）。`exhaust_resumes`で人に渡すときも同じ
- **次のresumeの前**: `close_left_resume_workspaces`が前の試行が残したresumeのworkspaceを閉じ、`workspace_closed`（`by: supervisor`、`resume_attempt`）を記録する
- **手での`integrate`・`recover`・人の`ready` / cancel、supervisorの外で終わったrun**: 下の掃除が閉じる

**掃除**（`sweep_ended_workspaces`）: supervisorのfill passごとに、`sweep_interval`（既定60秒、最初のpassは即時）に1回、終わったrun（`integrated`・`succeeded`・`failed`・`interrupted`）のうち、生きているsupervisorのleaseが無く（staleなlease（`lease_is_stale`: pidが死んでいるかheartbeatが`HEARTBEAT_TIMEOUT_SECS`より古い）は無いものとして扱う。runを終わった状態にしてから`release_lease`までの間にsupervisorが死ぬと、leaseが残ってworkspaceが永久に拾われなくなるため。task 396。staleなleaseの行は消さずに残す）、slotに居らず、triageの対象（`in_progress`のtaskの最新の`failed` / `interrupted`のrun）でないrunのworkspaceを集める（`ended_run_workspaces`。閉じた記録の有無に関わらずrunが開いたworkspaceすべて）。候補があれば全windowの`cmux workspace list`を1回だけ取り（`WorkspaceBackend::listed_workspace_ids`。task 246の`exists`と同じ listing）、listされているものだけを閉じる。DBの記録ではなくcmuxの一覧が正で、listされていないworkspaceには何も書かないので、2回目以降は何もしない。閉じたら`workspace_closed`（`workspace_id`、`by: supervisor`、`reason`: `failed` / `interrupted`は`superseded`、それ以外は`ended`。最初のsessionなら`workspace_closed_at`も）を記録し、そのrunの閉じていない`stuck_exit` / `answer_prompt` / `stalled`のaskを閉じる。closeの失敗は`cleanup_failed`（`workspace_id`、`message`、`by: supervisor`。`last_error`は変えない）にして残りを続け、次の掃除で再び閉じようとする（`cleanup_failed`はsupervisorのプロセスごとにworkspaceあたり1回だけ記録する）。listingの失敗やDBのerrorはsupervisor logに書いてその回を飛ばし、ループは止めない。worktree・branchは同じ回の[Run worktrees](run-worktrees.md#run-worktrees)の掃除が扱い、run_dirは消さない。

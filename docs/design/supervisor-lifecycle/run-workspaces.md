---
id: design-supervisor-lifecycle-run-workspaces
type: design
title: "Run workspaces（runのsessionの記録と停止）"
status: current
created: 2026-09-26
scope: runtime
related:
  - adr-t1433-3
  - adr-t1404-1
  - design-supervisor-lifecycle
  - adr-t1228-1
  - design-authorization
---

# Run workspaces

[ADR-t1433-3](../../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)（goal 92、task 1440）から、runのsession wrapperはcmuxのworkspaceでなく、supervisorから切り離したbackgroundのprocessだけで動く（[非対話のworker](headless-worker.md#workspaceなしのbackgroundのwrapper)）。supervisorはrunのworkspaceもqueueのworkspace groupも作らず、runのworkspaceのdescriptionもresumeのworkspaceのtitleも無い。runのsessionの後始末は、残ったwrapperの停止だけ（[ADR-t1404-1](../../adr/2026-10-03-t1404-1-headless-wrappers-run-as-detached-background-processes.md)決定3）。名前の「workspace」は記録の欄と関数の名前に残る。

runが開いたsessionは、最初のsession（`task_runs.workspace_id`と`workspace_created`）と、resumeごとのsession（`start_resume`が起動の直後に記録する`workspace_created`（`workspace_id`、`resume_attempt`）。これより前のresumeは`resume_finished`の`workspace_id`）と、開き直し（`session_reopened`と`workspace_created`の`reopened`）で、すべてrun_eventsに残る。新しいrunの`workspace_id`はどれもwrapperのhandle（`background:<pid>:<起動時刻>`）。止めたかどうかはsessionごとで、`workspace_closed`がそのworkspace_idを名指すか、resumeの`resume_finished`が`workspace_closed: true`か、最初のsessionなら`workspace_closed_at`がnullでないとき止めている（`domain::run::run_workspaces`）。`workspace_closed_at`は最初のsessionの分だけを表す。reviseは生きているsessionに送るので新しいsessionを開かない。ADR-t1433-3より前にworkspaceで開いたsessionの`workspace_created` / `workspace_closed`のeventと`workspace_id`はそのまま残り、読める（domainは変えない）。

**記録の無いsession**（task 806の読み替え）: 起動したwrapperのhandleを記録できなければ（最初のsessionは`TaskRun::workspace_id`、resumeはrun_eventの`workspace_created`、その後の`wrapper_launched`）、supervisorはそのwrapperを止めてからerrorを返す。wrapperは自分の`wrapper_launched`を見つけるまで登録しない（45秒で`wrapper_refused`をlogに書いて終わる）ので、記録の無いwrapperが依頼を取ることは無い。登録を断られたwrapperはworkspaceを閉じない。記録の無いworkspaceをdescriptionから探して閉じる経路は、runにもruntimeのplannerにも無い（runtimeのplannerもtask 1441からbackgroundのwrapperだけで動き、handleを記録できなければwrapperを止める。[`plan` / `planners`](plan-planners.md)）。wrapperを起動できない失敗は、envの`DAGQ_RUN_ID`が名指すrunの`backend_call_failed`（op `launch_background`）になる。

runが終わる経路ごとの停止（task 180）。止めるのはhandleの`stop_background`（wrapperとそれが起動したturnを止める。cmuxに聞かない）で、止めたら`workspace_closed`（`workspace_id`はhandle）、失敗は`cleanup_failed`を記録する。止めるたびに、どう終わったか（SIGTERM・SIGKILL・既に居なかった）と止めた経路を`wrapper_stopped`にも記録する（task 1657。[非対話のworker](headless-worker.md)の「停止の記録」）。生きているかは`run_session_open`（handleのpidが記録した起動時刻のまま居るか、残ったturnが居るか）で見る。ADR-t1433-3より前のworkspaceのIDは閉じない（人が自分のterminalで閉じる）が、扱いは経路で分かれる: `close_session`・`finish_resume`・`give_up_resume`は`stop_run_session`を通り、supervisor logに人に任せたことを書いて止めたものとして扱う（`close_session`は`workspace_closed`を、`finish_resume`は`resume_finished`の`workspace_closed: true`を記録し、`give_up_resume`は何も記録しない）。`close_open_workspaces`・掃除・`close_left_resume_workspaces`・`close_lost_workspace`は`run_session_open`で動いていないとみなして黙って飛ばし、何も記録しない。

- **着地**: supervisorのslotが`integrated`（か`succeeded`）で終わったら、止めた記録の無いsessionのうちまだ動いているwrapperを止める（`close_open_workspaces`、`workspace_closed`、`by: supervisor`、`reason: ended`）。sessionのwrapperはその前に終了の依頼と停止で終わっているので、残るのは時間切れで手放したresumeのwrapperなど
- **failed / interrupted**: triageが止める（[Triage](triage.md#triage-supervisor)の7）。`exhaust_resumes`で人に渡すときも同じ
- **次のresumeの前**: `close_left_resume_workspaces`が前の試行が残したresumeのwrapperを止め、`workspace_closed`（`by: supervisor`、`resume_attempt`）を記録する
- **手での`integrate`・`recover`・人の`ready` / cancel、supervisorの外で終わったrun**: 下の掃除が止める

**掃除**（`sweep_ended_sessions`）: supervisorのfill passごとに、`sweep_interval`（既定60秒、最初のpassは即時）に1回、終わったrun（`integrated`・`succeeded`・`failed`・`interrupted`）のうち、生きているsupervisorのleaseが無く（staleなlease（`lease_is_stale`: pidが死んでいるかheartbeatが`HEARTBEAT_TIMEOUT_SECS`より古い）は無いものとして扱う。runを終わった状態にしてから`release_lease`までの間にsupervisorが死ぬと、leaseが残ってwrapperが永久に拾われなくなるため。task 396。staleなleaseの行は消さずに残す）、slotに居らず、triageの対象（`in_progress`のtaskの最新の`failed` / `interrupted`のrun）でないrunのsessionを集める（`ended_run_workspaces`。止めた記録の有無に関わらずrunが開いたsessionすべて）。cmuxの一覧は取らず、それぞれを`run_session_open`で見て、まだ動いているbackgroundのwrapperだけを止める。記録ではなくwrapperのprocessが正で、動いていないものには何も書かないので、2回目以降は何もしない。ADR-t1433-3より前のworkspaceのIDは飛ばす（cmuxを呼ばず、eventも書かない）。止めたら`workspace_closed`（`workspace_id`、`by: supervisor`、`reason`: `failed` / `interrupted`は`superseded`、それ以外は`ended`。最初のsessionなら`workspace_closed_at`も）を記録し、そのrunの閉じていない`stuck_exit` / `answer_prompt` / `stalled`のaskを閉じる。停止の失敗は`cleanup_failed`（`workspace_id`、`message`、`by: supervisor`。`last_error`は変えない）にして残りを続け、次の掃除で再び止めようとする（`cleanup_failed`はsupervisorのプロセスごとにwrapperあたり1回だけ記録する）。DBのerrorはsupervisor logに書いてその回を飛ばし、ループは止めない。worktree・branchは同じ回の[Run worktrees](run-worktrees.md#run-worktrees)の掃除が扱い、run_dirは消さない。plannerの行の掃除（`close_abandoned_planners`）は[`plan` / `planners`](plan-planners.md)が持つ（runtimeのplannerはbackgroundのhandleで判じ、cmuxの一覧は取らない。人のplannerの行はsupervisorのpassがcmuxを呼ばずに閉じる）。testは`tests/it/runtime_sweep.rs`の`the_sweep_stops_the_wrappers_of_failed_runs_the_triage_does_not_take`と`the_sweep_stops_every_wrapper_left_running_by_a_landed_run`（過去のworkspaceのIDを閉じず記録もしないことも確かめる）。

**人の片付け**（[ADR-t1228-1](../../adr/2026-10-02-t1228-1-inbox-and-planner-reach-sessions-through-the-dagq-cli.md)の決定6・7、task 1231。ADR-t1433-3決定3でやめた）: `dagq run close-workspaces [RUN | --task ID] [--apply] [--cmux]`は引数を受けて無視し、拒否（`run close-workspaces is refused: the runtime opens no workspace for a run any more and stops a run's background wrapper itself (ADR-t1433-3); close a workspace a run opened before in your own terminal`）を返す。runtimeはrunのworkspaceを開かず、残ったwrapperは上の経路と掃除が止めるので、片付けるものが無い。権限の検査は今までどおり先に行う（capability `workspace.cleanup`をuserとinboxだけが持ち、ほかのroleは`authorization_denied`。[Authorization](../authorization.md)）。ADR-t1433-3より前にrunが開いて残ったworkspaceは、人が自分のterminalで閉じる（ADR-t1228-1決定1の表の人自身のterminalの行と同じ扱い）。使い捨てqueueのスモークのworkspace groupの削除（`cmux workspace-group delete`）も人自身のterminalに残る。

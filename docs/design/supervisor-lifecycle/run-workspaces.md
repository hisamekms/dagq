---
id: design-supervisor-lifecycle-run-workspaces
type: design
title: "Run workspaces"
status: current
created: 2026-09-26
updated: 2026-10-03
last_verified: 2026-10-03
scope: runtime
related:
  - adr-t1404-1
  - design-supervisor-lifecycle
  - adr-t1228-1
  - design-authorization
---

# Run workspaces

> **予定（goal 92）**: [ADR-t1433-3](../../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)で非対話のwrapperはbackgroundだけになり、supervisorはrunとplannerのworkspaceを作らない。この文書の終わったrunのworkspaceのsweep・`run close-workspaces`・queueのworkspace group・runのworkspaceのdescriptionはやめ、後始末は残ったwrapperの停止だけになる。過去に残ったworkspaceは人が自分のterminalで閉じる。実装は後続のtask。

runが開いたworkspaceは、最初のsession（`task_runs.workspace_id`と`workspace_created`）と、resumeごとのworkspace（`start_resume`がcreateの直後に記録する`workspace_created`（`workspace_id`、`resume_attempt`）。これより前のresumeは`resume_finished`の`workspace_id`）で、すべてrun_eventsに残る。閉じたかどうかはworkspaceごとで、`workspace_closed`がそのworkspace_idを名指すか、resumeの`resume_finished`が`workspace_closed: true`か、最初のsessionなら`workspace_closed_at`がnullでないとき閉じている（`domain::run::run_workspaces`）。`workspace_closed_at`は最初のsessionの分だけを表す。reviseは生きているsessionに送るので新しいworkspaceを開かない。

**記録の無いworkspace**（task 806）: cmuxのcreateが失敗を返しても（createの時間切れなど）workspaceが実際には作られていることがあり、そのUUIDはどこにも記録されないので、下の経路のどれも閉じない。次の3つで残さない。

- **createの失敗**: `ActorExecutor`（`close_unrecorded`）が、runの`create` / `create_resume`が失敗したら`WorkspaceBackend::workspaces_described`でそのworkspaceに付けたdescription（最初のsessionは`dagq role=worker queue=<queue hash> run=<run-id> task=<task-id>`、resumeは`run <run-id> resume`）が一致するworkspaceを全windowの一覧から探して閉じ、閉じたか・閉じるのに失敗したかをcreateのerrorに足す（一覧が取れなければerrorはそのまま）。resumeのdescriptionは同じrunの前の試行とも同じだが、resumeは前のsessionが終わってから開き、前の試行のworkspaceは`close_left_resume_workspaces`が先に閉じているので、残っているものは閉じてよい
- **記録の失敗**: createの後に`workspace_created`（最初のsessionは`TaskRun::workspace_id`、resumeはrun_eventの`workspace_created`）を書けなければ、そのworkspaceを閉じてからerrorを返す
- **断られたwrapper**: runのwrapper（`session`、`--resume`を含む）は、supervisorが最初のsessionのworkspaceを記録するのを45秒待って来なかったとき（`workspace registration timed out`）と、`register_wrapper` / `register_resume_wrapper`が断ったとき、断られたことをlog（`<queue dir>/logs/session-*.jsonl`の`warn`）に書き、自分のworkspace（`CMUX_WORKSPACE_ID`）がrunの記録（`domain::run::run_workspaces`）に無ければ閉じてからerrorで終わる（`application::session::wrapper_refused`）。一覧の後に遅れて作られたworkspaceはこれで閉じる。記録にあるworkspaceは上の経路に任せて閉じず、記録が読めなければ閉じない。閉じるcmuxはhiddenの`--cmux`（既定`cmux`、PATHで解決）。testは`tests/it/runtime_session.rs`の`a_refused_run_wrapper_closes_its_workspace_the_run_does_not_record`と、`src/application/actor_executor.rs`の`a_workspace_cmux_made_although_its_create_failed_is_closed`

runが終わる経路ごとの close（task 180）:

- **着地**: supervisorのslotが`integrated`（か`succeeded`）で終わったら、閉じた記録の無いworkspaceのうちcmuxが list しているものを閉じる（`workspace_closed`、`by: supervisor`、`reason: ended`）。sessionのworkspaceはその前に`/exit`とcloseで閉じているので、残るのは時間切れで手放したresumeのworkspaceなど
- **failed / interrupted**: triageが閉じる（[Triage](triage.md#triage-supervisor)の6）。`exhaust_resumes`で人に渡すときも同じ
- **次のresumeの前**: `close_left_resume_workspaces`が前の試行が残したresumeのworkspaceを閉じ、`workspace_closed`（`by: supervisor`、`resume_attempt`）を記録する
- **手での`integrate`・`recover`・人の`ready` / cancel、supervisorの外で終わったrun**: 下の掃除が閉じる

**掃除**（`sweep_ended_workspaces`）: supervisorのfill passごとに、`sweep_interval`（既定60秒、最初のpassは即時）に1回、終わったrun（`integrated`・`succeeded`・`failed`・`interrupted`）のうち、生きているsupervisorのleaseが無く（staleなlease（`lease_is_stale`: pidが死んでいるかheartbeatが`HEARTBEAT_TIMEOUT_SECS`より古い）は無いものとして扱う。runを終わった状態にしてから`release_lease`までの間にsupervisorが死ぬと、leaseが残ってworkspaceが永久に拾われなくなるため。task 396。staleなleaseの行は消さずに残す）、slotに居らず、triageの対象（`in_progress`のtaskの最新の`failed` / `interrupted`のrun）でないrunのworkspaceを集める（`ended_run_workspaces`。閉じた記録の有無に関わらずrunが開いたworkspaceすべて）。候補があれば全windowの`cmux workspace list`を1回だけ取り（`WorkspaceBackend::listed_workspace_ids`。task 246の`exists`と同じ listing）、listされているものだけを閉じる。DBの記録ではなくcmuxの一覧が正で、listされていないworkspaceには何も書かないので、2回目以降は何もしない。閉じたら`workspace_closed`（`workspace_id`、`by: supervisor`、`reason`: `failed` / `interrupted`は`superseded`、それ以外は`ended`。最初のsessionなら`workspace_closed_at`も）を記録し、そのrunの閉じていない`stuck_exit` / `answer_prompt` / `stalled`のaskを閉じる。closeの失敗は`cleanup_failed`（`workspace_id`、`message`、`by: supervisor`。`last_error`は変えない）にして残りを続け、次の掃除で再び閉じようとする（`cleanup_failed`はsupervisorのプロセスごとにworkspaceあたり1回だけ記録する）。listingの失敗はsupervisor logに書いてその回のworkspaceを飛ばし（一覧を見ないbackgroundのsessionのhandleはそのまま閉じる）、DBのerrorはsupervisor logに書いてその回を飛ばし、ループは止めない。worktree・branchは同じ回の[Run worktrees](run-worktrees.md#run-worktrees)の掃除が扱い、run_dirは消さない。

**人の片付け**（`run close-workspaces`、[ADR-t1228-1](../../adr/2026-10-02-t1228-1-inbox-and-planner-reach-sessions-through-the-dagq-cli.md)の決定6・7、task 1231）: supervisorが居ないあいだ（`down`の後、止まったin-cmuxのsupervisor）は上の掃除が動かず、終わったrunのworkspaceが残る（2026-10-01には69個をinboxがcmuxの一覧からUUIDを引き直して手で閉じた）。`dagq run close-workspaces [RUN] [--task ID] [--apply] [--cmux PATH]`（`src/application/workspace_cleanup.rs`の`close_ended_workspaces`）は、supervisorが居なくても、queueが記録したrunのworkspaceのうちcmuxの全windowの一覧（`listed_workspace_ids`を1回）にまだ居るものを、掃除と同じ条件で閉じる。宛先はrunかtaskのIDで、workspaceのUUIDは引数に取らない。

- **対象**: IDを指さなければ掃除の対象（`ended_run_workspaces`。triageの対象を除く）を全部。`RUN`を指せばそのrunの全てのworkspace（`domain::run::run_workspaces`）を、triageの対象（`triage by hand`のrun）でも閉じる。`--task ID`はそのtaskの終わったrun全部を`RUN`と同じ条件で見る
- **閉じないもの**: runが終わっていない（`integrated`・`succeeded`・`failed`・`interrupted`以外。生きているrunも、人の答えを待つrunも）、生きているsupervisorのlease（pidが生きていてheartbeatが`HEARTBEAT_TIMEOUT_SECS`以内）がある、wrapperかagentのprocess（`run_processes`の終了の記録の無い行）が生きている。`RUN`で指したrunならerrorで止まり、`--task`と一括では`skipped`（`run_id`と`reason`）に載せて残りを続ける。`up`が記録したinboxとsupervisorのworkspace（`session_workspaces`）と、全てのplanner（閉じた行も）のworkspaceは、runの記録に同じUUIDがあっても閉じない。cmuxが一覧に出さないworkspaceには何もしない
- **dry-runが既定**: `--apply`が無ければ一覧（`workspaces`の各項目の`outcome: would_close`）だけを返し、何も閉じず何も書かない。出力は`{"dry_run", "workspaces": [{"run_id", "task_id", "run_status", "workspace_id", "outcome", "error"?}], "skipped"}`で、`outcome`は`would_close`・`closed`・`failed`
- **記録**: 閉じたら`workspace_closed`（`workspace_id`、`by`: 呼び出し元のrole（`user` / `inbox`）、`reason: cleanup`。最初のsessionなら`workspace_closed_at`も）を、呼び出し元をeventのactorにして記録し、そのrunの閉じていない`stuck_exit` / `answer_prompt` / `stalled`のaskを掃除と同じく閉じる。closeの失敗は`cleanup_failed`（`workspace_id`、`message`、`by`）にして残りを続ける。listingの失敗はerrorで止まるが、一覧を見ないbackgroundのsessionのhandleは先に閉じて（`--apply`のとき）からerrorにする
- **権限**: capability `workspace.cleanup`をuserとinboxだけが持つ（supervisorは上の自分の掃除を使い、このCLIを使わない）。ほかのroleは`authorization_denied`になる（[Authorization](../authorization.md)）。状態を変えるコマンドなので、dry-runもqueueを書き込みで開き、本番queueでは固定バイナリで打つ
- **関係**: supervisorが居れば上の掃除が同じものを閉じるので、このCLIはsupervisorが居ないあいだの残りと、掃除が除くtriageの対象のrunを人が閉じるためのもの。supervisorと同時に走っても、先に閉じた側の後はcmuxの一覧に居ないので二重には記録しない（閉じる途中で消えたものは`cleanup_failed`になる）。goal 54の片付け（plannerの行の`close_abandoned_planners`と`runner`の削除、[Plan planners](plan-planners.md)、[Run worktrees](run-worktrees.md)）はworkspaceを閉じず、runのworkspaceは扱わない。使い捨てqueueのスモークのworkspace groupの削除（`cmux workspace-group delete`）は本番queueのIDで指せないので、ADR-t1228-1の決定1のとおり人自身のterminalに残し、このCLIに含めない
- test: `tests/it/runtime_workspace_cleanup.rs`の`ended_runs_workspaces_are_listed_then_closed_by_run_task_or_all`（stubのcmuxで、dry-run・一括・task・runの指定、生きているrunとinboxのworkspaceを閉じないこと、eventのactor、拒むrole）、`src/application/commands/operations.rs`の`only_the_user_and_the_inbox_close_ended_runs_workspaces`

**backgroundのwrapper**: [ADR-t1404-1](../../adr/2026-10-03-t1404-1-headless-wrappers-run-as-detached-background-processes.md)（goal 89）で、`dagq.toml`の`[headless] wrapper = "background"`を選んだ非対話のrunは、最初のsession・resume・開き直しのどれでもworkspaceを作らず、workspaceのIDの代わりにwrapperのhandle（`background:<pid>:<起動時刻>`）を記録する。上の記録・close・掃除・`run close-workspaces`はhandleにもそのまま効き、cmuxの一覧の代わりにwrapperのprocessが生きているかで見て、closeはwrapperと、それが起動したturnのprocessをsignalで止める。workspaceを選んだrunと対話のrunは上のまま。詳細は[非対話のworker](headless-worker.md#workspaceなしのbackgroundのwrapper)。

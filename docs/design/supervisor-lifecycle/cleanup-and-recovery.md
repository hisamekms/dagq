---
id: design-supervisor-lifecycle-cleanup-and-recovery
type: design
title: "Cleanup and recovery"
status: current
created: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
  - adr-0054
  - adr-t1228-1
  - adr-t1433-3
---

# Cleanup and recovery

sessionの終了はsupervisorが行う。runのsession wrapperはworkspaceなしのbackgroundのprocessだけで動き（[ADR-t1433-3](../../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)）、終わらせるのは終了の依頼と、残っていればhandleの`stop_background`でwrapperを止めること（[ADR-t1404-1](../../adr/2026-10-03-t1404-1-headless-wrappers-run-as-detached-background-processes.md)の決定3）だけで、runのためにcmuxを呼ばない。leaseはrun単位で（[ADR-0054](../../adr/0054-run-lease-ownership-parallel-supervisors-and-recover.md)の決定4）、1 runの復旧が他のrunに影響しない。receipt検証を通った`awaiting_integration`のrunは、reviewのverdict（pass・concern・reviewの失敗）の後にsessionの終了を待ってwrapperだけを止め（reviseの間は生きているsessionに差し戻すので止めない。[`supervise`](supervise.md#supervise)の10、ADR-0054の決定1）、worktreeとbranchは`integrate`が着地するまで残す（着地後に`integrate`が削除する）。止めたことは`workspace_closed`（`workspace_id`はbackgroundのhandle）に、止められなかったことは`cleanup_failed`に記録する。どう終わったか（SIGTERM・SIGKILL・既に居なかった）と止めた経路は、止めるたびに`wrapper_stopped`に記録する（[非対話のworker](headless-worker.md)の「停止の記録」）。`failed`（非0終了、検証拒否）、provisioningや検証処理のエラー、wrapper heartbeat切れの場合はworktreeを調査のため残す（worktreeのビルド成果物だけは消す。[Run worktrees](run-worktrees.md#run-worktrees)）。調べる材料はrun dirのlogとturnで、`dagq run log RUN`で読む。

containerで動くrunでworktreeとbranchに当たるのは、clone・制御側のrun branch・着地用のworktree・run dirのbundleと状態のファイルで、片付けはcloneでgitを走らせずsymlinkを辿らない（[ADR-t2179-1](../../adr/2026-10-12-t2179-1-control-side-runs-no-git-in-a-container-runs-clone-and-lands-in-its-own-worktree.md)決定7、[生きているsessionの復旧job](background-recovery-job.md)の「後片付けの境界と約束」）。

`failed`・`interrupted`のrunにまだ動いているwrapperがあれば、triageの後にsupervisorが止める（[Triage](triage.md#triage-supervisor)）。
人が`ready` / cancelした後のrunや着地したrunに残ったwrapperは、次の掃除（下の「終わったrunの掃除」）が止める。
ADR-t1433-3より前にworkspaceで開いたrunのsession（`workspace_id`がbackgroundのhandleでないもの）は、supervisorは閉じず、cmuxにも聞かない。

supervisorの後始末（上のreview後・triage後の停止、終わったrunの掃除）はsupervisorが動いている間だけ働く。
runtimeはrunのworkspaceを開かないので、`dagq run close-workspaces`は認可の後に理由を示して拒む（ADR-t1433-3の決定3）。
過去に作られて残ったrunのworkspaceは、人が自分のterminalで閉じる。
goal 54の片付け（plannerの行と`runner`）はこれと重ならない。

## runのsessionの記録<a id="runのsessionの記録"></a>

- runが開いたsessionは、最初のsession・resumeごとのsession・開き直しのどれもrun_eventsに残り、止めたかはsessionごとに判じる（`domain::run::run_workspaces`）。
  `task_runs.workspace_closed_at`は最初のsessionの分だけを表す。
- reviseは生きているsessionに送るので、新しいsessionを開かない。

## 終わったrunの掃除<a id="終わったrunの掃除"></a>

- supervisorのfill passは間隔を置いて、終わったrunのうち生きたsupervisorのleaseが無く、slotに居らず、triageの対象でないrunのsessionを集め、まだ動いているwrapperだけを止める（`sweep_ended_sessions`）。
- staleなleaseは無いものとして扱う。
  runを終わった状態にしてからleaseを放すまでにsupervisorが死ぬと、leaseが残ってwrapperが永久に拾われなくなるため。
- 正は記録ではなくwrapperのprocessで、動いていないsessionには何も書かないので、同じrunを何度掃除しても記録は増えない。
- 止められなければ`cleanup_failed`を残して次の掃除で止め直し、DBのerrorでもループは止めない。
- worktreeとbranchは同じ回の[Run worktrees](run-worktrees.md)の掃除が扱い、run dirは消さない。

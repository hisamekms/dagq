---
id: design-supervisor-lifecycle-cleanup-and-recovery
type: design
title: "Cleanup and recovery"
status: current
created: 2026-09-26
updated: 2026-10-05 # task 1440: runのsessionの終わりをwrapperの停止にし、runのworkspaceのcloseとrun close-workspacesの片付けを除いた
last_verified: 2026-10-05 # task 1440
scope: runtime
related:
  - design-supervisor-lifecycle
  - adr-0054
  - design-manual-smoke
  - adr-t1228-1
  - adr-t1433-3
---

# Cleanup and recovery

sessionの終了はsupervisorが行う。runのsession wrapperはworkspaceなしのbackgroundのprocessだけで動き（[ADR-t1433-3](../../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)）、終わらせるのは終了の依頼と、残っていればhandleの`close`でwrapperを止めること（[ADR-t1404-1](../../adr/2026-10-03-t1404-1-headless-wrappers-run-as-detached-background-processes.md)の決定3）だけで、runのためにcmuxを呼ばない。leaseはrun単位で（[ADR-0054](../../adr/0054-run-lease-ownership-parallel-supervisors-and-recover.md)の決定4）、1 runの復旧が他のrunに影響しない。receipt検証を通った`awaiting_integration`のrunは、reviewのverdict（pass・concern・reviewの失敗）の後にsessionの終了を待ってwrapperだけを止め（reviseの間は生きているsessionに差し戻すので止めない。[`supervise`](supervise.md#supervise)の10、ADR-0054の決定1）、worktreeとbranchは`integrate`が着地するまで残す（着地後に`integrate`が削除する）。止めたことは`workspace_closed`（`workspace_id`はbackgroundのhandle）に、止められなかったことは`cleanup_failed`に記録する。`failed`（非0終了、検証拒否）、provisioningや検証処理のエラー、wrapper heartbeat切れの場合はworktreeを調査のため残す（worktreeのビルド成果物だけは消す。[Run worktrees](run-worktrees.md#run-worktrees)）。調べる材料はrun dirのlogとturnで、`dagq run log RUN`で読む。

`failed`・`interrupted`のrunにまだ動いているwrapperがあれば、triageの後にsupervisorが止める（[Triage](triage.md#triage-supervisor)）。人が`ready` / cancelした後のrunや着地したrunに残ったwrapperは、次の掃除（[Run workspaces](run-workspaces.md)）が止める。ADR-t1433-3より前にworkspaceで開いたrunのsession（`workspace_id`がbackgroundのhandleでないもの）は、supervisorは閉じず、cmuxにも聞かない。

supervisorの後始末（上のreview後・triage後の停止、[Run workspaces](run-workspaces.md)の掃除）はsupervisorが動いている間だけ働く。runtimeはrunのworkspaceを開かないので、`dagq run close-workspaces`は認可の後に理由を示して拒む（ADR-t1433-3の決定3）。過去に作られて残ったrunのworkspaceは、人が自分のterminalで閉じる。goal 54の片付け（plannerの行と`runner`）はこれと重ならない。

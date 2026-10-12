---
id: design-supervisor-lifecycle-naming
type: design
title: "Naming"
status: current
created: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
  - adr-0028
  - adr-0018
  - adr-0011
  - adr-0044
  - adr-0026
  - adr-0031
  - adr-t2159-1
---

# Naming

dagqはcmuxを呼ばず、workspaceを作らず、名前も付けない（[ADR-t2159-1](../../adr/2026-10-09-t2159-1-dagq-does-not-use-cmux-and-the-person-opens-the-inbox.md)決定1・3）。
inboxは人が自分のterminalで`dagq inbox`で開き、supervisorはlaunchdの下で、runとruntimeのplannerはbackgroundのwrapperで動く（[ADR-t1433-3](../../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)、ADR-t1433-2決定3）。

sessionのroleとqueueは環境変数の`DAGQ_ROLE`と`DAGQ_QUEUE`で渡す（ADR-t2159-1決定2）。
`DAGQ_ROLE`の値は`SessionRole`の文字列で、actor idなどほかの変数は[Roles](roles.md#環境変数)が持つ。

識別の正はqueue DBの記録で、titleでは探さない。
runは`task_runs.workspace_id`に、runtimeのplannerは`planners.workspace_id`に、backgroundのwrapperのhandle `background:<pid>:<start>`を持ち、生死はそのprocessで判定する。
古いバイナリが残したcmuxのworkspaceのUUID（ADR-t1433-3より前のrun、登録済みのin-cmuxのsupervisorの`supervisors.workspace_id`、`session_workspaces`のinbox・supervisorの行、廃止前に人が開いたplannerの行）は、cmuxに問わずに読み、閉じた扱いにする（ADR-t2159-1決定6）。
そのworkspace自体は人が閉じる。

queue hashは`QueueLocation::hash()`: repositoryのqueueはqueueディレクトリ名（LaunchAgentのlabel`com.dagq.<hash>`と同じ）。`up`が`supervise`を`--db`で起動しても同じ値になるよう、`--db`のqueueでもファイル名が`queue.db`で隣に`repository`ファイルがあればディレクトリ名を使い、それ以外の`--db` queueはlabelのhash。

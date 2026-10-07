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
---

# Naming

> **予定（goal 92）**: cmuxはinboxだけが使う（[ADR-t1433-1](../../adr/2026-10-03-t1433-1-cmux-is-used-only-by-the-inbox.md)）。in-cmux modeのsupervisor（ADR-0011。置き換え済み、[ADR-t1433-4](../../adr/2026-10-03-t1433-4-supervisor-resides-without-cmux.md)）のworkspaceの名前は対象が無くなり、workspaceの名前はinboxの`[<repo>]inbox`だけが残る。in-cmux modeは後続のtask 1443が実装するまでの今の姿である（plannerのworkspaceの名前はtask 1441で無くした）。

cmux workspaceの名前は複数repositoryで同じcmuxを使うためrepository名を含み、`[<repo>]<role>`の形をとる（[ADR-0028](../../adr/0028-workspace-titles-are-repo-and-role.md)。ADR-0021の`dagq`入りの書式を上書き）。`<repo>`はrepository rootのbasename（basenameが空ならpath自体）で、`]`の直後に空白を入れない。runはworkspaceを開かず、session wrapperはworkspaceなしのbackgroundのprocessだけで動くので（[ADR-t1433-3](../../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)。runのworkspaceの名前を決めた[ADR-0018](../../adr/0018-run-workspace-named-after-the-task.md)を置き換え、task 1440）、runのworkspaceの名前・description・groupは無い。runtimeのplannerも同じくworkspaceを持たず、backgroundのwrapperとturnだけで動くので（ADR-t1433-2決定3、task 1441）、workspaceの名前・description・groupは無い。名前の組み立ては`infrastructure::adapters`の純粋関数:

- supervisor（in-cmux mode）: `[<repo>]supervisor`（`supervisor_workspace_name`。[ADR-0011](../../adr/0011-cmux-socket-password-and-in-cmux-fallback.md)の決定3）
- planner: 名前は無い（`planner_workspace_name`の`[<repo>]planner#<planner-id> - proposal <proposal-id>`などは無くした。task 1441）。runtimeのplannerはworkspaceを開かず、`dagq plan`の廃止（ADR-t1394-1）の前に人が開いたplannerのworkspaceが残るだけ
- inbox: `[<repo>]inbox`（`inbox_workspace_name`）。`up`が開く

`DAGQ_ROLE`の値は`application::lifecycle`の`WORKER_ROLE` / `PLANNER_ROLE` / `INBOX_ROLE` / `OBSERVER_ROLE` / `REVIEWER_ROLE`（`SessionRole`の文字列。supervisorは`SessionRole::Supervisor`）。

titleは表示専用で、runtimeはどのworkspaceもtitleで探さない（[ADR-0026](../../adr/0026-identify-workspaces-by-uuid-env-and-queue-group.md)）。
識別の正はqueue DBのUUID（runは`task_runs.workspace_id`にbackgroundのwrapperのhandle `background:<pid>:<start>`を持ち、cmuxのworkspaceのUUIDを持つのはADR-t1433-3より前に開いたrunだけ）: in-cmux supervisorの登録は`supervisors.workspace_id`、inboxとin-cmux supervisorのworkspace（登録が消えても残るもの）は`session_workspaces(role, workspace_id, created_at)`（schema v11）、plannerは1つずつ`planners.workspace_id`（schema v23。runtimeのplannerはbackgroundのwrapperのhandle `background:<pid>:<start>`を持ち、cmuxのlistでなくそのprocessで判定する。廃止前に人が開いたplannerの行はworkspaceのUUIDを持つが、runtimeはcmuxに問わず`closed`と読み（`retired_workspace`）、supervisorがcmuxを呼ばずに`person_retired`で閉じる（ADR-t1433-2決定5。[`plan` / `planners`](plan-planners.md)の「生存とidleの判定」と「人のplannerの終わり」）。plannerは作り直さない。古いbinaryがworkspaceに立てたruntimeのplannerの行はcmuxで探さず`closed`と読む）。
存在判定は`WorkspaceBackend::exists`（cmux adapterは`cmux --json --id-format uuids list-windows`で全windowを列挙し、windowごとの`cmux --json --id-format uuids workspace list --window <window>`をまとめたlistのidに大文字小文字を問わず一致するか）で、listに居ないUUIDの行は消して作り直す。
`--window`なしの`workspace list`は呼び出し元のwindowのworkspaceしか返さないので、別のwindowへ移したinbox・supervisor・人のplannerのworkspaceも居るものとして扱うために全windowを見る（task 246）。
1つのwindowでもlistに失敗すれば判定全体をerrorにする（見落としたworkspaceを閉じたと誤らないため）。
この文書で「`workspace list`に居る」と書くのはこの全windowのlistのこと。
window一覧とwindowごとのlistの間にwindowが閉じると判定はerrorになり、その間にworkspaceが未走査のwindowから走査済みのwindowへ移ると見落とす（どちらも人の操作と重なったときだけ）。

workspaceは作成時にtitleとcommandのほかに`WorkspaceTags`を持つ（`workspace_create_arguments`が`--name`、`--description`、`--env`、`--group`、`--command`、`--focus false`の順に並べ、`--cwd`を足す）:

- **env**: `DAGQ_ROLE=<role>`と、inbox・supervisorの`DAGQ_QUEUE=<canonical db path>`（`application::actor_executor::actor_env`。actor idなどほかの変数は[Roles](roles.md#環境変数)）。roleは`SessionRole`（workspaceを持つのは`supervisor` / `inbox`と、廃止前に人が開いた`planner`）。`cmux workspace env <id> --json`で読め、workspaceの全shellに継承される。inbox session内の`up`の`skipped`判定とpluginのhookはこれを読む。plannerは`DAGQ_ROLE=planner`・`DAGQ_QUEUE`とさらに`DAGQ_PLANNER_ORIGIN=<person|runtime>`（`submit`がproposalの持ち主の種別にする）と`DAGQ_PLANNER_ID=<planner-id>`を持つ。runtimeのplannerではworkspaceでなくbackgroundのwrapperとそのturnのprocessに渡り、人が開いたplannerのworkspaceは作成時に持った。
- **description**: `dagq role=<role> queue=<queue hash>`（`workspace_description`）の1行。人向けの補助で、判定には使わない。
- **group**: queueのworkspace group。`WorkspaceBackend::ensure_group(queue hash, "[<repo>]")`（cmux adapterは`cmux --json --id-format uuids workspace-group create --name "[<repo>]" --external-id <queue hash>`。既にあれば同じgroupが返る）のUUIDを`--group`で渡す。`up`はinboxのworkspaceを開くときに最初の1回だけ求め（`QueueWorkspaces::group`）、supervisorはrunとruntimeのplannerのためにgroupを求めない（どちらもworkspaceを開かない。ADR-t1433-3の決定3、ADR-t1433-2の決定3）。作れなければwarning（`up`はJSONの`warnings`）にしてgroupなしでworkspaceを作る。cmuxは`--from`なしの`workspace-group create`でgroupのanchor workspaceを生成するので、queueごとに1つ見出しのworkspaceが増える。

workspaceを閉じる経路はすべて`WorkspaceBackend::close`を通る（`down`と入れ替えの`close_supervisor_workspaces`、登録を断られた人のplannerのwrapperが記録の無い自分のworkspaceを閉じる経路。supervisorは人のplannerのworkspaceを閉じない。runとruntimeのplannerのsessionの後始末はbackgroundのwrapperを止めるだけで（`stop_background`。止めるたびに`wrapper_stopped`を記録する）、cmuxのworkspaceを閉じない。[Cleanup and recovery](cleanup-and-recovery.md)）。cmux 0.64.25はピン留めしたworkspaceの`workspace close`を`Error: protected: ピン留めされたワークスペースは閉じられません。先にピンを外してください。`で拒む（確認は出さず、exit 1）ので、cmux adapterの`close`は`cmux workspace-action --action unpin --workspace <id>`を打ってから`cmux workspace close <id>`を打つ。unpinはピン留めされていないworkspaceにも成功し、失敗（workspaceが既に無いなど）してもcloseを止めない。今は`down`はinbox / plannerを閉じないが、閉じる経路が増えても同じcloseを通るのでピンが邪魔をしない（[ADR-0031](../../adr/0031-color-pill-and-pin-for-inbox-and-planner-and-unpin-before-close.md)）。`cmux workspace-group delete --close-workspaces`はピン留めしたworkspaceも閉じる（e2eのGroupGuardが使う）。

queue hashは`QueueLocation::hash()`: repositoryのqueueはqueueディレクトリ名（LaunchAgentのlabel`com.dagq.<hash>`と同じ）。`up`が`supervise`を`--db`で起動しても同じ値になるよう、`--db`のqueueでもファイル名が`queue.db`で隣に`repository`ファイルがあればディレクトリ名を使い、それ以外の`--db` queueはlabelのhash。

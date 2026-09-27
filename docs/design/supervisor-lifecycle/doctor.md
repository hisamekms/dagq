---
id: design-supervisor-lifecycle-doctor
type: design
title: "`doctor`"
status: current
created: 2026-09-26
updated: 2026-09-27
last_verified: 2026-09-27
scope: runtime
related:
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-landing-branch
  - design-supervisor-lifecycle-language
  - adr-0049
---

# `doctor`

ユースケースは`application::health::doctor`で、worktreeとrun directoryとreceiptの有無は`RunFiles`、PIDの生死は`ProcessControl`で見る。

`dagq doctor`は状態を変えずにJSONで報告する。以下は`doctor --full`の内容で、既定の出力はsupervisor 1件・run 1件につき1行相当に圧縮する（ADR-0016の決定4。キー名は変えず省くだけ）: supervisorは`pid`、`alive`、`registered`、`mode`、`workspace_id`、`binary_version`、`parallel`、`parallel_source`、`max_waiting`、`max_waiting_source`（値と出どころ。[Run environment](run-environment.md)の`[supervisor]`）、`auto_update`、`heartbeat_age_secs`、`stale`、`run_ids`、runは`run_id`、`task_id`、`status`、`lease_stale`（leaseがなければnull）、`recoverable`、`blocker_count`（`blockers`の件数）、`workspace_id`、`worktree_path`（`RunHealth::summary` / `SupervisorHealth::summary`）。

- `supervisors`: `status`と同じ。staleな登録は報告するだけで、`doctor`も`recover`も`integrate`も消さない。
- `runs`: `claimed`/`starting`/`running`/`validating`/`integrating`のrunごとに、`workspace_id`、worktreeとrun directoryとreceiptの存在、`last_error`、そのrunの`lease`（PID、`kill -0`による生存、heartbeatの経過秒数、30秒を超えた`stale`。なければnull）、登録済みwrapper/agentプロセスのPID・生存・heartbeat経過秒数・終了コード。`exited_at`が記録済みのプロセスはPIDが再利用されうるため生存確認せず`alive: null`にする。
- `blockers`: そのrunの`recover`を拒む理由の一覧。そのrunのprocessとleaseだけを見る。空なら`recoverable: true`。
- `run_env`（既定の出力にも出す）: queueが束縛されたrepositoryのmain checkoutの`dagq.toml`の`[run.env]`が名指すプログラム（`RUSTC_WRAPPER`など）を、`doctor`を打ったプロセスのPATHで解決した結果（`config`、`path`、`programs[]`の`variable` / `value` / `resolved`（見つからなければnull）、`missing`）と、supervisorが最後に記録した`run_env_program_missing` / `run_env_program_found`（`supervisor_last`、無ければnull）。`dagq.toml`が読めなければ`error`。`dagq.toml`が無く記録も無いrepositoryでは欄ごと出さない。状態は変えない（[ADR-0049](../../adr/0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)の決定9、[Run environment](run-environment.md#runenvが名指すプログラムの検査)）。
- `repository`（既定の出力にも出す）: 着地先のbranchとpushのremoteの解決の結果（`branch`・`branch_source`・`remote`・`remote_source`・`remote_exists`・`push`、解決できなければ`error`だけ。明示した`remote`が無くpushするときは欄に`error`を並べる）。queueがcheckoutに束縛されていなければ出さない。repositoryにmain checkoutが無い（bare、またはmain worktreeの分からない`--separate-git-dir`）ときはその理由が`error`になり、`run_env`は出さない（[Run environment](run-environment.md#main-checkoutの決め方)）。規則は[Landing branch](landing-branch.md#upのpreflightとdoctor)（ADR-t615-1）。
- `language`（既定の出力にも出す）: AIが人に向けて書く文の言語の解決の結果（`tag`（未設定ならnull）・`source`（`repository` / `user` / `unset`）・`user_config`のpath・promptに足す`instruction`、書式の誤りなら`error`）。規則は[Language](language.md#upとdoctor)（ADR-t616-2）。
- `d2`（既定の出力にも出す）: `graph --format svg`が使う`d2`と`d2plugin-tala`を、`doctor`を打ったプロセスのPATHで解決した結果（`d2` / `tala`のそれぞれに`path`と、linkならその先の`resolved`。見つからなければnull）と、見つからないものを名指す`error`（揃っていれば出さない）。queueの状態に関わらず出す（[当面の依存図](dependency-diagram.md#描画infrastructured2)、ADR-0077の決定4）。
- `schema`: `migrate --check`と同じqueueのschemaの状態（`schema_version`、`binary_schema_version`、`floor`、`migrate`が適用する`pending`とその`compatible`、このバイナリがそのまま開けるかの`opens`）。既定の出力にも含める。`migrate`が要るqueueでも、floorがこのバイナリを拒むqueueでも報告する（ADR-0045の決定5）。前者のrunとsupervisorはread-onlyのコピーを`migrate`した上で読む。後者は`supervisors`と`runs`を省き、拒む理由を`error`に書く。どちらでもrepositoryの束縛は先に検査する（`SqliteQueue::inspect_read_only`がschemaの状態と、読めるqueueか、floorが拒むときは束縛だけを読む接続と拒む理由を、1つの接続から返す）。

cmux workspaceの存在は確認しない（cmuxなしで動く）。IDを見てユーザーが`cmux workspace list`で確認する。

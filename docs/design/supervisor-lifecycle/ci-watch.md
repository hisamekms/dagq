---
id: design-supervisor-lifecycle-ci-watch
type: design
title: "CI watch（supervisorが着地先のbranchのCIを見張る）"
status: current
created: 2026-10-06
scope: runtime
related:
  - adr-t2034-1
  - adr-t1920-1
  - adr-t2928-1
  - adr-0047
  - adr-0049
  - adr-0051
  - design-supervisor-lifecycle-supervise
  - design-supervisor-lifecycle-run-environment
  - design-supervisor-lifecycle-finding-planners
  - design-supervisor-lifecycle-build-identifier
---

# CI watch

## 目的

設定（[Run environment](run-environment.md)の`[ci_watch]`）を書いたrepositoryで、supervisorが着地先のbranchへのpushのCIの結果をhostの`gh`で定期的に読み、queueのeventに記録する（[ADR-t1920-1](../../adr/2026-10-06-t1920-1-supervisor-watches-main-ci-keeps-known-failures-and-files-fixes-through-findings.md)）。
赤になれば`ci_failure`のfindingを記録してruntimeのfindingのplanner（[Finding planners](finding-planners.md)）に修正taskを作らせ、既に落ちているtestの一覧（以下「一覧」）を持って、着地の検証とworkerとreviewに渡す。
CIの結果をqueueに届ける経路で、GitHubのissue（`.github/workflows/ci-failure.yml`）とは別に動き、issueは残る（ADR-t1920-1決定8）。

## 流れ

```text
pass: [ci_watch]を読み直す → 終わった確かめの答え（読めなかったjobsの数を含む）を受け取る → 期限なら次の確かめをjobのthreadで始める → 保留の変化を記録
確かめ: 読む手段 → 終わった実行の一覧 → 未処理の実行を作られた順に
        └─ 成否の決まらない実行は次に決まった実行が引き受ける
        └─ 決まった実行ごとに 落ちたjobとstep・JUnit → 一覧の足し引き → 1つのトランザクションで記録（ci_checked・赤緑の転換・finding・resolved）
読み手: ci failures・status・着地の検証・workerとreviewのprompt
```

## 責務と境界

- 何を読むか（GitHubの答えの意味・一覧・finding・保留の理由）は`domain::ci_watch`、1回の確かめは`application::ci_watch::check`、passごとの読み直しと保留は`application::supervise::observation::ci_watch`が持つ。
- GitHubとGitはport `application::ci_watch::CiSource`越しに読み、実装は`infrastructure::ci_watch::GhSource`である。
- queueへの記録は`infrastructure::ci_watch_store`が持つ。
- 確かめるのは`SuperviseOptions::ci_watch`（`CiWatchPort`）を持つsupervisorだけである。
- 一覧から外すtestの適用は、`dagq.toml`の着地の検証のコマンドとrepositoryのscriptが受け持ち、runtimeは一覧を渡すだけである。
- 自動更新（[Auto-update](auto-update.md)）は一覧もCIの結果も見ない。

## 不変条件

- 一覧の正本は`ci_checked`のeventの積み重ねだけで、表を足さない（`WatchState::fold`）。
- 実行はIDとattemptごとに1度だけ記録し、2つのsupervisorが同じ実行を読んでも1つだけが書く（`SqliteQueue::record_ci_check`）。
- 後に作られた実行が決めた一覧と状態を、遅れて終わった古い実行で戻さない（`domain::ci_watch::decide`）。
- 名指したjobが揃って通ったと分からない`success`の実行を緑と読まないので、一覧はjobの分からない実行では空にならない（[ADR-t2034-1](../../adr/2026-10-07-t2034-1-skip-rust-ci-jobs-on-docs-only-changes-and-do-not-read-skipped-runs-as-green.md)）。
- 読む手段が無い間と最初の答えの前は、新しいclaim・resume・着地を始めない。

## いつ確かめるか

- passごとの読み直し・期限・jobのthreadは`application::supervise::observation::ci_watch`のmodule docと`ObservationState::ci_watch_pass`、期限は`domain::ci_watch::check_due`が持つ。
- handoffのexecは走っている確かめの終わりを待つ（`ObservationState::handoff_waits`）。
- 同じ実行の記録の取り合いは`SqliteQueue::record_ci_check`と`CiCheckRecord::previous`。

## ghの呼び方

- 呼び方（main checkout・repositoryの明示・時間切れ）は`infrastructure::ci_watch`のmodule docが持つ。
- repositoryは`[repository] remote`のURLから決め（`domain::ci_watch::remote_repo`・`github_repo`）、GitHubでなければ設定の誤り、Gitが動かなければ一時の失敗にする。
- 読む手段は`CiSource::access`、未処理の実行の選び方（遅れて終わった実行・re-run・最初の確かめ・`gap`）は`domain::ci_watch::runs_to_process`、落ちたjobとstepは`job_failed`、JUnitは`parse_junit`・`merge_outcomes`と`Junit`が持つ。
- 赤の実行の落ちたjobsが読めなくても見張りは止まらない（`application::ci_watch::check`のコメント）。

## 実行の扱い

- 成否は`domain::ci_watch::classify`、名指したjobによる緑の読みは`read_green`と`Undecided`、jobsを読めない実行の数え方は`JobsUnread`が持つ（ADR-t2034-1）。
- 名指したjobが無い実行の知らせは`CI_JOBS_MISSING`と`WatchState::jobs_missing_event`・`standing_jobs_missing`が持つ。
- 遅れて終わった実行とre-runの扱いは`WatchState::is_late`と`decide`のdoc commentが持つ。
- testの名前は、JUnitを読む最初の宣言したprofileが道具の表示と同じに作る（`Toolchains::junit_test_name`、[Toolchain profiles](../toolchain-profiles.md)）。
- testの名前が取れない失敗をjobとstepの項目にする規則は`decide`と`FailedJob`が持つ。
- 落とし穴: docsだけのpushで落ちたdocの検査の項目は、docsだけの修正の実行では外れず、Rustのjobも流して通る実行で外れる（ADR-t2034-1決定4）。

## 一覧

- 足し引きの規則は`domain::ci_watch::decide`、項目は`KnownFailure`、畳み込みは`WatchState::fold`が持つ。
- 外したことは`ci_checked`の`removed`に理由付きで残る。

## eventの種類

- 種類ごとのいつ書くかは`domain::ci_watch`の各kindの定数と`Access::transition`のdoc commentが持ち、kindは`domain::event_kind`に足す（kindの追加は互換、[ADR-0073](../../adr/0073-kind-additions-are-compatible.md)）。
- 一時の失敗は`FailureStreak`と`available_after_failure`が持ち、claimも着地も止めない。

## 読む手段が無いとき

[Run environment](run-environment.md)の「`[run.env]`が名指すプログラムの検査」（[ADR-0049](../../adr/0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)決定9）と同じ形にする（ADR-t1920-1決定2）。

- `up`のpreflightは`lifecycle::Ports::ci_watch_preflight`、実装は`infrastructure::ci_watch::preflight`で、理由と対処は`Access::message`が持つ。
- supervisorの保留は`domain::ci_watch::hold_reason`と`CiWatchState::held`・`unreadable`、着地の保留は`domain::landing_hold::judge`が持つ。
- drain・claimの停止の最中は、最後の答えがだめなときだけleaseを返し、最初の答えを待つ間は返さない（handoffはどちらでも返さない）。
- attentionは`ci_watch_unavailable`だけで（`domain::event_attention`）、対処は`Unavailable::next`、`status`の行は`application::health::attention`が出す。
- 保留の記録は`CI_WATCH_HOLD`（`domain::claim_hold::OwnHold`）と`ObservationState::record_ci_watch_hold`が持ち、`ci_watch_unavailable` / `ci_watch_available`はqueueの答えで、supervisorが保留していることは示さない。
- `doctor`は`application::ci_watch::doctor`と`infrastructure::ci_watch::doctor_view`。

## finding（修正taskの材料）

- 記録は`SqliteQueue::record_ci_check`が実行の記録と同じトランザクションで行い、findingの組み立ては`application::ci_watch::check`と`decide`の`FailureFinding`、鍵は`failure_key`が持つ。
- 同じ鍵の`resolved`は開き直り、`dismissed`は回数と根拠を足すだけである（`domain::finding::merge`、[ADR-0047](../../adr/0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)決定18）。
- 修正taskが持つものは`FailureFinding::detail`、Buildが範囲を含むかは`binary_contains`（[Build identifier](build-identifier.md)）。
- findingは改善の上限に数えず待たせず、runtimeのplannerの枠が空いた回にdraftと改善のfindingより先にplannerが立つ（[ADR-t2928-1](../../adr/2026-10-20-t2928-1-only-the-ci-watchs-ci-failure-findings-are-repairs-outside-the-improvement-limit.md)）。
- `ci_failure`のfindingを書くのはこの記録だけで、`finding record`はどの役割でも`ci_failure`を拒み、上限の外に置くのは`recorded_by`が`supervisor`の行だけである。
- plannerへの載せ方と立つ順は[Finding planners](finding-planners.md)の「CIの見張りの`ci_failure`のfinding」。
- 閉じ方は`CiCheckRecord::resolve`のdoc commentが持ち、plannerが立っているfindingはruntimeが閉じない。

## 一覧の読み方

- `dagq ci failures`は読み取り専用で、入口は`application::ci_watch::known_failures`、出力は`--help`、capabilityは[Authorization](../authorization.md)。
- `status`の`ci`は`domain::ci_watch::status_view`。
- 着地の検証へは`application::integrate`の`landing_env`が一覧のファイルとpathをenvに渡し（[Validation](validation.md#着地の検証)）、名前と意味は`domain::landing_verification`のdoc commentが持つ。

### workerとreviewの材料

- 読み手はclaimのprompt、resumeと衝突の依頼、runの全体のreviewのpromptで、agentのjobには載せない（差し戻しを決めるのは全体のreviewのため）。
- 新しいsessionに切り替えたresumeの依頼は一覧を持たず（reviseの依頼はもともと持たない）、resume・reviseの引き継ぎのpromptが一覧を持つ（[ADR-t2852-1](../../adr/2026-10-19-t2852-1-handoff-carries-local-check-records-resume-timeout-and-findings-once.md)決定5）。
  runのcommitと未commitの変更が触るpathとtaskが宣言したpathの名前にtestのmoduleのpathが当たるものとtask自身が直すものだけを載せ、外したものがあればその件数と全部を持つファイルを書く。
  test binaryやpackageの名前とtest自身の名前では当てず、名前の取り方と照らし方は`application::prompt::known_failures`のdoc commentが持つ。
  taskのpromptの節はclaimの時の写しのまま残る。
- 読み方は`application::ci_watch::run_known_failures`と`Supervisor::known_failures`（`supervise/execution/session.rs`）、節の文面と選ぶ順と上限は`application::prompt::known_failures`が持つ。
- workerには自分の変更が原因でなければ直さずreceiptに書くよう、reviewにはそれを差し戻しの理由にしないよう言う。

## 修正taskのrun

- runのtaskが直す`ci_failure`のfindingの判定は`SqliteQueue::ci_failure_findings_of`で、そのfindingの項目は一覧から`kept_for_task`に移り、他の項目は外す対象のまま残る。
- taskに印や欄は足さない。

## 保存（findingsの列）

- `covered_by_task`は外部キーもCHECKも持たず（[persistence](../persistence.md)の「CIの見張り」、[ADR-t876-1](../../adr/2026-09-28-t876-1-no-sqlite-check-constraints-until-schema-is-stable.md)）、値の規則は`domain::finding::check_covered_by`、書き手は`SqliteQueue::set_finding_status_covered`が持つ。

## 既存の仕組みとの関係

- observerはstatsではなくeventとfindingで読む（`ci_check_failed`と`ci_failure`のfindingの滞留）。

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
  - adr-0047
  - adr-0049
  - adr-0051
  - design-supervisor-lifecycle-supervise
  - design-supervisor-lifecycle-run-environment
  - design-supervisor-lifecycle-finding-planners
  - design-supervisor-lifecycle-build-identifier
---

# CI watch

設定（[Run environment](run-environment.md)の`[ci_watch]`）を書いたrepositoryで、supervisorが着地先のbranchへのpushのCIの結果をhostの`gh`で定期的に読み、queueのeventに記録し、赤になったら`ci_failure`のfindingを記録してruntimeのfindingのplanner（[Finding planners](finding-planners.md)）に修正taskを作らせ、既に落ちているtestの一覧（以下「一覧」）を持つ。
CIの結果をqueueに届ける経路で、GitHubのissue（`.github/workflows/ci-failure.yml`）とは別に動く。

## いつ確かめるか

- supervisorのループの各passで、`[ci_watch]`をmain checkoutの`dagq.toml`から読み直す（`ci_watch_pass`）。
  ファイルか表が無ければ何もしない（eventも書かない）。
  読めなければ（書式の誤りなど）warnを1回出し、使っている表のまま続ける。
  表が変われば次のpassで確かめる。
- 前の確かめの始まりから間隔が経ったpassで1回確かめ、プロセスの最初のpass（起動とexecの引き継ぎの直後）は経過を問わず確かめる（`domain::ci_watch::check_due`）。
  drain・handoffの間も確かめ、handoffのexecは走っている確かめの終わりを待つ。
  確かめ（`application::ci_watch::check`）はjobのthreadで行い、ループを待たせない。
  GitHubとGitはport `application::ci_watch::CiSource`越しに読み、実装は`infrastructure::ci_watch::GhSource`。
  終わった確かめの答えは次のpassで受け取る。
- 実行1件の記録は1つの書き込みトランザクション（`SqliteQueue::record_ci_check`、`infrastructure::ci_watch_store`）で、`BEGIN IMMEDIATE`の中で最新の`ci_checked`のevent IDを読み直し、確かめで読んだものと違えば（他のsupervisorが記録した）何も書かずにその確かめを終える（同じ実行の同じattemptを2度記録しない）。
  同じトランザクションで`ci_checked`・`ci_turned_red` / `ci_turned_green`・finding・runtimeの`resolved`（下の「閉じ方」）を書く。
- 確かめるのは`SuperviseOptions::ci_watch`（`CiWatchOptions`）を持つsupervisorだけで、`dagq supervise`は`--once`でなければ付ける。
  `--once`のsupervisorと、付けないlibraryの呼び出し元（tests）は確かめない。

## ghの呼び方

どの呼び出しもmain checkoutで行い、repositoryを明示し、時間で止める（`infrastructure::ci_watch`のmodule doc）。
repositoryは`[repository] remote`の`git remote get-url`がGitHubのURLなら、そこから決める（`domain::ci_watch::github_repo`）。
GitHubのURLでない・remoteが無いときは設定の誤りとして`ci_watch_unavailable`にし、`git`が動かない・時間切れは一時の失敗（下の「eventの種類と欄」）にする。

1. **読む手段の確かめ**（毎回）: `gh`を`[run.env]`のプログラムの検査と同じ規則（`resolve_program`）で解決し、`gh auth status`の終了コードを見る。
2. **実行の一覧**: 着地先のbranchへのpushの終わった実行を読み、`ci_checked`がまだ記録していない実行とattemptを作られた順に処理する（`domain::ci_watch::runs_to_process`）。
   遅れて終わった実行・re-run・見張りの最初の確かめ・返った実行より前に残る未処理の実行（`gap`）の扱いは、そのdoc commentが持つ。
3. **落ちたjobとstep**: 赤の実行ごとにjobsを読み、落ちたjobとそのstepを取る（`domain::ci_watch::job_failed`）。
   読めなければwarnを出してjobなしで続ける（1つの実行で見張りが止まらない）（`ci-failure.yml`のissueの本文と同じ読み方）。
4. **JUnit**: `junit_artifacts`があれば、成否の決まった実行ごとに成果物をqueue dirの下に落とし、その下の`*.xml`を全部読む（`domain::ci_watch::parse_junit`）。
   成果物が無い・落とせない・XMLが読めないときはその実行のtestの成否を「分からない」とし、jobとstepだけを使う（`domain::ci_watch::Junit`）。
   宣言したprofileがJUnitを読まなければ落とさず、jobとstepだけを使う（下の「testの名前」）。

## 実行の扱い

- 実行の成否は`domain::ci_watch::classify`が決め、成否の決まらない実行は飛ばして次の`ci_checked`に数え、範囲は次に成否の決まった実行が引き受ける。
- **名指したjobが飛んだsuccess**（[ADR-t2034-1](../../adr/2026-10-07-t2034-1-skip-rust-ci-jobs-on-docs-only-changes-and-do-not-read-skipped-runs-as-green.md)）: `required_jobs`があれば`success`の実行のjobsも読み、名指したjobが全部`success`の実行だけを緑とする（`domain::ci_watch::read_green`）。
  緑でない`success`は飛ばす実行に数え、次に成否の決まった実行が理由とともに引き受ける。
  - 名指したjobが無い: `ci_jobs_missing`で知らせ、名指したjobが揃った緑までinbox宛てのattentionにする（`WatchState::jobs_missing_event`）。
    設定がそのjobを名指さなくなれば緑を待たずに消える。
    一覧と状態は変えない（`WatchState::standing_jobs_missing`）。
  - jobsを読めない: その実行も後の実行も処理せず次の間隔で読み直し、実行ごとに上限まで続けば`ci_check_failed`を1回記録して飛ばす（`JobsUnread`）。
  - どれも緑と読まないので、既に落ちているtestの一覧は消えない。
    docsだけのpushで落ちたdocの検査の項目は、Rustのjobも流して通る実行で外れる。
- **遅れて終わった実行**: 記録した実行のうち最後に作られたもの（以下「先頭」）より前に作られた実行（先頭のre-runは除く）は、`ci_checked`に`late`を付けて記録するだけで、一覧・状態・範囲を変えない（`ci_turned_red` / `ci_turned_green`もfindingも書かない。`domain::ci_watch::decide`）。
  後に作られた実行がより新しいcommitで既に決めたことを、古いcommitの成否で戻したり上書きしたりしないためで、落ちたtestとjobはその`ci_checked`に残る。
- **re-run**: 先頭の実行の新しいattemptは、新しい実行と同じに扱う。
  赤だった先頭をre-runして緑になれば一覧の全ての項目を外して`ci_turned_green`を書き、赤のままなら他の赤の実行と同じに、一覧に無い落ちたtestを足し、JUnitで通ったtestを外す（下の「一覧」）。
  先頭より前に作られた実行のre-runは上の遅れて終わった実行と同じに記録だけする。
- **testの名前**: JUnitの`testcase`の`classname`と`name`から、JUnitを読む最初の宣言したprofileが道具の表示と同じ名前を作る（`Toolchains::junit_test_name`、[Toolchain profiles](../toolchain-profiles.md)）。
  同じ名前が複数のファイルにあれば（macOSとLinuxのjob）、`domain::ci_watch::merge_outcomes`がまとめる。
- **testの名前が取れない失敗**: 赤の実行で、JUnitが無いか読めないか読むprofileが無いか、JUnitが落ちたtestを1つも名指さないときは、落ちたjobの落ちたstepごとに`job:<job名>/step:<step名>`（落ちたstepの無い落ちたjobは`job:<job名>`）を一覧の項目にする。
  JUnitのファイルとjobを結びつけられないので、jobごとではなく実行ごとに決める。

## 一覧

一覧はqueueのeventから求めるビューで、正本は`ci_checked`の`added` / `removed`の積み重ね（表を足さない。畳み込みは`domain::ci_watch::WatchState::fold`）。

- **足す**: 赤の実行で落ちたtest（とtestの名前が取れない失敗の項目）のうち、一覧に無いもの。
- **外す**: 後の赤の実行のJUnitでそのtestが通ったとき（JUnitが無いか読めないか読むprofileの無い実行と、そのtestを流していない実行では外さない）。
  緑の実行では、一覧の全ての項目を外す。
  jobとstepの項目は緑の実行でだけ外す。
  外したことは`removed`に理由付きで記録する。
- 項目は、足した実行と、その実行で記録したfindingを持つ（`domain::ci_watch::KnownFailure`）。

## eventの種類と欄

どれもqueueのevent（taskにもrunにも付かない）で、種類ごとのいつ書くかは`domain::ci_watch`の各kindの定数と`Access::transition`のdoc commentが持ち、kindは`domain::event_kind`に足す（kindの追加は互換、[ADR-0073](../../adr/0073-kind-additions-are-compatible.md)）。

通信の失敗・`gh`の非0の終了（認証以外）・時間切れは`ci_watch_unavailable`にせず、logにwarnを出して次の間隔でやり直す。
この失敗はclaimも着地も止めない（その後はqueueの最新の`ci_watch_unavailable` / `ci_watch_available`の答えに従う。失敗の前に`ci_watch_available`を記録した確かめなら、そこで保留が解ける）。
同じ理由で上限（`CI_WATCH_FAILURE_LIMIT`）まで続いたら`ci_check_failed`を1回記録する（attentionにしない。observerが読む）。
jobsを読めない実行の`ci_check_failed`は上の「実行の扱い」（`application::ci_watch::check`）。

## 読む手段が無いとき

[Run environment](run-environment.md)の「`[run.env]`が名指すプログラムの検査」（[ADR-0049](../../adr/0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)決定9）と同じ形にする（ADR-t1920-1決定2）。

- **`up`のpreflight**: `[ci_watch]`があれば、`[run.env]`の検査の後に`up`のPATHで上の「読む手段の確かめ」を行い（`lifecycle::Ports::ci_watch_preflight`、実装は`infrastructure::ci_watch::preflight`）、だめならsupervisorを起動せず、理由と対処（`domain::ci_watch::Access::message`）を挙げ、supervisorを起動しなかったと言うerrorで止まる。
- **supervisor**: 確かめるたびに答えを見て、`[ci_watch]`があり、このプロセスがまだ答えを持たないか最後の答えがだめな間は、各fill passのclaimをせず、reviewを通ったrunは着地slotを取らずに`awaiting_integration`で待たせ、着地の列のrunも待たせる（`domain::ci_watch::hold_reason`）。
  drain・handoff・claimの停止の最中なら、最後の答えがだめなときだけleaseを返し、最初の答えを待っている間は返さずに待つ。
  どれも`[run.env]`のプログラムが見つからないとき（「着地の保留」）と同じで、走っているrun・review・triage・始まった着地・始まったresumeは止めず、新しいresumeは始めない。
  一覧はそのまま（最後に記録した状態）で読まれ続ける。
  戻れば`ci_watch_available`を記録して次のpassからclaimと着地を再開する。
- **attention**: `ci_watch_unavailable`はinbox宛てのattentionで（`domain::event_attention`）、`watch`はこのeventで起き、`ci_watch_available`では起きない。
  `next`は理由ごとに`domain::ci_watch::Unavailable::next`が決める。
  `status`は最新が`ci_watch_unavailable`の間、`run_env_program_missing`の行と同じ形で`status: unavailable`の行を出し（[`status`](commands.md)）、`ci_watch_available`で消える。
- **保留の記録**: `ci_watch_unavailable` / `ci_watch_available`はqueueの答えで、supervisorが保留していることは示さないので、supervisorは保留に入ったとき・理由が変わったとき・抜けたときだけ`ci_watch_held` / `ci_watch_resumed`を記録する（`domain::claim_hold::OwnHold`）。
  各supervisorは自分の最新の記録と比べ、passごとには記録せず、`[ci_watch]`の無いsupervisorは記録しない。
  `ci_check_failed`はこの保留の始まりと終わりではない。
  `candidates`の`held`は生きたsupervisorの記録だけを読み、`status`は登録された各supervisor（生きていないものも）の項目にそのsupervisor自身の最新の保留を出す。
  どちらもattentionにはしない（人の手が要る読めないときは`ci_watch_unavailable`が知らせる）。
- **`doctor`**: 結びついたcheckoutの`dagq.toml`に`[ci_watch]`があれば、`doctor`のPATHでの読む手段の確かめと、supervisorの最後の`ci_watch_unavailable` / `ci_watch_available`を出す（`application::ci_watch::doctor`）。

## finding（修正taskの材料）

赤の実行の`added`が空でなければ、そのとき1件のfindingを`record_ci_check`のトランザクションの中で`record_finding`と同じ規則（observerの`finding record`と同じ一致と更新。CLIの認可を通らないruntimeの書き込み、`recorded_by`は`supervisor`）で記録し、そのIDを`ci_checked`と`ci_turned_red`に同じトランザクションで書く。
findingの`kind`・対象・`impact`は`application::ci_watch::check`が、`subject`・`summary`・`detail`は`domain::ci_watch::decide`が組む`FailureFinding`が持ち、根拠はその実行の`ci_checked`と（あれば）`ci_turned_red`のevent。

- **鍵**: `added`の名前の組から決め（`domain::ci_watch::failure_key`）、同じ種類・対象・鍵のfindingがあれば新しい行を作らずに更新する（`record_finding`の一致: 閉じていないものか、無ければ最後に閉じたもの）。
  `resolved`のfinding（下の「閉じ方」で閉じた同じ組がまた落ちた）は`open`に戻って回数と根拠が増え、印を付け直す。
  `dismissed`のfindingは回数と根拠を足すだけで印を付け直さない（ADR-0047決定18）。
- **修正taskが持つもの**: findingの`detail`（plannerがtaskの`description`に写すもの）は、足したtest（jobとstepの項目を含む）、その実行の落ちたjobとstep、最後の緑からこの実行までのcommitの範囲（cancelで飛ばした実行の分も含む）、実行のURLと実行、supervisorのbuildが名乗るcommitとそれが範囲を含むか（`domain::ci_watch::binary_contains`、[Build identifier](build-identifier.md)）を持つ。
- **plannerへの載せ方**: [Finding planners](finding-planners.md)の「CIの見張りの`ci_failure`のfinding」。
- **閉じ方**: findingの項目が全部一覧から外れたら（`removed`）、findingが`open`で閉じていないruntimeのplannerが無ければ、その実行を記録する同じトランザクションで`resolved`（`by: runtime`）にする。
  plannerが立っていれば閉じず、plannerの決定（proposalか`dismiss`）に任せ、promptに載らない後からの緑は次のpassの`ci failures`で読める。
  `proposed`のfindingはproposalの終わり（`settle_findings`）で決まる。

## 一覧の読み方

- **CLI**: `dagq ci failures`は読み取り専用で、queueを読み取り専用で開き、結びついたmain checkoutの`dagq.toml`の`[ci_watch]`を読む（出力は`--help`、入口は`application::ci_watch::known_failures`、capabilityは[Authorization](../authorization.md)）。
- **`status`**: 最上位の`ci`の欄をどのroleの`status`にも出す（`domain::ci_watch::status_view`）。
- **runtimeの中の読み手**（CLIと同じ`application::ci_watch::known_failures`を使う）: 着地の検証は、`integrate`がtaskの検証のコマンドを着地の検証のコマンドに置き換えるとき（[Validation](validation.md#着地の検証)）、runのtaskについて読んだ一覧（上のCLIの`--task`の出力と同じ形）をrunのrun dirに書き、そのpathと修正taskのrunかを置き換えたコマンドのenvに足す（`application::integrate`の`landing_env`。envとファイルの名前と意味は`domain::landing_verification`のdoc comment）。
  どのtestを外すかの適用は`dagq.toml`の着地の検証のコマンドとrepositoryのscriptが受け持つ。

### workerとreviewの材料

- 読み手: claimのprompt、resumeと衝突の依頼、runの全体のreviewのprompt。
  agentのjobには載せない（差し戻しを決めるのは全体のreviewのため）。
- 材料: supervisorが`--task`の一覧の各項目にそのfindingを直すtask（`fix_tasks`）を足し（`ci_watch::run_known_failures`）、項目があるときだけ全体をrun directoryのファイル（名前は`KNOWN_FAILURES_FILE`のdoc comment）に書いて節を載せる。
  読めなければwarnを出して節を省く。
- 文面: 項目はtest・CIのURL・修正taskで、workerには自分の変更が原因でなければ直さずreceiptの`summary`に書くよう、reviewにはそれを差し戻しの理由にしないよう言う。
  修正taskのrunでは自分の項目を直す対象として先に示す。
- 上限: 自分の項目を先に、ほかをCIで落ちた新しい順に件数とbyteの上限まで取り、省いた件数とファイルを書く（`known_failures_section`）。
  各promptの全体の上限は節の上限（`KNOWN_FAILURES_LIMIT`、値と理由と上限のtestはdoc comment）の分だけ大きい。

## 修正taskのrun

runのtaskが属するproposal（`tasks.proposal_id`）を`findings.proposal_id`に持つか、そのtaskを`covered_by_task`に持つ`kind = ci_failure`のfindingがあれば、そのrunは修正taskのrunで、そのfindingの項目を一覧から除いて渡す（他のfindingの項目は外す対象のまま）。
taskに印や欄は足さない。
この判定（`ci_failure_findings_of`）は`ci failures --task`・着地の検証・workerとreviewの材料（上）が使い、どれもそのfindingの項目が`failures`から`kept_for_task`に移る。

## 保存（findingsの列）

- **列**: findingsの`covered_by_task`は、互換のmigrationが`REFERENCES`を足せないので外部キーを持たず（[persistence](../persistence.md)の「CIの見張り」）、CHECKも持たず（ADR-t876-1）、taskの存在と状態は書く前に確かめる。
  値の規則は`domain::finding::check_covered_by`が持つ。
- **書き手**: `finding dismiss --covered-by`（capabilityは`finding.dismiss`のまま）とqueue serviceの`finding_dismiss`で、`set_finding_status_covered`が同じトランザクションで状態・列と`finding_status_changed`を書く。
- **読み手**: 上の「修正taskのrun」と、[Finding planners](finding-planners.md)の`settle_findings`（`dismissed`は対象外）。

## 既存の仕組みとの関係

- `ci-failure.yml`のissueは残る（ADR-t1920-1決定8）。
- 自動更新（[Auto-update](auto-update.md)）は一覧もCIの結果も見ない。
- observerはstatsではなくeventとfindingで読む（`ci_check_failed`と`ci_failure`のfindingの滞留）。

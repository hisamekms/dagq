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
  - design-ci-failure-issues
---

# CI watch

> [ADR-t1920-1](../../adr/2026-10-06-t1920-1-supervisor-watches-main-ci-keeps-known-failures-and-files-fixes-through-findings.md)の見張り・event・finding・一覧・`dagq ci failures`・`finding dismiss --covered-by`はtask 1921が実装した。
> 着地の検証に一覧を渡すこと（goal 157の段3b）も実装済みで、workerのpromptとrunのreviewの材料に一覧を載せること（段5）だけが**予定（未実装）**である（下の「一覧の読み方」の「runtimeの中の読み手」と「修正taskのrun」に印を付けた）。

設定（[Run environment](run-environment.md)の`[ci_watch]`）を書いたrepositoryで、supervisorが着地先のbranchへのpushのCIの結果をhostの`gh`で定期的に読み、queueのeventに記録し、赤になったら`ci_failure`のfindingを記録してruntimeのfindingのplanner（[Finding planners](finding-planners.md)）に修正taskを作らせ、既に落ちているtestの一覧（以下「一覧」）を持つ。CIの結果をqueueに届ける経路で、GitHubのissue（[CI failure issues](../ci-failure-issues.md)）とは別に動く。

## いつ確かめるか

- supervisorのループの各passで、`[ci_watch]`をmain checkoutの`dagq.toml`から読み直す（`[provider_fallback]`の後の`ci_watch_pass`）。ファイルか表が無ければ何もしない（eventも書かない）。読めなければ（書式の誤りなど）warnを1回出し、使っている表のまま続ける。表が変われば次のpassで確かめる。
- 前に確かめてから`interval_secs`が経ったpass（前の確かめの始まりからの経過をsupervisorの注入した`Clock::monotonic`で測り、`domain::ci_watch::check_due`が決める）で1回確かめる（プロセスの最初のpass、つまり起動とexecの引き継ぎの直後は経過を問わず確かめる）。drain・handoffの間も確かめ、handoffのexecは走っている確かめの終わりを待つ。確かめ（`application::ci_watch::check`。GitHubとGitはport `application::ci_watch::CiSource`越しに読み、実装は`infrastructure::ci_watch::GhSource`）はjobのthreadで行い、ループを待たせない。終わった確かめの答えは次のpassで受け取る。
- 実行1件の記録は1つの書き込みトランザクション（`SqliteQueue::record_ci_check`、`infrastructure::ci_watch_store`）で、`BEGIN IMMEDIATE`の中で最新の`ci_checked`のevent IDを読み直し、確かめで読んだものと違えば（他のsupervisorが記録した）何も書かずにその確かめを終える（同じ実行の同じattemptを2度記録しない）。
  同じトランザクションで`ci_checked`・`ci_turned_red` / `ci_turned_green`・finding・runtimeの`resolved`（下の「閉じ方」）を書く。
- 確かめるのは`SuperviseOptions::ci_watch`（`CiWatchOptions`: `program`は既定`gh`、`interval`はtestが間隔を縮める上書き）を持つsupervisorだけで、CLIの`dagq supervise`は`--once`でなければ付ける。`--once`のsupervisorと、付けないlibraryの呼び出し元（tests）は確かめない。

## ghの呼び方

どれも`[ci_watch]`のmain checkoutをcwdにし、1回の呼び出しは60秒（`CI_WATCH_CALL_TIMEOUT`）で止める。repositoryは`--repo <owner>/<name>`で明示し、`[repository] remote`（既定`origin`）の`git remote get-url`がGitHubのURL（`https://github.com/<owner>/<name>(.git)`・`git@github.com:<owner>/<name>(.git)`・`ssh://git@github.com/<owner>/<name>(.git)`）なら、そこから決める（`domain::ci_watch::github_repo`）。GitHubのURLでない・remoteが無いときは設定の誤りとして`ci_watch_unavailable`（`reason: not_github`）にし、`git`が動かない・時間切れは一時の失敗（下の「eventの種類と欄」の`ci_check_failed`の数え方）にする。

1. **読む手段の確かめ**（毎回）: `gh`を`[run.env]`のプログラムの検査と同じ規則（supervisorのPATH、`/`を含む値はそのpath。`resolve_program`）で解決し、`gh auth status --hostname github.com`の終了コードを見る。
2. **実行の一覧**: `gh run list --repo R --workflow <workflow> --branch <branch> --event push --status completed --limit 50 --json databaseId,number,attempt,headSha,conclusion,url,createdAt,displayTitle`。
   `createdAt`の古い順に並べ、`ci_checked`が記録した実行（成否の決まった実行と`skipped_runs`）の`(run_id, attempt)`に無いものを処理する（`domain::ci_watch::runs_to_process`）。
   作られた順より遅れて終わった実行と、re-runの新しいattemptも、こうして次の確かめで読む。
   ただし見張りが最初に記録した実行より前に作られた実行は読まない。
   見張りの最初の確かめ（queueに`ci_checked`が無い）は最新の1件だけを処理し、過去を遡らない。
   supervisorが長く止まっていて、返った50件（`RUN_LIST_LIMIT`）の最も古いものより前に未処理の実行が残る（50件が返り、最も古いものが記録した実行のうち最後に作られたものより後に作られている）ときは、遡らずに返った分だけを処理し、最初の`ci_checked`に`gap: true`を書く（`range`の`from`はqueueに記録した最後の緑のまま）。
3. **落ちたjobとstep**: 赤の実行ごとに`gh run view <id> --repo R --json jobs`の`conclusion`が`failure`・`timed_out`・`cancelled`・`startup_failure`のjobとそのstep（同じconclusionのstep）。読めなければwarnを出してjobなしで続ける（1つの実行で見張りが止まらない）（[CI failure issues](../ci-failure-issues.md)の本文と同じ読み方）。
4. **JUnit**: `junit_artifacts`があれば、成否の決まった実行ごとに`gh run download <id> --repo R --pattern <glob> --dir <queue dir>/ci-watch/<id>/<globの番号>`（globごとに1回、重なるglobがぶつからないようglobごとのdir。落とせないglobはwarnを出して残りを読む）で落とし、その下の`*.xml`を全部読む（`domain::ci_watch::parse_junit`）。読み終えたらdirを消す。成果物が無い・落とせない・XMLが読めないときはその実行のtestの成否を「分からない」とし（`junit: missing`）、jobとstepだけを使う。`junit_artifacts`が空なら落とさない（`junit: not_configured`）。

## 実行の扱い

- `conclusion`が`success`なら緑、`failure`か`timed_out`なら赤、それ以外（`cancelled`・`skipped`・`neutral`・`action_required`・`startup_failure`・`stale`）は飛ばす。飛ばした実行は次の`ci_checked`の`skipped_runs`に数え、範囲は次に成否の決まった実行が引き受ける（`ci.yml`の`concurrency`は変えない）。
- **名指したjobが飛んだsuccess**（[ADR-t2034-1](../../adr/2026-10-07-t2034-1-skip-rust-ci-jobs-on-docs-only-changes-and-do-not-read-skipped-runs-as-green.md)）: `CiWatchConfig`の`required_jobs`があれば`success`の実行のjobsも読み、名指したjobが全部`success`の実行だけを緑とする（`domain::ci_watch::read_green`）。
  緑でない`success`は飛ばす実行に数え、次に成否の決まった実行が理由（`Undecided`）とともに引き受ける。
  - 名指したjobが無い: `ci_jobs_missing`で知らせ、名指したjobが揃った緑までinbox宛てのattention（`fix dagq.toml`）にする（`WatchState::jobs_missing_event`）。
  - jobsを読めない: その実行も後の実行も処理せず次の間隔で読み直し、実行ごとに上限まで続けば`ci_check_failed`を1回記録して飛ばす（`JobsUnread`）。
  - どれも緑と読まないので、既に落ちているtestの一覧は消えない。
    docsだけのpushで落ちたdocの検査の項目は、Rustのjobも流して通る実行で外れる。
- **遅れて終わった実行**: 記録した実行のうち最後に作られたもの（以下「先頭」）より前に作られた実行（先頭のre-runは除く）は、`ci_checked`に`late: true`を付けて記録するだけで、一覧・状態・範囲を変えない（`ci_turned_red` / `ci_turned_green`もfindingも書かない。`domain::ci_watch::decide`）。
  後に作られた実行がより新しいcommitで既に決めたことを、古いcommitの成否で戻したり上書きしたりしないためで、落ちたtestとjobは`failed_tests`・`failed_jobs`に残る。
- **re-run**: 先頭の実行の新しいattemptは、新しい実行と同じに扱う。
  赤だった先頭をre-runして緑になれば一覧の全ての項目を外して`ci_turned_green`を書き、赤のままなら他の赤の実行と同じに、一覧に無い落ちたtestを足し、JUnitで通ったtestを外す（下の「一覧」）。
  先頭より前に作られた実行のre-runは上の遅れて終わった実行と同じに記録だけする。
- **testの名前**: JUnitの`testcase`の`classname`（nextestではbinary id。例`dagq::it`）と`name`（例`runtime_claim::claims_in_order`）を空白1つでつないだ`dagq::it runtime_claim::claims_in_order`（nextestの表示と同じ）。`failure`か`error`の子を持てば落ちた、`skipped`の子を持てば流していない、どちらも無ければ通った。同じ名前が複数のファイルにあれば（macOSとLinuxのjob）、どれかで落ちれば落ちた、どれでも落ちずどれかで通れば通った。
- **testの名前が取れない失敗**: 赤の実行で、JUnitが`missing`か`not_configured`か、JUnitが落ちたtestを1つも名指さないときは、落ちたjobの落ちたstepごとに`job:<job名>/step:<step名>`（落ちたstepの無い落ちたjobは`job:<job名>`）を一覧の項目にする。JUnitのファイルとjobを結びつけられないので、jobごとではなく実行ごとに決める。

## 一覧

一覧はqueueのeventから求めるビューで、正本は`ci_checked`の`added` / `removed`の積み重ね（表を足さない。畳み込みは`domain::ci_watch::WatchState::fold`）。

- **足す**: 赤の実行で落ちたtest（とtestの名前が取れない失敗の項目）のうち、一覧に無いもの。
- **外す**: 後の赤の実行のJUnitでそのtestが通ったとき（`junit: missing` / `not_configured`の実行と、そのtestを流していない実行では外さない）。緑の実行では、一覧の全ての項目を外す。jobとstepの項目は緑の実行でだけ外す。外したことは`removed`に`reason`（`passed` / `green`）付きで記録する。
- 項目は、足した実行（`added`: `run_id`・`attempt`・`sha`・`url`・`at`（その`ci_checked`を記録した時刻））と、その実行で記録したfindingの`finding_id`（無ければnull）を持つ。

## eventの種類と欄

どれもqueueのevent（taskにもrunにも付かない）。`run_events`はkindのCHECKを持たないので（migration 0050、ADR-t876-1）migrationは要らず、kindは`domain::event_kind`に足す（kindの追加は互換、[ADR-0073](../../adr/0073-kind-additions-are-compatible.md)）。どれも`supervisor`（token）を持ち、`ci_checked`・`ci_turned_red`・`ci_turned_green`は`workflow`・`branch`も持つ。

| kind | いつ | payloadの欄 |
| --- | --- | --- |
| `ci_checked` | 成否の決まった実行の1つのattemptを処理したとき（間隔ごとの空振りでは書かない） | `run_id`（GitHubの`databaseId`）・`run_number`・`attempt`・`sha`・`url`・`created_at`・`conclusion`・`state`（`green` / `red`）・`skipped_runs`（この実行より前に飛ばした実行の`run_id`の配列）・`skipped_attempts`（同じ位置の`skipped_runs`のattempt）・`junit`（`read` / `missing` / `not_configured`）・`failed_tests`（名前の配列）・`failed_jobs`（`{job, steps[]}`の配列）・`added`（一覧に足した項目の名前の配列）・`removed`（`{name, reason}`の配列）・`known_failures`（処理の後の一覧の件数）・`finding_id`（この実行で記録・更新した`ci_failure`のfinding。無ければ欄を書かない）・`gap`（上の「実行の一覧」の飛びがあったときだけ`true`）・`late`（遅れて終わった実行のときだけ`true`） |
| `ci_turned_red` | 直前の`state`が`green`か無いときに赤の実行を処理したとき | `run_id`・`sha`・`url`・`last_green`（`{run_id, sha, url}`か、記録が無ければnull）・`finding_ids`（このとき記録・更新したfinding。`[]`か1件） |
| `ci_turned_green` | 直前の`state`が`red`のときに緑の実行を処理したとき | `run_id`・`sha`・`url`・`red_since`（最初の赤の`{run_id, sha, url}`）・`red_secs`（最初の赤の`created_at`からこの実行の`created_at`まで） |
| `ci_watch_unavailable` | 読む手段が無いと分かり、queueの最新の`ci_watch_unavailable` / `ci_watch_available`が同じ`reason`の`ci_watch_unavailable`でないとき（理由が変われば、例えば`gh_missing`から`gh_unauthenticated`へ、また記録する） | `reason`（`gh_missing` / `gh_unauthenticated` / `not_github`）・`program`（`gh`の値）・`path`（探したPATH）・`message` |
| `ci_watch_available` | 読む手段があり、queueの最新が`ci_watch_unavailable`のとき | `program`・`resolved`（解決したpath） |
| `ci_jobs_missing` | 上の「実行の扱い」 | `WatchState::jobs_missing_event` |

通信の失敗・`gh`の非0の終了（認証以外）・時間切れは`ci_watch_unavailable`にせず、logにwarnを出して次の間隔でやり直す。この失敗はclaimも着地も止めない（その後はqueueの最新の`ci_watch_unavailable` / `ci_watch_available`の答えに従う。失敗の前に`ci_watch_available`を記録した確かめなら、そこで保留が解ける）。同じ理由で3回（`CI_WATCH_FAILURE_LIMIT`）続いたら`ci_check_failed`（`error`・`failures`・`since`（unix秒）・`supervisor`）を1回記録する（attentionにしない。observerが読む）。
jobsを読めない実行の`ci_check_failed`は上の「実行の扱い」（`application::ci_watch::check`）。

## 読む手段が無いとき

[Run environment](run-environment.md)の「`[run.env]`が名指すプログラムの検査」（[ADR-0049](../../adr/0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)決定9）と同じ形にする（ADR-t1920-1決定2）。

- **`up`のpreflight**: `[ci_watch]`があれば、`[run.env]`の検査の後に`up`のPATHで上の「読む手段の確かめ」を行い（`lifecycle::Ports::ci_watch_preflight`、実装は`infrastructure::ci_watch::preflight`）、だめならsupervisorを起動せず、理由と対処（`gh`を入れる・`gh auth login`・remoteを直す、または`dagq.toml`から`[ci_watch]`を外すtaskを登録する）を挙げ`; the supervisor was not started`で終わるerrorで止まる。
- **supervisor**: 確かめるたびに答えを見て、`[ci_watch]`があり、このプロセスがまだ答えを持たないか最後の答えがだめな間（`ci_watch_held`）は、各fill passのclaimをせず、reviewを通ったrunは着地slotを取らずに`awaiting_integration`で待たせ（`Phase::AwaitingSlot`。着地の列のrunの`start_approved_landings`も待つ）、drain・handoff・claimの停止の最中なら、最後の答えがだめなとき（`ci_watch_unreadable`）だけleaseを返す（理由`the CI [ci_watch] watches cannot be read`。最初の答えを待っている間は返さずに待つ）。どれも`[run.env]`のプログラムが見つからないとき（「着地の保留」）と同じで、走っているrun・review・triage・始まった着地・始まったresumeは止めず、新しいresumeは始めない。一覧はそのまま（最後に記録した状態）で読まれ続ける。戻れば`ci_watch_available`を記録して次のpassからclaimと着地を再開する。
- **attention**: `ci_watch_unavailable`はinbox宛てのattentionで（`domain::event_attention`の`ATTENTION_KINDS`に足す。[`events` / `watch`](events-watch.md)）、`watch`はこのeventで起き、`ci_watch_available`では起きない。`next`は`reason`が`gh_missing`なら`install tool`、`gh_unauthenticated`なら`log in to gh`、`not_github`なら`fix dagq.toml`（後の2つは`AttentionNext`に足す）。`status`は最新が`ci_watch_unavailable`の間`kind: ci_watch_unavailable`、`status: unavailable`、その`next`、`last_error`に`message`を出す（`run_id` / `task_id`はnull。`run_env_program_missing`の`status: missing`の行と同じ形、[`status`](status.md)）。`ci_watch_available`で消える。
- **保留の記録**: `ci_watch_unavailable` / `ci_watch_available`はqueueの答えで、supervisorが保留していることは示さないので、supervisorは保留に入ったとき・理由が変わったとき・抜けたときだけ`ci_watch_held` / `ci_watch_resumed`を記録する（`domain::claim_hold::OwnHold`）。
  各supervisorは自分の最新の記録と比べ、passごとには記録せず、`[ci_watch]`の無いsupervisorは記録しない。
  `ci_check_failed`はこの保留の始まりと終わりではない。
  `candidates`の`held`は生きたsupervisorの記録だけを読み、`status`は登録された各supervisor（生きていないものも）の項目にそのsupervisor自身の最新の保留を出す。
  どちらもattentionにはしない（人の手が要る読めないときは`ci_watch_unavailable`が知らせる）。
- **`doctor`**: 結びついたcheckoutの`dagq.toml`に`[ci_watch]`があれば`ci_watch`の欄に`config`（読んだ`[ci_watch]`）・`gh`（`doctor`のPATHで解決したpathか null）・`authenticated`（bool。`gh auth status --hostname github.com`が0で終わるか）・`repo`（`<owner>/<name>`か null）・`supervisor_last`（最後の`ci_watch_unavailable` / `ci_watch_available`の`{kind, created_at, payload}`か null）を出す。ファイルが読めなければ`{error}`、表が無ければ欄ごと出さない。

## finding（修正taskの材料）

赤の実行の`added`が空でなければ、そのとき1件のfindingを`record_ci_check`のトランザクションの中で`record_finding`と同じ規則（observerの`finding record`と同じ一致と更新。CLIの認可を通らないruntimeの書き込み、`recorded_by`は`supervisor`）で記録し、そのIDを`ci_checked`の`finding_id`と`ci_turned_red`の`finding_ids`に同じトランザクションで書く。

| 欄 | 値 |
| --- | --- |
| `kind` | `ci_failure` |
| 対象 | `queue` |
| `subject` | 鍵（下）。例`ci_failure:3f9a1c0d2b7e4a51` |
| `summary` | `CI <workflow> on <branch> fails: <件数> new failure(s) since <short sha>` |
| `detail` | 下の「修正taskが持つもの」をJSONの1 objectで（`tests`・`failed_jobs`・`range`・`url`・`binary_contains`・`binary_commit`） |
| `impact` | `high` |
| 根拠 | その実行の`ci_checked`と（あれば）`ci_turned_red`のevent ID |
| proposalを求める印 | `propose_reason`: `CI on <branch> is red; a fix task is needed (ADR-t1920-1)` |

- **鍵**: `added`の名前を辞書順に並べ、`\n`でつないだUTF-8のSHA-256の16進の先頭16文字に`ci_failure:`を付ける（`domain::ci_watch::failure_key`）。同じ種類・対象・鍵のfindingがあれば新しい行を作らずに更新する（`record_finding`の今の一致: 閉じていないものか、無ければ最後に閉じたもの）。`resolved`のfinding（下の「閉じ方」で閉じた同じ組がまた落ちた）は`open`に戻って回数と根拠が増え、印を付け直す。`dismissed`のfindingは回数と根拠を足すだけで印を付け直さない（ADR-0047決定18）。
- **修正taskが持つもの**（findingの`detail`と、plannerがtaskの`description`に写すもの）:
  - `tests`: `added`の名前（jobとstepの項目を含む）
  - `failed_jobs`: その実行の落ちたjobとstep
  - `range`: `{from, to, commits}`。`from`は`last_green`の`sha`（その実行より前で最後に緑だった実行。queueに無ければnull）、`to`はこの実行の`sha`、`commits`は`git rev-list --count from..to`（`from`がnullならnull）。cancelで飛ばした実行の分も含む
  - `url`: 実行のURL
  - `binary_contains`: supervisorのbuild識別子が名乗るcommit（`build_id::named_commit`、[Build identifier](build-identifier.md)）を`binary_commit`に書き、`to`がその祖先なら`all`、`from`がnullでなく、名乗るcommitが`from`か`from`の祖先なら`none`、名乗るcommitが`from..to`の中なら`some`、名乗るcommitが無い（リリース・`+unknown`）か判定できなければ`unknown`
  - `run`: その実行の`{run_id, attempt, sha, url}`
- **plannerへの載せ方**: `finding_planner_prompt`は`kind`が`ci_failure`のとき節`## A CI failure`を足し、`detail`の各項をtaskの`description`に写すこと、`dagq search`で同じtestを直すtaskが既にあればtaskを作らず`finding dismiss <id> --covered-by <task> --reason '...'`にすること（`--covered-by`はfindingの`covered_by_task`に書き、`finding_status_changed`に`covered_by_task`を載せる。`ci_failure`のfindingにだけ受け付け、taskは閉じていないものに限る）、改善の規則どおり`--priority`を付けないこと（[Finding planners](finding-planners.md)の6）を指示する。
- **閉じ方**: findingの項目が全部一覧から外れたら（`removed`）、findingが`open`で閉じていないruntimeのplannerが無ければ、その実行を記録する同じトランザクションで`resolved`にする（理由`its tests passed on <shaの先頭12文字>`、`finding_status_changed`の`by: runtime`）。plannerが立っていれば閉じず、plannerの決定（proposalか`dismiss`）に任せ、promptに載らない後からの緑は次のpassの`ci failures`で読める。`proposed`のfindingは今までどおりproposalの終わり（`settle_findings`）で決まる。

## 一覧の読み方

- **CLI**: `dagq ci failures [--task ID]`（読み取り専用。queueを読み取り専用で開き、結びついたmain checkoutの`dagq.toml`の`[ci_watch]`を読む。capability `ci.read`で、user・inbox・planner・observer・supervisorが持ち、worker・wrapper・integratorとjobには許さない（[Authorization](../authorization.md)）。queue serviceのユースケースではなく、clientのmodeでは`no_use_case`で断る）。出力:

  ```json
  {
    "enabled": true,
    "workflow": "ci.yml",
    "branch": "main",
    "state": "red",
    "watch": "available",
    "checked_at": "2026-10-06T12:00:00Z",
    "latest_run": {"run_id": 123, "attempt": 2, "sha": "…", "url": "…", "conclusion": "failure"},
    "failures": [
      {"name": "dagq::it runtime_claim::claims_in_order", "kind": "test",
       "added": {"run_id": 120, "attempt": 1, "sha": "…", "url": "…", "at": "…"},
       "finding_id": 45}
    ],
    "kept_for_task": []
  }
  ```

  `workflow`は`[ci_watch]`のもので、表が無ければ最新の`ci_checked`のもの。
  `branch`は`[ci_watch] branch`、書かなければ最新の`ci_checked`のもの（見張りが着地先のbranchで記録した名前）で、どちらも無い（`branch`を書かず、まだ何も記録していない）ときはnull。
  `state`は`green` / `red` / `unknown`（記録が無い）で、`latest_run`とともに「実行の扱い」の先頭の実行の最後に記録したattemptのもの（遅れて終わった実行は`latest_run`にも`state`にもならず、一覧を戻さない）。
  `failures`の`added`はその項目を足した実行とattempt。
  `watch`は`available` / `unavailable` / `disabled`。
  `kind`は`test`か`job_step`。
  `--task ID`を付けると、そのtaskのproposalに紐づいたか、そのtaskを`covered_by_task`に持つ`ci_failure`のfinding（`ci_failure_findings_of`）の項目を`failures`から除き`kept_for_task`に移す（下の「修正taskのrun」）。
  `[ci_watch]`が無く記録も無ければ`{"enabled": false, "state": "unknown", "watch": "disabled", "failures": []}`。
- **`status`**: 最上位の`ci`の欄に`{state, watch, failures: <件数>, checked_at, latest_run_url}`（どのroleの`status`にも出す）。見張りの記録が無ければnull。
- **runtimeの中の読み手**（CLIと同じ`application::ci_watch::known_failures(queue, config, branch, task)`を使う）: 着地の検証は、`integrate`がtaskの検証のコマンドを着地の検証のコマンドに置き換えるとき（[Validation](validation.md#着地の検証)）、runのtaskについて読んだ一覧（上のCLIの`--task`の出力と同じ形）をrunのrun dirに書き、そのpathと修正taskのrunかを置き換えたコマンドのenvに足す（`application::integrate`の`landing_env`。envとファイルの名前と意味は`domain::landing_verification`のdoc comment）。
  どのtestを外すかの適用は`dagq.toml`の着地の検証のコマンドとrepositoryのscriptが受け持つ。
  workerのprompt（claimとresume）とrunのreviewの材料（段5）に`failures`の名前と`added.url`を載せることは予定（未実装）。

## 修正taskのrun

runのtaskが属するproposal（`tasks.proposal_id`）を`findings.proposal_id`に持つか、そのtaskを`covered_by_task`に持つ`kind = ci_failure`のfindingがあれば、そのrunは修正taskのrunで、そのfindingの項目を一覧から除いて渡す（他のfindingの項目は外す対象のまま）。
taskに印や欄は足さない。
この判定（`ci_failure_findings_of`）は`ci failures --task`と着地の検証（上の「runtimeの中の読み手」）が使い、着地の検証の一覧ではそのfindingの項目が`failures`から`kept_for_task`に移る。
workerのpromptとreviewの材料に渡すこと（段5）は予定。

## 保存（findingsの列）

eventの種類にはmigrationが要らないが（上の「eventの種類と欄」）、`--covered-by`はfindingsの表に列を足すmigrationが要る。

- **列**: `findings.covered_by_task INTEGER`（NULL可、既定NULL）。migration `0069_finding_covered_by.sql`（互換）が`ALTER TABLE findings ADD COLUMN`する。互換のmigrationは`REFERENCES`を足せないので（0068と同じ）外部キーは持たず、taskの存在と状態は書く前に確かめる。CHECKは足さない（ADR-t876-1。値の規則は`domain::finding::check_covered_by`が持つ: `kind`が`ci_failure`のときだけ、閉じていない（`completed` / `canceled`でない）taskだけを、`dismissed`への遷移と一緒にだけ書ける）。
- **書き手**: `finding dismiss <id> --reason R --covered-by <task>`（capabilityは`finding.dismiss`のまま。`set_finding_status_covered`が同じトランザクションで状態・列と`finding_status_changed`を書く）。queue serviceの`finding_dismiss`の引数`covered_by`（省略可）も同じ。`finding_status_changed`のpayloadに`covered_by_task`（task IDか、無ければ欄を書かない）を足す。`findings`と`findings ID --full`の各行に`covered_by_task`（null可）を出す。
- **読み手**: 下の「修正taskのrun」と、[Finding planners](finding-planners.md)の`settle_findings`（変えない。`dismissed`は対象外）。

## 名前を書く文書

この仕組みの名前と、それを書く文書。

| 名前 | 直す文書 |
| --- | --- |
| `[ci_watch]` | [Run environment](run-environment.md)、[`supervise`](supervise.md)（passごとの読み直しとclaim・着地の保留） |
| `dagq ci failures`（新しい読むコマンド） | [domain-model](../domain-model.md)の「Current operations」、[Authorization](../authorization.md)の「Policy」と「ほかのコマンド」（`ci.read`。workerとjobには許さない）と[Security](../security.md)、[Queue service](../queue-service.md)の「まだ無いもの」、pluginのdagqの`reference/inspect.md`と`reference/authority.md`（documents.mdの「権限の表を写す文書」） |
| `finding dismiss --covered-by`・`covered_by_task` | [domain-model](../domain-model.md)の`finding dismiss`、[Queue service](../queue-service.md)の`finding_dismiss`（引数`covered_by`）、[Authorization](../authorization.md)（capabilityは`finding.dismiss`のままで、表は変えない）、[persistence](../persistence.md)の「findings」、pluginの`dagq-planner`の`SKILL.md`と`reference/register.md`の`finding dismiss`の書き方 |
| `status`の`ci`、attention `ci_watch_unavailable`・`ci_jobs_missing`と`next`の値 | [`status`](status.md)、[`events` / `watch`](events-watch.md)、pluginの`dagq-inbox`の`reference/status.md`（`next`の手当て: `log in to gh`は人が`gh auth login`を打つ） |
| `doctor`の`ci_watch` | [`doctor`](doctor.md)、pluginの`dagq-recover`の`reference/doctor.md` |
| `up`のpreflightの`gh` | [`up` / `down`](up-down.md)、pluginの`dagq-recover`の`reference/up-down.md` |
| eventの種類（`ci_checked`ほか） | `domain::event_kind`（`is_queue`）だけ（migrationは要らない） |
| 着地の検証のコマンドに渡す一覧のenv（段3b。名前は`domain::landing_verification`） | [integrate](integrate.md)の着地の検証の置き換え、[Validation](validation.md#着地の検証)、[Run environment](run-environment.md)の`[landing_verification]`の項 |

## 既存の仕組みとの関係

- [CI failure issues](../ci-failure-issues.md)のissueは残る（ADR-t1920-1決定8）。
- 自動更新（[Auto-update](auto-update.md)）は一覧もCIの結果も見ない。
- observerはstatsではなくeventとfindingで読む（`ci_check_failed`と`ci_failure`のfindingの滞留）。

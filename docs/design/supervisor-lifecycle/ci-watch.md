---
id: design-supervisor-lifecycle-ci-watch
type: design
title: "CI watch（supervisorが着地先のbranchのCIを見張る）"
status: draft
created: 2026-10-06
updated: 2026-10-06 # task 1920
last_verified: 2026-10-06 # task 1920
scope: runtime
related:
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

> **予定（未実装、goal 157 の段 1）**: この文書は[ADR-t1920-1](../../adr/2026-10-06-t1920-1-supervisor-watches-main-ci-keeps-known-failures-and-files-fixes-through-findings.md)の予定の姿で、今のruntimeは何もしない。設定の書式・eventの種類と欄・findingの鍵・一覧の読み方・ghの呼び方は、実装のtask（1921・1922・1926・1927・1930）がここから読む。実装で変えたらこの文書を直す。

設定（[Run environment](run-environment.md)の`[ci_watch]`）を書いたrepositoryで、supervisorが着地先のbranchへのpushのCIの結果をhostの`gh`で定期的に読み、queueのeventに記録し、赤になったら`ci_failure`のfindingを記録してruntimeのfindingのplanner（[Finding planners](finding-planners.md)）に修正taskを作らせ、既に落ちているtestの一覧（以下「一覧」）を持つ。CIの結果をqueueに届ける経路で、GitHubのissue（[CI failure issues](../ci-failure-issues.md)）とは別に動く。

## いつ確かめるか

- supervisorのループの各passで、`[ci_watch]`をmain checkoutの`dagq.toml`から読み直す（`[provider_fallback]`の後）。表が無ければ何もしない（eventも書かない）。
- 前に確かめてから`interval_secs`が経ったpassで1回確かめる（起動とexecの引き継ぎの直後の最初のpassは経過を問わず確かめる）。drain・handoffの間も確かめる。確かめはjobのthreadで行い、ループを待たせない。同じqueueに複数のsupervisorが居るときは、`BEGIN IMMEDIATE`の中で最新の`ci_checked`を読み直し、他のsupervisorが記録済みの実行を記録しない（同じ実行を2度記録しない）。
- `--once`のsupervisorと`SuperviseOptions::ci_watch`を持たないlibraryの呼び出し元（tests）は確かめない。

## ghの呼び方

どれも`[ci_watch]`のmain checkoutをcwdにし、1回の呼び出しは60秒（`CI_WATCH_CALL_TIMEOUT`）で止める。repositoryは`--repo <owner>/<name>`で明示し、`[repository] remote`（既定`origin`）の`git remote get-url`がGitHubのURL（`https://github.com/<owner>/<name>(.git)`か`git@github.com:<owner>/<name>(.git)`）なら、そこから決める。GitHubのURLでなければ設定の誤りとして`ci_watch_unavailable`（`reason: not_github`）にする。

1. **読む手段の確かめ**（毎回）: `gh`を`[run.env]`のプログラムの検査と同じ規則（supervisorのPATH、`/`を含む値はそのpath。`resolve_program`）で解決し、`gh auth status --hostname github.com`の終了コードを見る。
2. **実行の一覧**: `gh run list --repo R --workflow <workflow> --branch <branch> --event push --status completed --limit 50 --json databaseId,number,headSha,conclusion,url,createdAt,displayTitle`。`createdAt`の古い順に並べ、最新の`ci_checked`の`run_id`より後に作られたものだけを処理する。見張りの最初の確かめ（queueに`ci_checked`が無い）は最新の1件だけを処理し、過去を遡らない。supervisorが長く止まっていて、返った50件の最も古いものより前に未処理の実行が残る（最も古いものが最新の`ci_checked`の`run_id`より後に作られている）ときは、遡らずに返った分だけを処理し、最初の`ci_checked`に`gap: true`を書く（`range`の`from`はqueueに記録した最後の緑のまま）。
3. **落ちたjobとstep**: 赤の実行ごとに`gh run view <id> --repo R --json jobs`の`conclusion`が`failure`のjobとそのstep（[CI failure issues](../ci-failure-issues.md)の本文と同じ読み方）。
4. **JUnit**: `junit_artifacts`があれば、成否の決まった実行ごとに`gh run download <id> --repo R --pattern <glob> --dir <queue dir>/ci-watch/<id>`（globごとに1回）で落とし、その下の`*.xml`を全部読む。読み終えたらdirを消す。成果物が無い・落とせない・XMLが読めないときはその実行のtestの成否を「分からない」とし（`junit: missing`）、jobとstepだけを使う。

## 実行の扱い

- `conclusion`が`success`なら緑、`failure`か`timed_out`なら赤、それ以外（`cancelled`・`skipped`・`neutral`・`action_required`・`startup_failure`・`stale`）は飛ばす。飛ばした実行は次の`ci_checked`の`skipped_runs`に数え、範囲は次に成否の決まった実行が引き受ける（`ci.yml`の`concurrency`は変えない）。
- **testの名前**: JUnitの`testcase`の`classname`（nextestではbinary id。例`dagq::it`）と`name`（例`runtime_claim::claims_in_order`）を空白1つでつないだ`dagq::it runtime_claim::claims_in_order`（nextestの表示と同じ）。`failure`か`error`の子を持てば落ちた、`skipped`の子を持てば流していない、どちらも無ければ通った。同じ名前が複数のファイルにあれば（macOSとLinuxのjob）、どれかで落ちれば落ちた、どれでも落ちずどれかで通れば通った。
- **testの名前が取れない失敗**: 赤の実行で、JUnitが`missing`か、落ちたtestが1つも無い落ちたjobは、`job:<job名>/step:<step名>`（落ちたstepごと）を一覧の項目にする。

## 一覧

一覧はqueueのeventから求めるビューで、正本は`ci_checked`の`added` / `removed`の積み重ね（表を足さない）。

- **足す**: 赤の実行で落ちたtest（とtestの名前が取れない失敗の項目）のうち、一覧に無いもの。
- **外す**: 後の実行のJUnitでそのtestが通ったとき（`junit: missing`の実行と、そのtestを流していない実行では外さない）。緑の実行では、一覧の全ての項目を外す。jobとstepの項目は緑の実行でだけ外す。外したことは`removed`に`reason`（`passed` / `green`）付きで記録する。
- 項目は、足した実行（`run_id`・`sha`・`url`・`added_at`）と、紐づいたfindingの`finding_id`を持つ。

## eventの種類と欄

どれもqueueのevent（taskにもrunにも付かない）。`run_events`はkindのCHECKを持たないので（migration 0050、ADR-t876-1）migrationは要らず、kindは`domain::event_kind`に足す（kindの追加は互換、[ADR-0073](../../adr/0073-kind-additions-are-compatible.md)）。共通の欄は`supervisor`（token）と`workflow`・`branch`。

| kind | いつ | payloadの欄 |
| --- | --- | --- |
| `ci_checked` | 成否の決まった実行を1件処理したとき（間隔ごとの空振りでは書かない） | `run_id`（GitHubの`databaseId`）・`run_number`・`sha`・`url`・`created_at`・`conclusion`・`state`（`green` / `red`）・`skipped_runs`（この実行より前に飛ばした実行の`run_id`の配列）・`junit`（`read` / `missing` / `not_configured`）・`failed_tests`（名前の配列）・`failed_jobs`（`{job, steps[]}`の配列）・`added`（一覧に足した項目の名前の配列）・`removed`（`{name, reason}`の配列）・`known_failures`（処理の後の一覧の件数） |
| `ci_turned_red` | 直前の`state`が`green`か無いときに赤の実行を処理したとき | `run_id`・`sha`・`url`・`last_green`（`{run_id, sha, url}`か、記録が無ければnull）・`finding_ids`（このとき記録・更新したfinding） |
| `ci_turned_green` | 直前の`state`が`red`のときに緑の実行を処理したとき | `run_id`・`sha`・`url`・`red_since`（最初の赤の`{run_id, sha, url}`）・`red_secs`（最初の赤の`created_at`からこの実行の`created_at`まで） |
| `ci_watch_unavailable` | 読む手段が無い状態に変わったとき（queueの最新と答えが変わったときだけ） | `reason`（`gh_missing` / `gh_unauthenticated` / `not_github`）・`program`（`gh`の値）・`path`（探したPATH）・`message` |
| `ci_watch_available` | 読む手段が戻ったとき | `program`・`resolved`（解決したpath） |

通信の失敗・`gh`の非0の終了（認証以外）・時間切れは`ci_watch_unavailable`にせず、logにwarnを出して次の間隔でやり直す。同じ理由で3回（`CI_WATCH_FAILURE_LIMIT`）続いたら`ci_check_failed`（`error`・`failures`・`since`）を1回記録する（attentionにしない。observerが読む）。

## 読む手段が無いとき

[Run environment](run-environment.md)の「`[run.env]`が名指すプログラムの検査」（[ADR-0049](../../adr/0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)決定9）と同じ形にする（ADR-t1920-1決定2）。

- **`up`のpreflight**: `[ci_watch]`があれば、`[run.env]`の検査の後に`up`のPATHで上の「読む手段の確かめ」を行い、だめならsupervisorを起動せず、理由と対処（`gh`を入れる・`gh auth login`・`dagq.toml`から`[ci_watch]`を外すtaskを登録する）を挙げたerrorで止まる。
- **supervisor**: 確かめるたびに答えを見て、だめな間は見張りを止め、各fill passのclaimをせず、reviewを通ったrunは着地slotを取らずに`awaiting_integration`で待たせ（`Phase::AwaitingSlot`。着地の列のrunの`start_approved_landings`も待つ）、drain・handoff・claimの停止の最中ならleaseを返す。どれも`[run.env]`のプログラムが見つからないとき（「着地の保留」）と同じで、走っているrun・review・triage・始まった着地・始まったresumeは止めず、新しいresumeは始めない。一覧はそのまま（最後に記録した状態）で読まれ続ける。戻れば`ci_watch_available`を記録して次のpassからclaimと着地を再開する。
- **attention**: `ci_watch_unavailable`はinbox宛てのattentionで（`domain::event_attention`の`ATTENTION_KINDS`に足す。[`events` / `watch`](events-watch.md)）、`watch`はこのeventで起き、`ci_watch_available`では起きない。`next`は`reason`が`gh_missing`なら`install tool`、`gh_unauthenticated`なら`log in to gh`、`not_github`なら`fix dagq.toml`（後の2つは`AttentionNext`に足す）。`status`は最新が`ci_watch_unavailable`の間`kind: ci_watch_unavailable`、`status: unavailable`、その`next`、`last_error`に`message`を出す（`run_id` / `task_id`はnull。`run_env_program_missing`の`status: missing`の行と同じ形、[`status`](status.md)）。`ci_watch_available`で消える。
- **`doctor`**: `ci_watch`の欄に`config`（読んだ`[ci_watch]`か、無ければ欄ごと出さない）・`gh`（`resolved`か null）・`authenticated`（bool）・`repo`・`supervisor_last`（最後の`ci_watch_unavailable` / `ci_watch_available`）を出す。

## finding（修正taskの材料）

赤の実行の`added`が空でなければ、そのとき1件のfindingを`record_finding`（observerの`finding record`と同じstoreの関数。CLIの認可を通らないruntimeの書き込み）で記録する。

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
  - `range`: `{from, to, commits}`。`from`は`last_green`の`sha`（その実行より前で最後に緑だった実行。queueに無ければnull）、`to`はこの実行の`sha`、`commits`は`git rev-list --count from..to`（`from`がnullならnull）。cancelで飛ばした実行の分も含む
  - `url`: 実行のURL
  - `binary_contains`: supervisorのbuild識別子が名乗るcommit（`build_id::named_commit`、[Build identifier](build-identifier.md)）を`binary_commit`に書き、`to`がその祖先なら`all`、`from`がnullでなく、名乗るcommitが`from`か`from`の祖先なら`none`、名乗るcommitが`from..to`の中なら`some`、名乗るcommitが無い（リリース・`+unknown`）か判定できなければ`unknown`
- **plannerへの載せ方**: `finding_planner_prompt`は`kind`が`ci_failure`のとき、`detail`の各項をtaskの`description`に写すこと、`dagq search`で同じtestを直すtaskが既にあればtaskを作らず`finding dismiss <id> --covered-by <task> --reason '...'`にすること（`--covered-by`はfindingの`covered_by_task`に書き、`finding_status_changed`に`covered_by_task`を載せる。`ci_failure`のfindingにだけ受け付け、taskは閉じていないものに限る）、`--priority`は`normal`（ADR-0051決定26）を付けることを指示する節を足す。plan reviewの規則は変えない（`high`以上は今までどおり`normal`に下がる）。
- **閉じ方**: findingのtestが全部一覧から外れたら（`removed`）、findingが`open`で閉じていないruntimeのplannerが無ければsupervisorが`resolved`にする（理由`its tests passed on <sha>`、`finding_status_changed`の`by: runtime`）。plannerが立っていれば閉じず、plannerの決定（proposalか`dismiss`）に任せ、promptに載らない後からの緑は次のpassの`ci failures`で読める。`proposed`のfindingは今までどおりproposalの終わり（`settle_findings`）で決まる。

## 一覧の読み方

- **CLI**: `dagq ci failures [--task ID]`（読み取り専用。queueを開く権限があればどのroleでも打てる。worker・jobは打たない）。出力:

  ```json
  {
    "enabled": true,
    "workflow": "ci.yml",
    "branch": "main",
    "state": "red",
    "watch": "available",
    "checked_at": "2026-10-06T12:00:00Z",
    "latest_run": {"run_id": 123, "sha": "…", "url": "…", "conclusion": "failure"},
    "failures": [
      {"name": "dagq::it runtime_claim::claims_in_order", "kind": "test",
       "added": {"run_id": 120, "sha": "…", "url": "…", "at": "…"},
       "finding_id": 45}
    ],
    "kept_for_task": []
  }
  ```

  `state`は`green` / `red` / `unknown`（記録が無い）、`watch`は`available` / `unavailable` / `disabled`。`kind`は`test`か`job_step`。`--task ID`を付けると、そのtaskが`ci_failure`のfindingに紐づいたproposalのtaskなら、そのfindingの項目を`failures`から除き`kept_for_task`に移す（下の「修正taskのrun」）。`[ci_watch]`が無く記録も無ければ`{"enabled": false, "state": "unknown", "watch": "disabled", "failures": []}`。
- **`status`**: 最上位の`ci`の欄に`{state, watch, failures: <件数>, checked_at, latest_run_url}`。見張りの記録が無ければnull。
- **runtimeの中の読み手**（同じ`application::ci_watch::known_failures(task)`を使う）: 着地の検証（goal 157の段3b）は、runのrun dirに`ci-known-failures.json`（上のCLIの出力と同じ形）を書き、検証コマンドのenvに`DAGQ_CI_KNOWN_FAILURES=<そのpath>`を足す（`DAGQ_`はruntimeの予約なので`[run.env]`と衝突しない）。どのtestを外すかの適用は`dagq.toml`の検証コマンドとrepositoryのscriptが受け持つ。workerのprompt（claimとresume）とrunのreviewの材料（段5）は`failures`の名前と`added.url`を載せる。

## 修正taskのrun

runのtaskが属するproposal（`tasks.proposal_id`）を`findings.proposal_id`に持つか、そのtaskを`covered_by_task`に持つ`kind = ci_failure`のfindingがあれば、そのrunは修正taskのrunで、そのfindingの項目を一覧から除いて渡す（他のfindingの項目は外す対象のまま）。taskに印や欄は足さない。

## 保存（findingsの列）

eventの種類にはmigrationが要らないが（上の「eventの種類と欄」）、`--covered-by`はfindingsの表に列を足すmigrationが要る。

- **列**: `findings.covered_by_task INTEGER REFERENCES tasks(id)`（NULL可、既定NULL）。新しいmigration（番号は実装のtaskが[migrationの規則](../../development/migrations.md)の「番号」で決める。列の追加だけなので互換のmigration）で`ALTER TABLE findings ADD COLUMN`する。CHECKは足さない（ADR-t876-1。値の規則は`domain::finding`が持つ: `kind`が`ci_failure`のときだけ、`dismissed`への遷移と一緒にだけ書ける）。
- **書き手**: `finding dismiss <id> --covered-by <task>`（`set_finding_status`が同じトランザクションで列と`finding_status_changed`を書く）。`finding_status_changed`のpayloadに`covered_by_task`（task IDか、無ければ欄を書かない）を足す。`findings`と`findings ID --full`の各行に`covered_by_task`（null可）を出す。
- **読み手**: 下の「修正taskのrun」と、[Finding planners](finding-planners.md)の`settle_findings`（変えない。`dismissed`は対象外）。
- 実装のtaskは同じ変更で[persistence](../persistence.md)の「findings」の列と[domain-model](../domain-model.md)の`finding dismiss`の構文を直す（下の「実装で直す文書」）。

## 実装で直す文書

この文書が足す名前と、実装のtaskが同じ変更で直す文書。今の文書には「予定」の注記だけを置き、振る舞いの本文は実装のときに書く。

| 名前 | 直す文書 |
| --- | --- |
| `[ci_watch]` | [Run environment](run-environment.md)（予定の項は書いた）、[`supervise`](supervise.md)（passごとの読み直しとclaim・着地の保留） |
| `dagq ci failures`（新しい読むコマンド） | [domain-model](../domain-model.md)の「Current operations」、[Authorization](../authorization.md)の「Policy」と「ほかのコマンド」の表（読み取りのcapability。workerとjobには許さない）、[Queue service](../queue-service.md)の読み取りのユースケース（serviceに載せるならその行。載せなければ「まだ無いもの」）、pluginのdagqの`reference/inspect.md`と`reference/authority.md`（documents.mdの「権限の表を写す文書」） |
| `finding dismiss --covered-by`・`covered_by_task` | [domain-model](../domain-model.md)の`finding dismiss`、[Queue service](../queue-service.md)の`finding_dismiss`（引数に`covered_by`を足す。API versionの互換は同文書の「API versionと互換」に従う）、[Authorization](../authorization.md)（capabilityは`finding.dismiss`のままで、表は変えない）、[persistence](../persistence.md)の「findings」、pluginの`dagq-planner`の`SKILL.md`と`reference/register.md`の`finding dismiss`の書き方 |
| `status`の`ci`、attention `ci_watch_unavailable`と`next`の値 | [`status`](status.md)、[`events` / `watch`](events-watch.md)、pluginの`dagq-inbox`の`reference/status.md`（`next`の手当て: `log in to gh`は人が`gh auth login`を打つ） |
| `doctor`の`ci_watch` | [`doctor`](doctor.md)、pluginの`dagq-recover`の`reference/doctor.md` |
| `up`のpreflightの`gh` | [`up` / `down`](up-down.md)、pluginの`dagq-recover`の`reference/up-down.md` |
| eventの種類（`ci_checked`ほか） | `domain::event_kind`だけ（migrationは要らない） |
| `DAGQ_CI_KNOWN_FAILURES` | [integrate](integrate.md)の検証コマンドのenv、[Run environment](run-environment.md)の`DAGQ_`の予約の記述 |

## 既存の仕組みとの関係

- [CI failure issues](../ci-failure-issues.md)のissueは残る（ADR-t1920-1決定8）。
- 自動更新（[Auto-update](auto-update.md)）は一覧もCIの結果も見ない。
- observerはstatsではなくeventとfindingで読む（`ci_check_failed`と`ci_failure`のfindingの滞留）。

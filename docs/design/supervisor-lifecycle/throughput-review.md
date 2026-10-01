---
id: design-supervisor-lifecycle-throughput-review
type: design
title: "スループットの見直し（`throughput-review`）"
status: current
created: 2026-09-29
updated: 2026-10-02
last_verified: 2026-10-02
scope: runtime
related:
  - design-supervisor-lifecycle
  - adr-t996-1
  - adr-0047
  - adr-0051
  - design-supervisor-lifecycle-observer
  - design-supervisor-lifecycle-report
  - design-supervisor-lifecycle-kpi
  - design-supervisor-lifecycle-events-watch
---

# スループットの見直し（`throughput-review`）

[ADR-t996-1](../../adr/2026-09-29-t996-1-supervisor-runs-throughput-review-jobs-and-reports-to-inbox.md)の実装。supervisorがtimerで毎時・日次・週次のスループットの見直しを始め、結論をinboxに知らせるだけのattentionで届ける。手順はpluginのdagq skillの`reference/kpi.md`の「Raising throughput: the weekly review」をそのまま使う（binaryが`include_str!`で持ち、promptはその節を写すだけで手順を二重に持たない。crateの`include`にこのファイルを足した）。

## 起動（supervisor）

- `supervise --throughput-review <bool>`（既定`true`、`--once`では`false`）。`up`が起動するsupervisorは既定で有効で、止めるには`supervise --throughput-review false`。`SuperviseOptions::throughput_review`と、時間帯の`utc_offset`（既定はhostの`clock::local_utc_offset`）
- 各passで`throughput_review_pass`（`src/application/supervise/throughput_review.rs`）が、走っているものを回収し、無ければ期限の来たものを1つ子processで始める。observerと同じく`--parallel`の枠を使わず、1度に1つで、claimを止めたsupervisor・停止中・queueのhold（loginとusageの壁）のあいだは始めない
- 期限: 時（直前の確定した1時間。期間が最も早く過ぎる）→日（前日）→週（前のISO週）の順に、`domain::throughput_review::window`がhostの時間帯で求めた期間のラベル（`2026-W39`・`2026-09-28`・`2026-09-29T13`）に`throughput_review_finished`（直近`HISTORY_EVENTS`=400件から、`mode`と`period`が一致するもの。outcomeは問わない）が無く、同じ期間の`throughput_review_started`が35分（`RUNNING_MS`。jobの時間の上限と余裕）以内に無ければ期限（`domain::throughput_review::reviewed`・`running`）。失敗した期間はやり直さない。どのsupervisorが記録したものでもよい。記録の前に死んだ子processを毎passで起こし直さないように、このprocessが始めた`(mode, period)`も覚える
- コマンドは`dagq --db <db> throughput-review --mode <mode> --at <判定したunix秒> --utc-offset <秒> --claude <claude>`。envはobserverと同じくsupervisorのactor（`supervisor:<pid>`）で、agentのroleはコマンドが付ける。supervisorが止まるときは子processとその子孫をkillし（`stop_throughput_review`）、記録の無い期間は始まりから35分を過ぎたら次のsupervisorが始め直す。execの引き継ぎではkillせず、jobは新しいprocessの下で続いて自分の終わりを記録し、その始まりの記録が次のprocessに同じ期間を始めさせない（自動更新の引き継ぎは1時間に何度も来うるので、長い週次のjobが殺されて始め直し続けないように）。jobはexecの後も同じpidのsupervisorの子なので、`throughput_review_finished`に自分の`pid`と`parent_pid`を書き、execで引き継いだsupervisor（`handoff_token`あり）は最初のpassから`RUNNING_MS`のあいだ、直近10件の終わりのうち`parent_pid`が自分で最初のpassの60秒前より後のもの（`domain::throughput_review::children_finished`）の`pid`を`ProcessControl::reap`で1度だけ回収する（zombieはpidを持ち続けるので他のprocessを回収しない。自分が始めたjobは自分で待つので除く）。`--once`は走っているものを待つ

## 毎時の規則（`domain::throughput_review`）

コマンドが`run_integrated`を直前の確定した1時間までの26時間ぶん1時間ごとに数え（`bucket_counts`）、`judge_hourly`が判定する。当たったときだけagentを起動し、当たらなければ`throughput_review_finished`（`outcome: skipped`、`hourly`に判定）だけを書く。閾値は2026-09-28に人が決めた初期値。

| 規則 | 理由の名前 | 値 |
|---|---|---|
| その1時間の着地数が、その前の6時間（`BASELINE_HOURS`）の平均から、平均の50%（`DEVIATION_RATIO`）以上かつ3件（`DEVIATION_MIN`）以上ずれた | `deviation` | 平均が0なら3件以上で当たる |
| 3時間（`SHORT_HOURS`）の平均が24時間（`LONG_HOURS`）の平均を30%（`DROP_RATIO`）以上下回る状態が、その1時間で終わる3時間（`DROP_HOURS`）続いた | `sustained_drop` | 24時間の平均が0なら当たらない |
| その1時間に着地が無い | `no_landing` | |

どれか1つに当たった1時間ごとに起動する（`triggered`。ADR-t996-1の決定2のとおりで、続く状態も当たる1時間ごとに見直す）。着地が0の時間が続けば毎時起動し、1日じゅう何も着地しないqueueでも毎時起動する。続く状態を始まりの1時間だけにする抑えは、人の決めた規則を狭めるので入れていない（ask 198で差し戻された）。

## コマンド（`throughput-review`、`src/throughput_review.rs`）

1. 期間を`--at`（無ければqueueの時計の今）と`--utc-offset`（無ければhostの時間帯）で決め、着地を読む。毎時は判定し、当たらなければ上のとおりskippedで終わる（`--dry-run`は判定に関わらずpromptを返す）
2. 入力（`input.json`）: `period`、`landings`（期間の合計、前の同じ長さの期間の合計、毎時は判定と同じ26時間・日次は24時間の`by_hour`・週次は7日の`by_day`、期間の`run_integrated`の最大200件）、`hourly`（毎時の判定）、`kpi`（毎時は日の2期間、日次は日の8期間、週次は週の5期間。`at`は期間の終わりの直前）、`stats`（毎時は直前6時間、日次・週次は期間）、`claim_deferred`（`stats`と同じ範囲の`claim_deferred`の`reason`ごとの件数）、`asks`（同じ範囲で開いたaskの`kind`ごとの件数と、今開いているaskの`kind`ごとの件数）、`timelines`（期間に着地したrunのうち最初のeventから着地までが長い3件の`timeline`、gapは300秒以上）。読めない部分は`{"error": ...}`。promptにはこの全体ではなく、下の「promptの入力」の要約だけを載せる
3. `<queue dir>/reports/reviews/<mode>-<period>/`（あれば`-1`…を付ける）を作り、`prompt.md`・`input.json`（入力の全体）を書き、`throughput_review_started`（`mode`・`period`・`reasons`・`dir`・`session_id`・`launch`。`launch`は`provider`を含む。[Actor model](actor-model.md)）を記録する
4. agentはactor executorの`HeadlessProgram::Job`（権限の意図`ACCESS`は`queue_cli`で、Claude Codeは`--allowedTools Bash(dagq:*)`に訳す。MCPを読まない。[Agent provider lifecycle](../provider-lifecycle.md#headless-jobのinterface)）で`DAGQ_ROLE=throughput-review-job`・`DAGQ_ACTOR_ID=throughput-review-job:<mode>:<period>`として起動し、stdoutを`output.out`、stderrを`output.err`に分けて書く（`Streams::Files`。結果はproviderの`job_reply`が`output.out`だけから取り出した返答から読み、stderrは混ぜない）。時間の上限は`--timeout`（既定1800秒）で、過ぎたら子孫ごとkillする。model / effortは`dagq.toml`の`[roles.throughput_review]`（無ければproviderの既定）
5. 成功したら`output.out`に`job_reply`を当てた返答を`parse_output`で読む: `## Conclusion`の見出しの下の行（無ければ先頭の行）を最大5行（`MAX_CONCLUSION_LINES`）の結論にし、`next_move`でfenceしたJSON（`summary`・`detail`・`why`）を次の一手として外した残りを全文として`review.md`に、結論・次の一手・読めなかった理由（`next_move_error`）・findingのIDを`review.json`に書く
6. 週次だけ、次の一手をfindingにする: kind `throughput`、対象queue、subject `weekly/<period>`、summaryは`summary`、detailは`detail`と`Why:`、根拠は`throughput_review_started`のevent、`propose`（proposalを求める印）の理由は`why`、記録者は`supervisor`。runtimeのplannerの既存の経路（[Finding planners](finding-planners.md)）でproposalになり、`[kpi] max_improvement_proposals`に従う。毎時・日次の出力にblockがあっても記録しない
7. `throughput_review_reported`（`mode`・`period`・`reasons`・`conclusion`・`path`（`review.md`）・`dir`・`finding_id`・`next_move_error`）を記録する。これがinbox宛ての知らせるだけのattention（`report the review`）
8. `throughput_review_finished`（`mode`・`period`・`outcome`（`succeeded` / `failed`（非0終了）/ `error`（起動できない・時間切れ・保存や記録の失敗））・`exit_code`・`error`・`reasons`・`dir`・`session_id`（startと同じ。区間を閉じる鍵、task 1086。skippedには無い）・`duration_secs`、成功なら`reported_event_id`・`finding_id`・`path`）を記録する。`outcome`が`error`か`failed`のもの（modeを問わない）はinbox宛ての知らせるだけのattention（`check the failed review`、task 1099）になり、askにはならず、claimと着地を止めない。失敗した期間はやり直さない（上の「期限」）。skippedと`succeeded`はattentionではない。attentionなので、KPIの`attentions_per_landing`（queueのeventのattentionも数える）にも加わる

手順8のfinishは、期間が決まった後の処理の出口で1回だけ記録する（task 1111）。agentの起動前の着地の読み取り・入力の収集・checkoutの解決・dirの作成・promptと入力の保存・startedの記録の失敗も、`outcome: error`・`exit_code: null`・原因を含む失敗の文（`{:#}`）の`error`を持つpayloadとして記録して返す。分かれば`dir`・`session_id`も残し、`pid`・`parent_pid`は常に残す。dirの作成前なら`dir`はnullで、ログもまだ無い。queueを開く前・期間が決まる前（DBのcanonicalize・open）の失敗は、記録先や期間が無いので対象外でErrを返す。finish自体を記録できないときもErrを返し、記録を再試行しない。supervisorは子の非0終了からfinishを補わない。`--dry-run`は失敗時もeventを記録しない。

過去の見直しの`output.log`はそのまま残す。runtimeは完了した見直しの出力を再parseせず、保存した`review.md` / `review.json`とeventを読むため、移行や古い名前へのfallbackは要らない。

## sessionの区間（task 1086）

jobのsessionは、他のheadlessのjob（review・triage・plan review・goal review・observer）と同じく区間（`session_opened` / `session_closed`、[Actor model](actor-model.md)・[stats](stats.md)）を持つ。区間はobserverと同じqueueのevent（task・goal・runを持たない）で、kindは`throughput_review`（`domain::sessions::THROUGHPUT_REVIEW`）。queueのeventの区間はjobごとのkindで探し（`domain::sessions::queue_span_kind`）、observerのeventはobserverの区間だけを、見直しのeventは見直しの区間だけを開け閉めする（observerの扱いは変えていない）。

- 開く: `throughput_review_started`が開く。payloadは`kind`・`session_id`（runtimeがjobに渡したもの）・`cwd`（見直しのdir。Claude Codeのtranscriptはこのcwdとsession idで探す）・`attempt`（null）・`launch`・`mode`・`period`
- 閉じる（`job_finished`）: `session_id`の一致する`throughput_review_finished`が閉じる。閉じるときtranscriptの実際のmodel / effort・turn・active time・tokensが他のjobと同じ経路（task 579、`read_before`で書き込みの前に読む）で`session_closed`に入る
- 重なり: 見直しは同時に複数走りうる。execの引き継ぎではjobを殺さないので、前のprocessが残したjob（例: 週次）が走るあいだに次のprocessが別の期間（例: 毎時）を始め、supervisorが複数あれば別のsupervisorのjobも走る（同じ期間は`running`が35分のあいだ始めさせない）。そのため始まりは他の区間を閉じず、終わりは自分の`session_id`の区間だけを閉じる
- 終わりの無い区間（jobが死んだ・supervisorの停止で殺された・記録の前に落ちた）は`inferred`で閉じる。規則: (1) 同じ`mode`と`period`の`throughput_review_started`が来たら、前の区間を閉じる（`RUNNING_MS`を過ぎて始め直されたので、前のjobは終わっている）。(2) 見直しのevent（`throughput_review_started`・skippedを含む`throughput_review_finished`）が記録されたとき、開いてから`RUNNING_MS`（35分。jobの時間の上限`--timeout`の既定1800秒と余裕。`domain::sessions::THROUGHPUT_REVIEW_OPEN_MS`）以上たった区間を閉じる。その時までにjobは終わったか時間切れで殺されている。毎時の判定はskippedでも`throughput_review_finished`を書くので、supervisorが動いていれば残った区間はおおむね1時間半以内に閉じる。`inferred`の区間はtranscriptの最後の記録で終わる（ADR-0048 決定 7）。supervisorが居ないあいだは閉じない
- `stats`の`sessions.by_kind`に`throughput_review`が出る

## promptの入力（task 1099）

Claudeのheadlessのjobは、promptを`claude -p`の位置引数で受ける（[Agent provider lifecycle](../provider-lifecycle.md#headless-jobのinterface)）。引数とenvの合計はhostの`ARG_MAX`（macOSで1MiB）を超えられないが、日次・週次の入力の全体は数MB（2026-09-28の日次で2.75MB、2026-W39の週次で1.27MB。大きいのは`kpi.periods`の各期間の全KPIの全層・比較・marks、`stats`の`runs`・`versions`・`goals`、`timelines`）で、起動が`Argument list too long (os error 7)`で失敗していた。stdinで渡してもこの大きさはagentの文脈に収まらないので、promptに載せる入力を要約して上限を設ける（providerのinterfaceは変えない）。

- 上限: prompt全体が`PROMPT_LIMIT`（128KiB）以下、そのうち入力（pretty JSON）が`PROMPT_INPUT_LIMIT`（96KiB）以下。残りは指示・手順・言語の行。`ARG_MAX`の1/8で、envを足しても当たらない。2026-09-28の日次の入力は要約で約62KB、2026-W39の週次は約70KBで、どれも落とさずに収まる
- 要約（`prompt_input`）: `period`、`landings`（`events`を除く）、`hourly`、`kpi`（`cores`、`periods`は各期間の`label`・`partial`・`runs`だけ、`latest`は見直す期間（最後の期間）の各KPIの`all`層と、その`comparison`の`all`層（`judged`・`delta`を除く）と`unavailable`、`targets`は各目標から`periods`を除き、最後の期間の判定を`latest`に置いたもの）、`stats`（`parts`に`overall`・`landing_utilization`・`waiting`・`claim_deferrals`・`claim_holds`・`landing_holds`・`escalations`・`backend_failures`・`verification_failures`・`provider_switches`（`PROMPT_STATS`）、`omitted`に残りのkeyの名前）、`claim_deferred`、`asks`、`timelines`（`run_id`と`secs`だけ）。読めなかった部分の`{"error": ...}`はそのまま載せる。kpi.mdの手順のCadenceが日次（外れ値と目標割れ）・週次（手順1〜5）で見る数値は`latest`と`targets`と`stats.parts`にあり、層ごとの値・過去の期間の値・runごとの値・timelineはjobが読むコマンドで取りに行く
- 切り詰め: 要約がなお`PROMPT_INPUT_LIMIT`を超えるときは、`stats` → `kpi.latest` → `timelines` → `kpi.targets` → `kpi.periods` → `asks` → `claim_deferred` → `landings` → `kpi`（形の分からない`kpi`は丸ごと載るので最後）の順（`DROP_ORDER`。大きく、コマンドで読み直しやすいものから）に、収まるまで丸ごと落とし、落としたものを`omitted_to_fit`に名前で残す。`period`と`hourly`は落とさない
- promptは要約だと明かし、入力の全体が見直しのdirの`input.json`にあること（人が読むためのもの）と、細部は`kpi`（他の期間・層・host）・`stats --since <入力のstatsの始め（毎時は期間の終わりの6時間前、日次・週次は期間の始め）> --until <期間の終わり> --full`（run）・`timeline RUN`（長いrun）・`events --full`（着地は`--kind run_integrated`）で取りに行くことを指示する。dry runはdirを作らず、作るはずのdirの`input.json`を名指す
- jobの権限（`ACCESS`の`queue_cli`）は広げない。`input.json`を読ませるにはClaude Codeの`Read`を許すことになり、queueの外のファイルも読めるようになる。要約から外したものはどれも読むコマンドで同じものが得られるので、ファイルを読む必要はない

## 権限

`throughput-review-job`は`review-job`などと同じheadlessのjob（`ActorRole::is_headless_job`）で、policyは`QueueRead`だけ（[Authorization](../authorization.md)）。`kpi`・`stats`・`timeline`・`events`などの読むコマンドは打て、note・mark・finding・ask・task・goalなど状態を変えるコマンドはCLIが`reviewer may not change queue state`で拒み、`authorization_denied`に残る。`throughput-review`コマンド自体はobserverの`observe`と同じ`observe.run`（supervisorと人とinbox）で、Claudeのsettingsのdenyにも入る。

## event

`throughput_review_started` / `throughput_review_finished` / `throughput_review_reported`はqueueのevent（task・goal・runを持たない）。attentionは`throughput_review_reported`（`report the review`）と、`outcome`が`error` / `failed`の`throughput_review_finished`（`check the failed review`。task 1099）の2つで、どちらも知らせるだけ（[Events and watch](events-watch.md)）。`events`・`watch`のcompact形は`throughput_review_reported`に`mode`・`period`・`reasons`・`conclusion`・`path`・`finding_id`を、`throughput_review_finished`に`mode`・`period`・`outcome`・`dir`（`output.out` / `output.err`のあるdir）と`exit_code`・`reason`（payloadの`error`）を載せる。

## test

- `src/domain/throughput_review.rs`: 期間とラベル（日本時間の時・日・ISO週、0時の時）、bucketの数え方、平常の時間・50%と3件の両方が要ること・3時間続く低下と着地の無い時間は続くあいだ毎時起動すること、execで引き継いだ子の選び方（`children_finished`）・判定の入力の長さ、出力の結論（見出しあり・なし、5行まで）と次の一手（読めない・summaryが空・閉じていないblockは本文に残す）、期間の記録の有無と35分以内の始まり
- `src/throughput_review.rs`: `reference/kpi.md`の節だけを写すこと、頻度ごとのpromptと週次だけが次の一手を求めること、promptが`input.json`の場所と読むコマンド（`kpi`・`stats`・`timeline`・`events`）で細部を取りに行く指示を持つこと、MB級の入力の要約が上限に収まり見直しに要る部分を残すこと、上限を超える要約が`DROP_ORDER`の順に落として`omitted_to_fit`に名を残すこと
- `src/domain/mod.rs`: `throughput_review_finished`は`error` / `failed`だけが`check the failed review`で、skippedと`succeeded`はattentionでないこと
- `tests/it/runtime_throughput_review.rs`: 規則に当たらない時間はagentを起動せずskippedだけを書くこと（dry runはpromptを返す）、当たった時間の保存（`review.md`・`review.json`・`input.json`）とroleとMCPなしと、jobのnote・finding・mark・readyが拒まれることと`kpi`は読めること、inboxの`events`（`watch`と同じ判定）に`report the review`と結論が載ること、週次の次の一手がproposalを求めるfindingになること、`failed`（非0終了）と`error`（起動できない）の失敗がinboxの`events`に`check the failed review`として載りaskを開かないこと、起動前にreports/reviewsを作れない失敗もerrorを1件だけ記録してinboxに届きaskを開かず、同じfixtureのdry runは記録しないこと、起動後の成功・failed・errorもfinishが1件だけであること、MB級の入力（8日分のmarks）の日次と週次がagent（promptを引数で受けるstub）を起動して`succeeded`になりpromptが`PROMPT_LIMIT`以下であること、supervisorが時・日・週を1度ずつ始めて同じ期間を2度始めないことと`throughput_review: false`で始めないこと、失敗するjobがclaimと着地を止めずattentionだけを残すこと
- `src/domain/sessions.rs`: 見直しの区間が始まりで開き、自分の`session_id`の終わりで閉じ、別の期間の見直しとobserverの区間を閉じないこと、同じ期間の始め直しと`RUNNING_MS`を過ぎた区間を`inferred`で閉じること（`throughput_review_spans_close_by_session_period_or_age`）
- `src/infrastructure/sessions.rs`: 区間がqueueのeventで開き、終わりでtranscriptのmodel / effortを`read_before`で読んで閉じること、並ぶ見直しが開いたまま残り、時間を過ぎた区間がskippedの終わりで`inferred`としてtranscriptの最後で閉じること、observerの区間は自分の終わりでだけ閉じること（`a_throughput_review_span_records_its_launch_and_the_model_of_its_transcript`）
- `tests/it/runtime_throughput_review.rs`の当たった時間のtest: 区間が開いて`job_finished`で閉じ、`stats`の`sessions.by_kind.throughput_review`に数えられること
- `tests/it/cli_*.rs`のroleの一覧に`throughput-review-job`を足し、状態を変えるコマンドが拒まれることを確かめる

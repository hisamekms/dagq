---
id: design-supervisor-lifecycle-throughput-review
type: design
title: "スループットの見直し（`throughput-review`）"
status: current
created: 2026-09-29
updated: 2026-10-04 # task 1615: the command moved into application, infrastructure and compose
last_verified: 2026-10-04 # task 1615
scope: runtime
related:
  - adr-t1566-1
  - design-supervisor-lifecycle-prompt
  - design-supervisor-lifecycle
  - adr-t996-1
  - adr-0047
  - adr-0051
  - design-supervisor-lifecycle-observer
  - design-supervisor-lifecycle-report
  - design-supervisor-lifecycle-kpi
  - design-supervisor-lifecycle-events-watch
  - design-supervisor-lifecycle-actor-model
  - adr-t1063-1
  - adr-t1204-1
---

# スループットの見直し（`throughput-review`）

[ADR-t996-1](../../adr/2026-09-29-t996-1-supervisor-runs-throughput-review-jobs-and-reports-to-inbox.md)の実装。supervisorがtimerで毎時・日次・週次のスループットの見直しを始め、結論をinboxに知らせるだけのattentionで届ける。手順はpluginのdagq skillの`reference/kpi.md`の「Raising throughput: the weekly review」をそのまま使う（binaryが`include_str!`で持ち、promptはその節を写すだけで手順を二重に持たない。crateの`include`にこのファイルを足した）。

## 起動（supervisor）

- `supervise --throughput-review <bool>`（既定`true`、`--once`では`false`）。`up`が起動するsupervisorは既定で有効で、止めるには`supervise --throughput-review false`。`SuperviseOptions::throughput_review`と、時間帯の`utc_offset`（既定はhostの`clock::local_utc_offset`）
- 各passで`throughput_review_pass`（`src/application/supervise/throughput_review.rs`）が、走っているものを回収し、無ければ期限の来たものを1つ子processで始める。observerと同じく`--parallel`の枠を使わず、1度に1つで、claimを止めたsupervisor・停止中・queue serviceが動いていないあいだは始めない。どのproviderで始めるか（始めないか）は下の「Codexで動かす」の行き先（`Supervisor::throughput_review_route`）が決め、`[roles.throughput_review]`にproviderの無い見直しは今までどおり、queueのhold（loginとusageの壁）のあいだと`--no-claude`では始めない
- 期限: 時（直前の確定した1時間。期間が最も早く過ぎる）→日（前日）→週（前のISO週）の順に、`domain::throughput_review::window`がhostの時間帯で求めた期間のラベル（`2026-W39`・`2026-09-28`・`2026-09-29T13`）に`throughput_review_finished`（直近`HISTORY_EVENTS`=400件から、`mode`と`period`が一致するもの。outcomeは問わないが、`provider_unusable`を持つものは除く）が無く、同じ期間の`throughput_review_started`が35分（`RUNNING_MS`。jobの時間の上限と余裕）以内に無ければ（同じ`dir`の`throughput_review_finished`があるstartは終わったものとして数えない）期限（`domain::throughput_review::reviewed`・`running`）。失敗した期間はやり直さない（例外はCodexが使えなかった見直しで、下の「Codexで動かす」のとおりもう一方のproviderで始め直す）。どのsupervisorが記録したものでもよい。記録の前に死んだ子processを毎passで起こし直さないように、このprocessが始めた`(mode, period)`も覚える
- コマンドは`dagq --db <db> throughput-review --mode <mode> --at <判定したunix秒> --utc-offset <秒> --claude <claude> --codex <codex> --launch <行き先のlaunchのJSON>`（と、あれば`--codex-home <dir>`（testだけ）・`--switchable`・`--unavailable <理由>`。どれも`--help`に出さない）。envはobserverと同じくsupervisorのactor（`supervisor:<pid>`）で、agentのroleはコマンドが付ける。supervisorが止まるときは子processとその子孫をkillし（`stop_throughput_review`）、記録の無い期間は始まりから35分を過ぎたら次のsupervisorが始め直す。execの引き継ぎではkillせず、jobは新しいprocessの下で続いて自分の終わりを記録し、その始まりの記録が次のprocessに同じ期間を始めさせない（自動更新の引き継ぎは1時間に何度も来うるので、長い週次のjobが殺されて始め直し続けないように）。jobはexecの後も同じpidのsupervisorの子なので、`throughput_review_finished`に自分の`pid`と`parent_pid`を書き、execで引き継いだsupervisor（`handoff_token`あり）は最初のpassから`RUNNING_MS`のあいだ、直近10件の終わりのうち`parent_pid`が自分で最初のpassの60秒前より後のもの（`domain::throughput_review::children_finished`）の`pid`を`ProcessControl::reap`で1度だけ回収する（zombieはpidを持ち続けるので他のprocessを回収しない。自分が始めたjobは自分で待つので除く）。`--once`は走っているものを待つ

## 毎時の規則（`domain::throughput_review`）

コマンドが`run_integrated`を直前の確定した1時間までの26時間ぶん1時間ごとに数え（`bucket_counts`）、`judge_hourly`が判定する。当たったときだけagentを起動し、当たらなければ`throughput_review_finished`（`outcome: skipped`、`hourly`に判定）だけを書く。閾値は2026-09-28に人が決めた初期値。

| 規則 | 理由の名前 | 値 |
|---|---|---|
| その1時間の着地数が、その前の6時間（`BASELINE_HOURS`）の平均から、平均の50%（`DEVIATION_RATIO`）以上かつ3件（`DEVIATION_MIN`）以上ずれた | `deviation` | 平均が0なら3件以上で当たる |
| 3時間（`SHORT_HOURS`）の平均が24時間（`LONG_HOURS`）の平均を30%（`DROP_RATIO`）以上下回る状態が、その1時間で終わる3時間（`DROP_HOURS`）続いた | `sustained_drop` | 24時間の平均が0なら当たらない |
| その1時間に着地が無い | `no_landing` | |

どれか1つに当たった1時間ごとに起動する（`triggered`。ADR-t996-1の決定2のとおりで、続く状態も当たる1時間ごとに見直す）。着地が0の時間が続けば毎時起動し、1日じゅう何も着地しないqueueでも毎時起動する。続く状態を始まりの1時間だけにする抑えは、人の決めた規則を狭めるので入れていない（ask 198で差し戻された）。

## コマンド（`throughput-review`、`src/application/throughput_review.rs`）

本体は`application::throughput_review::review`で、`compose::throughput_review`がDBをcanonicalizeしてqueueを開き、`stats`・KPI・bound checkout・findingの記録（`ThroughputReviewSources`）と、見直しのdirのファイル・`[roles.throughput_review]`・hostの時間帯とprocess id・agentのprocess（`ThroughputReviewHost`、`infrastructure::throughput_review::LocalThroughputReview`）を注入する。`--launch`の無いCLIの起動は`compose::throughput_review_launch`が`[roles.throughput_review]`を読む（task 1615がレイヤーの外の`src/throughput_review.rs`から移した）。

1. 期間を`--at`（無ければqueueの時計の今）と`--utc-offset`（無ければhostの時間帯）で決め、着地を読む。毎時は判定し、当たらなければ上のとおりskippedで終わる（`--dry-run`は判定に関わらずpromptを返す）
2. 入力（`input.json`）: `period`、`landings`（期間の合計、前の同じ長さの期間の合計、毎時は判定と同じ26時間・日次は24時間の`by_hour`・週次は7日の`by_day`、期間の`run_integrated`の最大200件）、`hourly`（毎時の判定）、`health`（workerの経路ごとの健全性とdiskの空きの最小値と中央値。`kpi`の最後の期間（日次は見直す日、週次は週、毎時はその時を含む日のここまで）の`health`（[kpi](kpi.md#期間の健全性)、task 1371）に、その期間の`label`を`period`として足したもの。`kpi`が読めなければその`error`）、`kpi`（毎時は日の2期間、日次は日の8期間、週次は週の5期間。`at`は期間の終わりの直前）、`stats`（毎時は直前6時間、日次・週次は期間）、`claim_deferred`（`stats`と同じ範囲の`claim_deferred`の`reason`ごとの件数）、`asks`（同じ範囲で開いたaskの`kind`ごとの件数と、今開いているaskの`kind`ごとの件数）、`timelines`（期間に着地したrunのうち最初のeventから着地までが長い3件の`timeline`、gapは300秒以上）。読めない部分は`{"error": ...}`。promptにはこの全体ではなく、下の「promptの入力」の要約だけを載せる
3. `<queue dir>/reports/reviews/<mode>-<period>/`（あれば`-1`…を付ける）を作り、`prompt.md`・`input.json`（入力の全体）を書き、`throughput_review_started`（`mode`・`period`・`reasons`・`dir`・`session_id`（Claudeはruntimeが渡すid、Codexはnull）・`launch`。`launch`は`provider`を含む。[Actor model](actor-model.md)。promptのbyte数の`prompt_bytes`・`prompt_limit`・`input_bytes`・`input_limit`・`omitted_to_fit`、下の「promptの入力」のbyte数の記録）を記録する
4. agentは`--launch`（supervisorが行き先として渡したもの。無ければ`[roles.throughput_review]`）の`provider`（既定のClaude、または下の「Codexで動かす」のCodex）で、actor executorの`HeadlessProgram::Job`（権限の意図`ACCESS`は`queue_cli`で、Claude Codeは`--allowedTools Bash(dagq:*)`に、Codexは読み取りだけのsandbox（queue serviceのjobのprofile）に訳す。MCPを読まない。[Agent provider lifecycle](../provider-lifecycle.md#headless-jobのinterface)）で`DAGQ_ROLE=throughput-review-job`・`DAGQ_ACTOR_ID=throughput-review-job:<mode>:<period>`として起動し、stdoutを`output.out`、stderrを`output.err`に分けて書く（`Streams::Files`。結果はproviderの`job_reply`が`output.out`だけから取り出した返答から読み、stderrは混ぜない）。時間の上限は`--timeout`（既定1800秒）で、過ぎたら子孫ごとkillする。model / effortは`dagq.toml`の`[roles.throughput_review]`（無ければproviderの既定。もう一方のproviderに切り替えたときはその既定）
5. 成功したら`output.out`に`job_reply`を当てた返答を`parse_output`で読む: `## Conclusion`の見出しの下の行（無ければ先頭の行）を最大5行（`MAX_CONCLUSION_LINES`）の結論にし、`next_move`でfenceしたJSON（`summary`・`detail`・`why`）を次の一手として外した残りを全文として`review.md`に、結論・次の一手・読めなかった理由（`next_move_error`）・findingのIDを`review.json`に書く
6. 週次だけ、次の一手をfindingにする: kind `throughput`、対象queue、subject `weekly/<period>`、summaryは`summary`、detailは`detail`と`Why:`、根拠は`throughput_review_started`のevent、`propose`（proposalを求める印）の理由は`why`、記録者は`supervisor`。runtimeのplannerの既存の経路（[Finding planners](finding-planners.md)）でproposalになり、`[kpi] max_improvement_proposals`に従う。毎時・日次の出力にblockがあっても記録しない
7. `throughput_review_reported`（`mode`・`period`・`reasons`・`conclusion`・`path`（`review.md`）・`dir`・`finding_id`・`next_move_error`）を記録する。これがinbox宛ての知らせるだけのattention（`report the review`）。`mode`が`hourly`のものは`watch --role inbox`を単独では起こさず、inboxが次に別の件で起きたときにその`events`に古い順に載る（daily / weeklyは今までどおり起こす。[ADR-t1418-1](../../adr/2026-10-03-t1418-1-quiet-notices-do-not-wake-the-inbox-watch.md)、[Events and watch](events-watch.md)）
8. `throughput_review_finished`（`mode`・`period`・`outcome`（`succeeded` / `failed`（非0終了）/ `error`（起動できない・時間切れ・保存や記録の失敗））・`exit_code`・`error`・`reasons`・`dir`・`session_id`（Claudeはstartと同じ。区間を閉じる鍵、task 1086。Codexはthreadのid。skippedには無い）・`duration_secs`、Codexなら`model`・`model_unknown`、Codexが使えなかったなら`provider_unusable`（`provider`・`reason`）、成功なら`reported_event_id`・`finding_id`・`path`、promptを作った後なら`prompt_bytes`・`prompt_limit`・`input_bytes`・`input_limit`・`omitted_to_fit`（下の「promptの入力」のbyte数の記録））を記録する。`outcome`が`error`か`failed`のもの（modeを問わない）はinbox宛ての知らせるだけのattention（`check the failed review`、task 1099）になり、askにはならず、claimと着地を止めない。失敗した期間はやり直さない（上の「期限」）。例外は`provider_unusable`を持つ終わり（Codexが認証・利用上限・起動の失敗・実行ファイルが無いことで使えなかった見直し。下の「Codexで動かす」）で、attentionにならず、supervisorがCodexを控えてその期間をもう一方のproviderで始め直す（`--no-claude`では始め直しが理由付きの`error`の終わりになり、それがattentionになる）。skippedと`succeeded`はattentionではない。attentionなので、KPIの`attentions_per_landing`（queueのeventのattentionも数える）にも加わる

手順8のfinishは、期間が決まった後の処理の出口で1回だけ記録する（task 1111）。agentの起動前の着地の読み取り・入力の収集・checkoutの解決・dirの作成・promptと入力の保存・startedの記録の失敗も、`outcome: error`・`exit_code: null`・原因を含む失敗の文（`{:#}`）の`error`を持つpayloadとして記録して返す。分かれば`dir`・`session_id`も残し、`pid`・`parent_pid`は常に残す。dirの作成前なら`dir`はnullで、ログもまだ無い。queueを開く前・期間が決まる前（DBのcanonicalize・open）の失敗は、記録先や期間が無いので対象外でErrを返す。finish自体を記録できないときもErrを返し、記録を再試行しない。supervisorは子の非0終了からfinishを補わない。`--dry-run`は失敗時もeventを記録しない。

過去の見直しの`output.log`はそのまま残す。runtimeは完了した見直しの出力を再parseせず、保存した`review.md` / `review.json`とeventを読むため、移行や古い名前へのfallbackは要らない。

## sessionの区間（task 1086）

jobのsessionは、他のheadlessのjob（review・triage・plan review・goal review・observer）と同じく区間（`session_opened` / `session_closed`、[Actor model](actor-model.md)・[stats](stats.md)）を持つ。区間はobserverと同じqueueのevent（task・goal・runを持たない）で、kindは`throughput_review`（`domain::sessions::THROUGHPUT_REVIEW`）。queueのeventの区間はjobごとのkindで探し（`domain::sessions::queue_span_kind`）、observerのeventはobserverの区間だけを、見直しのeventは見直しの区間だけを開け閉めする（observerの扱いは変えていない）。

- 開く: `throughput_review_started`が開く。payloadは`kind`・`session_id`（runtimeがjobに渡したもの。Codexはnull）・`cwd`（見直しのdir。Claude Codeのtranscriptはこのcwdとsession idで探す）・`attempt`（null）・`launch`・`mode`・`period`
- 閉じる（`job_finished`）: `session_id`か`dir`（区間の`cwd`）の一致する`throughput_review_finished`が閉じる。Codexの見直しは開くときsession idを持たないので`dir`で閉じ、`session_closed`に終わりのeventの`session_id`（thread）・`model`・`model_unknown`を写す（task 1220）。Claudeの見直しは閉じるときtranscriptの実際のmodel / effort・turn・active time・tokensが他のjobと同じ経路（task 579、`read_before`で書き込みの前に読む）で`session_closed`に入る
- 重なり: 見直しは同時に複数走りうる。execの引き継ぎではjobを殺さないので、前のprocessが残したjob（例: 週次）が走るあいだに次のprocessが別の期間（例: 毎時）を始め、supervisorが複数あれば別のsupervisorのjobも走る（同じ期間は`running`が35分のあいだ始めさせない）。そのため始まりは他の区間を閉じず、終わりは自分の`session_id`の区間だけを閉じる
- 終わりの無い区間（jobが死んだ・supervisorの停止で殺された・記録の前に落ちた）は`inferred`で閉じる。規則: (1) 同じ`mode`と`period`の`throughput_review_started`が来たら、前の区間を閉じる（`RUNNING_MS`を過ぎて始め直されたので、前のjobは終わっている）。(2) 見直しのevent（`throughput_review_started`・skippedを含む`throughput_review_finished`）が記録されたとき、開いてから`RUNNING_MS`（35分。jobの時間の上限`--timeout`の既定1800秒と余裕。`domain::sessions::THROUGHPUT_REVIEW_OPEN_MS`）以上たった区間を閉じる。その時までにjobは終わったか時間切れで殺されている。毎時の判定はskippedでも`throughput_review_finished`を書くので、supervisorが動いていれば残った区間はおおむね1時間半以内に閉じる。`inferred`の区間はtranscriptの最後の記録で終わる（ADR-0048 決定 7）。supervisorが居ないあいだは閉じない
- `stats`の`sessions.by_kind`に`throughput_review`が出る

## promptの入力（task 1099）

task 1099のとき、Claudeのheadlessのjobはpromptを`claude -p`の位置引数で受けていた（Codexのjobも`codex exec`の位置引数。task 1220）。今はどのheadless jobもpromptをstdinで受け、引数の上限には当たらない（task 1560。[Agent provider lifecycle](../provider-lifecycle.md#headless-jobのinterface)の「promptの渡し方」）。引数とenvの合計はhostの`ARG_MAX`（macOSで1MiB）を超えられず、日次・週次の入力の全体は数MB（2026-09-28の日次で2.75MB、2026-W39の週次で1.27MB。大きいのは`kpi.periods`の各期間の全KPIの全層・比較・marks、`stats`の`runs`・`versions`・`goals`、`timelines`）で、起動が`Argument list too long (os error 7)`で失敗していた。stdinで渡してもこの大きさはagentの文脈に収まらないので、promptに載せる入力を要約して上限を設ける（providerのinterfaceは変えない）。

- 上限: prompt全体が`PROMPT_LIMIT`（128KiB）以下、そのうち入力（pretty JSON）が`PROMPT_INPUT_LIMIT`（96KiB）以下。残りは指示・手順・言語の行。`ARG_MAX`の1/8で、promptを位置引数で渡していたときもenvを足して当たらなかった（今はstdinなので、上限はagentの文脈のため）。2026-09-28の日次の入力は要約で約62KB、2026-W39の週次は約70KBで、どれも落とさずに収まる
- 要約（`prompt_input`）: `period`、`landings`（`events`を除く）、`hourly`、`health`（そのまま。小さいので、上限を超えたときの`DROP_ORDER`では`asks`の後に落とす）、`kpi`（`cores`、`periods`は各期間の`label`・`partial`・`runs`だけ、`latest`は見直す期間（最後の期間）の各KPIの`all`層と、その`comparison`の`all`層（`judged`・`delta`を除く）と`unavailable`、`targets`は各目標から`periods`を除き、最後の期間の判定を`latest`に置いたもの）、`stats`（`parts`に`overall`・`landing_utilization`・`waiting`・`claim_deferrals`・`claim_holds`・`landing_holds`・`escalations`・`backend_failures`・`verification_failures`・`provider_switches`（`PROMPT_STATS`）、`omitted`に残りのkeyの名前）、`claim_deferred`、`asks`、`timelines`（`run_id`と`secs`だけ）。読めなかった部分の`{"error": ...}`はそのまま載せる。kpi.mdの手順のCadenceが日次（外れ値と目標割れ）・週次（手順1〜5）で見る数値は`latest`と`targets`と`stats.parts`にあり、層ごとの値・過去の期間の値・runごとの値・timelineはjobが読むコマンドで取りに行く
- 切り詰め: 要約がなお`PROMPT_INPUT_LIMIT`を超えるときは、`stats` → `kpi.latest` → `timelines` → `kpi.targets` → `kpi.periods` → `asks` → `health` → `claim_deferred` → `landings` → `kpi`（形の分からない`kpi`は丸ごと載るので最後）の順（`DROP_ORDER`。大きく、コマンドで読み直しやすいものから）に、収まるまで丸ごと落とし、落としたものを`omitted_to_fit`に名前で残す。`period`と`hourly`は落とさない
- promptは要約だと明かし、入力の全体が見直しのdirの`input.json`にあること（人が読むためのもの）と、細部は`kpi`（他の期間・層・host）・`stats --since <入力のstatsの始め（毎時は期間の終わりの6時間前、日次・週次は期間の始め）> --until <期間の終わり> --full`（run）・`timeline RUN`（長いrun）・`events --full`（着地は`--kind run_integrated`）で取りに行くことを指示する。dry runはdirを作らず、作るはずのdirの`input.json`を名指す
- jobの権限（`ACCESS`の`queue_cli`）は広げない。`input.json`を読ませるにはClaude Codeの`Read`を許すことになり、queueの外のファイルも読めるようになる。要約から外したものはどれも読むコマンドで同じものが得られるので、ファイルを読む必要はない。Codexの見直し（task 1220）は読み取りだけのsandboxがファイルの読み取りを許すが、promptはClaudeと同じで、細部は同じ読むコマンドで取りに行かせる
- この節の上限・要約・省いたものを読むコマンドの示し方は、headlessのjobに共通の方針（[ADR-t1566-1](../../adr/2026-10-03-t1566-1-headless-job-prompts-carry-decision-material-within-limits.md)）の先行の例で、全jobの渡し方と上限は[Prompt](prompt.md#headlessのjobのprompt)の「headlessのjobのprompt」の節の表が持つ。
- byte数の記録（ADR-t1566-1の決定6、task 1572）: 起動ごとに`throughput_review_started`と`throughput_review_finished`（成功・失敗・起動できなかった`error`のどれでも、promptを作った後なら）に`prompt_bytes`（言語の行を含むprompt全体）・`prompt_limit`（`PROMPT_LIMIT`）・`input_bytes`（要約のpretty JSON）・`input_limit`（`PROMPT_INPUT_LIMIT`）・`omitted_to_fit`（`DROP_ORDER`で落とした名前。無ければ空の配列）を書く。欄の名前はobserverの`observe_started`（task 1567）にそろえた平たい形で、dry runの出力にも同じ欄が載る。promptを作る前に終わったもの（毎時のskipped、`--unavailable`、入力の準備の失敗）には無い。値は`throughput_review::job_prompt`が作る

## Codexで動かす（task 1220）

`dagq.toml`の`[roles.throughput_review]`に`provider = "codex"`を書くと、見直しのjobはCodexの`codex exec --json`で動く（[ADR-t1063-1](../../adr/2026-09-29-t1063-1-headless-job-provider-per-role-with-intent-permissions.md)、[Actor model](actor-model.md)の`CODEX_ROLES`）。jobは状態を変えず読むだけ（[ADR-t996-1](../../adr/2026-09-29-t996-1-supervisor-runs-throughput-review-jobs-and-reports-to-inbox.md)の決定4）なので、権限の意図`ACCESS`（`queue_cli`）はそのままで、Codexの実装が読み取りだけのsandboxに訳す（queue serviceが動いていればgoal reviewと同じjobのpermission profile `dagq_job`。`:read-only`を継ぎ、jobの`dagq`はクライアントモードでserviceに読みに行く。[Agent provider lifecycle](../provider-lifecycle.md)）。promptは変えず、Claudeのplugin・skill・hookに頼らない。

- 行き先（`Supervisor::throughput_review_route`）: providerを書かない役割は今までどおりClaudeで、queueのholdのあいだと`--no-claude`では始めない（[ADR-t1204-1](../../adr/2026-09-30-t1204-1-explicit-no-claude-operation.md)の決定2）。providerを書いた役割は`job_route`で、そのproviderが使えれば（supervisorにagentがあり控えられていない）そこで、使えなければもう一方のproviderで（launchに`switched_from`・`switch_reason`）始め、どちらも使えなければ待つ。`--no-claude`ではClaudeは使えないので、Codexを設定した見直しはCodexで始まり、Codexも使えなければ子processに`--unavailable`で理由（`provider_disabled: Claude is disabled by --no-claude and codex cannot be used (<理由>); handle this role manually`）を渡す。子processは期間を決めて毎時の判定をした後（判定に当たらない時間はskippedのまま）、agentを起動せずに`throughput_review_finished`（`outcome: error`、`error`に理由）を記録し、inboxの`check the failed review`になる
- 子process: supervisorは行き先のlaunchを`--launch`で渡し、コマンドはその`provider`のagent（`ClaudeCode`か`Codex`）で起動する。`--launch`が無い（人が手で打つ）ときは束縛されたcheckoutの`[roles.throughput_review]`（`compose::throughput_review_launch`）。実行ファイルを解決できない（supervisorが見つけた後に消えた）ときもコマンドは`review`の前で失敗せず、与えられたpathのまま起動を試み、起動の失敗を下の`provider_unusable`（`executable_missing`）として終わりに記録する（期間の記録が残らずにこのprocessで期間が落ちることがないように）。Codexのjobは`session_id`をnullで始め（`--session-id`を渡さない）、終わりにproviderの`job_session`（`thread.started`のthreadと、rolloutの実際のmodel）を`throughput_review_finished`の`session_id`・`model`・`model_unknown`に書く（`JobSession::record`）。結論はproviderの`job_reply`（Codexは最後の`agent_message`）を`parse_output`で読み、保存・週次のfinding・`throughput_review_reported`はClaudeと同じ
- Codexが使えなかったとき: `--switchable`（役割がproviderを書いた）でCodexのjobが成功しなかったとき、providerの`job_failure`（`output.out` / `output.err`。agentが起動しなかったときは出力が無いので起動のerrorを`application::job_start_failure`で分類する。引数・envの大きさによる`E2BIG`とstdinの一時ファイルを用意できない`StdinUnprepared`は`other`で、使えないことを示さない。task 1560）が使えないこと（認証・利用上限・起動の失敗・実行ファイルが無い）を示せば、`throughput_review_finished`に`provider_unusable`（`provider`・`reason`）を書く。これはattentionにならず（`domain::event_attention`）、その期間は`reviewed`に数えない。supervisorは自分の子のその終わりを見てCodexを控え（`hold_provider`。利用上限はCodexの出力から再開の時刻を読む）、このprocessが始めた期間の記録から外すので、次のpassでその期間はもう一方のproviderで始まる。execの引き継ぎで前のprocessから渡った見直し（`reap_handed_over_reviews`が回収するもの）の終わりも同じく`provider_unusable`を見てCodexを控える（`hold_codex_for`）ので、引き継ぎの間に止まったCodexの見直しの期間も、もう一度Codexでは始まらない。`--no-claude`ではClaudeに行かず、上の`--unavailable`で理由を記録する。控えを書けなかったときは期間をやり直さない（同じCodexをすぐ起動し直さないため）。ClaudeのjobはClaudeの壁に当たっても今までどおり失敗として記録する（queueのhold askには入れない）
- 起動の引数: jobのcwdはjobのdir（Gitの外）なので、`codex exec`の起動に`--skip-git-repo-check`を付ける（無いとCodexがGitのrepositoryの外での起動を拒み、jobは`exit_code` 1で落ちる。task 1378、[Codexのheadless job](../provider-lifecycle.md#codexのheadless-job)）。`tests/it/throughput_review_codex.rs`のstubのcodexは実物と同じく、Gitのwork treeの外でflagが無ければ拒む
- 時間の上限: Claudeと同じく`--timeout`を過ぎたらjobと子孫をpidで止める（`infrastructure::observer::run_agent`。Codexが別のprocess groupで走らせたコマンドも、killの前に列挙した子孫として止まる）
- 記録: `throughput_review_started`の`launch.provider`が`codex`、終わりのevent・区間の`session_closed`にthreadのidとmodel（読めなければ`model_unknown`）。`stats`の`jobs.throughput_review`の`by_provider`・`by_model`と`kpi`の`job.*.throughput_review`の`provider=`の層でClaudeと分けて読める（jobの始まりと終わりは`mode`・`period`・`dir`で結ぶ。`domain::stats::jobs`）
- この repositoryの`dagq.toml`は`[roles.throughput_review]`に`provider = "codex"`を置き（model / effortは書かずCodexの既定。task 1221、goal 80）、本番のスループットの見直しはCodexで動く。古いバイナリは`CODEX_ROLES`に無い役割の`codex`を設定の誤りとして拒むので、task 1221のverifyの関門が、固定バイナリのbuild識別子のcommitがtask 1220の着地commitを含むことを確かめてから着地させた

## 権限

`throughput-review-job`は`review-job`などと同じheadlessのjob（`ActorRole::is_headless_job`）で、policyは`QueueRead`だけ（[Authorization](../authorization.md)）。`kpi`・`stats`・`timeline`・`events`などの読むコマンドは打て、note・mark・finding・ask・task・goalなど状態を変えるコマンドはCLIが`reviewer may not change queue state`で拒み、`authorization_denied`に残る。`throughput-review`コマンド自体はobserverの`observe`と同じ`observe.run`（supervisorと人とinbox）で、Claudeのsettingsのdenyにも入る。

## event

`throughput_review_started` / `throughput_review_finished` / `throughput_review_reported`はqueueのevent（task・goal・runを持たない）。attentionは`throughput_review_reported`（`report the review`）と、`outcome`が`error` / `failed`の`throughput_review_finished`（`check the failed review`。task 1099）の2つで、どちらも知らせるだけ（[Events and watch](events-watch.md)）。inboxのwatchを起こさないのは`mode`が`hourly`の`throughput_review_reported`だけで、失敗の`throughput_review_finished`はmodeを問わず起こす（ADR-t1418-1）。`provider_unusable`を持つ`throughput_review_finished`は`outcome`が`error` / `failed`でもattentionにしない（その期間はもう一方のproviderで始め直すので、人に知らせることが無い。task 1220）。`events`・`watch`のcompact形は`throughput_review_reported`に`mode`・`period`・`reasons`・`conclusion`・`path`・`finding_id`を、`throughput_review_finished`に`mode`・`period`・`outcome`・`dir`（`output.out` / `output.err`のあるdir）と`exit_code`・`reason`（payloadの`error`）を載せる。

## test

- `src/domain/throughput_review.rs`: 期間とラベル（日本時間の時・日・ISO週、0時の時）、bucketの数え方、平常の時間・50%と3件の両方が要ること・3時間続く低下と着地の無い時間は続くあいだ毎時起動すること、execで引き継いだ子の選び方（`children_finished`）・判定の入力の長さ、出力の結論（見出しあり・なし、5行まで）と次の一手（読めない・summaryが空・閉じていないblockは本文に残す）、期間の記録の有無と35分以内の始まり
- `src/application/throughput_review.rs`: `reference/kpi.md`の節だけを写すこと、頻度ごとのpromptと週次だけが次の一手を求めること、promptが`input.json`の場所と読むコマンド（`kpi`・`stats`・`timeline`・`events`）で細部を取りに行く指示を持つこと、MB級の入力の要約が上限に収まり見直しに要る部分を残すこと、上限を超える要約が`DROP_ORDER`の順に落として`omitted_to_fit`に名を残すこと、`job_prompt`が記録するpromptと入力のbyte数が`DROP_ORDER`で落とした後も上限以下で`omitted_to_fit`の名を持ち、何も落とさなければ空の配列であること（`the_job_prompt_records_its_bytes_within_the_limit_after_dropping_parts`、task 1572）
- `src/domain/mod.rs`: `throughput_review_finished`は`error` / `failed`だけが`check the failed review`で、skippedと`succeeded`と`provider_unusable`を持つもの（task 1220）はattentionでないこと
- `tests/it/runtime_throughput_review.rs`: 規則に当たらない時間はagentを起動せずskippedだけを書くこと（dry runはpromptを返す）、当たった時間の保存（`review.md`・`review.json`・`input.json`）とroleとMCPなしと、jobのnote・finding・mark・readyが拒まれることと`kpi`は読めること、inboxの`events`（`watch`と同じ判定）に`report the review`と結論が載ること、週次の次の一手がproposalを求めるfindingになること、`failed`（非0終了）と`error`（起動できない）の失敗がinboxの`events`に`check the failed review`として載りaskを開かないこと、起動前にreports/reviewsを作れない失敗もerrorを1件だけ記録してinboxに届きaskを開かず、同じfixtureのdry runは記録しないこと、起動後の成功・failed・errorもfinishが1件だけであること、MB級の入力（8日分のmarks）の日次と週次がagent（promptをstdinで受けるstub）を起動して`succeeded`になりpromptが`PROMPT_LIMIT`以下であること、毎時・日次・週次の見直しのstartedとfinishedと起動できなかった見直しのfinishedの`prompt_bytes`が`prompt.md`のbyte数と同じで、MB級の入力の日次と週次の`input_bytes`が`PROMPT_INPUT_LIMIT`以下であること（task 1572）、supervisorが時・日・週を1度ずつ始めて同じ期間を2度始めないことと`throughput_review: false`で始めないこと、失敗するjobがclaimと着地を止めずattentionだけを残すこと
- `src/domain/sessions.rs`: 見直しの区間が始まりで開き、自分の`session_id`の終わりで閉じ、別の期間の見直しとobserverの区間を閉じないこと、同じ期間の始め直しと`RUNNING_MS`を過ぎた区間を`inferred`で閉じること（`throughput_review_spans_close_by_session_period_or_age`）
- `src/infrastructure/sessions.rs`: 区間がqueueのeventで開き、終わりでtranscriptのmodel / effortを`read_before`で読んで閉じること、並ぶ見直しが開いたまま残り、時間を過ぎた区間がskippedの終わりで`inferred`としてtranscriptの最後で閉じること、observerの区間は自分の終わりでだけ閉じること（`a_throughput_review_span_records_its_launch_and_the_model_of_its_transcript`）
- `tests/it/runtime_throughput_review.rs`の当たった時間のtest: 区間が開いて`job_finished`で閉じ、`stats`の`sessions.by_kind.throughput_review`に数えられること
- `tests/it/throughput_review_codex.rs`（task 1220）: `provider = "codex"`の時間・日次・週次の見直しがstubの`codex exec --json`（jobのpermission profile、`-m`と`model_reasoning_effort`、`--session-id`なし）で起動し、jobのroleとactor、queue service経由の読み取り、最後の返答の保存・週次のfinding・`throughput_review_reported`、`throughput_review_started`の`launch.provider`と`session_id`のnull、終わりと区間のthreadとmodel、`stats`の`by_provider` / `by_model`と`kpi`の`provider=codex`。利用上限で止まったCodexが`--switchable`でだけ`provider_unusable`を書きattentionにならないこと、`--unavailable`がagentを起動せず理由を記録すること、消えたCodexを渡したコマンド（実バイナリ）が`review`の前で失敗せず`provider_unusable`（`executable_missing`）の終わりを記録すること、execで引き継いだsupervisorが`provider_unusable`の見直しを回収してCodexを控え、その期間と他の期間をClaudeで始めること、`--no-claude`のsupervisorがCodexを設定した見直しを始めClaudeを起動しないこと、Codexが利用上限で失敗してもClaudeに行かず、Codexを控えて期間をやり直し理由を記録すること
- `src/domain/throughput_review.rs`の`a_period_is_reviewed_once_its_finish_is_recorded`（`provider_unusable`の終わりを数えないこと、終わりのあるstartを走っているとみなさないこと）、`src/domain/stats/jobs.rs`の`a_codex_throughput_review_is_counted_under_codex_and_its_model`
- `tests/it/cli_*.rs`のroleの一覧に`throughput-review-job`を足し、状態を変えるコマンドが拒まれることを確かめる

---
id: design-execution-tokens
type: design
title: Executionのトークン数
status: current
created: 2026-10-08
scope: provider
tags:
  - runtime
  - provider
related:
  - adr-t1486-1
  - adr-t813-2
  - adr-t1233-1
  - design-provider-lifecycle
  - design-supervisor-lifecycle-headless-worker
  - design-supervisor-lifecycle-stats
  - design-supervisor-lifecycle-kpi
---

# Executionのトークン数

supervisorはトークン数をExecutionごとに記録する。
Executionは`claude -p` / `codex exec`の1回の呼び出しで、非対話のturnの1回とheadlessのjobの1回にあたる（[ADR-t1486-1](../adr/2026-10-04-t1486-1-supervisor-records-token-usage-per-execution.md)）。
区間（session span）のトークン数は[provider-lifecycle](provider-lifecycle.md#トークン数とコスト)が持つ。

## 入口

| 知りたいこと | コードの入口 |
| --- | --- |
| 記録の形と欄の値 | `domain::tokens::ExecutionTokens`・`ModelTokens`・`TokenSource`と理由の定数 |
| Claudeの出力の読み | `claude_turns::result_tokens`・`job_result` |
| Codexのrolloutの読み | `codex_turns::rollout_usage`、`domain::tokens::RolloutUsage::tokens` |
| rolloutの場所 | `Codex::sessions_dir` |
| turnの分（累計との差、前のturnが数えたturn） | `domain::turn::turn_own_models`・`turn_own_cost`・`counted_rollout_turns`、`headless_session.rs`の`turn_tokens` |
| jobの終わりへの記録 | `AgentProvider::job_session`（Codexは`codex.rs`の`job_tokens`）、`domain::headless_job::JobSession::record`、死んだsupervisorのjobは`supervise::jobs`の`close_taken_over`と`taken_over_end` |
| 対話のsessionの区切り | `infrastructure::session_tokens`、`domain::tokens::SpanTotals`、`domain::sessions::TOKEN_CUT_KINDS`と間隔の定数 |
| 日・週×actor×provider×modelへの集約 | `domain::stats::executions`、`domain::kpi::window`の`Context::token_kpis`（下の[statsとkpiでの集約](#statsとkpiでの集約)） |

## 記録の形

turnの`turn_finished`とjobの終わりのeventは、providerによらず同じ欄を持つ。

| 欄 | 中身 |
| --- | --- |
| `tokens` | Executionの分で、全てのmodelとsubagent・子のthreadを足したもの |
| `tokens_by_model` | modelごとの同じ数。providerがmodelごとに出さなければ空 |
| `tokens_source` | 数えた元 |
| `tokens_reason` | 数えられなかった理由か、一部が抜ける元で数えた理由 |
| `children` | Executionが起こしたsubagent（子のthread）の数。providerが言わなければ`null` |
| `tokens_turns` | Codexのrolloutから数えたroot turn。同じthreadの後のExecutionが再び数えないため |
| `peak_context` | 1回のAPI呼び出しの入力側（cacheを含む）の、Executionの中の最大 |
| `context_window` | modelのcontext window。分からなければ`null` |
| `compactions` | Executionの中でcontextをcompactした回数 |
| `context_reason` | `peak_context`か`compactions`を数えられなかった理由 |

- 正常な0は`tokens`の各数が0のobjectで、未計測は`tokens`が`null`で`tokens_reason`を持つ。
- 見張りが止めた復旧のjobの`recovery_finished`、捨てたplan reviewとproposalが先に進んだplan reviewの`plan_review_discarded`、handoff・slotの見張りの終わり・ループの終わり・dropで他の終わりのeventを書かずに止めた自分のjobの`headless_job_stopped`（jobの`kind`でactorを決める）、前のsupervisorのreviewを引き継いだ側が書く`review_failed`、死んだsupervisorのjobを止めた側・既に終わっていた（`gone`）か止めなかった（`not_the_job`）と閉じた側が書く`headless_job_stopped`も、agentを起動したjobの終わりとしてこの欄を持ち、providerの読みが出力からトークン数を返さなければ未計測にする。
  引き継いだ側は、前のreviewの開始の後の終わりのeventが既にこの欄を持てば足さない。
  死んだsupervisorのjobを閉じた側は、`headless_jobs`の行が持つstdoutの置き場を行の`provider`で読み、置き場が無い（古い行）・読めなければ未計測にする。
  同じjobの終わりのevent（その`kind`の終わりか`headless_job_stopped`で、同じrun・proposal・goalと`attempt`のもの）がjobの開始の後に既にこの欄を持てば足さない（`domain::headless_job::JobOn`）。
  agentを起動していない終わり（起動の前に失敗したreview、`program`のjobなど）はExecutionではなく、この欄を持たない。
- 非対話の区間は`tokens`が`null`のturnを足さない。
- 親とsubagentごとの内訳は記録しない（ADR-t1486-1決定7）。
- contextの欄も、正常な0（compactionの無いExecution）と未計測（`null`と`context_reason`）を分ける。
  呼び出しが1回も無いExecutionは`peak_context`だけが`null`で、理由を持つ。

## Claudeの数える元

- `result`の`modelUsage`をmodelごとに足す。
  `modelUsage`はsubagentの分を含み、`result.usage`は含まない。
- `modelUsage`は`total_cost_usd`と同じくsessionの累計である。
  jobは1回ごとに新しいsessionなので、累計がそのままjobの分になる。
- turnの分は、同じsessionの前のturnの累計との差で、costと同じ規則で取る。
  差の元は、同じsessionのturnのうち累計を記録した最後のもの。
  resumeでないturn、同じsessionのどのturnも累計を記録していないturn、累計が前より減ったturn（別のsession）は、累計をそのままturnの分にする。
  差はmodelごとに取り、どれかのmodelの数が減れば累計をそのまま使う。
  累計は`turn_finished`の別の欄に残り、次のturnがそこから差を取る。
- `modelUsage`の無い出力（古いClaude Code）は`result.usage`で数え、subagentの分が抜けたことを`tokens_reason`が示す。
  Claudeでsubagentの分が抜けるのはこのときだけ。
- subagentの数は`subagent_stats`から取り、sessionの累計ではなくturnの分である。
- jobは`--output-format stream-json`で起動し、返答もその最後の`result`から読む。
  jobのsessionのidとmodelは、出力からではなく開始とtranscriptから取る。

## Codexの数える元

- Executionのroot thread（`thread.started`のid）のsessionの`token_usage_record`のうち、今回のExecutionのroot turnのものを、`(thread_id, response_id)`ごとに1回だけ足す。
  レコードの`usage`は1回の応答の分なので足し、累計の`turn_token_usage`・`thread_token_usage`は使わない。
  inputとoutputの分け方は`turn.completed`と同じで、cachedのinputを除いた分とcache_readに分け、reasoningはoutputに含まれるので足さない。
- 子のthreadは自分のrolloutに書き、その`session_meta`とレコードの`session_id`がroot threadで、`root_turn_id`がrootのturnである。
  rolloutのday directoryのファイルのうち、Executionが始まった後に書かれたものからそれを探す。
  子のthreadの数は数えたレコードのthreadのうちrootでないものの数。
- 今回のExecutionのroot turnは、rootのrolloutの`task_started`のうちExecutionが始まった後のもの。
  `codex exec --json`の出力はturnのidを持たない。
- 前のExecutionまでに数えたroot turnを再び数えない。
  数えたroot turnは`turn_finished`とjobの終わりのeventの`tokens_turns`に残り、次のturnは同じsessionの`turn_finished`からそれを集めて除く。
  eventだけから組み立てるので、wrapperの引き継ぎや再起動の後も同じになる。
  jobは1回ごとに新しいthreadなので除くものは無い。
- rolloutが無い・読めない・`token_usage_record`の無い旧形式・今回のturnが無いときは、`turn.completed`のthreadの累計に落とし、理由を`tokens_reason`に書く。
  turnは同じsessionの前のturnの累計（`tokens_total`）を引き、前の累計が無ければ累計をそのまま使う（Claudeと同じ、`domain::turn::thread_total_own`）。
  jobは新しいthreadなので累計がjobの分になる。
  turnの前に、累計を記録せずrolloutから数えたturn（`turn.completed`を読めなかったもの）があれば、その分も引いて二重に数えない。
  `turn.completed`の累計は子のthreadの分を含まない。
  `turn.completed`も無ければ`tokens`は`null`（未計測）で、理由はrolloutのものを書く。
  rolloutにturnがあり応答が無いとき、今回のturnが全て前に数えたものだったときは計測した0である。
- rolloutの場所は`Codex::sessions_dir`の1か所で、turnもjobもそこから読む。
  今はCodexのhome（`supervise --codex-home`、`$CODEX_HOME`、`~/.codex`の順）の`sessions`で、podmanのbackend（まだ無い）では、制御側が終わった後に読めるrun dirの下をここが指す（ADR-t1233-1）。

### 本番のrolloutで確かめたこと

2026-10-08に、hostの`~/.codex/sessions`の10/01〜10/08のrollout 1,399件（codex-cli 0.155.1・0.159.2・0.160.0）と、09/30〜10/08のdagqのCodexのExecutionの出力1,238件（worker の turn 264・job 974。runのturnのJSONLとplan review・goal review・reviewの出力）を突き合わせた。
見た欄は、rolloutの`session_meta`の`id`・`session_id`・`source`、`event_msg`の`task_started`の`turn_id`・`root_turn_id`、`token_usage_record`の`thread_id`・`turn_id`・`session_id`・`root_turn_id`・`response_id`・`usage`・`turn_token_usage`・`thread_token_usage`、出力の`thread.started`・`turn.started`・`turn.completed`。

- 子のthread: 子のrolloutは13件（10/03〜10/04、どれもdagqの外のeval）で、どれも子のthreadのidのファイルに書かれ、`session_meta`とレコードの`session_id`がroot thread、`root_turn_id`がrootのturnだった。
  rootのrolloutには子のレコードが無い。
  dagqのExecutionに子のthreadは無かった。
- `turn.completed`の子の分: rootの`thread_token_usage`は子の分を含まず、`turn.completed`の`usage`は1,070件でrootのturnの最後の`thread_token_usage`と一致した。
  よって子の分は入らない。
  残りの168件（どの版にもある）は`turn.completed`がrolloutのレコードの合計より小さかった（例: inputが1,252,324に対しレコードの合計1,503,802）。
- 重複: 14,224件のレコードで`(thread_id, response_id)`の重複は無く、`response_id`の無いものも無かった。
  それでも同じ応答が2回書かれたときに備えて除く。
- resume: 2回以上のExecutionを持つthread 65本のどれも、Executionごとに別のroot turnに当たり、rootのレコードの`root_turn_id`は`turn_id`と`task_started`の`turn_id`に等しかった。
  rolloutのレコードは同じファイルに続くので、sessionの全レコードを足すと前のExecutionを再計上する。
- turnの特定: `codex exec --json`の出力の`thread.started`はthreadのidだけ、`turn.started`はidを持たない。
  rolloutの`task_started`はturnのidと時刻を持つので、Executionが始まった時刻で分ける。

## contextの大きさとcompaction

worker や job の context が大きくなりすぎていないかを読むための記録で、トークン数と同じExecutionの同じ出力から取る。
数えるのはトップレベルのエージェント（Codexはroot thread）の呼び出しだけで、Claudeのsubagentの呼び出しとCodexの子のthreadは混ぜない。
親とsubagentごとの内訳は記録しない。
対象は非対話のturnとheadlessのjobで、inboxと人が開いた対話のsessionは対象にしない。

- Claudeは`claude -p --output-format stream-json`の出力（jobも同じ）を読む。
  - `peak_context`は`assistant`の`usage`の`input_tokens`・`cache_read_input_tokens`・`cache_creation_input_tokens`の和の最大。
    入力側は生成を始めた時点で決まっているので、streamの途中の`usage`でよい。
    出力側は使わない。
  - `compactions`は`system`の`compact_boundary`の数。
  - subagentのeventは`parent_tool_use_id`を持つので除く。
  - `--resume`でも読むのは今回のExecutionの出力だけなので、前のExecutionのcompactionは入らない。
  - `context_window`は`system/init`のmodelの`modelUsage`の`contextWindow`、無ければmodelから決まる値（1か所の対応表に置き、1Mのmodelを区別する）、どちらも無ければ`null`。
  - 出力がstreamでない（`system/init`が無い）ときは未計測。
- Codexはroot thread（`thread.started`のid）のrolloutだけを読み、子のthreadのrolloutは読まない。
  - rolloutの`token_count`と最上位の`compacted`はturnのidを持たないので、直前の`task_started`のturnのものとする。
  - 今回のExecutionの範囲はトークン数と同じく今回のroot turn（Executionが始まった後に始まり、前のExecutionが数えていないturn）。
    resumeで同じrolloutに積まれた前のExecutionのturnの`compacted`は数えない。
  - `peak_context`は範囲の`token_count`の`last_token_usage.input_tokens`（cachedを含む）の最大、`context_window`は`model_context_window`、`compactions`は範囲の`compacted`の数。
  - rolloutが無い・読めない、または範囲が決まらない（`token_usage_record`の無い旧形式、今回のturnが無い）ときは、0にせず`null`とトークン数と同じ理由にする。

## 対話のsessionの区切り

inboxと人が開いたplannerの区間（hookが記録する対話のsession）はExecutionを持たないので、supervisorがtranscriptを区切って`session_tokens`に記録する（ADR-t1486-1決定3）。
runtimeのplannerはturnのExecutionで数えるので区切らない。

- 開いている区間は、開いてから、または前の区切りから1時間たつと区切る。
  判定は記録した区切りの時刻で行うので、supervisorの入れ替えや再起動の後も間隔は変わらない。
  間隔の判定はturnの取り込みと同じ周期（`SESSION_TURNS_INTERVAL`）で行うので、区切りの間は1時間よりその周期の分まで長くなる。
  何も足さない区切り（使わなかった間）は書かないので、使われていない区間は周期ごとにtranscriptを読み直す。
- 閉じた区間は、閉じた時刻を区切りの時刻にして1回だけ区切る（`final: true`）。
  supervisorがtranscriptから`inferred`に閉じた区間はtranscriptの最後のレコードで終わり、`session_closed`と同じくそのレコードまで数える。
  hookが閉じた区間（`inferred`を含む）は、最終の`session_turns`と同じく閉じた時刻の前まで数える。
  開いている間の区切りより前には戻さない。
  hookの`SessionEnd`は閉じるだけでtranscriptを読まず、閉じたときの区切りはsupervisorが後で作る。
  閉じてから区切りの窓（`TOKEN_CUT_CLOSED_WINDOW_MS`）を過ぎた区間は区切らない。
  区切りの無い区間のトークン数は閉じたときの区間のトークン数（下の節）だけが持ち、開いている間の区切りのある区間では最後の区切りの後の分が区切りに入らない。
  transcriptが読めない・usageを数えられないときは、開いている区間は次の周期に回し、閉じた区間は未計測の区切りを1回書く。
- 区切りは区間の開始から区切りの時刻までの累計（`tokens_total`・`tokens_total_by_model`）を持ち、区切りの分はその累計から前の区切りの累計を引いたもの。
  前の位置は記録した区切りの時刻と累計で、supervisorの記憶に持たない。
  1つのmessageのレコードが区切りをまたいでも、後の区切りは増えた分だけを足すので、どのmessageも1回だけ数える。
- 数え方は閉じたときの区間のトークン数と同じ（`message.id`ごとに1回、sidechainも数える）で、modelごとの内訳はmessageを最初にmodelを名乗るレコードのmodelに入れる。
- transcriptの読み取りと解析は書き込みのロックの外で済ませ、ロックの中では区間の区切りを読み直して引き算と書き込みだけをする。
  別のsupervisorが先に同じ時刻まで区切っていれば書かない。

### 閉じたときの区間のトークン数との関係

閉じたときの区間のトークン数は、閉じるときにtranscriptを読んだ区間では`session_closed`の`tokens`にある。
hookが閉じた区間では`session_closed`は`tokens`を持たず（`active_unavailable`が`hook_intake_pending`）、supervisorの取り込みが後で書く最終の`session_turns`（`final: true`）の`tokens`にある（[provider-lifecycle](provider-lifecycle.md#transcriptと稼働時間)）。
`stats`の区間の`tokens`もこの順で読む（`domain::stats::sessions`）。

閉じたときの区切りまでの区切りの合計は、閉じたときの区間のトークン数と同じmessageを数える。
区切りは同じトークン数を区切りの時刻で分けたもので、別の消費ではない。
日やactorで足すときは、inboxと人のplannerの区間について区切りだけを足し、`session_closed`の`tokens`も最終の`session_turns`の`tokens`も足さない。
`stats`と`kpi`の期間の集計（`domain::stats::executions`）もそうしていて、区切りの無い区間（区切りが入る前に閉じたもの）は閉じたときの区間のトークン数でも補わず、期間の集計に入らない。

## statsとkpiでの集約

`stats`の期間の集計と`kpi`は、Executionと対話のsessionの区切りの記録を同じ関数（`domain::stats::executions`）で窓に集める（ADR-t1486-1決定1・3）。
taskとrunへの集約は区間の`tokens`で行い（[stats](supervisor-lifecycle/stats.md#トークン数)）、日とactorには分けない。

- 窓に入れるのは窓の中で終わったExecutionと、区切りの時刻が窓の中の区切りで、eventを書いた時刻ではない。
  日をまたいで開いた区間は区切りごとの日に入り、閉じた日にまとめない。
  `stats`の窓は`--since`の時刻（無ければ窓の始まりのeventの時刻）から`host`の窓の終わりまでで、`kpi`は期間の窓の`stats`を期間の時刻で呼ぶ。
- actorはsupervisorが起動した区間のkindで、runのturnはそのrunで最後に開いたworker・resume・reviseの区間のkind、runtimeのplannerのturnは`runtime_planner`、jobはそのkind、区切りは区間のkind。
  復旧のjobは`recovery_finished`だけを数え、同じjobの`triage_*`は数えない。
- providerはeventの`provider`、無ければ`tokens_source`の元（区切りはClaude Code）。
  modelは`tokens_by_model`の内訳で、内訳の無いExecutionは`model`の欄（無ければ`unknown`）、内訳に入らない残りは`unknown`に入れる。
- `sessions`のkindごと・経路ごとの`tokens`は、同じ窓のactorごと・経路（turnは`headless`、区切りは`interactive`）ごとの和。
- 記録の形より前の記録（`tokens_source`の無いturnや、トークン数の無いjobの終わり）と、区間が閉じたときの`tokens`は混ぜない。
  記録の始まりは`recorded_from`（全体・actor・providerごと）で、窓のうち記録のある分は`coverage`が示す。
- `kpi`の`tokens`と`tokens_per_landing`は`--by`によらず`all`・`actor=`・`provider=`・`model=`と3つを掛けた`cross:actor=…|model=…|provider=…`の層を出す（`--cross`の交差層には足さない）。
  Executionはactorとproviderの層には丸ごと、modelの層にはmodelごとの分で入る。
  記録が窓の全体を覆わない期間は値をnull（0でも判定する一部でもない）にして理由を`unavailable`に入れ、内訳は`details.tokens`に写す。
  `actor=`の層はそのactorの`coverage`でも同じにする。
- `--area` / `--change`との組み方: runに属するExecution（workerのturn・review・復旧のjob）だけがtaskの`change=`とrunの`area=`（着地していないrunは`unknown`）の層に入り、`tokens_per_landing`の分母はその層の着地。
  runを持たないExecution（plan review・goal review・observer・見直し・plannerのturn・区切り）はこの2つの層に入らず、ほかの層の分母は`all`の着地。
- `--compare`は`strata`に同じ層の前後を並べ、`before` / `after`の`token_coverage`が窓ごとの`coverage`を示す。
- 記録は計測用で助言的で、課金・認可・上限の判断に使わない（ADR-t1486-1決定6）。
  目標を置いても判断の材料にとどめる。

## 今の穴

- 子のthreadのrolloutはExecutionが始まった後に更新されたファイルから探すので、ファイルの更新時刻が書き換えられると見落とす。
- 累計に落としたturnが前のturnのrolloutから数えた分を引くとき、その分は子のthreadを含み累計は含まないので、子のthreadがあれば引きすぎうる（0は下回らない）。
- 閉じてから区切りの窓のうちにsupervisorが動かなかった区間は、最後の区切りの後の分が区切りに入らない。
- 閉じたときにtranscriptが読めない・usageを数えられない区間は閉じたときの区切りが未計測になり、開いている間の区切りがあれば最後の区切りの後の分が区切りに入らない。
  区切りのある区間は区切りだけを足すので、閉じたときの区間のトークン数でも補わない。
- 区切りの無い区間（区切りの窓のうちにsupervisorが区切らなかったもの）のトークン数は、`stats`と`kpi`の期間の集計に入らない。
- jobの終わりのeventが`provider`を持たず、トークン数も数えられなかったjobは、`stats`と`kpi`でproviderが`unknown`になる。
- 死んだsupervisorのjobのうち、pidが別のprocessになったか起動時刻が読めず止めなかったもの（`not_the_job`）は、jobがまだ走っていれば閉じた時点までの出力で数えるので、その後の分は数えない。
- `kpi`の`all`・`provider=`・`model=`の層は全体の`coverage`で判定するので、actorごとに記録の始まりが違うと、始まりの遅いactorの記録より前の分は欠けたまま値になる（`actor=`の層と`details.tokens`の`recorded_from`で分かる）。
- streamの`assistant`の`usage`は生成を始めた時点の途中の値なので、トークン数には使わない（`peak_context`は入力側だけなので使う）。
- Codexのturnの最初の`token_count`が前のExecutionの最後の呼び出しの`last_token_usage`を持ち越すと、その間にcompactionがあったとき`peak_context`を大きく読みうる。

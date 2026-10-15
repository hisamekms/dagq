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
  - design-supervisor-lifecycle-commands
---

# Executionのトークン数

supervisorはトークン数をExecutionごとに記録する。
Executionは`claude -p` / `codex exec`の1回の呼び出しで、非対話のturnの1回とheadlessのjobの1回にあたる（[ADR-t1486-1](../adr/2026-10-04-t1486-1-supervisor-records-token-usage-per-execution.md)）。
区間（session span）のトークン数は[provider-lifecycle](provider-lifecycle.md#トークン数とコスト)が持つ。

## 入口

| 知りたいこと | コードの入口 |
| --- | --- |
| 記録の形と欄の意味 | `domain::tokens::ExecutionTokens` |
| Claudeの出力の読み | `infrastructure::claude_turns` |
| Codexのrolloutの読み | `domain::tokens::RolloutUsage` |
| rolloutの場所 | `Codex::sessions_dir` |
| turnの分（累計との差、前のturnが数えたturn） | `domain::turn` |
| jobの終わりへの記録 | `domain::headless_job::JobSession` |
| 死んだsupervisorのjobの記録 | `supervise::jobs::close_taken_over` |
| 対話のsessionの区切り | `infrastructure::session_tokens` |
| 日・週×actor×provider×modelへの集約 | `domain::stats::executions`（下の[statsとkpiでの集約](#statsとkpiでの集約)） |

## 記録の形

turnの`turn_finished`とjobの終わりのeventは、providerによらず同じ欄を持つ（欄の意味は`domain::tokens::ExecutionTokens`のdoc comment）。

- 正常な0はトークン数の各数が0のobjectで、未計測はトークン数が`null`で理由を持つ。
- 見張りが止めた復旧のjobの`recovery_finished`、捨てたplan reviewとproposalが先に進んだplan reviewの`plan_review_discarded`、他の終わりのeventを書かずに止めた自分のjobの`headless_job_stopped`、前のsupervisorのreviewを引き継いだ側が書く`review_failed`、死んだsupervisorのjobを止めた側・既に終わっていたか止めなかったと閉じた側が書く`headless_job_stopped`も、agentを起動したjobの終わりとしてこの欄を持ち、providerの読みが出力からトークン数を返さなければ未計測にする。
  引き継いだ側は、前のreviewの開始の後の終わりのeventが既にこの欄を持てば足さない。
  死んだsupervisorのjobを閉じた側は、jobの行が持つstdoutの置き場を行のproviderで読み、置き場が無い（古い行）・読めなければ未計測にする。
  同じjobの終わりのevent（その種類の終わりか`headless_job_stopped`で、同じrun・proposal・goalと試行のもの）がjobの開始の後に既にこの欄を持てば足さない（`domain::headless_job::JobOn`）。
  runのreviewは1つの試行に全体のreviewとagentごとのjobを持つので、agentが同じ終わりだけを同じjobとし、jobを起動し直す（やり直しか保留の）`review_job_finished`はそのjobの終わりに数えない（[Headless job processes](supervisor-lifecycle/headless-job-processes.md#引き継ぎ)）。
- runのreviewではjobごとの`review_job_finished`がこの欄を持ち、全てのjobの後の`review_finished`・`review_failed`は全体のreviewのsessionを写すだけで持たない。
  agentを起動していない終わり（起動の前に失敗したreview、プログラムのjobなど）はExecutionではなく、この欄を持たない。
- 非対話の区間は未計測のturnを足さない。
- 親とsubagentごとの内訳は記録しない（ADR-t1486-1決定7）。
- contextの欄も、正常な0（compactionの無いExecution）と未計測（`null`と理由）を分ける。
  呼び出しが1回も無いExecutionは最大の入力だけが`null`で、理由を持つ。

## Claudeの数える元

- `result`の`modelUsage`をmodelごとに足す。
  `modelUsage`はsubagentの分を含み、`result.usage`は含まない。
- `modelUsage`は`total_cost_usd`と同じくsessionの累計である。
  jobは1回ごとに新しいsessionなので、累計がそのままjobの分になる。
- turnの分は、同じsessionの前のturnの累計との差で、costと同じ規則で取る。
  差の元は、同じsessionのturnのうち累計を記録した最後のもの。
  resumeでないturn、同じsessionのどのturnも累計を記録していないturn、累計が前より減ったturn（別のsession）は、累計をそのままturnの分にする。
  差はmodelごとに取り、どれかのmodelの数が減れば累計をそのまま使う。
  累計は`turn_finished`に残り、次のturnがそこから差を取る。
- `modelUsage`の無い出力（古いClaude Code）は`result.usage`で数え、subagentの分が抜けたことを理由が示す。
  Claudeでsubagentの分が抜けるのはこのときだけ。
- subagentの数は`subagent_stats`から取り、sessionの累計ではなくturnの分である。
- jobはstreamの出力で起動し、返答もその最後の`result`から読む。
  jobのsessionのidとmodelは、出力からではなく開始とtranscriptから取る。

## Codexの数える元

- Executionのroot thread（`thread.started`のid）のsessionの`token_usage_record`のうち、今回のExecutionのroot turnのものを、`(thread_id, response_id)`ごとに1回だけ足す。
  レコードの`usage`は1回の応答の分なので足し、累計の`turn_token_usage`・`thread_token_usage`は使わない。
  inputとoutputの分け方は`turn.completed`と同じで、cachedのinputを除いた分とcache_readに分け、reasoningはoutputに含まれるので足さない。
- 子のthreadは自分のrolloutに書き、その`session_meta`とレコードの`session_id`がroot threadで、`root_turn_id`がrootのturnである。
  rolloutのday directoryのファイルのうち、Executionが始まった後に書かれたものからそれを探す。
  子のthreadの数は数えたレコードのthreadのうちrootでないものの数。
- 今回のExecutionのroot turnは、rootのrolloutの`task_started`のうちExecutionが始まった後のもの。
  `codex exec`の出力はturnのidを持たない。
- 前のExecutionまでに数えたroot turnを再び数えない。
  数えたroot turnは`turn_finished`とjobの終わりのeventに残り、次のturnは同じsessionの`turn_finished`からそれを集めて除く。
  eventだけから組み立てるので、wrapperの引き継ぎや再起動の後も同じになる。
  jobは1回ごとに新しいthreadなので除くものは無い。
- rolloutが無い・読めない・`token_usage_record`の無い旧形式・今回のturnが無いときは、`turn.completed`のthreadの累計に落とし、理由を書く。
  turnは同じsessionの前のturnの累計を引き、前の累計が無ければ累計をそのまま使う（Claudeと同じ）。
  jobは新しいthreadなので累計がjobの分になる。
  turnの前に、累計を記録せずrolloutから数えたturn（`turn.completed`を読めなかったもの）があれば、その分も引いて二重に数えない。
  `turn.completed`の累計は子のthreadの分を含まない。
  `turn.completed`も無ければトークン数は`null`（未計測）で、理由はrolloutのものを書く。
  rolloutにturnがあり応答が無いとき、今回のturnが全て前に数えたものだったときは計測した0である。
- rolloutの場所は`Codex::sessions_dir`の1か所で、turnもjobもそこから読む。
  Codexのworkerはcontainerで動かさない（[Container runtime](container-runtime.md#fail-closedの境界)）。

## contextの大きさとcompaction

worker や job の context が大きくなりすぎていないかを読むための記録で、トークン数と同じExecutionの同じ出力から取る。
数えるのはトップレベルのエージェント（Codexはroot thread）の呼び出しだけで、Claudeのsubagentの呼び出しとCodexの子のthreadは混ぜない。
親とsubagentごとの内訳は記録しない。
対象は非対話のturnとheadlessのjobで、inboxと人が開いた対話のsessionは対象にしない。

- Claudeはstreamの出力（jobも同じ）の今回のExecutionの分だけを読み、streamでなければ未計測にする（読み方は`infrastructure::claude_turns::ContextCounter`）。
- Codexはroot thread（`thread.started`のid）のrolloutだけを読み、子のthreadのrolloutは読まない。
  - 今回のExecutionの範囲はトークン数と同じく今回のroot turn（Executionが始まった後に始まり、前のExecutionが数えていないturn）。
    resumeで同じrolloutに積まれた前のExecutionのturnの`compacted`は数えない。
  - rolloutが無い・読めない、または範囲が決まらない（`token_usage_record`の無い旧形式、今回のturnが無い）ときは、0にせず`null`とトークン数と同じ理由にする。

## 対話のsessionの区切り

inboxと人が開いたplannerの区間（hookが記録する対話のsession）はExecutionを持たないので、supervisorがtranscriptを区切って`session_tokens`に記録する（ADR-t1486-1決定3）。
runtimeのplannerはturnのExecutionで数えるので区切らない。

- 開いている区間は、開いてから、または前の区切りから一定の間隔（`TOKEN_CUT_INTERVAL_MS`）がたつと区切る。
  判定は記録した区切りの時刻で行うので、supervisorの入れ替えや再起動の後も間隔は変わらない。
  間隔の判定はturnの取り込みと同じ周期（`SESSION_TURNS_INTERVAL`）で行うので、区切りの間は間隔よりその周期の分まで長くなる。
  何も足さない区切り（使わなかった間）は書かないので、使われていない区間は周期ごとにtranscriptを読み直す。
- 閉じた区間は、閉じた時刻を区切りの時刻にして1回だけ区切る。
  supervisorがtranscriptから`inferred`に閉じた区間はtranscriptの最後のレコードで終わり、`session_closed`と同じくそのレコードまで数える。
  hookが閉じた区間（`inferred`を含む）は、最終の`session_turns`と同じく閉じた時刻の前まで数える。
  開いている間の区切りより前には戻さない。
  hookの`SessionEnd`は閉じるだけでtranscriptを読まず、閉じたときの区切りはsupervisorが後で作る。
  閉じてから区切りの窓（`TOKEN_CUT_CLOSED_WINDOW_MS`）を過ぎた区間は区切らない。
  区切りの無い区間のトークン数は閉じたときの区間のトークン数（下の節）だけが持ち、開いている間の区切りのある区間では最後の区切りの後の分が区切りに入らない。
  transcriptが読めない・usageを数えられないときは、開いている区間は次の周期に回し、閉じた区間は未計測の区切りを1回書く。
- 区切りは区間の開始から区切りの時刻までの累計を持ち、区切りの分はその累計から前の区切りの累計を引いたもの。
  前の位置は記録した区切りの時刻と累計で、supervisorの記憶に持たない。
  1つのmessageのレコードが区切りをまたいでも、後の区切りは増えた分だけを足すので、どのmessageも1回だけ数える。
- 数え方は閉じたときの区間のトークン数と同じ（`message.id`ごとに1回、sidechainも数える）で、modelごとの内訳はmessageを最初にmodelを名乗るレコードのmodelに入れる。
- transcriptの読み取りと解析は書き込みのロックの外で済ませ、ロックの中では区間の区切りを読み直して引き算と書き込みだけをする。
  別のsupervisorが先に同じ時刻まで区切っていれば書かない。

### 閉じたときの区間のトークン数との関係

閉じたときの区間のトークン数は、閉じるときにtranscriptを読んだ区間では`session_closed`にある。
hookが閉じた区間では`session_closed`はトークン数を持たず、supervisorの取り込みが後で書く最終の`session_turns`にある（[provider-lifecycle](provider-lifecycle.md#transcriptと稼働時間)）。
`stats`の区間のトークン数もこの順で読む（`domain::stats::sessions`）。

閉じたときの区切りまでの区切りの合計は、閉じたときの区間のトークン数と同じmessageを数える。
区切りは同じトークン数を区切りの時刻で分けたもので、別の消費ではない。
日やactorで足すときは、inboxと人のplannerの区間について区切りだけを足し、`session_closed`のトークン数も最終の`session_turns`のトークン数も足さない。
`stats`と`kpi`の期間の集計（`domain::stats::executions`）もそうしていて、区切りの無い区間（区切りが入る前に閉じたもの）は閉じたときの区間のトークン数でも補わず、期間の集計に入らない。

## statsとkpiでの集約

`stats`の期間の集計と`kpi`は、Executionと対話のsessionの区切りの記録を同じ関数（`domain::stats::executions`）で窓に集める（ADR-t1486-1決定1・3）。
taskとrunへの集約は区間のトークン数で行い（`domain::stats::tokens`）、日とactorには分けない。

- 窓に入れるのは窓の中で終わったExecutionと、区切りの時刻が窓の中の区切りで、eventを書いた時刻ではない。
  日をまたいで開いた区間は区切りごとの日に入り、閉じた日にまとめない。
  `stats`の窓は指定した始まりの時刻（無ければ窓の始まりのeventの時刻）から`host`の窓の終わりまでで、`kpi`は期間の窓の`stats`を期間の時刻で呼ぶ。
- actorはsupervisorが起動した区間のkindで、runのturnはそのrunで最後に開いたworker・resume・reviseの区間のkind、runtimeのplannerのturnは`runtime_planner`、jobはそのkind、区切りは区間のkind。
  復旧のjobは`recovery_finished`だけを数え、同じjobの`triage_*`は数えない。
- providerはeventのprovider、無ければ数えた元のprovider（区切りはClaude Code）。
  modelはmodelごとの内訳で、内訳の無いExecutionはeventのmodel（無ければ`unknown`）、内訳に入らない残りは`unknown`に入れる。
- `sessions`のkindごと・経路ごとのトークン数は、同じ窓のactorごと・経路ごとの和。
- 記録の形より前の記録（数えた元の無いturnや、トークン数の無いjobの終わり）と、区間が閉じたときのトークン数は混ぜない。
  記録の始まりと、窓のうち記録のある分は`stats`の出力が示す。
- `kpi`のトークン数の指標は、actor・provider・modelとその3つを掛けた層を出す（層と出力の形はpluginの`dagq`の`reference/kpi.md`）。
  Executionはactorとproviderの層には丸ごと、modelの層にはmodelごとの分で入る。
  記録が窓の全体を覆わない期間は値をnull（0でも判定する一部でもない）にし、`actor=`の層はそのactorの記録の範囲でも同じにする。
- runに属するExecution（workerのturn・review・復旧のjob）だけがtaskの変更の種類とrunの範囲（着地していないrunは`unknown`）の層に入り、着地あたりの分母はその層の着地。
  runを持たないExecution（plan review・goal review・observer・見直し・plannerのturn・区切り）はこの2つの層に入らず、ほかの層の分母は`all`の着地。
- 記録は計測用で助言的で、課金・認可・上限の判断に使わない（ADR-t1486-1決定6）。
  目標を置いても判断の材料にとどめる。

## 今の穴

- 子のthreadのrolloutはExecutionが始まった後に更新されたファイルから探すので、ファイルの更新時刻が書き換えられると見落とす。
- 累計に落としたturnが前のturnのrolloutから数えた分を引くとき、その分は子のthreadを含み累計は含まないので、子のthreadがあれば引きすぎうる（0は下回らない）。
- 閉じてから区切りの窓のうちにsupervisorが動かなかった区間は、最後の区切りの後の分が区切りに入らない。
- 閉じたときにtranscriptが読めない・usageを数えられない区間は閉じたときの区切りが未計測になり、開いている間の区切りがあれば最後の区切りの後の分が区切りに入らない。
  区切りのある区間は区切りだけを足すので、閉じたときの区間のトークン数でも補わない。
- 区切りの無い区間（区切りの窓のうちにsupervisorが区切らなかったもの）のトークン数は、`stats`と`kpi`の期間の集計に入らない。
- jobの終わりのeventがproviderを持たず、トークン数も数えられなかったjobは、`stats`と`kpi`でproviderが`unknown`になる。
- 死んだsupervisorのjobのうち、pidが別のprocessになったか起動時刻が読めず止めなかったものは、jobがまだ走っていれば閉じた時点までの出力で数えるので、その後の分は数えない。
- `kpi`のactor以外の層は全体の記録の範囲で判定するので、actorごとに記録の始まりが違うと、始まりの遅いactorの記録より前の分は欠けたまま値になる（`actor=`の層と内訳の記録の始まりで分かる）。
- streamの`assistant`の`usage`は生成を始めた時点の途中の値なので、トークン数には使わない（最大の入力は入力側だけなので使う）。
- Codexのturnの最初の`token_count`が前のExecutionの最後の呼び出しの`last_token_usage`を持ち越すと、その間にcompactionがあったとき最大の入力を大きく読みうる。
- `turn.completed`の累計はrolloutのレコードの合計より小さいことがある。

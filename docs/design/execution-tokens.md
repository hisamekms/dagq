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

## 概念

### 目的

supervisorは、Execution（`claude -p` / `codex exec`の1回の呼び出しで、非対話のturnの1回とheadlessのjobの1回）ごとにトークン数とcontextの大きさを記録し、日・週×actor×provider×modelで足せるようにする（[ADR-t1486-1](../adr/2026-10-04-t1486-1-supervisor-records-token-usage-per-execution.md)）。
inboxと人が開いたplannerの対話のsessionはExecutionを持たないので、supervisorがtranscriptを時刻で区切って記録する（同ADR決定3）。
区間（session span）のトークン数は[provider-lifecycle](provider-lifecycle.md#トークン数とコスト)が持つ。

### 全体の流れ

```text
turn / jobの終わり
  → providerの出力を読む（Claude: resultとstream / Codex: rollout、読めなければturn.completed）
  → turnは同じsessionの前のturnが記録した分を引いて自分の分にする（jobは毎回新しいsession）
  → turn_finished / jobの終わりのeventに同じ形で記録する（数えられなければnullと理由）
対話のsession（inbox・人のplanner）
  → 開いている間は一定の間隔で、閉じたら1回、transcriptを区切って区切りごとに記録する
stats / kpi
  → Executionと区切りを終わった時刻で窓に集め、actor×provider×modelで足す
```

### 責務と境界

- providerの出力の読みはinfrastructureのreader、turnの分の引き算と記録の形はdomain、窓への集約はdomainのstatsとkpiが持つ。
- 記録は計測用で助言的で、課金・認可・上限の判断に使わない（ADR-t1486-1決定6）。
  目標を置いても判断の材料にとどめる。
- taskとrunへの集約は区間のトークン数で行い（`domain::stats::tokens`）、日とactorには分けない。
- 親とsubagentごとの内訳は記録しない（ADR-t1486-1決定7）。

### 不変条件

- 正常な0と未計測（`null`と理由）を分け、トークン数もcontextも未計測を0にしない。
- 同じ消費を2回数えない。
  turnは同じsessionの前のturnが記録した累計か数えたroot turnを引き、jobの終わりは同じjobの終わりのeventが既にExecutionを持てば足さず、区切りは前の区切りの累計を引く。
- 差の元は記録したeventとqueueだけから組み立て、supervisorやwrapperの記憶に持たないので、引き継ぎと再起動の後も同じになる。
- runのreviewは1つの試行に全体のreviewとagentごとのjobを持ち、Executionはjobごとの終わりに記録し、全てのjobの後の終わりは全体のjobのsessionを写すだけで持たない。
  jobの行からrunと全体かagentかを見分け、jobを起動し直す終わりはその行の終わりに数えない。
- agentを起動していない終わり（起動の前に失敗したreview、プログラムのjobなど）はExecutionではない。
- 対話のsessionは区切りだけを足し、閉じたときの区間のトークン数を足さず、それで補いもしない。
  区切りは同じトークン数を区切りの時刻で分けたもので、別の消費ではない。

## 記録の形

| 知りたいこと | コードの入口 |
| --- | --- |
| 記録の形と欄の意味、未計測の理由 | `domain::tokens::ExecutionTokens`・`domain::tokens::TokenSource` |
| turnの分（累計との差、前のturnが数えたroot turn） | `domain::turn` |
| jobの終わりへの記録 | `domain::headless_job::JobSession` |
| 同じjobの終わりを重ねて数えない判定 | `domain::headless_job::JobOn` |
| runのreviewのjobと行の対応 | `domain::review_jobs::ReviewJobId::agent`・`supervise::execution::jobs::JobSubject::label`・`domain::review_jobs::runs_again` |
| 死んだsupervisorのjobの記録 | `supervise::execution::jobs::close_taken_over` |
| 前のreviewを引き継いだ側の記録 | `supervise::execution::adopt::adopted_review_failed` |

- 引き継ぎで閉じたjobの記録の置き場は[Headless job processes](supervisor-lifecycle/headless-job-processes.md#引き継ぎ)と同じ。

## Claudeの数える元

入口は`infrastructure::claude_turns`。

- トークン数は`result`の`modelUsage`（subagentを含むsessionの累計）から数え、`result.usage`（subagentを含まない）は`modelUsage`の無い古い出力のときだけ使い、subagentの分が抜けたことを理由が示す。
- jobのsessionのidとmodelは、出力からではなく開始とtranscriptから取る。

## Codexの数える元

入口は`infrastructure::codex_turns`（rolloutの読み）・`domain::tokens::RolloutUsage`（数え方）・`Codex::sessions_dir`（rolloutの場所）。

- トークン数はroot threadとその子のthreadのrolloutの応答ごとのレコードから数え、`turn.completed`のthreadの累計（子のthreadを含まない）はrolloutを数えられないときの代わりにだけ使う。
- 前のExecutionまでに数えたroot turnは`turn_finished`とjobの終わりのeventに残り、次のturnはそれを除く。
- Codexのworkerはcontainerで動かさないので、rolloutはhostの1か所から読む（[Container runtime](container-runtime.md#fail-closedの境界)）。
- 落とし穴: 子のthreadのrolloutはファイルの更新時刻で探すので、更新時刻が書き換えられると見落とす。
- 落とし穴: 累計に落としたturnが前のturnのrolloutから数えた分を引くと、子のthreadがあれば引きすぎうる（`domain::turn::thread_total_own`）。
- 落とし穴: `turn.completed`の累計はrolloutのレコードの合計より小さいことがある。

## contextの大きさとcompaction

入口は`domain::tokens::ExecutionContext`、Claudeは`infrastructure::claude_turns::ContextCounter`、Codexは`domain::tokens::TurnContext`。

- worker や job の context が大きくなりすぎていないかを読むための記録で、トークン数と同じExecutionの同じ出力から取る。
- 数えるのはトップレベルのエージェント（Codexはroot thread）の呼び出しだけで、Claudeのsubagentの呼び出しとCodexの子のthreadは混ぜない。
- 範囲はトークン数と同じく今回のExecutionの分だけで、resumeで同じrolloutに積まれた前のExecutionの分は数えない。
- 落とし穴: streamの`assistant`の`usage`は生成を始めた時点の途中の値なので、トークン数には使わず、最大の入力にだけ使う。
- 落とし穴: Codexのturnの最初の記録が前のExecutionの最後の呼び出しの値を持ち越すと、その間にcompactionがあったとき最大の入力を大きく読みうる。

## 対話のsessionの区切り

入口は`infrastructure::session_tokens`（区切りの作り方）・`domain::sessions::TOKEN_CUT_KINDS`（区切る区間）・`domain::tokens::SpanTotals`（数え方）。

- runtimeのplannerはturnのExecutionで数えるので区切らない。
- transcriptの読み取りと解析は書き込みのロックの外で済ませ、ロックの中では記録した区切りを読み直して引き算と書き込みだけをする。
- 閉じたときの区間のトークン数は`session_closed`か、hookが閉じた区間では最終の`session_turns`にある（[provider-lifecycle](provider-lifecycle.md#transcriptと稼働時間)、`domain::stats::sessions`）。
- 落とし穴: 閉じてから区切りの窓のうちにsupervisorが区切らなかった区間と、閉じたときの区切りが未計測の区間は、最後の区切りの後の分が区切りに入らない。

## statsとkpiでの集約

入口は`domain::stats::executions`（窓への集約とactor・provider・model）と`domain::kpi::window`（kpiの層）で、kpiのflagと層は`dagq kpi --help`、出力の形はJSONの出力そのもの、読み方はpluginの`dagq`の`reference/kpi.md`。

- 窓に入れるのは窓の中で終わったExecutionと区切りの時刻が窓の中の区切りで、eventを書いた時刻ではない。
- actorはsupervisorが起動した区間のkindで、区切りは区間のkind。
- 記録の形より前の記録と区間が閉じたときのトークン数は混ぜず、記録の始まりと窓のうち記録のある分は`stats`の出力が示す。
- 落とし穴: `kpi`のactor以外の層は全体の記録の範囲で判定するので、始まりの遅いactorの記録より前の分は欠けたまま値になる。
- 落とし穴: jobの終わりのeventがproviderを持たず、トークン数も数えられなかったjobは、providerが`unknown`になる。

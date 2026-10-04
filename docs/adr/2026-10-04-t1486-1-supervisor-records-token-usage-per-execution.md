---
id: adr-t1486-1
type: adr
title: actorごとのトークン消費は、supervisorがExecution（claude -p / codex exec の1回）ごとに今のturnとjobの記録に載せ、actorは起動したkindで決め、対話のsessionは一定間隔と閉じたときに区切り、podmanでも制御側が記録する。利用枠とdagqの外のsessionは記録せず、記録は助言的に使う（ADR-0048決定9をamends）
status: accepted
created: 2026-10-04
updated: 2026-10-04
accepted_on: 2026-10-04
amends:
  - adr-0048 decision 9
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - provider
  - operations
related:
  - adr-0048
  - adr-t813-2
  - adr-t1233-1
  - adr-t655-1
  - adr-t1091-1
  - design-provider-lifecycle
  - design-supervisor-lifecycle-stats
---

# ADR-t1486-1: actorごとのトークン消費はsupervisorがExecutionごとに記録する（ADR-0048決定9をamends）

## Context

2026-10-02のClaudeとCodexのトークン消費の増加を2026-10-03に人が開いたsessionで調べたところ、dagqの記録では日ごと・actorごとの比較ができなかった（goal 95）。見つかった穴は次のとおり。

- 区間（[ADR-0048](0048-record-claude-sessions-by-kind-with-open-and-active-time.md)決定1）のトークン数は区間が閉じたときに記録し、`stats`は閉じた日に数える。長く開く対話のsession（inbox）は閉じた日にまとめて数えられ、ある日に数日分が載る。
- Claudeの非対話のturnは`result.usage`から数えていて、subagentの分が抜ける。subagentを含む`modelUsage`は`total_cost_usd`と同じくsessionの累計である（task 1199）。
- Codexのjob（review・plan review・goal review）はトークン数を記録していない。Codexのworkerはthreadの累計の`usage`から数え、multi-agentの子のthreadの分が入らない可能性がある。
- stream-jsonの`assistant`の`usage`は生成を始めた時点の途中の値で、turnより細かい内訳には使えない。

ADR-0048決定9は、task 199のトークン数を区間ごとにtranscriptのportから数えると決めた。transcriptが無いCodexと、transcriptを読まずに出力から数える非対話の経路（[ADR-t813-2](2026-09-28-t813-2-provider-per-task-and-mutual-fallback.md)決定7）が増え、区間とtranscriptだけでは全てのactorを同じ形で数えられない。今の数える元と欄は[provider-lifecycle](../design/provider-lifecycle.md)の「トークン数とコスト」と[stats](../design/supervisor-lifecycle/stats.md)の「トークン数」が持つ。

## Decision

1. **計測の単位はExecutionにする。** Executionは`claude -p` / `codex exec`の1回の呼び出しで、dagqの非対話のturnの1回とheadlessのjobの1回にあたる。新しいentityは作らず、今のturnとjobの記録に載せる。task・run・日・actorへの集約は`stats` / `kpi`の層で行う。区間（ADR-0048決定1）は開いている時間と稼働時間の単位として残る。
2. **記録するのはsupervisor（制御側。wrapperを含む）だけにする。** job・inbox・人はdagqの記録（`stats` / `kpi`）を読むだけで、トークン数を書かない。actorはpromptの文面ではなく、supervisorが起動したkindで決める。
3. **全actorで記録の形を揃え、きっかけは2通りにする。** 非対話（workerのturn・全てのheadlessのjob・runtimeのplanner）はExecutionが終わるたびに記録する。対話（inboxと人が開いたplanner）は、一定間隔（毎時など）と閉じたときに、transcriptを前回読んだ位置から読み足して区切りごとに記録する。日への振り分けはExecutionが終わった時刻（対話は区切りの時刻）で行い、長く開くsessionを閉じた日にまとめない。
4. **podman（[ADR-t1233-1](2026-10-02-t1233-1-control-and-execution-sides-queue-service-broker-and-client-mode.md)）に移っても、記録の主体はsupervisorのままにする。** agentは実行側、wrapperは制御側にあり、実行側のファイルに頼るのはCodexのrolloutだけにする。コンテナではrolloutをrun dir（実行側に見せる範囲）に書かせ、Executionが終わった後に制御側が読む。
5. **利用枠とdagqの外のsessionは記録しない。** 利用枠（rate_limits）はアカウント全体の値で、dagqの外の使用も混ざるので記録しない。処理したトークン数を利用枠の消費と同じものとして扱わない。dagqの外のsession（人がcheckoutで直接開いたsessionなど、dagqが起動していないsession）は対象にしない。
6. **記録は計測用で助言的（advisory）にする。** 数える元は実行側が書き換えうるので、課金や認可に使わない。記録に失敗してもrun・job・sessionの結果は変えない（ADR-0048決定10と同じ）。
7. **範囲の外**: 親とsubagentごとの内訳（Claudeではtranscriptが要り、コンテナでは読めないので、subagentの数だけを記録する）、利用枠、Codexのcostの見積もり。

ADR-0048決定9のうち「task 199のトークン数は区間の鍵でtranscriptのportから数える」は、上の決定1〜3に改める。transcriptの読み取りを1つのモジュールにまとめること、読めない版の検出、対話の区間のトークン数をtranscriptから読むことは変えない。

providerごとのExecutionの数え方・eventの欄・`stats` / `kpi`の形は、後続のtaskが[docs/design/](../design/)に書く。

## Alternatives

- **区間の`tokens`のまま数える**: 閉じた日にまとめて数え、transcriptの無いCodexのjobを数えられない。日とactorで比べる目的に合わない。
- **Executionを新しいentity（表）にする**: turnとjobの記録が既に1回の呼び出しの単位を持ち、別の表は同じものの二重の記録になる。
- **job・inbox・agent自身に報告させる**: actorが自分の消費を書くと、promptの文面や書き忘れで抜け、書き換えもしやすい。起動したsupervisorが書けば、actorはkindで決まる。
- **stream-jsonの`assistant`の`usage`を足す**: 途中の値で、turnの合計にも内訳にも合わない。
- **rate_limitsも記録する**: dagqの外の使用が混ざり、actorに振り分けられない。
- **コンテナの中からqueueに書かせる**: 実行側から制御側へ書く経路を増やし、ADR-t1233-1の境界に反する。

## Consequences

- 日・週×actor×provider×modelと着地1件あたりで、同じ形の記録から比べられる。今の区間の`tokens`は残るが、日とactorの比較はExecutionの記録で読む。
- 恒久の記録が入るまでは、hostの集計script（task 1485）で過去と今を比べる。scriptは恒久の側が入ったら外す。
- Claudeのsubagentを含む値はsessionの累計なので、前のturnとの差を取る必要がある。Codexはrolloutを残すため、計測するExecutionではrolloutを残さない起動を使えない。
- 親とsubagentの内訳は見えない。subagentの増減はその数で読む。
- 実行側が数える元を書き換えれば記録も変わるので、記録を課金・認可・上限の判断に使う変更は新しい決定が要る。

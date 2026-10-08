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
  - design-provider-lifecycle
  - design-supervisor-lifecycle-headless-worker
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
| turnの分（累計との差） | `domain::turn::turn_own_models`・`turn_own_cost`、`headless_session.rs`の`turn_tokens` |
| jobの終わりへの記録 | `AgentProvider::job_session`、`domain::headless_job::JobSession::record` |

## 記録の形

turnの`turn_finished`とjobの終わりのeventは、providerによらず同じ欄を持つ。

| 欄 | 中身 |
| --- | --- |
| `tokens` | Executionの分で、全てのmodelとsubagent・子のthreadを足したもの |
| `tokens_by_model` | modelごとの同じ数。providerがmodelごとに出さなければ空 |
| `tokens_source` | 数えた元 |
| `tokens_reason` | 数えられなかった理由か、一部が抜ける元で数えた理由 |
| `children` | Executionが起こしたsubagent（子のthread）の数。providerが言わなければ`null` |

- 正常な0は`tokens`の各数が0のobjectで、未計測は`tokens`が`null`で`tokens_reason`を持つ。
- 非対話の区間は`tokens`が`null`のturnを足さない。
- 親とsubagentごとの内訳は記録しない（ADR-t1486-1決定7）。

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
  subagentの分が抜けるのはこのときだけ。
- subagentの数は`subagent_stats`から取り、sessionの累計ではなくturnの分である。
- jobは`--output-format json`で起動し、返答もその`result`から読む。
  jobのsessionのidとmodelは、出力からではなく開始とtranscriptから取る。

## 今の穴

- Codexの非対話のturnはthreadの累計からの差で、multi-agentの子のthreadの分が入らない可能性がある。
- Codexのjobはトークン数を記録しない。
- hookの区間（inbox・人のplanner）は閉じた後にだけ記録するので、長く開いたinboxは閉じた日にまとめて数えられる。
- streamの`assistant`の`usage`は生成を始めた時点の途中の値なので使わない。

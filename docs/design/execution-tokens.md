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
| jobの終わりへの記録 | `AgentProvider::job_session`（Codexは`codex.rs`の`job_tokens`）、`domain::headless_job::JobSession::record` |

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
  Claudeでsubagentの分が抜けるのはこのときだけ。
- subagentの数は`subagent_stats`から取り、sessionの累計ではなくturnの分である。
- jobは`--output-format json`で起動し、返答もその`result`から読む。
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

## 今の穴

- 子のthreadのrolloutはExecutionが始まった後に更新されたファイルから探すので、ファイルの更新時刻が書き換えられると見落とす。
- 累計に落としたturnが前のturnのrolloutから数えた分を引くとき、その分は子のthreadを含み累計は含まないので、子のthreadがあれば引きすぎうる（0は下回らない）。
- hookの区間（inbox・人のplanner）は閉じた後にだけ記録するので、長く開いたinboxは閉じた日にまとめて数えられる。
- streamの`assistant`の`usage`は生成を始めた時点の途中の値なので使わない。

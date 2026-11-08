---
id: plan-ledger-push-records-spike
type: plan
title: スパイク：ledger_push_records の 1 回約 30 秒と起動ごとの 3 回の内訳を測り、原因と減らし方を判定する
status: completed
created: 2026-11-08
owners:
  - hisamekms
tags:
  - planning
  - performance
  - measurement
related:
  - design-measurement
  - plan-ledger-row-reads-spike
  - plan-supervisor-startup-memory-spike
---

# スパイク：ledger_push_records の時間と回数の内訳を測る

task 4448（goal 266 の受け入れ 5）。問いは「本番で起動ごとに 3 回、1 回約 30 秒かかる ledger_push_records は、文の plan・読む行の量・答えの変換と転送のどれが原因で、3 回は要るか」。合成の queue で文の plan を読み、文の実行・答えの組み立て・転送を別々に測り、起動の中の要求を呼び手の印ごとに記録した。runtime（`src/`）は変えていない。本番の DB と host のログは読んでいない。

## 判定

**unresolved**。合成の queue では、要求の時間のほぼすべてが文の実行で、文の時間は 1 行の記録の数のおよそ 2 乗で増え、3 回は 1 つの呼び手のページだった。しかし、問いが聞く本番の 1 回約 30 秒の原因は、この根拠からは決まらない。理由は 2 つ:

- 合成で本番の 30 秒の桁を再現できたかが決まっていない。合成の 1 行 27 件では 1 要求 0.5〜2.6 秒で 30 秒に届かず、1 行の記録を増やせば届き得るが、本番の走査する run の行の数と 1 行の記録の数を読んでいないので、本番がその範囲にあるかは分からない（下の「桁」）。桁が再現しないと示せたわけでもないので、task 4448 の定義の does_not_hold（合成で桁が再現しない）ではなく、決められない unresolved とする。
- service の中の待ちを測っていない。本番の 30 秒が service の受けてから答えるまでの時間なら、読みの接続の待ちなども入り得る。答えの組み立てと転送が小さいことはこれらの段階を除くだけで、待ちを除かないので、本番の主の段階を文の実行と特定できない（下の「測っていないこと」）。

合成で確かめたことは下の根拠のとおりで、「推奨」の直しの案は合成で確かめた文の問題への案として残す。本番の主因がそこにあるかは、下の「判定を決めるのに足りない本番の記録」を inbox か人が読んで決める。

根拠（合成で確かめたこと）:

- **段階**: 合成のどの大きさでも、要求の時間のほぼすべてが文の実行だった（下の「表」）。答えの組み立て（JSON の parse と cut、ページの選び出し）と転送（答えの text の書き出しと読み戻し）は合わせて文の実行の 2 % 未満（957〜3,828 件で 5〜30 ms）。満杯の 1 ページ（1,000 件）の答えは約 200 KB で、転送は桁に効かない。
- **文の plan**: 下の「plan」のとおり、run の行を `(kind, key)` の主 key で選んだ後、各行の `records` を `json_each` で全部展開し、`push_failed` と `run_phase_changed` の記録の 1 件ごとに `EXISTS` の相関副問い合わせ（`CORRELATED SCALAR SUBQUERY`）が同じ行の `records` をもう一度全部走査する。統計（`sqlite_stat1`）の有無で変わる選択は無い（表は主 key の 1 本だけ、残りは virtual table）。
- **文の時間の主因は相関副問い合わせ**: 合成の queue で、同じ答えの `push_finished` だけを選ぶ文は今の文の 1/10〜1/27、`push_failed` を持つ行を相関なしの副問い合わせで 1 回だけ選ぶ文は 1/3〜1/7 だった。1 行の時間は行の記録の数のおよそ 2 乗で増える: 同じ 957 行の `records` を 2 倍・4 倍に複製すると（27・54・108 件／行）、文は 737・1,898・7,669 ms（2.6 倍、さらに 4.0 倍）。一方、記録の数を変えず payload だけ大きくした（行の text 2 倍）queue では時間は増えなかった。`run_phase_changed` は run ごとに何度も出るので、副問い合わせの評価の回数は（phase の記録の数）×（行の記録の数）になる。
- **桁**: 合成の 1 行 27 件では 1 要求 0.5〜2.6 秒（下の「起動の中の要求」の和÷要求の数）で、本番の 30 秒に届かない。1 行 108 件に複製した queue では 957 行で 7.7 秒（1 行 8.0 ms）で、この 1 行の時間なら 1 要求 30 秒は走査する run の行の約 3,700 行に当たる。本番の走査する行の数と 1 行の記録の数は読んでいないので、本番の桁が合うかはこの文書では決まらない。ページの数（下の「3 回」）は返る記録の数を数えるもので、走査する行の数を与えない（push の記録を返さない行も走査され、`push_failed` を持つ行は 1 行から `push_failed` と複数の `run_phase_changed` を返す）。合成で示せたのは、文の時間が走査する行の数に比例し、1 行の記録の数のおよそ 2 乗で増えることと、文の外の段階（答えの組み立てと転送、どの大きさでも記録 1 件あたり 0.02 ms 未満）は本番の 3 回が返す記録の数（下、多くて 2,999 件）でも 0.1 秒に届かないことまで。本番の cut した記録 1 件の大きさが合成と同じ桁なら、文の外のこの 2 つの段階は本番の 30 秒の主ではない。残る文の実行と service の中の待ちのどちらが主かは、service の中の待ちを測っていないので決まらない（下の「測っていないこと」）。
- **3 回**: 起動の中の `ledger_push_records` の要求は、どの大きさでもすべて `ledger.restore_pushes`（`start_builder` の中で 1 回だけ呼ばれる `restore_pushes`）から来た。supervisor の queue の `ledger_pages`（`src/application/supervisor_service/forward.rs`）は 1,000 件（`LEDGER_PUSHES_PAGE`）のページで聞き、満杯のページの後はもう 1 回聞いて、満杯でないページで止まる。上限超えの聞き直しが無ければ、要求の数は floor(返る記録の数 ÷ 1,000) + 1 回で、合成では 957 件で 1、1,914 件で 2、3,828 件で 4 回だった。返る記録がちょうど 1,000 の倍数のときは最後が空のページになる（2,000 件なら空の 3 回目を含めて 3 回）。答えが上限（`MAX_ANSWER_BYTES`、16 MiB）を超えた要求は `response_too_large` で返り、`ledger_pages` はページを 1/4 にして同じ位置から聞き直すので、その失敗した要求も要求の数に入る。したがって本番の 3 回は、聞き直しが無ければ返る記録 2,000〜2,999 件（2,000 件ちょうどなら 3 回目は空のページ）、1,000 件のページが上限を超えて 250 件で聞き直したなら失敗の 1 回と 250〜499 件の 2 ページに当たる。合成の満杯のページは約 200 KB で上限の 1/80 ほどなので、聞き直しには cut した記録 1 件が平均 16 KB を超える必要があり、前者と見込む（本番の答えの bytes は読んでいない）。どちらでも 3 回は 1 回の呼び出しのページで、別の呼び手ではない。service の `ledger_push_records` は、空のページと上限を超えて返る要求も含めて、要求ごとに store の読み全体（文の実行と全件の parse）をやり直し、`after` と `page` で絞るのはその後（`src/application/supervisor_service/dispatch.rs`）。ページは答えの上限に収めるために要るが、ページごとに全体を読み直すことは要らない。起動 1 回の時間は（走査する行の数）×（要求の数）で、どちらも履歴に比例するので履歴の 2 乗で増える。

計画への意味: 判定が unresolved なので、この Spike だけでは goal 266 の受け入れ 5 の「判定が直しを求める」には当たらない。直しの task を足すか、判定を holds に改めるかは、下の「次の判定の手順」で inbox か人が本番の記録を読んで決める。直しを足すなら「推奨」の 1 と 2 で、どちらも store・service・supervisor の queue の読みの中で閉じ、起動の追いつきの作り（goal 241）は変えずに済む。`restore_pushes` を起動ごとに 1 回呼ぶこと自体は今の判断のままでよい。

## 判定を決めるのに足りない本番の記録

本番の 30 秒の原因を決めるには、次の記録が要る（worker は本番のログを読まない）。

- service のログ（`logs/service-*.jsonl`）の周期の集計の `by_caller` の `ledger_push_records` の、1 回の所要・答えの bytes・回数。回数と bytes から、上の「3 回」の数え方で返る記録の数と聞き直しの有無が分かる。
- 走査する run の行の数か、1 行の記録の数の手がかり。例: 本番の queue の台帳の run の行の数と、その行の `records` の記録の数の分布を読む読み取りのコマンドの出力か、同じ起動で記録された run の数と run ごとの event の数。合成の 1 行の時間（上の「表」）と掛け合わせて、文の実行だけで 30 秒の桁に届くかを見る。
- service の中の待ちと文の実行を分ける記録。service のログが要求の受けから答えまでの所要だけを持つなら、読みの接続を得るまでの待ちと文の実行の時間を分けて記録したもの。分けた記録が無ければ、待ちが 0 に近い時間帯（ほかの読みの要求が無い時）の 1 回の所要でもよい。

## 次の判定の手順

1. inbox か人が、上の記録を本番のログと queue から読み、note に残す。
2. 文の実行が 1 回の所要のほぼすべてで、走査する行の数と 1 行の記録の数が合成の 1 行の時間で 30 秒の桁を与えるなら、この文書の判定を holds に改め、「推奨」の 1 と 2 の直しの task を goal 266 に足す（着地済みのものは足さない）。
3. 文の実行が 1 回の所要のほぼすべてでも、走査する行の数と 1 行の記録の数が合成の 1 行の時間で 30 秒の桁に届かないなら、合成で桁が再現しないので判定を does_not_hold に改め、本番の 1 行の時間が合成より長い理由を問う別の Spike を goal 266 に足すかを planner が決める。
4. 1 回の所要の主が service の中の待ちなら、判定は unresolved のままにし（文の問いの答えは変わらない）、待ちの原因を問う別の Spike を goal 266 に足すかを planner が決める。
5. 分けた記録が service のログに無く 2〜4 のどれにも決まらないなら、待ちと文の実行を分けて記録する task を足し、その着地の後に 1 からやり直す。

## 推奨

合成で確かめた文の問題と呼び方の問題への直しの案。本番の主因かは上の「次の判定の手順」で決める。

1. **文の直し**: `push_failed` を持つかを、記録の 1 件ごとではなく行ごとに 1 回だけ決める。合成で測ったのは相関なしの副問い合わせで `push_failed` を持つ行の key を 1 回選ぶ形（下の「打ったもの」の `failed rows picked once`）で、今の文の 1/3〜1/7。行ごとの印を派生表で 1 回だけ計算する形も同じ狙いで、どちらを取るかは実装の task で plan と時間を比べて決める。答えの記録・順・cut は変えない。
2. **呼び方の直し**: ページを記録の id ではなく run の行の key で切り、各ページが主 key の範囲の行だけを読むようにする（例: run の行を `r.key > ?` で `ORDER BY r.key LIMIT ?` と行の数で選ぶ副問い合わせから、その行の記録を返す。LIMIT は記録ではなく行を数える）。答えには記録のほかに、読んだ行の数と最後の行の key を持たせる（push の記録を持たない行があっても、`ledger_pages` の「読んだ数がページより少なければ終わり」と続きの位置が狂わないように）。記録の id の昇順は、全ページを集めた後に supervisor の queue の側で並べる（`restore_pushes` に渡す前）。こうすると起動 1 回の読みは run の行を 1 回通るだけになる。1 ページの答えは行の記録の和で、今の「上限を超えたら 1/4 で聞き直す」（`ledger_pages`）は 1 ページの行を減らすだけなので、行の数で切るだけでは上限を保証できない。`ledger_pages` は 1 ページ 1 行でも上限を超えるとエラーを返し、`push_failed` を持つ run の行はその行の `run_phase_changed` を全部返すので、記録の多い 1 行が答えの上限（`MAX_ANSWER_BYTES`）を超えると、今の記録単位のページでは読めるものが行単位のページでは読めなくなる。このため、続きの位置を（最後の行の key、その行の中の最後の記録の id）の組にして、1 行の記録が 1 ページに収まらないときは同じ行の続きから次のページを読めるようにする。行の中の続きは、その行の記録だけを id で絞って読むので、全部の行を読み直すことにはならない。1 行の記録の上限と、行の中で切る大きさは、実装の task で答えの上限から決める（合成で測った答えは満杯の 1 ページ（1,000 件）で約 199〜201 KB、`push_failed` の行は測っていない）。
3. 1 と 2 は台帳の行の形・ledger_v を変えずに済む。変えるのは読みの文（SQLite と Postgres）、store の port `LedgerStore::ledger_push_records` のページで読む形、service の use case `ledger_push_records` の引数と答えとページの切り方、supervisor の queue（`forward.rs`）のページの続け方と最後の並べ替えで、台帳の行は読むだけ（読みの答えの意味、つまり返す記録の集合と各記録の cut は同じ）。
4. **確かめ**: 実装の task は、この文書の example の `pushes` で同じ 3 段階の queue（と `records` を複製した queue）を測り直し、文が `push_finished` だけを選ぶ文と同じ桁になること、起動の要求の数が増えても 1 要求が全体を読まないことを確かめる。この文（`ledger_rows` の `json_each` の相関副問い合わせ）に plan の検査を当てるかも同じ task で決める。
5. **本番での確かめ（inbox か人）**: 着地と自動更新の後に、service のログの `by_caller` で `ledger_push_records` の呼び手が `ledger.restore_pushes` だけで、1 回の所要が下がったかを読む（goal 266 の受け入れ 6）。

## 環境

- 対象の commit: `b41b55f28e474b4c7c40b5390c12ac2bc745a750`（task 4445 の呼び手の印を含む main）に、この文書の example の変更を足したもの。
- 道具: rustc 1.99.1 (58cf8f9c3 2026-10-13)、cargo 1.99.1、`--release` の build。SQLite は rusqlite 0.40（libsqlite3-sys 0.38.2）の bundled の 3.53.2。`records` を複製した確かめだけ host の sqlite3 3.54.0 の CLI。
- host: macOS 27.2（26B123）、MacBook Air（M1、8 core、16 GiB）、arm64。dagq の worker の run の worktree の中。測った間、host は他の run で load average 14〜19 だったので、絶対値は周回ごとに 2 倍ほど揺れる。比（段階の割合、文どうしの比、要求の数）は揺れに左右されない。
- provider: Claude Code（`claude-opus-5-5`）の headless の worker の session。

## 打ったもの

計装は [examples/supervisor_startup_memory.rs](../../examples/supervisor_startup_memory.rs) の `pushes` だけにある。`prepare` が置いた queue（前の supervisor が最新まで台帳を作った store。`SqliteQueue::init` の schema で、`sqlite_stat1` は無い）を写し、次を大きさごとに 3 回流して中央値を出す。

1. 文の実行: `SqliteQueue` の `ledger_push_records` と同じ文（SQLite の形を `PUSH_RECORDS_SQL` に書き写したもの）を新しい接続で流し、答えの行を text のまま全部読み出す。書き写しが store の文と同じことは、store の読みの答えと一致することで毎回確かめる。
2. 答えの組み立て: 各行を `RunEvent` に parse して cut し、service と同じくページ（1,000 件）ごとに全件から選び出して JSON の値にする。
3. 転送: 各ページの答えを text に書き出し、読み戻して `RunEvent` の列にする（socket と supervisor の queue がすること）。
4. 比べる文: `push_finished` だけを選ぶ文と、`push_failed` を持つ行を相関なしの副問い合わせで選ぶ文（`PUSH_RECORDS_VARIANTS`）。
5. 起動の要求: 写した queue に、service を通る台帳の係（`SupervisorPorts` で包んだ `SupervisorQueues`、service はこの process の中で request と答えを socket と同じく text にして答える）で `start_builder` を流し、`ledger_push_records` の要求ごとに呼び手の印・時間・答えの bytes を記録する（`CountedCalls`）。

```sh
cargo build --release --example supervisor_startup_memory
B=target/release/examples/supervisor_startup_memory
for n in 100000 200000 400000; do
  $B prepare "$TMPDIR/q$n" $n
  $B pushes "$TMPDIR/q$n"
done
$B prepare "$TMPDIR/q200kpad" 200000 300   # payload を約 4 倍にした queue（行の text 2 倍）
$B pushes "$TMPDIR/q200kpad"
```

1 行の記録の数による伸びは、100,000 event の queue の写しの run の行の `records` を SQL で 2 倍・4 倍に複製し、host の sqlite3 で同じ文（数えるだけ）を 3 回ずつ流して測った。

```sh
DUP="UPDATE ledger_rows SET records=(SELECT json_group_array(json(v)) FROM
  (SELECT value v FROM json_each(records) UNION ALL SELECT value FROM json_each(records)))
  WHERE kind='run';"
sqlite3 k2.db "$DUP"; sqlite3 k4.db "$DUP $DUP"
sqlite3 k1.db ".timer on" "SELECT count(*) FROM (<PUSH_RECORDS_SQL の引数を埋めたもの>);"
```

## plan

`SqliteQueue::init` の schema（`sqlite_stat1` なし）での `EXPLAIN QUERY PLAN`。どの大きさでも同じ。

```text
SEARCH r USING INDEX sqlite_autoindex_ledger_rows_1 (kind=?)
SCAN j VIRTUAL TABLE INDEX 1:
CORRELATED SCALAR SUBQUERY 1
  SCAN f VIRTUAL TABLE INDEX 1:
USE TEMP B-TREE FOR ORDER BY
```

## 表

行の数は run の行（= push の記録の数。合成の run は `push_failed` を持たない）。ms は大きさごとの 3 回の中央値。1 行の記録は 27 件、行の text は平均 8.4 KB（payload を大きくした queue は 16.8 KB）。

| 行の数 | 段階 | 3 回の中央値の ms | ms÷行の数 |
| ---: | --- | ---: | ---: |
| 957 | 文の実行 | 850.9 | 0.889 |
| 957 | 答えの組み立て | 2.6 | 0.003 |
| 957 | 転送 | 2.3 | 0.002 |
| 1,914 | 文の実行 | 1,774.5 | 0.927 |
| 1,914 | 答えの組み立て | 21.9 | 0.011 |
| 1,914 | 転送 | 7.4 | 0.004 |
| 3,828 | 文の実行 | 3,319.7 | 0.867 |
| 3,828 | 答えの組み立て | 7.1 | 0.002 |
| 3,828 | 転送 | 6.9 | 0.002 |

同じ queue での比べる文（3 回の中央値の ms、括弧は ms÷行の数）:

| 行の数 | 今の文 | `push_finished` だけ | `push_failed` の行を 1 回で選ぶ |
| ---: | ---: | ---: | ---: |
| 957 | 850.9（0.889） | 31.0（0.032） | 115.4（0.121） |
| 1,914 | 1,774.5（0.927） | 128.1（0.067） | 451.1（0.236） |
| 3,828 | 3,319.7（0.867） | 303.4（0.079） | 1,131.7（0.296） |
| 1,914（payload 約 4 倍） | 1,514.5（0.791） | 142.9（0.075） | 344.7（0.180） |

1 行の記録の数による伸び（957 行、host の sqlite3、3 回の中央値）:

| 1 行の記録 | 行の text の平均 | 今の文の ms | ms÷行の数 |
| ---: | ---: | ---: | ---: |
| 27 | 8.4 KB | 736.8 | 0.770 |
| 54 | 16.9 KB | 1,897.6 | 1.983 |
| 108 | 33.7 KB | 7,668.9 | 8.013 |

起動の中の `ledger_push_records` の要求（service を通る `start_builder`、3 回の中央値の ms）:

| 行の数 | 要求の数 | 呼び手 | 1 要求の答えの bytes | 要求の和の ms |
| ---: | ---: | --- | --- | ---: |
| 957 | 1 | `ledger.restore_pushes` | 190,229 | 484.5 |
| 1,914 | 2 | `ledger.restore_pushes` | 198,830・183,715 | 3,359.2 |
| 3,828 | 4 | `ledger.restore_pushes` | 198,830・201,001・201,001・166,429 | 10,346.9 |
| 1,914（payload 約 4 倍） | 2 | `ledger.restore_pushes` | 198,830・183,715 | 4,357.7 |

要求の和は、どの大きさでも（要求の数）×（文の実行）の 0.6〜1.5 倍で同じ桁にあり、ページごとに文をやり直していることと合う（差は host の揺れの幅に入る）。

## 3 回の呼び手

- 呼び手は `ledger.restore_pushes` の 1 つだけ。`restore_pushes` は `start_builder` が R から続けるときに 1 回だけ呼び、その中で `ledger_push_records` を 1 回呼ぶ。supervisor の queue（`src/application/supervisor_service/forward.rs`）がそれを 1,000 件ずつのページの要求に分けるので、本番の 3 回はこの 1 回の呼び出しのページ（空の最後のページと上限超えの聞き直しを含み得る。要求の数の数え方は上の「判定」の「3 回」）。
- 1 回の呼び出し（restore_pushes の読み）は要る: 印から続ける起動が push の要約を作り直すのに、確定済みの run の行の push の記録を読む（`restore_pushes` の doc comment）。
- 3 ページに分けることは答えの上限のために要るが、ページごとに全部の行を読み直して文を流すことは要らない（推奨 2）。

## 測っていないこと

- `push_failed` を持つ run の行。合成の `prepare` は push が失敗した run を置かないので、`push_failed` の行で答えに入る `run_phase_changed` の量と、その分の組み立てと転送は測っていない（副問い合わせの評価の回数は `push_failed` の有無によらず `run_phase_changed` の数で決まるので、文の時間の判定は変わらない）。
- 本番の走査する run の行の数と 1 行の記録の数。本番の 3 回から分かるのは返る記録の数の幅だけで、走査する行の数は分からないので、本番の 30 秒を合成の 1 行の時間で再現できるかは決まっていない。
- service の中の待ち。本番の 1 回約 30 秒が service の受けてから答えるまでの時間なら、読みの接続の待ちなども入り得るが、合成の service はこの process の中で 1 つずつ答えたので測っていない。
- 実の socket の転送。service はこの process の中で request と答えを text にして答えた。1 ページ約 200 KB なので、socket を通しても桁は変わらないと見込む。
- Postgres の文。

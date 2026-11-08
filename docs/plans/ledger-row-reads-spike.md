---
id: plan-ledger-row-reads-spike
type: plan
title: スパイク：起動ごとの ledger_row の要求を呼び手ごとに数え、回数が確定済みの履歴に比例する呼び手と減らし方を判定する
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
  - plan-supervisor-startup-memory-spike
---

# スパイク：起動ごとの ledger_row の要求を呼び手ごとに数える

task 4447（goal 266 の受け入れ 4）。問いは「起動から 15 分の ledger_row の要求（本番の起動ごとに 22〜43 万回）は、どの呼び手から来て、確定済みの履歴の量に比例するか」。合成の store で、台帳の係の起動から追いつきの終わりと、その後の 15 分（1 分ごとの pass）までの ledger_row の要求を、呼び手の印（`caller::scope` の名）ごとに数えた。実時間ではなく回数を比べる。runtime（`src/`）は変えていない。本番の DB と host のログは読んでいない。

## 判定

**holds**（条件つき）。比例する呼び手は `ledger.fold_rows` で、減らし方も立つ。ただし比例するのは、台帳の係が確定済みの履歴を cursor の始めから読み直す起動のとき（測定）と、R が最新より遠く手前にある印からの起動のとき（コードからの推定）で、R が最新にある印からの起動では呼び手によらず回数は一定（4 回）だった。

根拠:

- 印からの起動（`start_builder` が R から続ける、R が最新の event にある store）では、履歴を 5,029・20,079・80,061 event と 4 倍ずつにしても、ledger_row は `ledger.fold_and_write` の 4 回だけで変わらない。`start_builder` の中と、追いつきの後の 15 回の pass では 0 回。合成の store は行がすべて確定済みなので、`start_builder` の中の 1 行ずつの読み（`ledger.books_of_heads`・`ledger.restore_sessions`・`ledger.restore_pushes`・`ledger.run_and_task_records`）は 0 回になる。これらは開いた行と要る行の数で増え、確定済みの履歴では増えない。
- 同じ store を cursor の始めから読む起動（印が無い・R の印が無い・印の ledger_v が今の LEDGER_V と違う（LEDGER_V を上げた後の最初の起動）・head が今より新しい ledger_v を持つ・W が R を切るとき。古い ledger_v の行があるだけなら R から続ける）では、`ledger.fold_rows` が 2,945・11,777・46,979 回で、履歴の行に 0.967〜0.969 回／行、event に 0.586〜0.587 回／event とほぼ一定の比で増える。回数は store の queue の行と session の行の和（2,944・11,773・46,958）に 1〜21 回を足した値と合う。`fold_rows` の掃き（`Builder::rows_to_sweep`）は、まだ掃いていない builder の最初の pass で、event の 1 ページ（`EVENTS_PER_READ` = 5,000 event）ごとの `sweep_page` と pass の終わりに、そのページで読んだ行のうち印のある session の行と入力の未確認の queue の行の 1 行ごとに `ledger_row` を 1 回読む。cursor の始めから読んだ builder では、確定済みの queue の行と session の行がすべてこれに当たり、後のページが変えた行は掃き直される（行の和より 1〜21 回多い分）。
- 同じ起動の `ledger.fold_and_write` は 0.020 回／event、`ledger.restore` は 0.0005 回／event でどちらも比例するが、`fold_rows` の 1/30 以下。合成の store は行がすべて今の版で書かれているので、LEDGER_V を上げた後の起動（行がすべて古い）では `fold_and_write` はこれより多くなり得る（この値は下限）。
- 本番の桁との突き合わせ: 行の種類の割合によらず持ち越せるのは 0.97 回／掃いた queue と session の行の比で、これを当てると 22〜43 万回はおよそ 22〜43 万行の掃きに当たる（event への換算は合成の 1 event あたりの行の割合に依るので出さない）。R が最新にある印からの起動ではこの桁にならない。したがって、本番の 22〜43 万回が台帳の係から来ているなら、その起動は cursor の始めから、または R の遠く前から読み直していたことになる。台帳の係の外の呼び手（判定の観測の `live_alerts`、`ledger_window` の時刻からの読み）は数えていないので、本番の回数が台帳の係から来るかは合成からは決まらない。task 4569（R を押さえる印を毎 pass 先に調べ、older_version の queue の印で R が止まらないようにした修正）の前は R が止まり得たので、R の遠く前からの起動があり得たことは下の推定と矛盾しないが、その経路は測っていない。どの呼び手と経路だったかは本番の記録で確かめる（下の「推奨」）。
- R が最新より手前にある印からの起動（R が遅れているとき）は測っていない。コードの上では、最初の pass の掃きが R の後のページで読んだ行のうち印のあるものを 1 行ずつ読むので、回数は R の後に触れた queue と session の行の数に比例すると見込む（推定で、測定ではない）。

計画への意味: 比例する呼び手は `ledger.fold_rows` の掃きの 1 行ずつの読みで、これを束ねて読む実装の task を planner が足せる。起動がどちらの経路を通るか（R が止まらないこと、古い行があるときに cursor の始めから読むこと）は起動の追いつきの作りで、goal 241 の側。

## 推奨

1. **`ledger.fold_rows` の掃きの読みを束ねる。** 今は掃く行の share（`CHECKS_PER_PASS` = 2,000 行ずつ）の中で `rows_to_sweep` の 1 行ごとに `ledger_row(kind, key)` を 1 要求で読む。share の行をまとめて 1 要求で読む読み（kind と key の組の一覧を受けて、その行を返す LedgerStore の読みを 1 つ足し、service の use case 1 つで答える）に替えれば、掃きは event のページごとに走るので、要求の数はページごとの share の数（最初の pass で少なくとも event の数 ÷ 5,000）に落ちる。読む行・掃きの判定・書く行は変わらないので、台帳の行の形と ledger_v は変えずに済む。1 要求の大きさは share の行の records の和になるので、束ねる大きさは service の 1 回の答えの上限（`supervisor_service::large`）の内に収める。
2. **その task の確かめ**: 履歴を 2 段階に変えた store で、cursor の始めからの起動の ledger_row（束ねた読みを含めた要求の数）が履歴に比例して増えないことを、この文書の example の `callers` か library の test で確かめる。
3. **本番での確かめ（inbox か planner）**: 呼び手の印が本番に入った後の起動の service のログで、`top_by_calls`・`by_caller` の ledger_row の主が `ledger.fold_rows` か、その起動が R の遠く前から読んだか（task 4569 の後で回数が既に減ったか）を読む。主が `ledger.fold_rows` でなければ、この判定は本番に当てはまらず、本番の記録の呼び手で改めて決める。
4. `ledger.fold_and_write`（0.020 回／event）は同じ起動で比例するが小さいので、1 の後に本番の記録で残りの主になったときに扱う。

## 環境

- 対象の commit: `11bef45867ae2692acd9243ec4c8f858e21e0c9d`（task 4445 の呼び手の印と task 4569 を含む main）に、この文書の example の変更を足したもの。
- 道具: rustc 1.99.1 (58cf8f9c3 2026-10-13)、cargo 1.99.1、`--release` の build。
- host: macOS 27.2（26B123）、Apple M1、16 GiB、arm64。dagq の worker の run の worktree の中。
- provider: Claude Code（`claude-opus-5-5`）の headless の worker の session。

## 打ったもの

計装は [examples/supervisor_startup_memory.rs](../../examples/supervisor_startup_memory.rs) の `callers` だけにある。`prepare` が置いた queue を `<dir>/callers/queue.db` に写し、本番の supervisor と同じく service を通る台帳の係（`SupervisorPorts` で包んだ `SupervisorQueues`、service はこの process の中で答える）で起動し、supervisor の queue が service に送る要求を、送った時点の呼び手の印ごとに数える（`CountedCalls`）。数えるのは service の周期の集計と同じ単位（要求の op と呼び手の印）。

```sh
cargo build --release --example supervisor_startup_memory
B=target/release/examples/supervisor_startup_memory
for n in 5000 20000 80000; do
  $B prepare "$TMPDIR/q$n" $n
  $B callers "$TMPDIR/q$n"
done
```

段階は次のとおり。

1. 印からの起動: `start_builder`、追いつき（mark が待たなくなるまでの `build_pass`、どの大きさでも 1 pass）、続く 1 分ごとの 15 回の `build_pass`。
2. 同じ store を cursor の始めから: 新しい `Builder` で追いつき（どの大きさでも 1 pass。行はすべて書かれていて、掃きが未書きの印をすべて外すため）。

周回は大きさごとに 1 回。20,079 event の store で 2 回目を流し、全部の値が同じだった（回数は決定的）。

## 表

履歴の event 数は `run_events` の件数、履歴の行数は `ledger_rows` の件数（queue・session・run・task と印の行の和）。

| 履歴の event 数 | 履歴の行数 | 起動 | 呼び手 | ledger_row の回数 | 回数÷履歴の行数 |
| ---: | ---: | --- | --- | ---: | ---: |
| 5,029 | 3,046 | 印から（`start_builder`） | （なし） | 0 | 0 |
| 5,029 | 3,046 | 印から（追いつき） | `ledger.fold_and_write` | 4 | 0.0013 |
| 5,029 | 3,046 | 印から（続く 15 pass） | （なし） | 0 | 0 |
| 5,029 | 3,046 | cursor の始めから | `ledger.fold_rows` | 2,945 | 0.9668 |
| 5,029 | 3,046 | cursor の始めから | `ledger.fold_and_write` | 106 | 0.0348 |
| 5,029 | 3,046 | cursor の始めから | `ledger.restore` | 3 | 0.0010 |
| 20,079 | 12,163 | 印から（`start_builder`） | （なし） | 0 | 0 |
| 20,079 | 12,163 | 印から（追いつき） | `ledger.fold_and_write` | 4 | 0.0003 |
| 20,079 | 12,163 | 印から（続く 15 pass） | （なし） | 0 | 0 |
| 20,079 | 12,163 | cursor の始めから | `ledger.fold_rows` | 11,777 | 0.9683 |
| 20,079 | 12,163 | cursor の始めから | `ledger.fold_and_write` | 398 | 0.0327 |
| 20,079 | 12,163 | cursor の始めから | `ledger.restore` | 10 | 0.0008 |
| 80,061 | 48,496 | 印から（`start_builder`） | （なし） | 0 | 0 |
| 80,061 | 48,496 | 印から（追いつき） | `ledger.fold_and_write` | 4 | 0.0001 |
| 80,061 | 48,496 | 印から（続く 15 pass） | （なし） | 0 | 0 |
| 80,061 | 48,496 | cursor の始めから | `ledger.fold_rows` | 46,979 | 0.9687 |
| 80,061 | 48,496 | cursor の始めから | `ledger.fold_and_write` | 1,560 | 0.0322 |
| 80,061 | 48,496 | cursor の始めから | `ledger.restore` | 41 | 0.0008 |

store の行の内訳（queue／session／run／task）は、5,029 event で 2,935／9／48／52、20,079 event で 11,735／38／192／196、80,061 event で 46,805／153／766／770。

## 測っていないこと

- 実時間と 1 要求の bytes（問いは回数）。
- R が最新より手前にある印からの起動（上の「判定」の最後の推定）。合成の `prepare` は前の supervisor が最新まで台帳を作った store だけを置く。
- 判定の観測の最初の pass（`live_alerts`）など、台帳の係の外の要求。台帳の係の外で `ledger_row` を 1 行ずつ読むのは `LedgerStore::ledger_window` の既定の実装のうち時刻から読む経路（`rows_wanted` の行、kpi・stats・report の読み）で、supervisor の台帳の係の経路ではない。

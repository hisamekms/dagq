---
id: plan-acceptance-precheck-spike
type: plan
title: スパイク：run の review の受け入れ条件の差し戻しを型に分け、review の前に受け入れ条件・差分・receipt だけで見つけられたかを判定し、review の前の確認の形を比べる
status: completed
created: 2026-11-12
owners:
  - hisamekms
tags:
  - planning
  - measurement
  - review
related:
  - plan-acceptance-check
  - plan-review-sendback-reasons
  - design-supervisor-lifecycle-review
  - adr-t1420-1
  - adr-t1895-1
  - adr-t2259-1
  - adr-t2599-1
  - adr-t4005-1
---

# スパイク：review の前の受け入れ条件の確認

goal 278、task 4689。問いは 3 つ。Q1: 窓の run の review の差し戻しと concern の理由のうち、受け入れ条件に当たるものはどの型か。Q2: 型ごとに、受け入れ条件・run の差分・receipt だけを読む確認（test を流さない）で review の前に見つけられたか。Q3: review の前の確認のどの形が、費用・見逃し・誤って戻す割合で引き合うか。

この文書は過去の記録を観測締切で区切って 1 回読んだ集計で、条件を切り替える介入も群も無い。同じコマンドを同じ `--until` で打てば同じ値になるので、周回は 1 回で交互に流す群も無い。型の分類と標本の判定は判断を含むので、理由ごとの型を [reasons.csv](acceptance-precheck-spike/reasons.csv) に、標本の run ごとの根拠を下の表に残した。率の差を因果とは言い切らない。

## 判定

**holds（ただし効く範囲は狭い）。** 受け入れ条件に固有の型、つまり形だけの test (a)・先送り (b)・項目の抜け (d)・受け入れ条件の書き方 (e) は、標本で 4〜5 / 5 が review の前に見つけられた（holds）。未接続 (c) は 3 / 5。差し戻しの無い 15 run には同じ読みで誤って戻す run が無かった（0 / 15、unresolved 0）。

一方で、件数の最も多い理由は「受け入れ条件の番号を名指す端の経路の実装の欠陥」(f) で、399 件中 208 件・115 run を占める。こちらは 2 / 5 しか見つけられない（does_not_hold）。見つけられた 2 件も、読み直すと型は d（項目の抜け）だった。f は受け入れ条件を名指していても、中身は code_defect の review の仕事になる。

その結果、review の前の確認（a〜e を見て f を見ない形）で丸ごと無くせる差し戻しの回は、Q2 の型ごとの holds の率で見積もって、受け入れ条件の理由で戻した（revise か concern の）246 回のうち 27.0 回（11%）にとどまる（revise だけなら 19.7 回）。その回の理由が全て受け入れ条件の理由で、確認がその全てを見つけた場合に限るからだ。全ての差し戻しが無くなる run の見込みは、差し戻しか concern のあった 250 run のうち 11.1 run になる。

計画への意味は次のとおり。形だけの test と先送りを review より先に戻すことはできる。ただし差し戻しの回と review の費用を大きく減らす手段にはならないので、費用の小さい形（receipt の機械の検査と書き方の規則）を先に置く。AI の確認の job は試行に留め、mark と `kpi --compare` で判定する（[Q3](#q3-形の比べ)）。

## 条件

| 項目 | 値 |
|---|---|
| 対象の commit | `9e4a863a4dbdc9d23f059bc567464d49beabc704`（この run の base） |
| dagq の版 | `~/.local/bin/dagq` = `dagq 0.4.0-dev+a33081c1014e9159265975509128b29dc4cdeacc` |
| 窓 | run の最初の `review_finished` の `created_at` が [2026-10-26T00:00:00Z, 2026-11-10T00:00:00Z) の reviewed run |
| 観測締切 | 2026-11-12T06:00:00Z（集計を打った時刻 2026-11-12T05:59:44Z の直後と、2026-11-13T00:00:00Z の早い方）。`events --until 2026-11-12T06:00:00Z` |
| 経路 | worker（Claude Code headless、claude-opus-5-5）。型の分類と標本の読みは同じ session の subagent（同じ model）に頼んだ。指示文は [classify-instructions.txt](acceptance-precheck-spike/classify-instructions.txt)・[sample-instructions.txt](acceptance-precheck-spike/sample-instructions.txt) |
| 環境 | macOS（Darwin 27.2.0）、python3 の標準ライブラリだけ（`python3 -I`） |

読んだのは、`dagq` の読むだけのコマンド（`events`・`timeline`）、run の資料（`~/.local/share/dagq/<queue>/runs/<run>/` の `review.md`・`receipt.json`・`review-N-agent-*.out`）、git（`git diff BASE HEAD`）だけ。queue の DB は開いていない。event の取り出し（`fetch.py` の出力で約 85,000 event）は queue の dump なので commit しない（前例は [acceptance-check](acceptance-check.md) の snapshot）。script は [acceptance-precheck-spike/](acceptance-precheck-spike/) に置いた。理由ごとの型は `reasons.csv`、Q2 で差分を読んだ理由の判定は `samples.csv`（`avoid.py` が読む）に置いた。

### 打ったコマンドと時刻（UTC）

| 時刻 | コマンド | 用途 |
|---|---|---|
| 2026-11-12T05:59:44Z〜06:03:49Z | `fetch.py`: `dagq events --full --kind review_finished --since 2026-10-26T00:00:00Z --until 2026-11-10T00:00:00Z --limit 1000 --after N` で 1,107 event・524 run を拾う。続けて run ごとに `dagq events --full --all --run RUN --until 2026-11-12T06:00:00Z --limit 1000 --after N` を尽きるまで打った（85,177 event） | 全ての集計の元 |
| 2026-11-12T06:08:33Z | `dagq timeline 008adf1b-9338-4545-8c1e-e9e498f84845` | 手直しの時間から引く人の答え待ちを、timeline の `landing_answer` の gap と照合した（下の「費用の求め方」） |
| 06:04Z〜06:16Z | `compute.py`・`cost.py`（上の出力だけを読む）、`git diff BASE HEAD`（標本の run ごと、subagent） | 理由の抜き出し・費用・標本の読み |
| 06:31Z〜06:40Z | `compute.py`・`cost.py`・`avoid.py` を同じ出力に打ち直した（run の review の差し戻しの指摘による。dagq のコマンドは打ち直していない） | 抽出の条件・費用の区間・丸ごと無くせる回の直し |
| 2 回目の差し戻しの後 | 指定の標本で増えた 9 件の理由の `git diff BASE HEAD`（subagent）と `avoid.py` の打ち直し | 指定の標本の判定・形ごとの見積もり |

`dagq` の集計のコマンド（`fetch.py`）の打ち直しは 0 回。手元の集計の script の打ち直しは 2 回（run の review の差し戻しごとに 1 回）。

## 窓の run

- reviewed run は 523。拾った 524 run のうち 1 run は、最初の `review_finished` が窓より前だったので外した。拾った `review_finished` は 1,119 で、そのうち 4 回は外した 1 run（d9d4f9ac）のもの。523 run の review の回は 1,115 で、全てを費用の区間に切った。
- 最初の review が pass の run は 288（55.1%）、全ての review が pass の run は 273、差し戻しか concern のある run は 250（47.8%）。これは `review_finished` の verdict だけで数えた値で、プログラムの段の差し戻し（`revise_requested`）を含まない。goal に書いた kpi の first_pass_rate・revise_rate とは定義が違い、同じ値にはならない。
- 受け入れ条件に当たる理由は 399 件・165 run にあった。拾い方は 2 つで、全体の review の理由のうち code に `acceptance_unmet` を含むもの、または理由の文が受け入れ条件の番号か記号を名指すもの（`受け入れ条件1`・`受け入れ条件(A)`・`AC2`・`acceptance 3` など。正規表現は `compute.py`）。番号を名指さない「acceptance criteria」の語だけの理由（受け入れ条件を含む情報を provider に送る security の指摘 2 件と、`acceptance_infeasible` の 1 件）は入れない。subagent の job の理由は code が rule の番号なので、文が名指すものだけになる。
- 未完の run は 11。締切の時点で最後の verdict が revise で、着地も cancel も無い run を数えた。そのうち 11 run とも受け入れ条件の理由を持つ（例: run 4dae84e4 / task 4332）。率の分母からは外していない。着地していない run は全部で 18 で、残りの 7 は最後が pass か concern。

## Q1 型の分類

型は理由の文を 1 件ずつ読んで決めた（subagent 4 つに 100 件ずつ。指示は classify-instructions.txt）。1 つの理由が 2 つの型に当たるときは主の 1 つに数え、従の型を「従」の列に書いた。同じ理由をキーワードの規則でも分けたが、読んだ型と一致したのは 232 / 399（58%）だった。キーワードでは分けきれないので、表は読んだ型で作った。

verdict 数は、その型の理由を 1 つ以上持つ verdict の数。「全体」は全体の review、「job」は subagent の job で、revise と concern に分けた。受け入れ条件の対応づけの欄は、理由の文が worker の receipt についてどう書いたかを数えた（goal 90 の対応づけ）。値は 3 つで、「書いた」は receipt がその項目を根拠に対応づけていた・満たしたと書いていた、「書かず」は対応づけが無い・満たしていないと認めていた、「不明」は文が receipt に触れていない。review の回ごとの receipt は残っていないので（`review.md` と `receipt.json` は最後の回だけを持つ）、理由の文を使った。

| 型 | run | task | 理由 | 全体 revise | 全体 concern | job revise | job concern | receipt 書いた / 書かず / 不明 | 従（多い順） | 例（run / task / ask） |
|---|---|---|---|---|---|---|---|---|---|---|
| (a) 形だけの test | 28 | 27 | 33 | 23 | 5 | 3 | 0 | 14 / 0 / 19 | d 14・e 5 | ae9142f4 / 4655 / ask 1626（従 e）、98918baf / 4319 / ask 1628、d6ad7496 / 4670 / ask 1623 |
| (b) 先送り | 32 | 30 | 51 | 22 | 9 | 14 | 0 | 0 / 51 / 0 | f 18・d 15・a 8・e 6・c 4 | d7116e8d / 4662 / ask 1621（従 d）、85bde064 / 4443 / ask 1539、eee35312 / 4093 / ask 1530 |
| (c) 未接続 | 11 | 11 | 11 | 11 | 0 | 0 | 0 | 1 / 3 / 7 | a 4・b 3・f 3 | 4dae84e4 / 4332 / ask 1629（従 b）、f080cad7 / 4660 / ask 1624（従 b）、fcf6b099 / 4059 / ask 1574 |
| (d) 項目の抜け・一部だけ | 59 | 59 | 81 | 62 | 4 | 10 | 1 | 16 / 17 / 48 | a 8・b 6・f 6 | 4dae84e4 / 4332 / ask 1629、f080cad7 / 4660 / ask 1624、0c8ce680 / 4652 / — |
| (e) 受け入れ条件の書き方 | 14 | 12 | 15 | 0 | 14 | 1 | 0 | 2 / 9 / 4 | b 5・a 3・d 3 | f05833b1 / 4071 / ask 1581、acb0f2f0 / 4474 / ask 1577、f629310a / 3978 / ask 1505（従 b） |
| (f) その他 | 115 | 110 | 208 | 138 | 17 | 13 | 1 | 22 / 14 / 172 | a 26・d 16・b 13・e 11 | 54310e37 / 4653 / ask 1625、4dae84e4 / 4332 / ask 1629、98918baf / 4319 / ask 1628 |
| 計 | 165（重なりを除く） | — | 399 | — | — | — | — | 55 / 94 / 250 | — | — |

- (f) の 208 件のうち 163 件は「端の経路の実装の欠陥」（並行・障害・再試行・丸め・境界をまたぐ値など）で、受け入れ条件の番号を名指している。残る 45 件は手順・測り方・文書の位置など。run の主の型（その run で最も多い型。同数なら b・a・c・d・e・f の順で先のもの。同数の run は 26）で数えると、f 84・d 40・b 20・a 13・e 6・c 2 run になる。
- (e) は 14 件が concern で、人の判断（受け入れ条件と ADR の食い違い、例えば e2e の receipt）に上がった。plan review の `incomplete_spec` と並べると、run の review まで残った書き方の問題がこの 15 件にあたる。
- (b) の 51 件は全て「receipt が follow_up に回した・満たしていないと認めた」で、receipt の文に現れていた。(a) は 14 件で worker がその test を根拠に挙げていた。goal 90 の対応づけは書かれているが、中身（形だけ・先送り）は見分けていない。
- 受け入れ条件の理由を持つ run の人の ask は、approve_landing 157 件（うち差し戻しの後に開いたもの 153）・worker_question 17 件（同 13）・decide 6 件（同 4）・blocked 2 件（同 2）・stalled 2 件（同 0）。窓の全 run では approve_landing 199（同 178）・worker_question 24（同 14）・decide 14（同 4）・stalled 6（同 0）・blocked 3（同 2）。approve_landing の 8 割弱は、受け入れ条件の理由を持つ run の差し戻しの後に開いた。

## Q2 見つけられたか

標本は、型ごとに、その型の理由を持つ run を最初の `review_finished` の新しい順に並べた先頭の 5 run で、計 30 件（21 run）。他の型の標本に入った run も除かない。例えば run 4dae84e4 は c・d・f の標本に入り、それぞれの型の理由を別に読んだ。判定は、その run のその型の最初の理由（attempt の早い順）を対象にした。読んだのはその回の差分 `git diff BASE HEAD`（`review_programs_started` の base と head）と、受け入れ条件（`review.md` の Task）と receipt。

最初の turn は、他の型で選んだ run を除く規則で標本を選んでいた。その規則で選んだ d の 5 件と f の 4 件は指定の標本に入らないので、率から外して[参考の読み](#参考指定の標本の外で読んだ理由)に残した。

**限界**: 最後の回でない receipt は残っていないので、その場合は review の理由が引いた receipt の文と最後の receipt を手がかりにした。判定の欄で「receipt は引用」とした run がそれにあたる。読み手は review の理由を知って読んでいるので、後知恵で holds に寄る偏りがある。差し戻しの無い標本では理由を見せずに読ませ、その偏りを測った。

### 型ごとの判定

| 型 | 標本 | holds | does_not_hold | unresolved | holds / 標本 | 型の判定 |
|---|---|---|---|---|---|---|
| (a) 形だけの test | 5 | 5 | 0 | 0 | 5 / 5 | holds |
| (b) 先送り | 5 | 5 | 0 | 0 | 5 / 5 | holds |
| (c) 未接続 | 5 | 3 | 2 | 0 | 3 / 5 | 分かれる（入口が差分の外なら does_not_hold） |
| (d) 項目の抜け・一部だけ | 5 | 4 | 1 | 0 | 4 / 5 | holds |
| (e) 受け入れ条件の書き方 | 5 | 4 | 1 | 0 | 4 / 5 | holds（見つけても人の判断が要る） |
| (f) その他 | 5 | 2 | 3 | 0 | 2 / 5 | does_not_hold（holds の 2 件は読み直すと d） |
| 計 | 30 | 23 | 7 | 0 | 23 / 30 | — |

標本の型を読み直すと、30 件中 22 件が分類の型と一致した。外れは、b が d に 2 件、f が d に 2 件、a が d に、c が f に、d が f に、e が f に各 1 件。

理由の 1 件ずつの分類でも、読み手が「受け入れ条件・差分・receipt だけで見つけられるか」を併せて付けた（reasons.csv の `precheck`）。yes になった割合は a 16 / 33、b 51 / 51、c 4 / 11、d 62 / 81、e 12 / 15、f 21 / 208 で、標本の判定と同じ向きだった。a の yes が低いのは、理由の文だけでは差分の test の形が分からない unclear が 14 件あるためで、差分を読んだ標本では 5 / 5 になった。

### 差し戻しの標本の run ごとの根拠

| 型（分類 / 読み） | run / task / 回 | 判定 | 根拠の箇所 |
|---|---|---|---|
| b / b | d7116e8d / 4662 / 2 | holds | 受け入れ 1・2・6 の候補 (1) を、差分の `src/domain/ledger/window/payload_text.rs` の module doc と spike 文書の「判定」が「試作に無く、測っていない」と書き、後の Spike へ回している |
| b / d | 4adea518 / 4472 / 1 | holds | 受け入れ 7 が名指す claim-hold.md の hunk が差分に無い。理由が引く receipt は「30 KiB の予算で触らない」と書いていた（receipt は引用） |
| b / b | 85bde064 / 4443 / 4 | holds | guard の既知の取りこぼしを receipt が follow_up にした。最後の receipt は「前の receipt の guard の follow-up をこの commit が直す」と書く（receipt は引用） |
| b / d | eee35312 / 4093 / 1 | holds | 受け入れ 3（EXPLAIN QUERY PLAN を変えない）に対し、差分の `src/infrastructure/asks.rs` の doc comment が SEARCH から SCAN に変わったと自分で書き、test は `USING INDEX` だけを確かめる |
| b / b | 6b7eb1ff / 4099 / 1 | holds | 受け入れ 3（他の task の着地順に依らない）を、receipt が他の branch と task 4098 の着地順に回した（receipt は引用） |
| a / a | 98918baf / 4319 / 1 | holds | 受け入れ 3 の唯一の test は、test 自身が書いた述語（`|c| c.1 && !c.2`）で `select` を呼ぶ。同じ差分の本物の pull は `|_| true` を渡す |
| a / a | ae9142f4 / 4655 / 3 | holds | 受け入れ 3 の「古い 1 行ずつの書き込み」との比べの古い側が、`Memory` の `queue_one_by_one` で同じ新しい `write_session_rows_marked` を回すだけ。新しいコード同士の比べ（ask 1626） |
| a / a | d6ad7496 / 4670 / 3 | holds | 受け入れ 2 (d) の根拠に挙げた `vi_a_claim_is_compared_with_the_claim_before_it_in_id` は supervisor の起動を記録しないので、差分の `start_announcing` の枝を通らない |
| a / d | 3b514575 / 4658 / 2 | holds | 受け入れ 2 の QueueBook の値を固める `frozen.rs` の `queue_row()` が、key・kind・record id などだけを写し、time・subject.id・outcome・detail を固めていない |
| a / a | c1b93a46 / 4616 / 1 | holds | 受け入れ 1 は範囲の読みの比べの unit test を求めるが、差分の spike 文書は「範囲の読みは足さない」とし、対照の test は行を落とすだけ |
| c / c | 4dae84e4 / 4332 / 1 | holds | 実の backend の `runner_rulesets` は常に `NotConfigured` を返し、compose の hunk は `ruleset_gate` を再公開するだけ。起動処理で組む hunk が無い（ask 1629）。理由が引く receipt は follow_up に回していた |
| c / c | f080cad7 / 4660 / 1 | holds | `forward.rs` の hunk は coverage だけを転送し、新しい port の既定の読みは bail する。受け入れ 1・8 の service の経路が繋がっていないことが差分に見える |
| c / c | 71105c2a / 4645 / 2 | holds | `ledger.rs` の `pass()` は `if builder.omits()` の中でだけ `begin_coverage` を呼び、`omits()` は空でない組。空の組では受け入れ 5 の停止に届かない |
| c / f | fcf6b099 / 4059 / 1 | does_not_hold | `SessionOfAnotherTarget` が本番の service の境界で `Refused` に作り直される。直列化の経路は差分の hunk の外にある |
| c / c | 12e3bfa9 / 4452 / 2 | does_not_hold | 送らない flag は `QueueRead::request()` にあり、差分に hunk が無い。差分に見える配線は揃っている |
| e / e | f05833b1 / 4071 / 3 | holds | 受け入れ 11 は変えてよい既存の test を 2 つに限るが、`src/main.rs` の hunk が 3 つ目の既存の test を変えている。条件と差分の照合で見える |
| e / f | acb0f2f0 / 4474 / 1 | does_not_hold | 掃除と cooldown の pass をまたぐ時機の筋書き。理由自身が受け入れ 1 の字面は満たすと認める |
| e / e | f629310a / 3978 / 1 | holds | `review-1-agent-receipt-evidence.out` が引く 1 回目の receipt が、LEDGER_V を上げた後の最初の起動を follow_up にしたと書く。条件同士の食い違いは人の判断 |
| e / e | 6ba3c17c / 4245 / 1 | holds | 受け入れ 1 は 3 つの関数の unit test を求めるが、差分の unit test は `events()` の 1 つだけ。残り 2 つは差分のどこも流さない example でだけ測る |
| e / e | 3e0829f7 / 3991 / 1 | holds | 差分の spike 文書の 56〜57 行の「container を使えない」の扱いが、description の unresolved の列挙と食い違う |
| d / d | 4dae84e4 / 4332 / 1 | holds | 受け入れ 1 は規則の抜けの理由に読んだ値を求めるが、差分の `src/domain/ruleset_gate.rs` の `judge()` は規則の無い ruleset を `continue` で飛ばし、`values()` は規則の値を出さない。test は禁止と規則の名前だけを確かめる |
| d / d | f080cad7 / 4660 / 1 | holds | 受け入れ 2 が名指す (vii) の場合（token が最後の被覆の event）が無い。差分の唯一の (vii) の test は token を `id(n-3)` に置き、後の queue の event が最新を覆う |
| d / f | d6ad7496 / 4670 / 3 | does_not_hold | 差分の spike 文書は event の cursor の判定・根拠・test を持ち、欠けた項目は無い。指摘は推論の誤り（U_E と同じ K で区切った探しで足りる）で、設計の推論が要る |
| d / d | d7116e8d / 4662 / 1 | holds | description は全体のピークを 1 回の読みの process の最大と決めるが、差分の `examples/kpi_read_memory.rs` は `read_peak.max(held + folds_peak)` を使い、spike 文書も process の最大は判定に使わないと書く |
| d / d | 3b514575 / 4658 / 2 | holds | description (viii) と受け入れ 1・4 は 3 つの kind の時刻の無い event を求めるが、差分の `testkit/frozen.rs` の `timeless()` は `authorization_denied` だけを作る（doc comment は 3 kind と書く） |
| f / f | 4dae84e4 / 4332 / 3 | does_not_hold | 受け入れ 4 のコードと test は差分にある。未回答の ask を残したまま保留が明けて再び保留すると、差分の外の `insert_ask` が同じ key の開いた ask を返す、という端の経路で、その筋書きの推論が要る |
| f / f | 98918baf / 4319 / 1 | does_not_hold | `wait_for_build` を飛ばすのは `claim_task_filtered` の変えていない部分で、hunk に出ない（同じ run の a の理由の形だけの test は別に見つかる） |
| f / d | f080cad7 / 4660 / 2 | holds（際どい） | 受け入れ 1 は C・S・要約を行と同じ `ledger_read_snapshot` で読ませるが、差分の `forward.rs` と `window/omitted.rs` のコメントが、service の経路は 1 つの transaction で読まないと自分で書く。理由が挙げた競合の例は更に推論が要る |
| f / f | 54310e37 / 4653 / 1 | does_not_hold | 受け入れ 2 のコードと test は差分にある。1 頁の queue 行が同じ Dirtied の位置を持つと R が進まない、という筋書きは差分の外の builder の振る舞いを要る |
| f / d | d6ad7496 / 4670 / 1 | holds | 受け入れ 2 の行の範囲の上限の判定（spike 文書の 105 行）が挙げる test は marks だけを比べ、文書自身が「試作が行を読まないのでコードから判断した」と認める。読みは根拠の欠けで戻すが、理由の反例そのものは作らない |

#### 参考：指定の標本の外で読んだ理由

最初の turn の標本の規則で読んだ d の 5 件と f の 4 件。型ごとの率には入れず、Q3 の見積もりではその理由の判定（読んだ型と holds か）として使う。

| 型（分類 / 読み） | run / task / 回 | 判定 | 根拠の箇所 |
|---|---|---|---|
| d / d | 0c8ce680 / 4652 / 2 | holds | 受け入れ 4 は入口の行に `domain::ledger::builder` だけを名指させるが、`measurement.md` の変えた行が 2 つを並べたまま |
| d / d | d6197161 / 4617 / 1 | holds | description の上限の行が求める「使った時間と試みた数」が receipt に無い（receipt は引用） |
| d / d | 69ead960 / 4591 / 1 | holds | description は 1 回の読みか少数の文を求めるが、`ledger_rows_of` の hunk は key ごとに `held_row` を呼ぶ |
| d / d | b9845a34 / 4542 / 4 | holds | 受け入れ 6 の `host-metrics.md` の hunk が節を消し、`claim-hold.md:107` が足したリンクの見出しが無い |
| d / d | f73f95bc / 4178 / 1 | does_not_hold | `broker.log` を無条件に作るのは `queue_broker.rs` で、差分に hunk が無い |
| f / f | 2c86f6dd / 4135 / 1 | does_not_hold | Spike の結論 (v) への反例を組み立てた指摘で、差分にも receipt にも欠けた項目は見えない |
| f / f | 393e252a / 4327 / 1 | does_not_hold | A→A の再送はコードと test がある。A→B→A は端の経路の受け入れ条件の読みで、差分の test はそこで行が増えることを意図して期待する |
| f / a | 444e1d7d / 4598 / 2 | holds | 受け入れ 1 の読んだ行の合計で、上限を超えた頁は `rows_read.fetch_add` の前に bail するので、test の `assert_eq!(rows_read, runs)` が読み直しを隠す。hunk の中で見える |
| f / f | f701612f / 4473 / 1 | does_not_hold | 着地が始まった後の `in_flight` の数え方の欠陥で、`run_loop.rs` の段の推論が要る |

### 差し戻しの無い run の標本

窓の reviewed run のうち、全ての review の verdict が pass の run から、最初の `review_finished` の新しい順に 15 run を選んだ。concern は pass でないので、受け入れ条件の concern を持つ run はここに入らない。読み手には review の結果を見せず、1 回目の差分と receipt に同じ読みを当てた。

| run / task | 判定 | 根拠の箇所 |
|---|---|---|
| ce6f5010 / 4298 | 戻さない | 受け入れ 1・5・6 が `postgres/mod.rs` の test と persistence.md の hunk に当たる。receipt の follow_up は description が外した task 4299 の範囲 |
| 148646f6 / 4145 | 戻さない | 受け入れ 1〜6 が `landing_approval.rs`・`run_env.rs` の test に当たる。使わない `_run` の解析が 1 つあるが、入口が無いので型で保証している |
| 236156c5 / 4259 | 戻さない | 受け入れ 1〜5 が store_contract の search と persistence.md の hunk と receipt の見積もりに当たる |
| 503bd31d / 4261 | 戻さない | 受け入れ 1〜4 が `search_writes.rs`・`landed_commits.rs`・persistence.md に当たる（receipt は後の回のもので手がかりだけ） |
| 8ed92987 / 4093 | 戻さない | 受け入れ 1〜3 が `shared_sql.rs` の検査・store_contract・persistence.md の行と EXPLAIN の test に当たる |
| 192dbba2 / 4668 | 戻さない | 受け入れ 1・2 は README の 2 行の追加だけで、差分がそれに当たる |
| e738212b / 4654 | 戻さない（際どい） | 受け入れ 4 の新しい test は両側を新しい builder で回す（形だけに近い）。receipt が変更の前に固めた値の既存の test（task 4658）も挙げているので、独立の基準がある。厳しい読みなら戻しうる |
| 3356a064 / 4644 | 戻さない | 受け入れ 1〜3 が新しい ADR の決定・measurement.md の 3 節・6 本の検査に当たる |
| fb33c0f3 / 4072 | 戻さない | 受け入れ 1〜9 が unit test・migration・security.md などの hunk に当たる |
| e0dfd14e / 4639 | 戻さない | 受け入れ 2・3 が fixture の順の直しと write_leader の assertion に当たる（1 回目の receipt は残らず、最後のものを手がかりにした） |
| a6dc1f63 / 4605 | 戻さない | 受け入れ 1・2 が measurement.md の 2 節の hunk に当たる |
| 17f845dc / 4362 | 戻さない | dagq.toml の +4 行だけで、受け入れ 1〜3 に当たる |
| 6d35c1d2 / 4106 | 戻さない | 受け入れ 2〜7 が migration・`goal_owner.rs`・store_contract・docs の hunk に当たる。既存の test を 1 つ変えたが、新しい migration の番号で強いられたと receipt が書く |
| 7baa2d8c / 4596 | 戻さない | 受け入れ 1〜5 が spike 文書の判定・節の hunk に当たる |
| 7613dd21 / 4447 | 戻さない | 受け入れ 1〜4 が spike 文書の判定・環境・表・推奨と example の差分に当たる |

**誤って戻す割合** = 誤って戻す run の数 0 ÷ 判定できた run の数 15 = **0%**（主の値）。幅は、下限が 0 ÷ 15 = 0%（unresolved を戻さないとみなす）、上限が（0 + unresolved 0）÷ 15 = 0%（unresolved を戻すとみなす）。

ただし 15 run は少ない。0 / 15 の二項の信頼区間の上限は、両側 95%（Clopper-Pearson）で約 22%、片側 95% で約 18% で、際どい 1 run（e738212b）を戻すと数えれば 1 / 15 = 6.7% になる。誤って戻す割合が低いことは標本と矛盾しない、とまでしか言えない。

## Q3 形の比べ

### 費用の求め方

区間と式は description のとおり（`cost.py`）。

- **review の 1 回**: 前の `review_finished` の後の最初の `review_programs_started`（無ければ `review_started`）から、次の `review_finished` まで。
  - job は payload の `attempt` で回に入れた。前の `review_finished` の attempt より大きく、この回の attempt 以下のもの。
  - review が途中で止まり、`review_finished` の無い attempt を挟んで起動し直した回（54 回）では、止まった attempt の job もその後の回に入る。例は run 008adf1b の attempt 2 の停止した 5 job で、attempt 3 の回に入る。
  - 最後の `review_finished` より後の attempt の job（16 件、6.6M トークン）は、どの回にも入れずに外した。
  - `review_programs_started` から `review_programs_finished` までは、プログラムの段として別に数えた。
- **トークン**: 回の `review_job_finished` と、kind が review の `headless_job_stopped` の tokens の input＋output＋cache_read＋cache_creation の和。
  - agent が無い `review_job_finished` を全体の review、agent のあるものを subagent の job（agent ごと）とした。
  - `headless_job_stopped` は agent を持たないので「停止した job」として別に数えた。同じ session_id の `review_job_finished` があれば重ねない。
  - tokens が null の job（held・job_failed など 304 件）は 0 として数えた。
  - `review_program_finished` はトークンを持たないので時間だけ数える。
- **時間**: 壁時計は回の始まりから `review_finished` まで。起動し直した回は最初の始まりから数える。計算量は回の `review_job_finished` の duration_secs の和。
- **手直しの時間**: `review_finished` から次の `review_programs_started` か `review_started` まで。人の答え待ちは、その区間に重なる ask の opened から answered までの和で、それを引いた値を並べた。
  - timeline との照合: run 008adf1b の 1 回目の手直しで、ask の span は 5,612 秒。`dagq timeline` の `landing_answer` の gap（ask 1295）は 1,518＋3,851 = 5,369 秒で、差の 243 秒は timeline が 300 秒未満の gap を出さない分と見る。
  - この差は手直しの時間の 3% 程度なので、ask の span を使った。

| 区間 | 回 | 全体のトークン（中央値 / 平均） | job のトークン（中央値 / 平均） | 停止した job（平均） | 合計（中央値 / 平均） | 壁時計 秒（中央値 / 平均） | 計算量 秒（中央値 / 平均） | プログラムの段 秒（中央値 / 平均） | 手直し 秒（待ちを含む / 引いた、中央値・平均） |
|---|---|---|---|---|---|---|---|---|---|
| 全ての回 | 1,115 | 1.01M / 2.15M | 0.90M / 1.02M | 0.05M | 2.05M / 3.22M | 354 / 729 | 463 / 524 | 130 / 516 | 987・7,149 / 929・3,320（差し戻しの 380 回） |
| 差し戻しか concern の回 | 429 | 2.27M / 2.96M | 1.05M / 1.23M | 0.08M | 3.82M / 4.27M | 457 / 745 | 548 / 592 | 121 / 477 | 同上 |
| 受け入れ条件の理由のある回 | 255 | 2.27M / 2.96M | 1.06M / 1.22M | 0.08M | 3.67M / 4.26M | 466 / 748 | 541 / 590 | 121 / 484 | 1,392・7,859 / 1,307・3,930（220 回） |

全ての回の和は次のとおり。

- トークン: 全体の review 2,396M、subagent の job 1,141M、停止した job 58M（計 3,595M）。
- 時間: 計算量 162.4 時間（全体の review 59.7 時間）、壁時計 225.7 時間。
- 差し戻しの後の手直し: 待ちを含めて 754.6 時間、引いて 350.5 時間。

受け入れ条件の理由のある回だけの和は、全体 755M・job 312M・停止した job 20M、計算量 41.8 時間・壁時計 53.0 時間、手直しは 480.3 時間（待ちを引いて 240.2 時間）。

agent ごとの内訳は次のとおり。トークンは M、計算量は時間で、回はその agent の job のあった回の数。

| job | 全ての回: トークン | 全ての回: 計算量 | 全ての回: 回 | 1 回の平均トークン | 受け入れ条件の理由のある回: トークン | 同: 計算量 | 同: 回 |
|---|---|---|---|---|---|---|---|
| 全体の review | 2,396 | 59.7 | 1,115 | 2.15 | 755.0 | 17.8 | 255 |
| receipt-evidence | 284 | 26.0 | 1,115 | 0.25 | 74.8 | 5.9 | 255 |
| security | 268 | 23.8 | 1,115 | 0.24 | 80.0 | 5.6 | 255 |
| test-rules | 266 | 19.5 | 884 | 0.30 | 73.1 | 4.9 | 219 |
| builtin:security | 164 | 17.8 | 930 | 0.18 | 49.1 | 4.5 | 228 |
| config-rules | 59 | 6.1 | 278 | 0.21 | 10.6 | 0.9 | 40 |
| design-layers | 50 | 4.4 | 283 | 0.18 | 10.4 | 1.0 | 61 |
| adr-rules | 31 | 2.3 | 97 | 0.32 | 9.4 | 0.7 | 27 |
| plugin-generic | 14 | 2.3 | 101 | 0.14 | 3.7 | 0.4 | 23 |
| migration-rules | 6 | 0.6 | 38 | 0.16 | 1.0 | 0.1 | 5 |
| 停止した job（agent 不明） | 58 | — | 35 | 1.64 | 20.1 | — | 16 |

**review の前の確認で丸ごと無くせる回**（`avoid.py`）: 受け入れ条件の理由で戻した回は 246 回（revise 209・concern 37）。候補の回は、そのうち次の 2 つを満たす 94 回。

- 全体の review の理由が全て受け入れ条件の理由であること。
- job の pass でない指摘が受け入れ条件の理由だけであること。

候補の回が丸ごと無くなる確率は、その回の理由の全てを、その形が見つける確率の積とした。理由ごとの確率は形が捕まえる型の組で決まる。

- Q2 で差分を読んだ理由（指定の標本と参考の読みの 39 件）は、その判定を使う。holds で読み直した型が形の組にあれば 1、そうでなければ 0。例えば run 12e3bfa9 の 2 回目は does_not_hold なので、どの形でも 0。
- 読んでいない理由は、型が形の組にあれば Q2 の指定の標本の型ごとの holds の率（a 1.0・b 1.0・c 0.6・d 0.8・e 0.8）、組に無ければ 0。
- (ii) は f を見ないので、読んでいない f の理由はどの形でも 0。上限として、f にも標本の率 0.4 を当てた値を並べる。f の標本の holds の 2 件は読み直すと d で、分類で f にした理由にも (ii) の読みで捕まる項目の抜けが混じるからだ。

| 形（捕まえる型） | 無くせる回 | うち revise | うち 1 回目の review | 全体のトークン | job のトークン | 停止した job | 計 | 計算量 | 壁時計 | 手直し（待ちを含む / 引いた） | 全ての差し戻しが無くなる run |
|---|---|---|---|---|---|---|---|---|---|---|---|
| (ii) a〜e（主） | 27.0 | 19.7 | 13.1 | 47.6M | 24.6M | 1.1M | 73.3M | 3.5 時間 | 4.2 時間 | 33.6 時間 / 19.4 時間 | 11.1 |
| (iii) b | 7.0 | 3.0 | 1.0 | 13.0M | 8.0M | 1.1M | 22.1M | 0.8 時間 | 1.2 時間 | 5.2 時間 / 3.5 時間 | 2.0 |
| (iii)＋(ii) a・c・d（推奨の組み合わせ） | 24.7 | 19.7 | 10.8 | 41.4M | 22.8M | 1.1M | 65.4M | 3.2 時間 | 3.8 時間 | 28.1 時間 / 18.1 時間 | 9.6 |
| (ii) a・c・d だけ | 16.3 | 15.3 | 8.4 | 25.1M | 12.9M | 0 | 38.0M | 2.2 時間 | 2.4 時間 | 21.0 時間 / 13.2 時間 | — |
| 上限: (ii) a〜e で f にも 0.4 | 46.4 | 37.4 | 21.0 | 97.9M | 46.2M | 3.5M | 147.6M | 6.7 時間 | 7.8 時間 | 63.0 時間 / 28.2 時間 | 18.2 |

主の見積もりの job のトークン 24.6M の内訳は、receipt-evidence 6.7M・security 6.7M・test-rules 5.7M・builtin:security 3.3M・config-rules 1.0M・design-layers 0.5M・adr-rules 0.5M・plugin-generic 0.2M。(iii) の 8.0M は security 2.3M・test-rules 2.1M・receipt-evidence 1.9M・builtin:security 1.1M ほか。

候補でない 152 回には、確認で見つけられない理由（f の欠陥や code_defect・docs_drift）が同じ回に並んでいた。確認が受け入れ条件の欠けを先に戻しても、これらの回は減らない。

b の理由を含む回の多くは、他の型の理由も並べている。b だけを捕まえる機械の検査で丸ごと無くなるのは、理由が全て b の 7 回だけだ（run 9daa267e・85bde064・6611e7b0・9a9fd74e・12c9ad0a の各 1 回と d7116e8d の 2 回）。理由が全て b に分類された回には 4adea518 の 1 回目もあるが、Q2 の読みで d とした（受け入れ条件の文書が未着手）ので、機械の検査では捕まらないとして外した。7 回のうち 4 回は concern で、人の approve_landing に上がっていた。

手直しの時間は、確認が戻した分の手直しとして前に移るだけで、無くならない。無くなるのは review の回の費用と、review を待つ slot・session の待ちだけなので、下の差し引きには手直しの時間を入れず、表の値として並べた。

### 5 つの形

見積もりの前提は 3 つ。

- 確認が戻す型と減る review の回は、上の表の形ごとの値。(ii) は a〜e で 27.0 回（f を見ない）、(iii) は b で 7.0 回。
- (ii) の確認の 1 回は、全体の review の 1 回の 0.15 倍のトークンと計算量とする。
  - 読むのは受け入れ条件・差分・receipt だけで、規則の文書も差分の外も読まないので、全体の review の prompt（規則が約 10KB）と探索の量より小さい。
  - 全体の review の平均 2.15M・193 秒に掛ける。
  - 軽い model か effort で 0.05 倍にできた場合も並べる。

| 形 | 捕まえる型（記録の件数） | 増える費用（窓の 1,115 回で） | 減る費用（手直しは前に移るだけで差し引きに入れない） | 差し引き（トークン） | 見逃し | 誤って戻す割合 | 他の goal との関係 |
|---|---|---|---|---|---|---|---|
| (i) worker の規則（prompt・local-checks.md の対応づけを型の点検に強める） | 原理では a〜d。worker の自己点検 | review のトークンは 0。worker の turn が少し増える（記録から測れない） | 測れない | — | 大きい。a の 14 / 33 件で worker はその形だけの test を根拠に挙げていた。goal 90 の対応づけの後も受け入れ条件の理由が差し戻しの多数を占める | 0（戻す主体が無い） | goal 90（ADR-t1420-1）の延長。単独では効きの根拠が無い |
| (ii) review の前の確認の job（受け入れ条件・差分・receipt だけを読む軽い job が、プログラムの段の後、全体の review と subagent の job の前に worker へ戻す） | a〜e（Q2 の指定の標本で a 5・b 5・c 3・d 4・e 4 / 5） | 0.15 倍で +359M トークン・計算量 +9.0 時間・壁時計 +9.0 時間（毎回直列）。0.05 倍で +120M・+3.0 時間 | 27.0 回分の −73.3M トークン（全体 47.6M・job 24.6M・停止 1.1M）・計算量 −3.5 時間・壁時計 −4.2 時間。手直しは 33.6 時間（待ちを引いて 19.4 時間）が前に移る | 0.15 倍で +286M（review のトークン 3,595M の +8.0%）、0.05 倍で +47M（+1.3%）。f にも 0.4 を当てた上限で 0.05 倍なら −28M。トークンで引き合うのは、0.05 倍の軽さで f に混じる項目の抜けまで捕まえたときだけ | f（208 件）と、差分の外に入口がある c | 0%（0 / 15、両側 95% の上限 約 22%） | goal 152〜154 が作ったプログラムの段と agent ごとの job の間に段を足す（ADR-t1895-1 の段の固定の形を変える）。goal 199（ADR-t2599-1）の手直しの回の数え方に入れるかを決める必要がある |
| (iii) runtime による receipt の機械の検査（受け入れ条件の項目ごとの対応を形のある欄にし、先送りを worker_question 無しに出せない） | b（51 件・32 run、標本 5 / 5）。e の一部は worker_question へ早く回る | ほぼ 0（プログラム。トークン無し、数秒） | 理由が全て b の 7.0 回分の −22.1M トークン（全体 13.0M・job 8.0M・停止 1.1M）・計算量 −0.8 時間・壁時計 −1.2 時間。手直しは 5.2 時間（待ちを引いて 3.5 時間）が前に移る。うち 4 回は concern で approve_landing が無くなる | −22.1M | a・c・d・f。欄に「満たした」と書けば通る。b と他の型が並ぶ回では回は減らない | 先送りの欄を読むだけなので、記録で誤るのは follow_up が task の範囲の外の場合（ce6f5010 の類）。欄で受け入れ条件の番号を名指させれば避けられる | goal 90 の対応づけ（ADR-t1420-1）を形のある欄に変える。goal 152・153 のプログラムの段に乗る。(ii) の入力も小さくなる |
| (iv) plan review の受け入れ条件の書き方の規則（比べる基準・入口への接続・観測できる値を名指させる） | e（15 件）と、a・c の起こりやすさ | plan review の数行（測れないほど小さい） | 記録から見積もれない | — | 書き方が良くても形だけの test は書ける | 0 | goal 191 の docs_drift と同じく「書き方」の側の手当てで重ならない。plan review の `incomplete_spec` と並べて効きを見る |
| (v) 全体の review（builtin:acceptance）を 1 段目に先に流し、受け入れ条件の欠けなら subagent の job を起動せずに戻す | 全体の review が受け入れ条件で revise にした全て（209 回。concern を含めると 246 回） | 毎回、全体の review の後に job を直列にするので壁時計 +193 秒 / 回（+59.7 時間）。job も指摘していた 107 / 209 回では、その指摘が次の回へずれ、回が最大 107 回増える（最大 +345M トークン、手直しは待ちを含めて最大 +212 時間・引いて +99 時間） | その 209 回の job のトークン 最大 −259M | −259M〜+86M。回が増える分で幅が大きい | 無し（判定の基準は今の全体の review） | 今と同じ | ADR-t1895-1 の並列の段を直列に変え、goal 154 の agent ごとの job の並列を崩す。A/B（goal 247・251、ADR-t4005-1）では群ごとの review の時間の比べが変わるので、着地に mark が要る |

人の ask への効き: 受け入れ条件の理由を持つ run の approve_landing は 157 件だった。受け入れ条件の理由を持つ全体の concern は 37 回。そのうち確認で戻せる型の理由を持つ回は a 5・b 9・d 4・e 14 回で、型が重なる回があるので和は回の数にならない。

- b は worker_question に変わるか、worker が直すので減る見込みがある。理由が全て b の concern の 4 回は (iii) で人に上がらなくなる。
- e は確認が見つけても人の判断が要り、worker_question として前に移るだけになる。
- f の concern は残る。

それ以上の ask の減りを件数で見積もる材料は、記録に足りない。

### 推奨

**(iii) を主にし、(iv) と (i) を添え、(ii) は軽い形の試行に留め、(v) はとらない。**

1. **(iii) receipt の受け入れ条件の欄と機械の検査**（最初に。ADR が要る）。
   - receipt に、受け入れ条件の項目ごとに `met` / `deferred` と根拠（変えたファイル・test の名前・コマンド）を持つ欄を足す。
   - runtime は、`deferred` が worker_question の答え無しにあれば review の job を起動する前に worker に戻す。
   - 受け入れ条件の番号を名指す follow_up も同じく扱う。
   - 費用はほぼ 0 で、標本 5 / 5 の b を捕まえ、e を worker_question へ早く回す。丸ごと無くせる回は理由が全て b の 7.0 回（−22.1M トークン・計算量 −0.8 時間・壁時計 −1.2 時間、うち concern 4 回）と小さいが、増える費用がほぼ無いので差し引きは減る側になる。
   - 変える決定: ADR-t1420-1（worker が receipt の前に受け入れ条件を根拠へ対応づける）を、文の対応づけから形のある欄と機械の検査へ amends する。receipt の形を持つ design（validation）も変わる。
2. **(iv) plan review の書き方の規則**（ADR は要らない。docs/development/task-registration.md の「plan reviewが当てはめる規則」）。比べる test の基準（新しいコード同士でない基準）・入口への接続（どの起動処理から呼ばれるか）・観測できる値を、受け入れ条件に名指させる。
3. **(i) worker の規則**: (iii) の欄の書き方として、prompt と local-checks.md の「受け入れ条件の対応づけ」に型（形だけ・未接続・先送り）の点検を足す。単独では入れない。
4. **(ii) review の前の確認の job は試行に留める**（ADR が要る）。
   - goal 278 の受け入れ (2) は、形だけの test が review の job の前に戻ることを求める。これは機械の検査では捕まらず (ii) が要る。
   - ただし窓の記録で減る review の回は、(ii) が a〜e を見ても Q2 の率で 27.0 / 1,115 で、トークンは 0.15 倍で +286M、0.05 倍でも +47M と増える。(iii) と組んで (ii) が a・c・d だけを見る形では、組で 24.7 回・−65.4M に対し (ii) の費用が同じだけ掛かる（0.05 倍で +54M）。
   - トークンで引き合うのは、f に分類した理由に混じる項目の抜けまで捕まえた上限（46.4 回）で、0.05 倍の軽さのとき（−28M）だけ。だから試行は、効きをトークンではなく、受け入れ条件の欠けが review の前に戻ること（goal 278 の受け入れ (2)）と first_pass_rate・revise_rate で測る。
   - そこで、プログラムの段の後、全体の review と subagent の job の前に、a・c・d だけを見る軽い job（effort low、出す理由は受け入れ条件の被覆だけ、code_defect・docs_drift・security は見ない）を置く。
   - 着地に mark を残し、`kpi --compare` で first_pass_rate・revise_rate・acceptance_unmet・review のトークンを前後で比べて、残すかを決める。
   - 変える決定: ADR-t1895-1（review の段を固定の形で動かす）の段の構成と、ADR-t2259-1（built-in の review agent と program を名前で設定する）。確認の job を built-in にするなら名前と on/off もここで決める。確認が戻した回を ADR-t2599-1 の手直しの回に数えるかも決める（数えると approve_landing が増えるので、別に上限を持たせる案を推す）。
   - A/B を壊さないために、確認の job の provider と設定は群に依らず同じにする（ADR-t4005-1）。
5. **(v) はとらない。** subagent の job のトークンは最大 259M 減りうるが、job の指摘を次の回へずらし（107 / 209 回、最大 +345M）、全ての回の壁時計を直列で延ばす。ADR-t1895-1 の並列の段と goal 154 の agent ごとの job を崩す割に、差し引きの符号が決まらない。

**順番**: (iii) の ADR を書く task → (iii) の実装（receipt の欄・検査・prompt と local-checks.md の (i)）と (iv) の規則の文書（並べられる）→ 着地と mark・`kpi --compare` → (ii) の ADR と試行の task。(ii) は (iii) の後にする。(iii) が b を先に除けば (ii) の読む範囲が a・c・d に絞れ、0.05 倍に近い軽さで試せるからだ。

**確信度**: (iii) が先送りを review の前に戻せることは高い（記録の 51 件全てで receipt に現れていた）。(ii) が形だけの test を見つけられることは中（標本 5 / 5 だが、後知恵の偏りと receipt の欠けがある）。この組み合わせで first_pass_rate・revise_rate が大きく上がるかは低い。全ての差し戻しが無くなる run の見込みは、推奨の組み合わせで 9.6 / 250（(ii) が a〜e を見ても 11.1 / 250）で、主な差し戻しは f の欠陥と、受け入れ条件以外の理由の並びにあるからだ。

### 推奨を実装する task の概要

| 順 | task | ADR | 変える決定・文書 |
|---|---|---|---|
| 1 | receipt の受け入れ条件の欄（項目・met / deferred・根拠）と、deferred を worker_question 無しに review へ進めない検査を決める ADR | 要る | ADR-t1420-1 を amends。受け入れ条件の番号を名指す follow_up の扱い |
| 2 | 1 の実装（receipt の検証・review の段の前の戻し・worker と resume と revise の prompt・local-checks.md の対応づけの節）。fake で、先送りの receipt が review の job を起動せずに worker に戻ることを確かめる | 1 に従う | validation と review の design の該当の節 |
| 3 | plan review の受け入れ条件の書き方の規則（比べる基準・入口への接続・観測できる値） | 要らない | docs/development/task-registration.md |
| 4 | review の前の確認の job（a・c・d だけ、軽い設定）の段を足すかを決める ADR。試行と判定の基準（mark と `kpi --compare`）を含む | 要る | ADR-t1895-1 の段の構成、ADR-t2259-1、ADR-t2599-1 の数え方、ADR-t4005-1 の群と独立 |
| 5 | 4 の実装と、形だけの test の receipt が review の job の前に戻る fake の test | 4 に従う | review の design |

着地の後の比べ（goal 278 の受け入れ (3)）は inbox か人が行う。

### 結果ごとの後続の判断への当てはめ

この Spike の判定は holds だが、件数の最も多い型（f）は does_not_hold で、受け入れ条件に固有の型だけが holds になった。description の「holds」の後続（推奨の形を実装する task を計画し、要る ADR を先に書く）に当たる。ただし Q3 の費用で AI の確認の job は、0.05 倍の軽さでも f を見ない限りトークンで引き合わない（+47M）ので、job は試行と判定の基準を持つ形に縮めることを勧める。goal 278 の constraints どおり、直す task の submit の前に planner が planner_question で人の承認を取る。

## 上限と使ったもの

| 項目 | 上限 | 使ったもの |
|---|---|---|
| 作業の時間 | 4 時間 | 2026-11-12T05:59Z の開始から 06:25Z の commit まで約 25 分、1 回目の review の指摘の直し（06:31Z〜06:45Z）に約 15 分、2 回目の直しに約 20 分、計 約 1 時間（壁時計。subagent 12 個の並列の読みを含む） |
| 標本の読み | 45 run（差し戻し 25・差し戻しの無い 20 が目安） | 45 run（差し戻しの側 30 run・差し戻しの無い標本 15 run）。差し戻しの側では 30 run の 39 件の理由を読んだ。指定の標本は 30 件・21 run、参考の読みは 9 件。2 回目の差し戻しの直しで読んだ 9 件は、どれも既に読んだ run の別の理由で、読んだ run の数は増えていない。合計は上限ちょうど |
| 同じ集計のコマンドの打ち直し | 3 回 | `dagq` の集計（`fetch.py`）は 0 回。手元の集計の script（`compute.py`・`cost.py`・`avoid.py`）を run の review の指摘の直しで 2 回打ち直した |

上限で止めた問いは無い。理由の型の分類（抽出を直す前の 402 件で分け、直した後の 399 件はその一部）は標本の読みではなく理由の文だけの読みで、上限の数には入れていない（subagent 4 つで 1 回ずつ）。unresolved とした問いは無い。

## 限界

- 型の分類は理由の文の読み（subagent の判断）で、キーワードの規則とは 58% しか一致しない。丸ごと無くせる回の見積もりは型ごとの holds の率（標本 5 件ずつ）を独立に掛けた期待値で、率の幅をそのまま持つ。f を 0 にするか 0.4 にするかで (ii) の回は 27.0 から 46.4 まで動く。型の境（特に d と f、a と d）には判断の幅がある。
- 標本は型ごとに 5 run で、率の幅は大きい。差し戻しの標本は理由を知って読んでいる。
- review の回ごとの receipt は残っていない。最後の回でない判定と Q1 の receipt の欄は、理由の文が引いた receipt に頼った。
- 形ごとの人の答え待ちを含む値と引いた値は 5 つの形の表と丸ごと無くせる回の表に並べた。手直しは前に移るだけとして差し引きに入れていない。
- (ii) の 0.15 倍・0.05 倍は置いた比で、測った値ではない。手直しの時間は前に移るだけとして減る側に入れていないが、review の slot と session の待ちの分は実際には減りうる。
- goal 247・251・152〜154・191・199 との関係は、この repository の ADR と design と goal 278 の記述から書いた。worker は goal を `dagq show` で開いていない。

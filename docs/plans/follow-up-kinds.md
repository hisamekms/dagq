---
id: plan-follow-up-kinds
type: plan
title: receipt の follow_up の種類と runtime の planner の判断の分類と、ラベルの定義案
status: completed
created: 2026-09-28
owners:
  - hisamekms
tags:
  - measurement
  - follow-up
  - planner
related:
  - adr-0037
  - adr-0044
  - adr-0046
  - adr-t808-1
  - design-supervisor-lifecycle-draft-planners
  - plan-review-sendback-reasons
  - plan-worker-question-topics
---

# receipt の follow_up の種類と runtime の planner の判断の分類と、ラベルの定義案

task 951（goal 64）。worker が receipt の `follow_ups` に書き、`integrate` の `register_follow_ups` が draft にした task を 1 件ずつ読み、中身を分類した。分類ごとに、runtime の planner（[Draft planners](../design/supervisor-lifecycle/draft-planners.md)）かそれ以前の担い手がどう決めたか（採用・不採用・重複・未決）、着地した割合、draft のまま残った時間、着地までの時間を出す。runtime の planner の判断にかかった時間と、planner の枠を待った時間も出す。そのうえで、runtime で使うラベルの定義案と、価値の低い follow_up を減らす・絞る候補を置く。この文書は決定をせず、task も登録しない。

今の follow_up は `{title, description}` だけで種類を持たない。draft の出どころ（`draft_origins` の `follow_up`）と、採用の記録（`follow_up_adopted` の `by` と `depth`）と、cancel の `duplicate_of`（[ADR-0046](../adr/0046-full-text-search-related-and-duplicate-of.md)）はあるが、何の follow_up だったか、なぜ不採用だったかは title と description を読まないと分からない。

## 要点

- **follow_up の draft は 492 件で、着地 521 件に対して約 0.94 件**。記録は 2026-09-23 12:01 UTC から始まる（最初の `follow_up_registered`）。直近 7 日（2026-09-21 14:00 UTC〜）は全期間を含むので、2 つの期間の数字は同じになる。代わりに、runtime の planner が立つようになった後（2026-09-25 23:00 UTC 以降の登録、352 件）と、直近 24 時間（97 件）を並べた。
- **全期間の結末は、採用 260（53%）、不採用 180（37%）、重複 50（10%）、未決 2**。着地（completed）は 152 件（31%）で、main の着地全体の 29% が follow_up から出た task だった。採用されて ready のまま待っている follow_up が 100 件あり、今の ready 155 件の 65% を占める（うち 60 件が low）。
- **runtime の planner の後は採用率が 33% から 61% に上がり、重複の記録も 3% から 13% に増えた**（`duplicate_of` は 2026-09-26 00:16 UTC の task 402 の着地から記録される）。採用率が高いのは `defect`（80%）と `test_gap`（93%）、低いのは `decision`（13%）と `ops`（33%）。`flaky_test` は 41% が重複で、同じ e2e の不安定を 5 つの follow_up が別々に書いていた（`up_in_cmux` の e2e。641 に 4 件、680 に 1 件）。
- **runtime の planner の判断そのものは短い**。planner を立ててから判断までの中央値は 0.6 分（p90 1.3 分）、planner の session の稼働（`active_secs`）の中央値も 0.6 分で、180 session の合計が 2.0 時間だった。
- **時間の大半は planner の枠を待つ時間**。登録から planner が立つまでの中央値は 79 分、p90 は 642 分（最大 752 分）で、draft が枠を待った時間の合計は約 1,181 draft 時間になる。planner は枠が空くと 1 時間に 29〜42 件立ち、空かない区間には 1 件も立たなかった。枠を長く塞いだのは、人の答えを待つ `planner_question` を持った planner（task 899 が 340 分、886 が 197 分）と、答えが planner に届くまでの遅れ（663・664 は答えが 08:11 JST、close が 14:49 JST）だった。未決の draft の山は 2026-09-27 21:00 JST の 79 件と、2026-09-28 08:00 JST の 57 件。
- **`planner_question` は follow_up に 42 件開き、40 件が `adopt`、1 件が `keep_draft`、1 件が方針の答え（task 418）だった**。答えまでの中央値は 1.7 分だが、p90 は 197 分で、合計は 25.8 時間。そのあいだ planner は枠を持ったまま待つ（`runtime_planners` の既定 1 の当時）。

## 方法

- 期間: `follow_up_registered` の最初（2026-09-23 12:01 UTC）から 2026-09-28 14:00 UTC（約 13:44 UTC の最後の登録まで）。runtime の planner の最初の `draft_planner_opened` は 2026-09-25 23:00 UTC（migration で導入前の draft も対象になった）。それより前の draft は follow-up triage job（[ADR-0037](../adr/0037-follow-up-triage-job-decides-follow-up-drafts.md)）か人が決めた。
- 読んだもの（固定バイナリ `~/.local/bin/dagq` の読み取り専用のコマンドだけ）: `dagq events --all --full`（`follow_up_registered`・`draft_planner_opened`・`draft_planner_exhausted`・`follow_up_adopted`・`task_status_changed`・`task_submitted`・`session_opened` / `session_closed` の `runtime_planner`・`planner_answer_claimed`・`plan_revise_sent`・`planner_unresponsive`）、`dagq list --all --full`（492 件の title・description・context・status・priority）、`dagq asks --all`（`planner_question` の問いと答え）。queue の状態を変えるコマンドは打っていない。`dagq show --full`・`dagq timeline`・`dagq notes` は、上の 3 つで必要な欄が揃ったので使っていない。
- 結末: draft から最初に出た遷移で決めた。`submitted` か `ready` なら採用、`canceled` で `duplicate_of` があれば重複（採用の後に重複で cancel されたものも重複に数える）、`duplicate_of` の無い `canceled` なら不採用、まだ `draft` なら未決。採用の後に重複でなく cancel されたものは 7 件（統合 ADR の組 D〜J、task 367〜373。ADR-t598-1 で方針が変わって cancel）で、採用に数えた。
- draft のまま残った時間: `follow_up_registered` → draft を出た遷移。着地までの時間: `follow_up_registered` → `completed`。枠待ち: `follow_up_registered` → 最初の `draft_planner_opened`。判断の時間: 判断の直前の `draft_planner_opened` → draft を出た遷移。planner の稼働: `session_closed`（`kind: runtime_planner`）の `active_secs` と、`session_opened` からの壁時計の時間（session の記録は 2026-09-27 12:18 UTC から）。
- 分類: title と description を 1 人（この task の worker）が読み、主のラベルを 1 つ付けた。title の語（`flaky`・`測る`・`決める` など）で下分けし、全件の title を読んで 97 件を付け直した。

## 分類ごとの集計

### 全期間（= 直近 7 日、492 件）

時間は分。「draft のまま」は判断のついたものの中央値と p90、「着地まで」は completed になったものの中央値。

| 分類 | 件数 | 採用 | 不採用 | 重複 | 未決 | 着地した割合 | draft のまま（中央値 / p90） | 着地まで（中央値） | 代表の task |
|---|---|---|---|---|---|---|---|---|---|
| 見つけた不具合（`defect`） | 145 | 99 | 42 | 3 | 1 | 37% | 292 / 1,002 | 1,053 | 723, 793, 971（採用）、342（不採用） |
| 不安定な test（`flaky_test`） | 37 | 15 | 9 | 13 | 0 | 27% | 204 / 657 | 953 | 682, 770（採用）、752・761・788・926（641 の重複） |
| test の不足（`test_gap`） | 16 | 13 | 1 | 2 | 0 | 50% | 230 / 695 | 1,349 | 440, 530（採用）、389（重複） |
| 文書と実装のずれ（`docs_drift`） | 72 | 29 | 33 | 10 | 0 | 25% | 136 / 721 | 832 | 449, 535（採用）、410・411（不採用）、709（重複） |
| 決まった事の残り（`remaining_scope`） | 82 | 47 | 26 | 9 | 0 | 37% | 199 / 655 | 1,093 | 441, 555（採用）、481・483（不採用） |
| 改善の案（`improvement`） | 79 | 39 | 34 | 6 | 0 | 29% | 285 / 691 | 1,122 | 509, 611（採用）、423（不採用） |
| 測定・確認の依頼（`measurement`） | 21 | 11 | 7 | 2 | 1 | 29% | 9 / 465 | 2,497 | 460, 537（採用）、760（563 の重複）、934（未決） |
| 人の判断の依頼（`decision`） | 25 | 3 | 19 | 3 | 0 | 8% | 15 / 662 | 2,377 | 146, 574（採用）、269, 270（不採用） |
| 運用・host の作業（`ops`） | 14 | 4 | 8 | 2 | 0 | 14% | 157 / 586 | 2,229 | 572, 720（採用）、391, 873（不採用） |
| その他（`other`） | 1 | 0 | 1 | 0 | 0 | 0% | 1,030 | — | 160 |
| 合計 | 492 | 260 | 180 | 50 | 2 | 31% | 232 / 694 | 1,071 | — |

### runtime の planner の後（2026-09-25 23:00 UTC 以降の登録、352 件）

| 分類 | 件数 | 採用 | 不採用 | 重複 | 未決 | 採用率 | 重複率 | 着地した割合 | 枠待ち（中央値 / p90） | draft のまま（中央値） | 着地まで（中央値） |
|---|---|---|---|---|---|---|---|---|---|---|---|
| `defect` | 106 | 85 | 17 | 3 | 1 | 80% | 3% | 37% | 170 / 637 | 232 | 1,038 |
| `flaky_test` | 29 | 13 | 4 | 12 | 0 | 45% | 41% | 28% | 87 / 605 | 87 | 746 |
| `test_gap` | 14 | 13 | 0 | 1 | 0 | 93% | 7% | 57% | 175 / 695 | 176 | 1,349 |
| `docs_drift` | 43 | 21 | 12 | 10 | 0 | 49% | 23% | 23% | 76 / 723 | 102 | 775 |
| `remaining_scope` | 58 | 32 | 19 | 7 | 0 | 55% | 12% | 38% | 14 / 614 | 40 | 1,093 |
| `improvement` | 55 | 33 | 16 | 6 | 0 | 60% | 11% | 31% | 226 / 642 | 253 | 921 |
| `measurement` | 20 | 11 | 6 | 2 | 1 | 55% | 10% | 30% | 8 / 448 | 5 | 2,497 |
| `decision` | 15 | 2 | 10 | 3 | 0 | 13% | 20% | 7% | 4 / 449 | 4 | 93 |
| `ops` | 12 | 4 | 6 | 2 | 0 | 33% | 17% | 17% | 127 / 360 | 136 | 2,229 |
| 合計 | 352 | 214 | 90 | 46 | 2 | 61% | 13% | 32% | 79 / 642 | 102 | 1,026 |

- それより前の 140 件は採用 46（33%）、不採用 90、重複 4 で、draft のままの中央値は 510 分。`decision` は 10 件のうち 9 件、`docs_drift` は 29 件のうち 21 件、`improvement` は 24 件のうち 18 件が不採用だった。
- 直近 24 時間（97 件）は採用 51、不採用 23、重複 21（22%）、未決 2。`flaky_test` は 9 件のうち 5 件、`decision` は 3 件のうち 2 件が重複だった。
- 登録の日ごと（JST）: 09-23 7、09-24 41、09-25 62、09-26 130、09-27 160、09-28 92。着地は同じ日に 51・33・40・102・158・101（09-22 の 36 件を足すと、要点の 521 件になる）。
- 元の task は 297 件で、1 件の receipt が出す follow_up は 1 件が 169、2 件が 83、3 件以上が 45。最多は task 210 の 11 件（ADR の棚卸し）、815 の 7 件。
- 重複の先 50 件のうち 21 件は、それ自身も follow_up だった。同じ所を別々の run の worker が follow_up に書いている。
- `measurement`・`decision`・`ops` は着地までが長い（中央値 37〜42 時間）。前提の着地を待つ測定や、人が値を決める作業を、他の task と同じ ready の列に入れているため。
- 不採用の理由は記録に無い（`task_status_changed` の payload は `from`・`to`・`duplicate_of` だけ）。上の分類の不採用が「価値が低い」のか「別の task が受け持った」のかは分けられない。

## runtime の planner の判断にかかった時間と、枠待ちの時間

| 項目 | 値 |
|---|---|
| planner が立った follow_up（2026-09-25 23:00 UTC 以降の登録） | 344 / 352 件。残りの 8 件は planner が立つ前に人か plan review が決めた |
| planner の回数（492 件全体。導入前の draft も migration で対象になった） | 1 回 395 件、2 回 3 件、0 回 94 件（`planner_question` の答えを持って立ち直したもの）、`draft_planner_exhausted` は 0 件 |
| 登録から planner が立つまで（枠待ち） | 中央値 79.4 分、p90 641.5 分、最大 752.4 分、合計 1,181 draft 時間 |
| planner が立ってから判断まで | 中央値 0.6 分、p90 1.3 分、合計 34.7 時間（`planner_question` の答え待ちを含む） |
| planner の session の稼働（180 session、2026-09-27 12:18 UTC 以降） | `active_secs` の中央値 0.6 分、合計 2.0 時間。壁時計の中央値 0.7 分、合計 21.1 時間 |
| 分類ごとの稼働の中央値 | `defect` 0.8 分、`test_gap` 0.7 分、`flaky_test`・`improvement` 0.6 分、`remaining_scope` 0.5 分、`docs_drift`・`measurement`・`decision`・`ops` 0.4 分 |
| `planner_question`（follow_up の draft に開いたもの） | 42 件。`adopt` 40、`keep_draft` 1、方針の自由文 1。答えまでの中央値 1.7 分、p90 197.1 分、合計 25.8 時間。分類は `defect` 20、`flaky_test` 7、`docs_drift` 5、その他 10 |

planner は枠が空いた時間にまとめて立った（1 時間に 29〜42 件）。枠が空かなかった主な区間と、その間に枠を持っていたもの:

| 区間（JST） | 長さ | 枠を持っていたもの（記録から分かる範囲） | 未決の山 |
|---|---|---|---|
| 09-27 08:11 → 21:00 ごろ | 約 13 時間 | 前半は 663・664 の planner。`planner_question`（ask 121・122）は 08:11 に答えられたが、close は 14:49。後半は記録から特定できない（revise の planner は 13:28 に `planner_unresponsive`） | 21:00 に 79 件 |
| 09-28 00:18 → 08:00 ごろ | 約 8 時間 | 記録から特定できない（01:10 に revise の `planner_unresponsive`） | 08:00 に 57 件 |
| 09-28 09:07 → 12:24 | 197 分 | 886 の planner（ask 164 の答え待ち） | 12:00 に 17 件 |
| 09-28 12:58 → 18:38 | 340 分 | 899 の planner（ask 176 の答え待ち。15:36 の `planner_unresponsive` の `holders` に `planner_question_open` と出る） | 18:00 に 32 件 |

判断そのものは 1 件 1 分に満たないので、draft の数を減らしても planner の稼働は大きく減らない（記録のある 180 session で合計 2 時間）。未決の draft がたまったのは、`runtime_planners` が 1 のまま、人の答えを待つ planner が枠を塞いだため。これは task 942（`runtime_planners = 2`）と goal 63 が扱っている。follow_up の数を減らすことの効き目は、枠が塞がったときにたまる量と、人に届く `planner_question` の数を減らすことにある。

## ラベルの定義案

goal 64 の後続（ADR と runtime の実装）の材料。worker が receipt の `follow_ups` の各要素に `kind` を付ける前提で書く（例: `{"title": ..., "description": ..., "kind": "flaky_test"}`）。ラベルの名前は snake_case の英語。件数は全期間 / runtime の planner の後。

### 付け方の規則（案）

1. worker は follow_up ごとにラベルを 1 つ付ける。迷ったら、follow_up を片付けたときに何が変わるかで選ぶ（runtime の挙動が直る → `defect`、test が安定する → `flaky_test`、文書が実装に追いつく → `docs_drift`）。
2. `flaky_test` と `test_gap` は、対象の test の名前（`<module>::<name>`）を description に必ず書く。重複の検出に使う。
3. 他の task と重なると分かっているなら、ラベルとは別に `duplicate_of` の候補（task の ID）を書く。ラベルに「重複」は作らない（重複は中身ではなく結末なので）。
4. どれにも当たらなければ `other` にして、短い説明を添える。この期間の `other` は 1 件（toolchain を上げるときに見る 1 行の注意）。
5. runtime は `follow_up_registered` の payload と `draft_origins` の `material` にラベルを記録し、`stats` と `kpi` はラベルごとに件数・採用・不採用・重複・着地・draft のままの時間を出す。ラベルの無い過去の follow_up は `unlabeled` として数え、書き換えない。
6. 不採用の理由（`cancel` の理由）は、ラベルとは別に planner が cancel のときに記録する欄に持つ（goal 64 の cancel の分類、task 952 の範囲）。

### ラベル

| ラベル | 定義 | 判定の例 | 件数 |
|---|---|---|---|
| `defect` | runtime（または script）が決まった仕様どおりに動かない経路を見つけた。再現の条件か、コードの場所と誤りが書ける | 723: 開閉中の window で workspace の一覧が失敗する。793: pid を別プロセスに取られた supervisor に signal を送る。342: adopt の answer が拒まれると status が applying のまま残る | 145 / 106 |
| `flaky_test` | 既存の test が負荷や順序で時々落ちる・時間切れになる。test の名前と落ち方を書く | 682: `runtime_adopt::a_supervisor_that_lost_its_lease_stops_touching_the_run` の負荷時の不安定。641 と、それに重なった 752・761・788・926 | 37 / 29 |
| `test_gap` | 経路に test が無い、または test の作りが弱い（上限の無い待ち、実時計への依存）。今は落ちていない | 440: e2e の上限の無い待ちに時間切れを付ける。530: held として park される経路の test。875: marker の mtime の前後に頼る test | 16 / 14 |
| `docs_drift` | 文書（`docs/design`・ADR の索引・plugin の skill・AGENTS.md）が実装か accepted の ADR とずれている。直す先の文書と、ずれの中身を書く | 535: supervise.md の手順 11 が古い。449: 分割で宛先の外れた「上の…」。709: SKILL.md が 8 KiB の上限に達した | 72 / 43 |
| `remaining_scope` | accepted の ADR か goal が決めた事のうち、この task で実装しなかった残り。ADR の決定の番号か goal を書く | 441: ADR-0047 決定 39・40 の残り。555: 決定 25 の /exit の再試行。367〜373: 統合 ADR の組 D〜J | 82 / 58 |
| `improvement` | 仕様の誤りではないが、よくする案（観測・集計の追加、refactor、速さ、使い勝手） | 509: land_phases.verify を検証コマンドごとに分ける。611: draft の流入と流出を KPI に足す。219: observer と watch を application に移す | 79 / 55 |
| `measurement` | 着地の後か期間の後に数えて確かめる依頼。前提（何の着地の後か、何 run 分か）を書く | 460: sccache の導入前後の llvm-cov。537: nextest の前後の verify。934: 918 の着地後の夜の答え待ち | 21 / 20 |
| `decision` | 人か planner の判断が要る問いで、作業の中身が決まっていない（「〜するか決める」） | 146: revise の回数の数え方を ADR で決める。269: goal 依存の循環検出で canceled の task を外すか。538: prompt に task の kind を渡すか | 25 / 15 |
| `ops` | repository の変更ではなく、人か inbox が host・本番 queue・外部サービスで行う作業（固定バイナリが対応した後の `dagq.toml` の値の追記を含む） | 391: queue dir の空の sqlite ファイルを消す。572: `dagq.toml` に KPI の目標を書く。908: broker の crate の初めての publish | 14 / 12 |
| `other` | どれにも当たらない。説明を添える | 160: toolchain を上げるときに 1 行を確かめる | 1 / 0 |

`decision` と `ops` は、follow_up（task の draft）にするのが合わない種類を runtime が数えるためのラベルでもある。付いた follow_up が多ければ、worker への指示か、follow_up 以外の行き先（下の候補 2）の直しどころを示す。

### 他の分類との関係

- review の差し戻しのラベル（[review-sendback-reasons](review-sendback-reasons.md)）の `docs_out_of_sync`・`test_missing` と、この文書の `docs_drift`・`test_gap` は同じ種類の問題を指す。review で差し戻す前に worker が follow_up に回したものか、review が見つけたものかを 1 つの集計で追えるよう、名前を揃えるとよい。
- worker の問いのラベル（[worker-question-topics](worker-question-topics.md)）の `out_of_scope_change` は、ask にせず follow_up に回すと、この文書の `defect`・`remaining_scope`・`docs_drift` のどれかになる。
- observer の finding の種類（`flaky_test` など）と名前が重なる。finding は runtime の観測から、follow_up は worker の作業から出るので、出どころは `draft_origins` の `origin` で分かる。

## 価値の低い follow_up を減らす・絞る候補

決定はしない。見込みは runtime の planner の後の約 2.63 日（352 件）の実績をそのまま当てはめたもの。planner の稼働は 1 件 0.4〜0.8 分なので、どの候補でも planner の稼働の節約は小さい（各候補に、分類ごとの稼働の中央値 × 件数の見込みを添えた）。効き目は、枠が塞がったときにたまる draft の数、人に届く `planner_question` の数、ready の列に積まれる follow_up の数に出る。

1. **`flaky_test` と `test_gap` は test の名前を必須にし、runtime が同じ test の名前を持つ開いた task（draft・submitted・ready・in_progress）に重なる follow_up を draft にせず、既存の task の note にする**。worker の prompt には、follow_up に書く前に `dagq search <test の名前>` で既存の task を確かめるよう書く。対象は `flaky_test` の重複 12 件（全期間 13 件。641 に 4 件、722 に 2 件）と `test_gap` の重複 1 件で、約 13 件（planner の後の 3.7%、1 日あたり約 5 件）の draft と、そのための planner の起動が減る。planner の稼働の見込みは 13 件 × 0.6 分 ≈ 8 分。自動の判定は名前の一致だけにし、名前の違う重複（同じ e2e の不安定を別の assertion で書いたもの）は今までどおり planner に残す。
2. **`decision` と `ops` を follow_up の draft にしない**。`decision` は worker が receipt の `summary` に「決めていないこと」として書き、goal の planner か人が読む（ask にするかは worker の問いの規則のまま）。`ops` は inbox に attention（たとえば `do by hand`）として出し、人か inbox が行って閉じる。対象は planner の後の `decision` 15 件（採用 2）と `ops` 12 件（採用 4）で、draft は 27 件（7.7%、1 日あたり約 10 件）減り、不採用・重複の 21 件は planner を通らなくなる。planner の稼働の見込みは 27 件 × 0.4 分 ≈ 11 分。採用された 6 件（574 の ADR、572・720 の目標の値など）は、別の経路（planner が人と決めて `add` する）で残す必要がある。
3. **`docs_drift` のうち、run の `--paths` の中で直せるものは同じ run で直し、follow_up にしない**（worker の prompt の指示）。paths の外の文書（plugin の skill、AGENTS.md）のずれは、同じ機能の follow_up を 1 件にまとめる。planner の後の `docs_drift` 43 件のうち 22 件（不採用 12、重複 10）が採られていない。自分の run の paths の中のずれがどれだけあったかは今の記録では分けられないので、見込みは最大 22 件（6%）の draft と、plugin の skill の同じ節への follow_up の重なり（709・671・666 など、8 KiB の上限の件は 5 件）。planner の稼働の見込みは最大 22 件 × 0.4 分 ≈ 9 分。
4. **`planner_question` で人の `adopt` を求める follow_up（goal が無いか、`follow_up_depth` が上限を超えるもの）のうち、`defect`・`flaky_test`・`test_gap` は plan review を関門にして人の adopt を求めない**（[ADR-t808-1](../adr/2026-09-28-t808-1-runtime-planners-submit-follow-ups-up-to-depth-two.md) の上限の見直しになる）。follow_up への `planner_question` 42 件のうち 40 件が `adopt` で、この 3 種類が 29 件（`defect` 20、`flaky_test` 7、`test_gap` 2）。当てはめると、人に届く ask が約 29 件（約 1 日 11 件）減り、答え待ちで枠を持った時間（899 の 340 分、886 の 197 分を含む合計 25.8 時間の大半）が消える。人の adopt が無くなる分、goal の無い follow_up が plan review だけで ready になり、ready の列（すでに follow_up が 100 件）がさらに伸びうる。そのため、goal の無い follow_up の優先度を `low` に固定するなど、ready に入った後の扱いと組にする必要がある。

候補 1〜3 を全部当てはめると、planner の後の 352 件のうち最大 62 件（約 18%）の draft が減る（`flaky_test` 13、`decision`・`ops` 27、`docs_drift` 22）。候補 4 は draft を減らさず、人に届く ask と枠の塞がりを減らす。枠待ちの長さ（中央値 79 分、p90 642 分）の主因は枠が人の答えで塞がることなので、候補 1〜3 だけでは枠待ちは大きく変わらず、候補 4 か task 942 の `runtime_planners = 2` が効く。

## 限界

- 分類は 1 人の読み手（この task の worker）が title と description から付けたもので、別の読み手との一致は測っていない。`defect` と `improvement`（仕様が曖昧な挙動の改善）、`defect` と `remaining_scope`（ADR が決めた事がまだ無い状態を不具合と呼ぶか）の境は判断が入る。
- 不採用の理由は記録に無いので、不採用が「価値が低い」か「別の task が受け持った（`duplicate_of` を付けずに cancel）」かを分けていない。`duplicate_of` は 2026-09-26 00:16 UTC より前には記録されないので、それより前の重複は不採用に数えている。
- 着地したものの効果（直った不具合がその後の run をどれだけ助けたか）は測っていない。採用されて ready のまま待っている follow_up が 100 件あり、着地した割合は今後も上がる。
- runtime の planner の session の記録は 2026-09-27 12:18 UTC から始まるので、それより前の planner の稼働は数えていない。枠を塞いだものの一部（09-27 の後半、09-28 の未明）は記録から特定できなかった。
- 集計に使った中間のファイル（events・list・asks の JSON と分類の表）は commit していない。上の表の task ID から読み直せる。

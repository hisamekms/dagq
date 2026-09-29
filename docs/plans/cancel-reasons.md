---
id: plan-cancel-reasons
type: plan
title: task の cancel の理由の分類と、ラベルの定義案
status: completed
created: 2026-09-28
updated: 2026-09-30
owners:
  - hisamekms
tags:
  - measurement
  - cancel
  - planner
related:
  - adr-0046
  - adr-t598-1
  - design-supervisor-lifecycle-draft-planners
  - plan-follow-up-kinds
  - plan-review-sendback-reasons
  - plan-worker-question-topics
---

# task の cancel の理由の分類と、ラベルの定義案

task 952（goal 64）。canceled の task を 1 件ずつ読み、cancel の理由を分類した。分類ごとに、件数、cancel した actor、登録から cancel までの時間、ready まで進んでから cancel したもの、run・plan review・runtime の planner を使ってから cancel したもの（計画と作業の無駄）を出す。そのうえで、`dagq cancel` で付けるラベルの定義案と、計画の無駄を減らす候補を置く。この文書は決定をせず、task も登録しない。

今の cancel が構造として持つ理由は、重複の先の `duplicate_of`（[ADR-0046](../adr/0046-full-text-search-related-and-duplicate-of.md)。`task_status_changed` の payload）だけで、それ以外の理由は planner の note（`observation` の `kind: note`）や、後から登録した task の context の自由文にしか無い。runtime の planner の cancel の多くは、どこにも理由が残っていない。

## 要点

- **canceled の task は 282 件で、登録された 979 件の 29%**（completed 524、ready 153、draft 16、他 4）。記録は 2026-09-22 04:50 UTC から始まり（最初の cancel は 12:55 UTC）、直近 7 日（2026-09-21 14:00 UTC〜）は全期間を含むので、2 つの期間の数字は同じになる。代わりに、runtime の planner が follow_up の draft を決めるようになった後（2026-09-25 23:00 UTC 以降の cancel、168 件）と、直近 24 時間（53 件）を並べた。282 件のうち 237 件（84%）は receipt の follow_up から出た draft だった。
- **理由が記録から読めるのは 165 件（59%）で、そのうち 53 件は `duplicate_of`（2026-09-26 00:16 UTC から記録される）、残りは planner の note と後継の task の context**。runtime の planner の後は 168 件のうち 89 件（53%）、直近 24 時間は 53 件のうち 29 件（55%。うち 25 件が `duplicate_of`）。重複以外の理由で runtime の planner が落とした draft は、ほとんど理由が残っていない（`not_worth` 43 件のうち記録あり 6 件）。残りは title・description・時刻と前後の event から推定した。
- **全期間の分類は、作り直し 58、重複 62、実装済み 21、取り込み 40、方針の変更で不要 26、費用に見合わない 49、判断の依頼が不要 11、repository の作業でない 13、放置で古くなった 2**。runtime の planner の後は、重複（54）と費用に見合わない（43）が大きく、合わせて 58%。
- **作り直し（58 件）は 2026-09-25 より前だけに起きた**。draft に verify・evidence・paths を後から付けるコマンドが無く、同じ中身を新しい ID で登録し直していた（09-24 22:07〜22:11 の draft 棚卸しで cancel された 42 件のうち 27 件）ほか、ADR の 4 桁の番号の衝突（212・213・215・261）で作り直した。`task_edited`（最初は 09-25 11:22 UTC）と `set-paths`、[ADR-t598-1](../adr/2026-09-26-t598-1-adr-id-is-task-id-small-adrs-and-design-holds-current-state.md) の ADR の ID の後は 0 件。
- **run を使ってから cancel したのは 2 件（44 分）だけ**。58（e2e の失敗で validating が受理せず、75 として登録し直し、13 分）と 74（`/exit` の時間切れから recover し、101 として登録し直し、31 分）で、どちらも 2026-09-23。無駄の中心は run ではなく計画の側にある: runtime の planner を 160 回（canceled の draft に対する `draft_planner_opened`）、plan review を 10 回、submit を 20 回使い、29 件は ready の状態で（ほかに 2 件は in_progress で）cancel された。
- **期間中に plan review が cancel した記録は無い**。plan review の verdict の `actions` には `add_dependency`・`lower_priority` のほかに、pass のときに `cancel --duplicate-of` と同じ記録で明らかな重複を cancel する `cancel_duplicate` もある（[Plan review](../design/supervisor-lifecycle/plan-review.md) の 5）が、期間中の `plan_review_finished` の `actions` は `add_dependency` と `lower_priority` だけで `cancel_duplicate` は 0 件だった（task 998 が 2026-09-30 に `dagq events --full --all --kind plan_review_finished` で確かめた。この時点までの全 436 件でも 0 件）。cancel はすべて planner（人が開いたものか runtime が立てたもの。actor の記録のある 77 件はどれも `planner`）か、actor の記録が入る前（2026-09-27 09:13 UTC より前の 205 件）の人と planner の対話による。plan review の判断は、revise の理由として planner の cancel に効いた（456・457 など）。

## 方法

- 期間: 最初の event（2026-09-22 04:50 UTC）から最後の cancel（2026-09-28 13:43 UTC、task 977）まで。runtime の planner の最初の `draft_planner_opened` は 2026-09-25 23:00 UTC。
- 読んだもの（固定バイナリ `~/.local/bin/dagq` の読み取り専用のコマンドだけ）: `dagq events --all --full`（`task_status_changed`・`task_created`・`follow_up_registered`・`draft_planner_opened`・`task_submitted`・`plan_review_finished`・`task_edited`・`observation`、canceled の task の run の event）、`dagq list --all --full`（979 件の title・description・context・status）、`dagq list --status canceled --full`、`dagq notes`。queue の状態を変えるコマンドは打っていない。`dagq show --full`・`dagq stats --full`・`dagq timeline` は、上で必要な欄が揃ったので使っていない（run を使った 2 件は `events` の run の event で時間を出した）。
- 理由の手がかり: (1) `task_status_changed` の `duplicate_of`、(2) cancel の ±30 分の planner の note でその task の ID を名指すもの、(3) 全期間の note・plan review の summary と reasons・他の task の title / description / context / 編集前の値のうち、その ID と「cancel・取り下げ・重複・統合・寄せ・置き換え・引き継ぐ・作り直し・着地済み・covered・dropped」などの語が近くにあるもの。どれも無いものは、title と description・cancel の時刻・同じ分に cancel された他の task・同じ title の後継の task から推定した。
- actor: `task_status_changed` の `actor` がある 77 件（2026-09-27 09:13 UTC 以降）はそれを使い、`planner:<id>` の id が `draft_planner_opened` の `planner_id` にあれば runtime の planner とした。actor の無い 205 件は、cancel の前 3 時間以内にその task の `draft_planner_opened` があれば runtime の planner（推定、77 件）、2026-09-25 23:00 UTC より前は人と planner の対話（note に「人の判断で」「planner と人」とあるもの、114 件）、それ以後は人が開いた planner（推定、14 件）とした。
- 時間: 登録（`task_created`）→ cancel（`task_status_changed` の `to: canceled`）。draft のまま、ready のままの時間を含む。人の答え待ち（`planner_question`）で延びた分は分けていない（follow_up の draft への `planner_question` は 42 件のうち 40 件が `adopt` で、cancel に至ったものは少ない。[follow-up-kinds](follow-up-kinds.md)）。
- 分類: 1 件に主の理由を 1 つ付けた。重複の先が cancel の時点で completed なら「実装済み」、開いていれば「重複」にした（456・457 は `duplicate_of` 付きだが、中身は後継の ADR-0075 の設計への置き換えなので「方針の変更で不要」にした）。付けたのはこの task の worker 1 人。

## 分類ごとの集計

### 全期間（= 直近 7 日、282 件）

時間は分。「合計」は登録から cancel までの時間の合計（時間）。actor は、人: 人と planner の対話（actor の記録の前）、RP: runtime の planner（推定を含む）、HP: 人が開いた planner（推定を含む）。

| 分類 | 件数 | 記録あり | actor（人 / RP / HP） | 登録→cancel（中央値 / p90） | 合計 | ready から | run | submit / plan review / RP の起動 | 代表の task |
|---|---|---|---|---|---|---|---|---|---|
| 作り直し（`re_registered`） | 58 | 46 | 58 / 0 / 0 | 526 / 1,473 | 637 | 10（in_progress から 2） | 2 件・44 分 | 0 / 0 / 0 | 140→235 など棚卸しの 27 件、149→183、58→75、74→101、212・213・261（ADR 番号） |
| 重複（`duplicate`） | 62 | 54 | 8 / 50 / 4 | 157 / 657 | 272 | 3 | 0 | 1 / 1 / 51 | 752・761・788・926（641 の重複）、289→285、380→409 |
| 実装済み（`already_done`） | 21 | 20 | 2 / 18 / 1 | 413 / 753 | 192 | 2 | 0 | 0 / 0 / 18 | 704→318、866→816、882（note は記録済み）、214→225 |
| 取り込み（`absorbed`） | 40 | 19 | 19 / 21 / 0 | 50 / 610 | 135 | 1 | 0 | 0 / 0 / 21 | 350・351・341（仕分けで引き継ぎ）、465・523（統合）、269・270・299→304 |
| 方針の変更で不要（`superseded`） | 26 | 18 | 14 / 1 / 11 | 789 / 1,917 | 397 | 13 | 0 | 19 / 9 / 8 | 367〜373（ADR-t598-1）、206・207（goal 29）、262〜264、456・457→501・503 |
| 費用に見合わない（`not_worth`） | 49 | 6 | 6 / 43 / 0 | 256 / 704 | 290 | 0 | 0 | 0 / 0 / 43 | 398、524・693（落ちた記録なし）、725・826、848（再現できない） |
| 判断の依頼が不要（`decision_moot`） | 11 | 1 | 3 / 8 / 0 | 15 / 662 | 41 | 0 | 0 | 0 / 0 / 8 | 374（ask 88 で答え済み）、519、538、772 |
| repository の作業でない（`not_repo_work`） | 13 | 1 | 2 / 11 / 0 | 358 / 586 | 66 | 0 | 0 | 0 / 0 / 11 | 391、484（観察）、855、873、908、939 |
| 放置で古くなった（`stale`） | 2 | 0 | 2 / 0 / 0 | 2,031 / 2,045 | 68 | 0 | 0 | 0 / 0 / 0 | 125、120 |
| 合計 | 282 | 165 | 114 / 152 / 16 | 307 / 1,339 | 2,097 | 29（in_progress から 2） | 2 件・44 分 | 20 / 10 / 160 | — |

- 作り直しの 58 件は全部が人と planner の対話によるもので、09-22〜23 の goal 6 の 7 件（52〜56 を登録し直した 57〜61 のうち 58・61 も、改名の後に 65・75 として登録し直し）、2026-09-23 12:39 の goal 3 の 7 件（34・36〜40・78。runtime の変更に e2e の evidence を後から付けられず、登録し直したと推定）、09-24 22:08〜22:11 の draft 棚卸しの 27 件（`draft 棚卸し（2026-09-25）で人が登録を決めた、follow_up の draft の置き換え`）、09-24 23:29 の draft 整理の 4 件を含む。
- 方針の変更で不要の 26 件は、ready で待っていた task が多い（13 件）。367〜373（統合 ADR の組 D〜J）は登録から約 25.6 時間、ready で待った後、ADR-t598-1（統合 ADR を作らず design が今の姿を持つ）で 09-26 14:31 UTC に一度に cancel された。この 7 件で submit 15 回・plan review 9 回を使った（367 は 5 回 submit）。
- 取り込みは、別の task の受け入れ条件や note に中身を移したもの。人と planner の 09-25 12:55 の follow_up の仕分け（8 件、note に「task N を引き継ぐ」）と、runtime の planner が「この task に統合した」と note を残したもの（465・523）が記録のある例。
- 実装済みの 21 件のうち 17 件は、`duplicate_of` の先が cancel の時点で completed だったもの（残りは note と後継の task から）。draft が枠を待つ間に、同じことをする task が着地していた（中央値 413 分）。

### runtime の planner の後（2026-09-25 23:00 UTC 以降の cancel、168 件）

| 分類 | 件数 | 記録あり | actor（RP / HP） | 登録→cancel（中央値 / p90） | 合計 | ready から | submit / plan review / RP の起動 |
|---|---|---|---|---|---|---|---|
| `duplicate` | 54 | 47 | 50 / 4 | 107 / 615 | 211 | 1 | 1 / 1 / 51 |
| `already_done` | 19 | 19 | 18 / 1 | 361 / 736 | 164 | 1 | 0 / 0 / 18 |
| `absorbed` | 21 | 6 | 21 / 0 | 5 / 475 | 62 | 0 | 0 / 0 / 21 |
| `superseded` | 12 | 9 | 1 / 11 | 1,537 / 1,537 | 185 | 9 | 19 / 9 / 8 |
| `not_worth` | 43 | 6 | 43 / 0 | 256 / 692 | 237 | 0 | 0 / 0 / 43 |
| `decision_moot` | 8 | 1 | 8 / 0 | 1 / 662 | 29 | 0 | 0 / 0 / 8 |
| `not_repo_work` | 11 | 1 | 11 / 0 | 127 / 586 | 42 | 0 | 0 / 0 / 11 |
| `re_registered`・`stale` | 0 | — | — | — | — | — | — |
| 合計 | 168 | 89 | 152 / 16 | 253 / 704 | 932 | 11 | 20 / 10 / 160 |

- 同じ期間の登録は 553 件、着地（`run_integrated`）は 346 件。cancel は着地 1 件あたり約 0.49 件。
- 直近 24 時間（2026-09-27 14:00 UTC 以降、53 件）は、全部が runtime の planner の cancel で、`duplicate` 16、`already_done` 10、`not_worth` 18、`not_repo_work` 6、`decision_moot` 2、`absorbed` 1。登録→cancel の中央値は 279 分、p90 は 605 分。直近 24 時間の登録は 155 件。
- 重複の先を数えると、641（e2e の `up_in_cmux_starts_a_supervisor_in_a_workspace_that_down_wait_stops_and_closes` の負荷下の失敗）に 4 件、816（Codex の adapter）に 3 件（うち `already_done` 3）、627・722・732・818・878 に 2 件ずつ。同じ test や同じ機能を、別々の run の worker が follow_up に書いている（[follow-up-kinds](follow-up-kinds.md) の `flaky_test` の重複と同じ）。
- 登録→cancel の時間の大半は、runtime の planner の枠を待つ時間（follow_up の draft の枠待ちの中央値 79 分、p90 642 分。[follow-up-kinds](follow-up-kinds.md)）で、判断そのものは 1 件 1 分に満たない。
- 日ごとの cancel（JST）: 09-22 6、09-23 14、09-24 13、09-25 81、09-26 66、09-27 52、09-28 50。

## ラベルの定義案

goal 64 の後続（ADR と runtime の実装）の材料。`dagq cancel ID --reason <label>` でラベルを 1 つ付ける前提で書く。名前は snake_case の英語。件数は全期間 / runtime の planner の後。

### 付け方の規則（案）

1. cancel は必ず `--reason` を 1 つ持つ。迷ったら「この task の中身は今どこにあるか」で選ぶ: 開いた別の task にある → `duplicate`、着地した task か main にある → `already_done`、別の task の一部になった → `absorbed`、同じ中身を新しい ID で登録し直した → `re_registered`、どこにも無く、要らなくなった → `superseded`・`not_worth`・`decision_moot`・`not_repo_work`・`stale`。
2. `--duplicate-of N` は今のまま残し、中身を受け持つ task を指す欄として `duplicate`・`already_done`・`absorbed`・`re_registered` で使う（`superseded` では後継の task があれば付ける）。`--duplicate-of` だけを付けた cancel は、runtime が cancel の時点の N の状態から `duplicate`（N が開いている）か `already_done`（N が completed）を補う。`related` と `search`（ADR-0046）が重複の組として読むのは `duplicate` と `already_done` だけにし、`absorbed` と `re_registered` の組は別に扱う。
3. `--reason` が `duplicate`・`already_done`・`absorbed`・`re_registered` で `--duplicate-of` が無ければ拒否する。それ以外の理由は短い説明（`--note` か、今の note）を添える。
4. runtime は `task_status_changed` の payload に `reason` を記録し、`stats` と `kpi` はラベルごとに件数・actor・登録から cancel までの時間・ready から cancel した件数・使った run と plan review と runtime の planner の数を出す。理由の無い過去の cancel は、`duplicate_of` があれば 2 の規則で補い、無ければ `unrecorded` として数え、書き換えない。
5. follow_up の draft には、receipt の `follow_ups` の種類（[follow-up-kinds](follow-up-kinds.md) のラベル）が別に付く。cancel の理由はそれと別の軸で、両方を並べると「どの種類の follow_up が、なぜ採られなかったか」が読める。

### ラベル

| ラベル | 定義 | 判定の例 | 件数 |
|---|---|---|---|
| `duplicate` | 同じ中身の開いた task（draft・submitted・ready・in_progress）がある。`--duplicate-of` にその task を書く | 752・761・788・926 → 641（同じ e2e の負荷下の失敗）。380 → 409（同じ hook の実装）。289 → 285 | 62 / 54 |
| `already_done` | 中身は着地した task か main ですでに満たされている。`--duplicate-of` に着地した task を書く | 704 → 318（着地済みの skill の直し）。866・857・860 → 816。882（goal 36 の note はすでに記録されていた） | 21 / 19 |
| `absorbed` | 中身の一部か全部を、別の task の受け入れ条件や description・note に移して閉じる。`--duplicate-of` に移した先を書く | 269・270・299 → 304（follow_up 3 件をまとめる）。465・523（stats の自動修正の task に統合）。350・351（仕分けで実装 task に引き継ぎ） | 40 / 21 |
| `re_registered` | 同じ意図を新しい ID で登録し直した（欄を後から直せない、番号の衝突、改名）。`--duplicate-of` に新しい task を書く | 140 → 235 など draft 棚卸しの 27 件。149 → 183（verify を付けるため）。213 → 215、261 → 286（ADR 番号の衝突） | 58 / 0 |
| `superseded` | 前提の決定や方針が変わり、task の中身が要らなくなった。変えた決定（ADR・goal・人の決定）を書く | 367〜373（ADR-t598-1 で統合 ADR を作らない）。206・207（goal 29 に置き換え）。526・527（build サービスの案をやめた）。158（ADR の決定と逆向き） | 26 / 12 |
| `not_worth` | 中身は正しいが、変更と検証の費用に見合わない（まれな端の場合、起きた記録が無い、効率だけ）。見送った理由を書く | 398（retry の待ちの見直し）。725・826（着地の関門と e2e に見合わない）。524・693（落ちた記録の無い待ち）。848（再現できない） | 49 / 43 |
| `decision_moot` | 判断を求める task で、答えがすでに出たか、判断しないことにした | 374（ask 88 で答え済み）。519・538・772（決めずに今のままにする） | 11 / 8 |
| `not_repo_work` | repository の変更ではなく、人か inbox が host・本番 queue・外部サービスで行う作業か、観察・確認だけ | 391（空の sqlite を消す）。855（host に TALA を入れる）。908（crates.io の初めての publish）。939（workflow_dispatch で確かめる） | 13 / 11 |
| `stale` | 前提（役割・ファイル・コマンド）が消えて、中身が意味を持たなくなった。後継が無い | 125（まだ無かった dagq-planner skill への移動）。120（maintainer の hook の前提） | 2 / 0 |
| `other` | どれにも当たらない。説明を添える | この期間は無し | 0 / 0 |

- `not_worth`・`decision_moot`・`not_repo_work` の 3 つは、follow_up の種類の `improvement`・`decision`・`ops` と重なることが多いが、同じではない（`defect` の follow_up が `not_worth` で落ちることもある。例: 769・916 のまれな端の場合）。
- `superseded` と `stale` の違いは、後継の決定があるかどうか。決定が変わったなら `superseded`、決定なしに前提が消えたなら `stale`。

## 計画の無駄を減らす候補

決定はしない。見込みは runtime の planner の後の約 2.6 日（168 件）の実績をそのまま当てはめたもの。run の無駄は 2 件・44 分（どちらも 09-23）なので、効き目は draft の数・runtime の planner の起動・plan review と submit の回数・ready の列に積まれる時間に出る。runtime の planner の判断は 1 件 0.4〜0.8 分（[follow-up-kinds](follow-up-kinds.md)）なので、planner の稼働の節約はどの候補でも小さい。

1. **follow_up を draft にする時点で、`search` / `related` の上位の候補を材料に付け、test の名前か task の ID が一致するものは draft にせず既存の task の note にする**（integrate の `register_follow_ups` と worker の prompt。worker には follow_up を書く前に `dagq search <test の名前か機能の語>` で確かめさせる）。runtime の planner の後の `duplicate` と `already_done` は 73 件（43%、211 + 164 = 375 draft 時間）。`duplicate_of` のある 51 件のうち、重複の先が draft より前に登録されていたのは 47 件で、draft と重複の先が同じ task の ID か同じ test の名前（`<module>::<name>`）を含むのは 6 件（248・389・713・822・933・957。74 draft 時間）、12 文字以上の同じ識別子（関数名・ファイル名など）まで広げると 16 件（129 draft 時間）。機械で閉じられるのは 6 件（1 日あたり約 2 件、runtime の planner の起動 6 回）で、残りは候補を材料に付けて planner の判断を速めるにとどまる。`already_done`（19 件）は、draft が枠を待つ間に重複の先が着地したものなので、枠待ちを縮める task 942（`runtime_planners = 2`）でも減る。
2. **worker が follow_up に書く基準を絞り、`not_worth`・`decision_moot`・`not_repo_work` になりやすいものを receipt の `summary` の「見送った懸念」に書かせる**（worker の prompt。[follow-up-kinds](follow-up-kinds.md) の候補 2 の `decision` と `ops` を draft にしない案と組にできる）。runtime の planner の後の 3 分類は 62 件（37%、309 draft 時間。1 日あたり約 24 件）で、description に「まれ」「起きたら」「検討する」「決める」「人が〜する」と書かれたものが多い（769・916・885・519・855 など）。半分を draft にしないとして約 31 件（runtime の planner の起動 31 回、稼働は約 15 分、draft 時間は約 150 時間）が減る。見送った懸念が後で起きたとき（885 の hang など）に辿れるよう、`search` が receipt の summary を読めることが前提になる。
3. **plan review で、submit された task が前提にする決定を、同じ時期に submit か ready になっている決定を変える task（ADR を置き換える task・方針を変える goal）と突き合わせ、崩れうるものは依存を付けるか draft に留める**（plan review の見方）。runtime の planner の後の `superseded` は 12 件のうち 9 件が ready から cancel され、submit 19 回・plan review 9 回を使い、ready で 174〜1,537 分待った。367〜373 は、同じ goal 23 の決定を変える ADR-t598-1 の task（598）が進む間に 15 回 submit された。見込みは、この期間で submit 最大 19 回・plan review 最大 9 回（1 日あたり 7 回・3 回）と、ready の列の 7〜9 件分の待ち。決定の変更そのものは防げないので、減るのは変更の前に積んだ計画の手間だけ。

- 作り直し（`re_registered`）は、draft の欄を後から直せるようになった（`edit`・`set-paths`、2026-09-25）後と、ADR の ID を task の ID から決めるようにした（ADR-t598-1）後は 0 件で、すでに無くなっている。候補には入れない。
- 放置で古くなった draft（`stale`）は 2 件だけで、draft の寿命に上限を付ける案は見込みが小さい。runtime の planner の後は、follow_up の draft は全部 planner が決めるので、draft がそのまま放置されることは記録上無い。

## 限界

- 理由の 41%（117 件）は記録に無く、title・description・時刻・後継の task から推定した。特に runtime の planner の `not_worth`（43 件のうち 37 件）と `absorbed`（21 件のうち 15 件）は推定で、`not_worth` と `decision_moot`、`absorbed` と `duplicate` の境は判断が入る。
- 分類は 1 人の読み手（この task の worker）が付けたもので、別の読み手との一致は測っていない。
- actor は 2026-09-27 09:13 UTC より前は記録が無く、`draft_planner_opened` の時刻と note から推定した。人と planner の対話と、人が planner に頼まずに打った cancel は分けられない。
- 登録から cancel までの時間は、draft のまま・ready のままの時間を分けていない。runtime の planner の枠待ちと、人の答え待ち（`planner_question`）もこの時間に含む。
- 使った計画の手間は、submit・plan review・runtime の planner の起動の回数だけを数え、それぞれの稼働時間は測っていない。
- 集計に使った中間のファイル（events・list・notes の JSON と分類の表）は commit していない。上の表の task ID から読み直せる。

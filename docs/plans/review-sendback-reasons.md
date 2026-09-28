---
id: plan-review-sendback-reasons
type: plan
title: review と plan review の revise と concern で差し戻された理由の分類と、ラベルの定義案
status: completed
created: 2026-09-28
updated: 2026-09-28
owners:
  - hisamekms
tags:
  - measurement
  - review
  - planning
related:
  - adr-0027
  - adr-0079
  - adr-t768-1
  - plan-spike-predictor-replay
---

# review と plan review の revise と concern で差し戻された理由の分類と、ラベルの定義案

task 945（goal 64）。headless の review（run ごと）と plan review（proposal ごと）が `revise` / `concern` を返した理由を、記録の自由文（`reasons`）を 1 件ずつ読んで分類し、件数・延びた時間・率を出した。goal 64 の後続（差し戻しの理由に分類コードを付ける ADR と runtime の実装）の材料として、ラベルの定義案を [ラベルの定義案](#ラベルの定義案) に置く。この文書は決定をせず、task も登録しない。

task 601（task に由来する手戻りの率の基準値）は率を出すもので、この文書は直近の理由の分類を出す。601 の節はまだ着地していないので、数え方は [spike-predictor-replay](spike-predictor-replay.md) と ADR-0079 決定 1（`stats` の `actual.task_rework`：`verification_failed` の resume・review の `concern`・`revise_requested` のどれか。衝突と kill は数えない）に合わせた。この文書が数えるのはそのうち review の `revise` と `concern` の部分だけで、`verification_failed` は数えない。

## 要点

- **review にかかった run 429 件のうち、47 件（11.0%）が revise か concern を 1 回以上受けた**（revise 29 件 6.8%、concern 21 件 4.9%、非 pass の verdict は計 52 回）。ADR-t768-1 の前後で率は変わらない（前 10.8%、後 11.4%）。
- **時間の大半は concern の人の答え待ち**。revise の手直しは 30 回で計 151 分（中央値 2.7 分）と小さい。concern 22 回の人の答え待ちは計 2,963 分（中央値 62 分）、send_back の後の resume は 7 件で計 1,009 分（夜通し止まった task 221 の 842 分を除くと 167 分）。人の答え待ちを除くと、revise の手直しと send_back の resume で計 318 分（task 221 を除く）。
- **concern の 22 回のうち 15 回（68%）は人が `land` と答えた**。15 回とも、worker が受け入れ条件や ADR からの逸脱を自分で決め、receipt か follow-up に書いていたもの。concern 22 回のうち 21 回で、逸脱は receipt に開示済みだった。一方、作業中に `worker_question` を出していたのは 1 件（task 770）だけ。
- **concern の主な理由は、受け入れ条件と ADR・design・他の条件・実際との食い違い（`acceptance_conflict`）で 10 回・答え待ち 1,401 分**。revise の主な理由は局所の誤記と置き場所（`local_slip`）9 回、文書とコメントの取り残し（`docs_out_of_sync`）8 回、test の不足（`test_missing`）5 回、条件の未達（`acceptance_unmet`）4 回。手直しの時間は `test_missing`（42 分）と `acceptance_unmet`（34 分）が長い。
- **大きい run ほど差し戻される**。変更行数の上位 3 分の 1 は 17.1%、下位は 4.9%。作業時間（claim から最初の receipt まで）の上位 3 分の 1 は 20.6%、下位は 3.5%。kind では runtime 11.2%、docs 13.2%、plugin 3.8%（26 件）、ci 0%（6 件）。
- **plan review は proposal 319 件のうち 38 件（11.9%）で revise 47 回、concern は 0 回**（approve_plan の ask もこの期間には無い）。差し戻しの理由は task どうしの関係（依存の欠け 24 回、他の task との重複・食い違い 15 回）と ADR の参照（superseded の ADR を引く 10 回、ADR の番号の衝突 7 回。どれもそのラベルが付いた revise の回数）が多く、runtime の review とは分布が違う。ADR-t768-1 の後は 5.5% に下がった（前 16.2%）。ADR の番号の衝突は ADR-t598-1 の後に 0 件になった。
- **plan review には runtime の review と同じラベルの集合は使えない**。共通に使えるのは ADR・design との食い違いと受け入れ条件の食い違い・あいまいさの 3 つと `other` だけで、残りは plan review だけのラベルが要る（[plan review のラベル](#plan-review-のラベル)）。

## 方法

- 期間: 直近 7 日（2026-09-21〜2026-09-28 13:00 UTC）。ただし headless の review の記録（`review_finished`）はこの queue では 2026-09-24 04:23 UTC から、plan review の記録（`plan_review_finished`）は 2026-09-25 23:01 UTC からしか無いので、実際の期間はそれぞれその時刻から 2026-09-28 12:54 UTC まで。
- 前後の境: ADR-t768-1（着地の検証で落ちた test を流し直して FLAKY なら着地を 1 回やり直す）の着地、2026-09-27 08:12:43 UTC。run は最初の `review_finished`、proposal は最初の `plan_review_finished` の時刻で分けた。ADR-t768-1 は review の基準を変えないので、前後は時期の違いとして読む。
- 読んだもの（固定バイナリの読み取り専用のコマンドだけ）: `dagq stats --full`、`dagq events --full --kind review_finished / revise_requested / landing_decided / ask_opened / ask_answered / resume_started / plan_review_finished`、非 pass の verdict のある 47 run の `dagq events --all --full --run`、`dagq list --all --full`（task の kind・paths・verify）、main の squash commit の `git log --numstat`（`Dagq-Run` の trailer で run と対応）。
- 分母: `review_finished` のある run（429 件、前 297・後 132）。plan review は `plan_review_finished` のある proposal（319 件、前 191・後 128）。
- 分類: 非 pass の verdict の `reasons` の各項目（1 回に 1〜5 項目）を 1 人（この task の worker）が読んで 1 つのラベルを付け、verdict の主の理由は最初の指摘の項目のラベルにした（review は止める理由を先に書く。「Minor」「for awareness」などの止めない項目は主にしない）。時間は主の理由のラベルに付け、2 重に数えない。
- 時間:
  - revise の手直し: `revise_requested` → `revise_finished`。その後の再 review（中央値 約 25 秒）は含めない。
  - concern の人の答え待ち: `approve_landing` の `ask_opened` → `ask_answered`。夜と人の不在を含む壁時計の時間。
  - send_back の後の resume: 理由が「the review's findings were sent back」の `resume_started` → その次の `resume_finished`（か次の `resume_started`）の合計。resume の後の `stuck_exit` の待ち（task 818 の約 4.7 時間）は含めない。
  - 答えから resume の開始まで（適用の遅れ）: `ask_answered` → 最初の send_back の `resume_started`。task 949（2026-09-28 着地）より前は、答えの適用が slot と着地スロットの空きを待っていた。
  - plan review: revise の `plan_review_finished` → 同じ proposal の次の `plan_review_finished`（planner の手直しと再 review）。
- kind: task の `kind` が無い 261 run は、title の接頭辞（`runtime:` / `docs:` / `plugin:` / `ci:`、`test:` は runtime）、無ければ verify（llvm-cov なら runtime）と paths（docs だけなら docs）から推した。
- 大きさ: squash commit の変更行数（追加 + 削除）と `stats` の `work`（claim → 最初の receipt。revise の手直しは含まない）の 3 分位。どちらも結果から分かる値で、登録の時点では分からない。

## review（run ごと）

### 件数と率

| 区分 | run | revise | concern | どちらか | 非 pass の verdict |
|---|---|---|---|---|---|
| 全体 | 429 | 29（6.8%） | 21（4.9%） | 47（11.0%） | 52 |
| 前（〜09-27 08:12 UTC） | 297 | 20（6.7%） | 15（5.1%） | 32（10.8%） | 37 |
| 後（09-27 08:12 UTC〜） | 132 | 9（6.8%） | 6（4.5%） | 15（11.4%） | 15 |

1 つの run が revise と concern の両方を受けたものがある（task 324・442・445）ので、revise と concern の和は「どちらか」より多い。

### 延びた時間

| 項目 | 回数 | 合計（分） | 中央値（分） | 前 / 後（分） |
|---|---|---|---|---|
| revise の手直し | 30 | 151 | 2.7 | 71 / 80 |
| concern の人の答え待ち | 22 | 2,963 | 62 | 2,488 / 475 |
| 　うち `land` と答えたもの | 15 | 1,987 | — | — |
| 　うち `send_back` と答えたもの | 7 | 976 | — | — |
| send_back の後の resume | 7 件（10 回） | 1,009（task 221 を除くと 167） | — | 898 / 111 |
| send_back の答えから resume の開始まで | 6 | 94 | 15 | — |

- revise の手直しが長いのは task 672（26 分、stress の失敗の原因を探す）、555（25 分、足りない test の追加）、324（17 分）、421（13 分）、429（11 分）、805（9 分）。task の説明にある「9〜26 分」はこれらに当たる。
- concern の答え待ちは、答えを代行する inbox が記録に現れる 09-26 以降（11 回）は計 667 分・中央値 10 分、それより前（11 回、`answered_by` の記録なし）は計 2,296 分で、夜をまたいだものが多い。
- send_back の後の resume は、task 221（resume の 2 回目の session が夜通し止まった。2026-09-24）の 842 分を除くと、task 818 の 101 分（2 回）、442 の 26 分、441 の 15 分、433 の 10 分、833 の 10 分、425 の 6 分。19 時の時点で slot を使っていた 818 と 833 はこの send_back の resume だった。

### 理由の分類

主の理由ごとの件数と時間。「付いた件数」は、主でない項目も含めてそのラベルが付いた verdict の数。

| 分類 | 主 revise | 主 concern | 付いた件数 | revise の手直し（分） | concern の答え待ち（分） | send_back 後の resume（分） | 代表の task |
|---|---|---|---|---|---|---|---|
| 受け入れ条件と ADR・design・他の条件・実際の食い違い（`acceptance_conflict`） | 0 | 10 | 10 | 0 | 1,401 | 151 | 171, 338, 441, 442, 818, 833, 757 |
| 受け入れ条件の未達（`acceptance_unmet`） | 4 | 4 | 10 | 34 | 393 | 0 | 324, 360, 362, 421, 770 |
| 局所の誤記・置き場所・書式・順序（`local_slip`） | 9 | 0 | 10 | 14 | 0 | 0 | 461, 553, 610, 617, 819 |
| test の不足（`test_missing`） | 5 | 0 | 10 | 42 | 0 | 0 | 445, 514, 555, 733, 805 |
| accepted の ADR・design との食い違い（`adr_design_mismatch`） | 1 | 4 | 10 | 11 | 756 | 10 | 290, 429, 433, 445 |
| 文書・コメントの取り残し（`docs_out_of_sync`） | 8 | 0 | 9 | 19 | 0 | 0 | 100, 194, 280, 534, 949 |
| 実装の誤り・回帰（`code_defect`） | 2 | 1 | 6 | 6 | 1 | 6 | 327, 425, 437 |
| 受け入れ条件のあいまいさ（`acceptance_ambiguous`） | 0 | 2 | 3 | 0 | 410 | 842 | 133, 221 |
| scope の外の変更（`out_of_scope_change`） | 0 | 1 | 3 | 0 | 2 | 0 | 567 |
| repository の規則に反する手順（`repo_rule_violation`） | 1 | 0 | 2 | 26 | 0 | 0 | 672 |
| review の誤り（`review_error`） | 0 | 0 | 0 | 0 | 0 | 0 | — |
| 合計 | 30 | 22 | — | 151 | 2,963 | 1,009 | — |

- ADR-t768-1 の後の主の理由: `acceptance_conflict` 4、`acceptance_unmet` 3、`test_missing` 3、`docs_out_of_sync` 3、`local_slip` 1、`repo_rule_violation` 1。前の期間に多かった `adr_design_mismatch`（5）と `local_slip`（8）が減り、条件の食い違いと test の不足の比が上がった。
- concern の 22 回は、worker が逸脱を自分で選び receipt か follow-up に書いていたものが 21 回（task 433 の一部の項目だけ開示なし）。その 21 回のうち、作業中に `worker_question` を出していたのは task 770 の 1 件。worker が「人が決めること」を作業の後に receipt に書き、review がそれを concern にして人に回す流れになっている。
- concern に `land` と答えた 15 回の内訳は `acceptance_conflict` 6、`acceptance_unmet` 4、`adr_design_mismatch` 3、`acceptance_ambiguous` 1、`out_of_scope_change` 1。review の指摘が事実として誤っていたと記録から分かるものは無かった（答えの理由の文は記録に無い）。
- task 461 の 2 回目の revise は、1 回目の revise の 2 項目めの誤り（doc comment の置き場所）が直っていなかったもの。revise の手直しでの取りこぼしはこの 1 件。
- review の 1 回の指摘は 1 項目が 24 回、2 項目が 14 回、3 項目以上が 14 回。複数の項目は別々のラベルになることが多い（52 回中 18 回で 2 つ以上のラベル）。

### kind ごとの率

| kind | run | revise | concern | どちらか | 非 pass の verdict |
|---|---|---|---|---|---|
| runtime | 329 | 22（6.7%） | 18（5.5%） | 37（11.2%） | 42 |
| docs | 68 | 6（8.8%） | 3（4.4%） | 9（13.2%） | 9 |
| plugin | 26 | 1（3.8%） | 0 | 1（3.8%） | 1 |
| ci | 6 | 0 | 0 | 0 | 0 |

- kind の 261 件は推定（[方法](#方法)）。
- docs の主の理由は `local_slip` 5（ADR の frontmatter の書式・決定の順序・索引の表の順・件数の誤り）、`acceptance_conflict` 2、`acceptance_unmet` 1、`docs_out_of_sync` 1。runtime は `acceptance_conflict` 8、`acceptance_unmet` 7、`docs_out_of_sync` 6、`adr_design_mismatch` 5、`test_missing` 5、`local_slip` 4、`code_defect` 3 と散らばる。

### 大きさごとの率

| 区分 | run | revise | concern | どちらか | 非 pass の verdict |
|---|---|---|---|---|---|
| 変更行数 170 行以下 | 143 | 3（2.1%） | 4（2.8%） | 7（4.9%） | 7 |
| 変更行数 171〜650 行 | 140 | 10（7.1%） | 5（3.6%） | 14（10.0%） | 15 |
| 変更行数 651 行以上 | 140 | 16（11.4%） | 10（7.1%） | 24（17.1%） | 28 |
| 作業時間 613 秒以下 | 142 | 3（2.1%） | 2（1.4%） | 5（3.5%） | 5 |
| 作業時間 614〜1,429 秒 | 142 | 7（4.9%） | 5（3.5%） | 12（8.5%） | 12 |
| 作業時間 1,430 秒以上 | 141 | 19（13.5%） | 13（9.2%） | 29（20.6%） | 34 |

squash commit か `work` の無い run（6 件・4 件）は除いた。大きい run ほど受け入れ条件の項目も多く、条件の食い違いや未達、test の不足が出やすい。

## plan review（proposal ごと）

### 件数と率

| 区分 | proposal | revise を受けた | revise の回数 | concern |
|---|---|---|---|---|
| 全体 | 319 | 38（11.9%） | 47 | 0 |
| 前（〜09-27 08:12 UTC） | 191 | 31（16.2%） | 40 | 0 |
| 後（09-27 08:12 UTC〜） | 128 | 7（5.5%） | 7 | 0 |

- proposal ごとの revise の回数（revise_count）: 0 回 281 件、1 回 29 件、2 回 9 件。3 回以上は無い。
- この期間の plan review はどれも `pass` か `revise` で、`approve_plan` の ask は開いていない（`overridden` も無い）。
- revise から次の plan review までは中央値 1.7 分（planner がその場で直して出し直す）。合計は 1,025 分で、長い 5 回（proposal 242 の 450 分、283 の 162 分、188 の 148 分と 41 分、184 の 108 分）で 9 割になる。plan review の revise は run の slot を使わず、延びるのは task が ready になるまでの時間。

### 理由の分類

| 分類 | 主 | 付いた件数 | 主の差し戻しから次の plan review まで（分） | 代表の proposal / task |
|---|---|---|---|---|
| 依存の欠け（`missing_dependency`） | 0 | 24 | 0 | 4/146, 5/428, 9/366, 10/367 |
| 他の task との重複・食い違い（`task_overlap`） | 6 | 15 | 567 | 5/428, 9/366, 110/566, 242/827 |
| superseded の ADR を引く・amends の先の誤り（`stale_adr_reference`） | 8 | 10 | 26 | 9/366, 161/649, 207/807, 317/963 |
| ADR の番号の衝突（`adr_number_collision`） | 7 | 7 | 37 | 4/146, 7/364, 40/450, 66/486 |
| `dagq lint` の違反（`lint_violation`） | 6 | 6 | 10 | 10/367, 26/402, 56/460, 180/627 |
| verify が AGENTS.md の組み合わせと違う（`verification_rule`） | 5 | 5 | 168 | 76/520, 89/243, 94/550, 273/906, 283/918 |
| 受け入れ条件を満たせない（`acceptance_infeasible`） | 2 | 5 | 5 | 56/460, 94/550, 107/563, 112/563 |
| 変える場所・条件の取りこぼし（`incomplete_spec`） | 1 | 5 | 1 | 68/311, 94/550, 168/669, 188/728 |
| description と acceptance・context の食い違い（`inconsistent_text`） | 2 | 4 | 42 | 55/451, 76/520, 188/728 |
| goal の constraints との食い違い（`goal_constraint_conflict`） | 2 | 4 | 149 | 104/558, 188/728, 207/807 |
| accepted の ADR を amends なしに変える（`adr_conflict`） | 3 | 4 | 13 | 143/610, 145/614, 227/812 |
| 現状の事実の見立て違い（`wrong_premise`） | 1 | 3 | 2 | 52/448, 73/197, 188/728 |
| paths が必要なファイルを含まない（`paths_insufficient`） | 2 | 3 | 2 | 56/460, 68/311, 180/627 |
| 着地すると queue を止める（`operational_hazard`） | 2 | 3 | 3 | 96/489, 186/698, 317/963 |

- 1 回の revise に 2 つ以上のラベルが付くのが 47 回中 36 回。`missing_dependency` は他の理由に添えて出ることが多く、主になったものは無い。
- ADR-t768-1 の後の 7 回の主の理由: `stale_adr_reference` 3、`verification_rule` 2、`adr_conflict` 1、`task_overlap` 1。`verification_rule` の 2 回はどちらも runtime の関門に `--workspace` が無い（ADR-t828-1 の後の登録）。`stale_adr_reference` の 3 回はどれも superseded の ADR-0044・ADR-0019 を amends の先にしていた。
- `adr_number_collision` の 7 回は 2026-09-26 02:56 UTC が最後で、ADR の ID を task の ID から決める ADR-t598-1 の後は出ていない。
- `lint_violation` の 6 回（依存先が completed か draft、verify が無い）は `dagq lint` が機械的に見つけるもので、planner が submit の前に lint を通していれば出ない。

## ラベルの定義案

goal 64 の後続（ADR と runtime の実装）の材料。ラベルの名前は snake_case の英語。件数はこの期間の「主の理由 / 付いた verdict の数」。

### 付け方の規則（案）

1. review の job は `reasons` の各項目に 1 つのラベルを付ける（1 項目 1 ラベル）。1 つの項目が 2 つに当たるときは、直すのに誰の判断が要るかの重い方を選ぶ（`acceptance_conflict` > `adr_design_mismatch` > `acceptance_ambiguous` > `acceptance_unmet` > `out_of_scope_change` > `repo_rule_violation` > `code_defect` > `test_missing` > `docs_out_of_sync` > `local_slip`）。
2. verdict にはラベルの集合と主のラベルを持たせる。主は verdict を決めた最初の項目のラベル（止めない項目、「Minor」「awareness」の項目は主にしない）。集計の時間は主のラベルにだけ付け、件数は集合で数える。
3. どれにも当たらない項目は `other` にし、短い自由文の説明を残す。この期間で主が `other` になるものは無かった（止めない項目の flaky の注意・性能の注意だけが当たる）。
4. 人の答えから分かるもの（下の [人の答えからしか分からないもの](#人の答えからしか分からないもの)）は review の記録を書き換えず、ask の答えの側に別の欄で残して stats が突き合わせる。過去の記録にはラベルが無いので、集計はラベルの無い verdict を `unlabeled` として数える。

### review の job が verdict を書く時点で判定できるもの

| ラベル | 定義 | 判定の例 | 件数（主 / 付いた） |
|---|---|---|---|
| `acceptance_conflict` | 受け入れ条件か description が、accepted の ADR・design・goal の constraints・同じ task の他の条件・実際（権限・環境・既存の挙動）と食い違い、両方は満たせない。worker が一方を選んで逸脱している | task 818: 条件は「両方の provider が使えなければ待つ」だが、起動できない場合に待っても直らないとして失敗させた。task 460: 条件の goal への note を worker の権限で書けない | 10 / 10 |
| `acceptance_ambiguous` | 条件の文言が 2 通り以上に読め、実装がその一方を選んだ。どちらが意図か人が決める | task 221: 「string literal と比べない」に SQL の文字列が入るか。task 133: 「runtime.rs は薄い入口だけ」がどの範囲を指すか | 2 / 3 |
| `acceptance_unmet` | 条件や description が明示した範囲の一部を満たしていない。気づかずの漏れと、follow-up に回した部分の実装の両方 | task 324: 上限の無い待ちが cli.rs と plugin.rs に残る。task 360: description の対象の一部だけを実装 | 8 / 10 |
| `adr_design_mismatch` | task の条件自体は ADR と整合するのに、実装が accepted の ADR・design の決定と食い違う（新しい ADR も amends も無い） | task 445: ADR-0062 の待ちの終わりの原因と違う event を書く。task 433: ADR-0051 決定 26 と違う場所で優先度を下げる | 5 / 10 |
| `out_of_scope_change` | 求められていない挙動を変えた（本番の挙動、他の task の範囲） | task 567: test の seam のはずが本番の poll の間隔と transaction の種類を変えた | 1 / 3 |
| `repo_rule_violation` | AGENTS.md の手順の規則に反する（stress の失敗を流し直しで済ませる、固定バイナリを使う など） | task 672: stress で 1 回落ちたのを流し直して通ったことで済ませた | 1 / 2 |
| `code_defect` | 実装の誤りか回帰。条件の文言は満たしても挙動が壊れる | task 327: supervisor の再起動で hold が消える。task 425: adopt の経路で ask が重複する | 3 / 6 |
| `test_missing` | 条件が求める test、または変えた経路の test が足りない | task 555: /exit の再試行の 4 つの経路の test が無い。task 805: finding の planner の test が無い | 5 / 10 |
| `docs_out_of_sync` | 変えた挙動を説明する design・README・skill・コード中のコメントが古いまま、または同じ記述の写しの一方だけを直した | task 949: 3 つの design 文書が古い挙動のまま。task 100: marketplace.json の説明の写しが古い | 8 / 9 |
| `local_slip` | 意味を変えない局所の誤り: 置き場所（doc comment の位置）、順序（表・決定の番号）、書式（frontmatter）、数字の誤記 | task 819: 関数の間に挿入して doc comment がずれた。task 610: `amends` を「決定 12」と書いた | 9 / 10 |
| `other` | どれにも当たらない | — | 0 / 0 |

`acceptance_conflict` と `acceptance_unmet` の境は「worker が、条件を満たすと別の決まりや事実に反すると書いているか」。書いていれば conflict、単に届いていなければ unmet。review の job は receipt と follow-up の文から判定できる。

### 人の答えからしか分からないもの

review の job は verdict の時点では判定できず、concern の ask への答え（と後の経過）で分かるもの。ラベルの集合とは別の欄（答えの側）に置く案。

| 欄の値 | 定義 | この期間 |
|---|---|---|
| `deviation_accepted` | concern に `land` と答えた。人が逸脱をそのまま受け入れた。review の escalate が不要だった候補（人でなく planner か review 自身が決められた候補）で、`review_error` の候補も含む | 15 回（答え待ち 1,987 分） |
| `deviation_rejected` | concern に `send_back` と答えた。review の指摘が直す価値のあるものだったと確かめられた | 7 回 |
| `review_error` | 人の答えの理由が、review の指摘そのものが事実として誤りだと言う（例: 「条件は満たしている」「その ADR は別のことを言っている」）。answer に短い理由のコードを選ばせないと数えられない | 0 回（答えの理由の文が記録に無く、判定できない） |
| `acceptance_changed` | 人が答えとともに受け入れ条件を変えた（`land` で follow-up にした、send_back で条件を直した）。`acceptance_conflict` / `acceptance_ambiguous` の後に起きれば、task の登録に原因があった印 | 記録から数えられない |

revise には人の答えが無いので、revise の指摘が誤りかどうかは、revise の session が指摘に反論した（直さずに説明だけした）かで見る以外に無い。この期間の revise 30 回のうち 28 回は次の review で pass になり（task 461 は再び revise、445 は concern）、反論した例は見つからなかった。

### plan review のラベル

plan review の revise 47 回の分布は、runtime の review と重なる所が小さい。

| plan review のラベル | runtime の review の対応 | 件数（主 / 付いた） |
|---|---|---|
| `adr_conflict`（accepted の ADR を amends なしに変える task） | `adr_design_mismatch` と同じ意味で使える | 3 / 4 |
| `stale_adr_reference`（superseded の ADR を引く・amends の先の誤り） | `adr_design_mismatch` に寄せられるが、直し方（参照の付け替え）が違うので分けたい | 8 / 10 |
| `inconsistent_text`（description と acceptance・context の食い違い）、`goal_constraint_conflict`、`acceptance_infeasible` | `acceptance_conflict` に寄せられる（着地の前に見つけた同じ種類の問題） | 2 / 4、2 / 4、2 / 5 |
| `incomplete_spec`（変える場所・条件の取りこぼし） | `acceptance_ambiguous` に近いが、plan review では「足りない」ことを指す | 1 / 5 |
| `missing_dependency`、`task_overlap` | 対応なし（task どうしの関係） | 0 / 24、6 / 15 |
| `lint_violation`、`verification_rule`、`paths_insufficient` | 対応なし（登録の形式。機械的に検査できる） | 6 / 6、5 / 5、2 / 3 |
| `operational_hazard`（着地すると queue を止める） | 対応なし | 2 / 3 |
| `wrong_premise`（現状のコード・文書の事実の見立て違い） | 対応なし | 1 / 3 |
| `adr_number_collision` | 対応なし。ADR-t598-1 の後は起きないので、ラベルにせず `other` でよい | 7 / 7 |

見立て: plan review には別の集合が要る。ただし「ADR・design との食い違い」と「受け入れ条件の食い違い」は両方で同じ名前（`adr_design_mismatch`、`acceptance_conflict`）を使うと、plan review で止められなかった食い違いが後で review の concern になった割合（例: task 818 の ADR-0047 決定 42 との食い違いは proposal 227 の plan review が指摘したが、着地の review でも concern になった）を 1 つの集計で追える。plan review だけのラベルは `missing_dependency`・`task_overlap`・`stale_adr_reference`・`lint_violation`・`verification_rule`・`paths_insufficient`・`operational_hazard`・`wrong_premise`・`other` の案。`lint_violation` と `verification_rule` と `paths_insufficient` は機械的な検査に移せば plan review のラベルとしては要らなくなる。

## 減らせる手の候補

決定はしない。数字はこの期間（review 約 4.4 日、plan review 約 2.6 日）の値。

1. **worker が receipt の前に、変えた挙動の説明の写しと、条件ごとの test の対応を自分で確かめる**（worker への指示）。効く分類: `docs_out_of_sync`（主 8）・`local_slip`（主 9）・`test_missing`（主 5）で、revise 30 回のうち 22 回、手直し 74 分。receipt の `tests` の evidence に「受け入れ条件の項目ごとにどの test か」を書かせ、変えた語（関数名・event の kind・旧い挙動の文）を repository 全体で grep してから receipt を書かせる。半分を防げれば revise 約 11 回（revise を受ける run の率 6.8% → 約 4.2%）と手直し約 37 分、再 review 11 回。1 回ずつは短い（中央値 2.7 分）ので、時間より review の回数と token を減らす手。この 3 つの分類の中で長い手直しは `test_missing` の 555（25 分）・805（9 分）。全体で最も長い 672（26 分）は `repo_rule_violation`、次の 324・421 は `acceptance_unmet` で、この手の対象の外。
2. **worker が逸脱を決めた時点で `worker_question`（`--because scope`）を出す**（worker への指示、AGENTS.md の今の規則の徹底）。効く分類: concern の `acceptance_conflict`・`acceptance_ambiguous`・`adr_design_mismatch`・逸脱を選んだ `acceptance_unmet`。concern 22 回のうち 21 回は逸脱を receipt に開示済みで、作業中に問いを出したのは 1 件だけ。作業中に聞けば、`send_back` の 7 件の resume（task 221 を除き 167 分、それと答えから resume までの適用の遅れ 94 分）と再 review が無くなり、答えを待つ間は run が slot を空ける（ADR-0071 の待ち）。人の答え待ちの時間自体は移るだけで減らない。
3. **plan review で、受け入れ条件の各項目が名指す ADR・design・goal の constraints・他の task と両立するか、worker の権限と実際で満たせるかを確かめ、`dagq lint` と verify の組み合わせは submit の時点で機械的に止める**（task の登録と plan review の基準）。効く分類: review の `acceptance_conflict`（主 10、答え待ち 1,401 分、resume 151 分）と `acceptance_ambiguous`（主 2、答え待ち 410 分）、plan review の `lint_violation`（6）・`verification_rule`（5）・`paths_insufficient`（2）。review の concern のうち、条件どうし・条件と ADR の食い違いで登録の時点で読めたもの（171・338・442・818・833・757 の 6 回、答え待ち約 1,310 分）の半分を防げれば、concern 約 3 回・答え待ち約 650 分。submit の機械的な検査は plan review の revise 47 回のうち主 13 回（28%）を無くす。
4. **concern の基準を変え、受け入れ条件の文言からの逸脱のうち、receipt に開示され follow-up があり、accepted の ADR と人の決定（goal の constraints）を変えないものは、人でなく runtime の planner か review 自身が決める**（review の基準、goal 64 の後続の ADR で扱う）。効く分類: 人が `land` と答えた 15 回（答え待ち 1,987 分）。そのうち ADR や人の決定に触れない条件の文言の逸脱（task 133・171・324・360・547・567・757・770 の 8 回、答え待ち約 870 分。360 は ADR-0047 決定 41 に触れる項目を含むので、除けば約 650 分）が対象の候補。人に届く ask が減り、夜の答え待ち（[night-human-wait-measurement](night-human-wait-measurement.md)）も減る。代わりに、人が send_back にしたはずのものが着地する危険がある。この期間の send_back 7 回はどれも ADR か条件の中心に触れるもので、この基準では人に残る。

## 限界

- 分類は 1 人の読み手（この task の worker）が付けたもので、別の読み手との一致は測っていない。`acceptance_conflict` と `acceptance_unmet`、`docs_out_of_sync` と `local_slip` の境は判断が入る。
- 期間が短く（review 約 4.4 日、plan review 約 2.6 日）、分類ごとの件数は 10 件以下。ADR-t768-1 の前後の差は時期の違い（登録の書き方、plan review の導入直後の ADR の番号の衝突）を含む。
- concern の答え待ちは壁時計の時間で、夜と人の不在を含む。人の答えの理由の文は記録に無いので、`review_error` は数えられない。
- kind の 61% は推定。大きさ（変更行数・作業時間）は結果から分かる値で、登録の時点の予測に使えるかは測っていない（[spike-predictor-replay](spike-predictor-replay.md) の重さの予測を参照）。
- 集計に使った中間のファイル（events と stats の JSON、分類の表）は commit していない。分類は上の表と代表の task ID から読み直せる。

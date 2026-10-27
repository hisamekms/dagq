---
id: plan-review-rounds-resume-spike
type: plan
title: スパイク：review の 3 回目以降まで進む run で後の回の指摘が前の回に出なかった理由（H1〜H4）と、人の send_back の後の resume の結末
status: completed
created: 2026-10-28
owners:
  - hisamekms
tags:
  - planning
  - measurement
  - review
related:
  - plan-docs-reduction
  - adr-t2599-1
---

# スパイク：review の 3 回目以降まで進む run で後の回の指摘が前の回に出なかった理由

finding 137（resumes_per_run の週の破り）から goal 227 が立てた Spike。全体の review の prompt に「差し戻す指摘を 1 回で出し切る」指示が入った（e4d3baad7）後にも review の回が重なる原因を、仮説 H1〜H4 に分けて判定し、人の send_back の回と、その後の resume の結末を数える。基準は [docs-reduction.md](docs-reduction.md) の「C の後の親だけの 2 回目以降の差し戻しの出どころ」（以下「3314 の節」）の表 1・表 2 で、数え直さない。

## 結論

- H1（取りこぼし）: **holds**。判じられた理由 J = 27 のうち取りこぼし 22（81.5%）。
- H2（直しが入れた）: **does_not_hold**。直しが入れた 5（J の 18.5%）。
- H3（大きい task に偏る）: **holds**。差分の行数の比 5.04、ファイル数の比 6.0（受け入れ条件の項目数の比は 0.29、番号の形を広げて数えると 1.25）。
- H4（範囲の外を求める）: **does_not_hold**。範囲外 0 / R = 41。
- 主: **H1**。割合を持つ holds の仮説は H1 だけで、H3 は割合で比べる仮説ではないため、H1 を主とし、H3 も成り立つことを推奨に並べる（下の「判定」）。
- 3316 の後も取りこぼしは残った: J の中の取りこぼしは 81.5%（22 / 27）で、3314 の 73%（22 / 30）から下がっていない。期間と選び方が違うので、これは指示の効果の判定ではない。
- 人の send_back の後の数える resume 112 件のうち、resume の session が resolved で終わったのは 85 件。そのうち次の review が `pass` だったのは 27 件（24.1%）で、48 件（42.9%）は次の review が再び `revise` を返した。resolved でない 27 件（24.1%）は、25 件が約 1 時間で `exit_requested` の後に receipt 無しで終わった。

## 環境

- 期間 W = [2026-10-19T00:00:00.000Z, 2026-10-27T12:00:00.000Z)（UTC、半開区間、event の `created_at` で切る）。
- main の commit: `5fcf28ca3cb7176a72d238e4e12db888de3ebe44`（この run の base）。
- dagq のバイナリ: `~/.local/bin/dagq`、`dagq 0.4.0-dev+2ae8486243284390e677f104b72a8fd7dbec4b69`。
- git 2.39.3（Apple Git-146）、python 3.9.6（`python3 -I`）、macOS 14.7.4（arm64）。
- 読んだのは本番 queue の event（読むだけのコマンド）と、この repository の git の object と、各 run の dir の `prompt.txt`（受け入れ条件の本文）。数えた script と中間の JSON は repository に置かない（run の `$TMPDIR` に置き、run の後に消える）。

## 打ったコマンド

```sh
# event（1000 件ずつ、--after に前の頁の cursor を渡して全部を読む）
dagq events --full --limit 1000 --kind supervisor_started
dagq events --full --limit 1000 --kind review_started  --since 2026-10-01 --until 2026-10-29
dagq events --full --limit 1000 --kind review_finished --since 2026-10-10 --until 2026-10-29
dagq events --full --limit 1000 --kind landing_decided --since 2026-10-19T00:00:00.000Z --until 2026-10-29
dagq events --full --limit 1000 --kind ask_opened --since 2026-10-15 --until 2026-10-27T12:00:00.000Z
dagq events --full --limit 1000 --kind resume_started --kind resume_finished --since 2026-10-18T15:00:00.000Z --until 2026-10-29
dagq events --full --limit 1000 --kind run_integrated --since 2026-10-19T00:00:00.000Z --until 2026-10-29
dagq timeline <RUN>                      # 表 B の resolved でない 27 件の区間
dagq kpi --period week --at 2026-10-26T00:00:00Z --last 2
# 版と量と各回の差分
git merge-base --is-ancestor e4d3baad7 <supervisor_started の版の commit>
git diff --shortstat --no-renames <subagents.base>...<subagents.head>
git diff --no-renames -U0 <base_{N-1}>...<head_{N-1}>    # P
git diff --no-renames -U0 <head_{N-1}> <head_N>          # F
GIT_INDEX_FILE=<一時> git read-tree <base_N>; git apply --cached --3way; git write-tree   # base が変わった回の X
```

W の外まで読んだのは、前の回の `review_started`（W の前）と、表 B の「次」の event（W の後）のため。数える範囲は下の各表の定義のとおり W で切った。

## 期間と定義の版

- 全体の review の prompt の版: 各 `review_started` の版は、それより前で最後の `supervisor_started` の `payload.dagq_version` の「+」の後の commit で決めた。`git merge-base --is-ancestor e4d3baad7 <commit>` が真なら「後」、偽なら「前」。
- e4d3baad7 を含む最初の `supervisor_started` は event 327829、2026-10-24T16:24:53.785Z、`0.4.0-dev+d079e0eab15f3b6cadce221c9bd489b9cde7a154` で、task の記述どおりだった。
- W の `review_started` 937 件の内訳は、前 691・後 246・版不明 0。`supervisor_started` の版の commit 859 種のうち 1 種はこの repository に無かったが、W の `review_started` の版には当たらなかった。

## 数え方（3314 の節との違いだけ）

各回の範囲（base_N・head_N・P・X・F・P'）、理由の箇所の起こし方、行を F の hunk のずれで X に写すこと、file だけの箇所の扱い、1 つの理由の箇所が分かれたときの扱いは 3314 の節の「数え方」のとおりにした。違いは次のとおり。

- 対象の理由: 詳細調査の run で、`attempt` が 2 以上の `review_finished` のうち verdict が `revise` か `concern` のもの。その全体の review（親）の `payload.reasons` のうち、同じ番目の `reason_codes` に `acceptance_unmet` か `code_defect` を含むもの。3314 は親だけの差し戻しに絞ったが、ここでは subagent の差し戻しを伴う回も含め、subagent（`agents[]`）の理由は数えない。
- 前の回が `review_finished` を返さずにやり直された回（head_N = head_{N−1}）の理由は、取りこぼしと別の行（「前の回が返さずにやり直し」）に数え、J に入れない。
- 箇所は、本文の `path:L`・`path:L–M`・`path（L–M行）`・`path の L 行` を機械で読み、basename だけの file は `reason_paths` に当てた。そのうえで 11 理由（event 340466・415962・1044349・341327・344574・1041712・416505 の各理由）は手で箇所を直した。規則の出典として名指す文書（`docs/plans/task-replanning.md`・`roles.md`・`integrate.md` の約束・`needs-session.md`）は箇所から外し、本文の行だけの箇所（`TomlScan::basic（400–427行）`・`replan_diagnosis.rs:952`・`apply_diagnosis:738`・`「用意」5（461–463行）` など）はその関数のある file の行にした。
- 受け入れ条件の項目数: run の dir の `prompt.txt` の `Acceptance criteria:` から `Verification commands` までを読み、「(n)」の番号の数（無ければ 1）を数えた（task の記述どおりの数え方。以下「(n) の数え方」）。古い task は「1. 2.」や「(a)」で番号を付けており、(n) の数え方では 1 になるため、「n.」と「(a)」も番号として数えた値（以下「広げた数え方」）を並べた。`dagq show` は worker が打たないため使っていない。

## 全集計

### 表 A（人の send_back の回）

分母 = W の `landing_decided` のうち `payload.answer` が `send_back` で `ask_id` を持つもの = 113 件・69 run。W の `landing_decided` の `send_back` は全て `ask_id` を持っていた（ほかは `cancel` 1 件）。回は、その ask の `ask_opened` より前で同じ run の最後の `review_finished` の `attempt`。「うち版後」は、その `review_finished` に対応する `review_started` の版。

| 回 | 件数 | 割合（÷ 113） | うち版「後」 |
| --- | --- | --- | --- |
| 1 | 23 | 20.4% | 8 |
| 2 | 20 | 17.7% | 6 |
| 3 | 31 | 27.4% | 8 |
| 4 | 24 | 21.2% | 2 |
| 5 以上 | 15 | 13.3% | 3 |
| 決められない | 0 | 0.0% | 0 |
| 計 | 113 | 100.0% | 27 |

finding 137 の値との差: [2026-10-19T00:00Z, 2026-10-26T00:00Z) で数え直すと `send_back` は 99 件・59 run で、finding の「99 件・59 run」に一致した。その 99 件はどれも `ask_id` を持つため、この分母の定義では 99 件で、finding の「88 件の人の send_back」とは 11 件違う。88 件の選び方（ask に答えた actor で分けたかなど）は確かめていない。W の 113 件との差 14 件は、2026-10-26T00:00Z からの 36 時間の分である。

1・2 回目の send_back が 43 件（38.1%）ある。ADR-t2599-1 の後も、`concern` の verdict の `scope`・`discard`・`low`・推奨なしは revise の残数に依らず人に回り（決定 8・9）、review の失敗・打ち切りからの ask もある（[review](../design/supervisor-lifecycle/review.md) の「全体の流れ」）ため、1・2 回目に人の send_back があること自体は方針に沿う。43 件がこれらのどの経路で ask になったかは、この Spike では確かめていない。

### 表 B（人の send_back の後の resume の結末）

分母 = W の `resume_started` のうち `payload.counted` が true で、`payload.reason` が「a person sent the run back in ask」で始まるもの = 112 件。各件に、同じ run で `payload.attempt` が同じでそれより後の最初の `resume_finished` を対応づけた（全件にあった）。

| outcome（status） | 次 | 件数 | 割合（÷ 112） |
| --- | --- | --- | --- |
| resolved（validating） | `review_finished` の `revise` | 48 | 42.9% |
| resolved（validating） | `review_finished` の `pass` | 27 | 24.1% |
| resolved（validating） | `review_finished` の `concern` | 5 | 4.5% |
| resolved（validating） | `resume_started` | 4 | 3.6% |
| resolved（validating） | `landing_decided` の `sent_back` | 1 | 0.9% |
| unresolved（needs_session） | - | 27 | 24.1% |
| 終わりなし | - | 0 | 0.0% |
| 計 | | 112 | 100.0% |

unresolved の 27 件の区間の終わり（`dagq timeline` のその区間の event と `resume_finished` の `work_breakdown`）:

- 25 件: 約 1 時間以上（3,601〜14,422 秒）で `exit_requested`・`session_closed`（exited）になり、receipt が無いまま終わった。うち 7 件は `exhausted` が true。最後の `turn_finished` は「checks を最終の tree で流し直している」「全体の IT を流している」などの検証の途中だった。
- 1 件（event 418095、task 3109）: 1,245 秒で API に届かない error（ENOTFOUND）で終わった。
- 1 件（event 340484、task 3125）: 273 秒で turn が `tool_use` の途中で止まった診断を残して終わった。

数える resume の他の reason（W、50 件）: review の差し戻し（「the review's findings were sent back by」）22、verification の失敗 19、`after task` の衝突のうち数えるもの 5、e2e の失敗 2、session が殺された 1、その他 1。

kpi との突き合わせ: `dagq kpi --period week --at 2026-10-26T00:00:00Z` は JST の週で区切るため 2026-W44 を返す。W43（[2026-10-18T15:00Z, 2026-10-25T15:00Z)）は `--last 2` の前の週で、数える resume 141・`sent_back` 121・その resolved 33（finding 137 の値）だった。同じ区間の event で数え直すと、数える resume 141（`after_task` の uncounted 83 を除く）、`sent_back` 121 = 人の send_back 101 ＋ review の差し戻し 20 で一致した。resolved だけは違う。`resume_finished` の outcome が resolved のものは 91（人 77・review 14）。kpi の resolved 33 は、その後に run が session 待ちに戻らなかったものだけを数える（`src/domain/stats/retries.rs` の `ResumeAttempt::resolved`）。W43 の差 58 件（91 − 33）は、resume が resolved で終わった後に run が再び session 待ちに戻ったもので、窓と分母の違う表 B（W、人の send_back だけ）では、次の review が再び `revise` を返した 48 件がその種類の大半に当たる。

### 表 C（H3）

- G3 = W に `attempt` が 3 以上の `review_finished` を持つ run の全部（151 run）。
- G12 = W に `run_integrated` があり、`review_finished` の `attempt` の最大が 1〜2 の run のうち、`run_integrated` の新しい順に 60 run。
- 量は、その run の最後の `review_started` の `git diff --shortstat --no-renames subagents.base...subagents.head` の追加＋削除の行数とファイル数、受け入れ条件の項目数。復元不能は 0 run、`prompt.txt` が読めない run も 0。

| 群 | run 数 | 差分の行数の中央値 | ファイル数の中央値 | 受け入れ条件の項目数の中央値（(n) の数え方 / 広げた数え方） |
| --- | --- | --- | --- | --- |
| G3 | 151 | 653 | 12 | 1 / 5 |
| G12 | 60 | 129.5 | 2 | 3.5 / 4 |
| 比（G3 ÷ G12） | | 5.04 | 6.00 | 0.29 / 1.25 |

最後の回の差分は前の回までの直しを含むため、G3 の量は回を重ねた結果でもある。最初の `review_started` の差分で比べても、行数の中央値は 464 対 129.5（3.58 倍）、ファイル数は 9 対 2（4.5 倍）で、比は 1.5 を超えたままだった。

## 詳細調査

対象の run は、G3 のうち `attempt` が 2 以上の `review_started` の版がすべて「後」の 33 run を、`attempt` の最大の降順、同じなら最初の `review_started` の古い順に並べた上の 30 run。残りの 3 run（attempt の最大がどれも 3）は数えていない。30 run のうち 16 run が対象の理由を持ち、R = 41。前の回の `review_started`・`subagents.base`/`head`・4 つの commit は全ての回で読め、復元不能の event は 0。base が変わった回は 10 event（328578・337995・339076・340466・340609・414198・421855・1041712・1041811・1044349）で、載せ替えの競合は 4 event（421855 の `docs/design/queue-service-host-cli.md`、1041811 の `src/application/supervisor_service.rs`、337995 の `plugins/claude-dagq/skills/dagq/reference/authority.md`・`locate.md`、340609 の `docs/design/queue-service-host-cli.md`）にあったが、どれも理由の箇所に当たらなかった。

### 表 1（理由単位、R = 41）

| 区分 | 件数 | 割合（÷ R） | J の中の割合（J = 27） | うち `acceptance_unmet` | うち `code_defect` | 3314 の表 1（R = 47） |
| --- | --- | --- | --- | --- | --- | --- |
| 取りこぼし | 22 | 53.7% | 81.5% | 10 | 18 | 22（46.8%、J の中 73.3%） |
| 直しが入れた | 5 | 12.2% | 18.5% | 5 | 3 | 8（17.0%、J の中 26.7%） |
| 判じられない | 13 | 31.7% | - | 8 | 8 | 17（36.2%） |
| 前の回が返さずにやり直し | 1 | 2.4% | - | 1 | 0 | （3314 は取りこぼしに含め 3 件） |
| 計 | 41 | 100.0% | | 24 | 29 | 47 |

両方の code を持つ理由（12 件）は後の 2 列の両方に入る。

判じられない 13 件の理由: 行が無く区別できない 12（339076、328752、337995、340609 の 2 件、415962 の 2 件目、1044349 の 2 件、341327 の 1 件目、344574 の 2 件目、344705 の 2 件）、run の差分の外 1（340466 の 1 件目、`goal_reviews.rs:34`）。復元不能・載せ替えの競合・箇所ごとに分かれる、はどれも 0。前の回が返さずにやり直した 1 件は 330271（task 3336）。

例の event: 取りこぼしは 1041811（`src/compose/control.rs:60`）・336603（`replan_apply.rs:438`・`session.rs:279`）・343471（`src/domain/queue_service.rs:1432`）・1041712（`replan_diagnosis.rs:952`・`995`）。直しが入れたは 421136 の 2 件目（`podman_machine.rs`）・336603 の 2 件目（`goal_reviews.rs`）・341414（`adapters.rs`）・341327 の 2 件目（`codex_inbox.rs` の 400–427 行）・344574 の 1 件目（`manual-smoke.md` の 461–463 行）。

判じられない 13 件が全て直しが入れたものでも、取りこぼし 22 対 18 で、J の中の取りこぼしは 55% になり、H1 の閾値（50%）を下回らない。取りこぼしの 1 件（416505、task 3385）は、前の回が `revise` を返した後に head が変わらないまま次の回の review が走ったもので、F が空だった。

### 表 1b（H4、R = 41）

| 区分 | 件数 | 割合（÷ R） |
| --- | --- | --- |
| 範囲内 | 41 | 100.0% |
| 範囲外 | 0 | 0.0% |
| 判じられない | 0 | 0.0% |

`acceptance_unmet` を持つ 24 件のうち 13 件は本文が受け入れ条件の番号を名指し（例: 421136 の (1)、332662 の (2)、341576 の (9)）、残りの 11 件は本文の求めが受け入れ条件の項目（task 3294 の (3) の ref を戻すこと、task 3439 の (2) など）、description の文（task 3386 の「CLI が queue DB を開かない」、task 2183 の stop・cancel の両経路）、description が読ませる ADR の決定（task 1526 の ADR-t1521-2 決定 5）のどれかに当たった。`acceptance_unmet` を持たない 17 件は全て `code_defect` を持ち、どれも run の差分が足した経路の上の欠陥（競合・権限・記録の欠け）を指した。

理由ごとの対応づけ（理由の番目は `payload.reasons` の 1 始まり、code は `acceptance_unmet`・`code_defect` のうち持つもの）。「code_defect の特則」は `acceptance_unmet` を持たず `code_defect` を持ち、run の差分の上の欠陥を指すために範囲内とした理由である。

| event | 理由の番目 | task | code | H1・H2 の区分 | H4 | 当たる受け入れ条件・description |
| --- | --- | --- | --- | --- | --- | --- |
| 421136 | 1 | 3386 | code_defect・acceptance_unmet | 取りこぼし | 範囲内 | 受け入れ条件 (1) |
| 421136 | 2 | 3386 | acceptance_unmet・code_defect | 直しが入れた | 範囲内 | 受け入れ条件 (1)（description の「CLI が queue DB を開かない」） |
| 421855 | 1 | 3386 | code_defect・acceptance_unmet | 取りこぼし | 範囲内 | 受け入れ条件 (1)（integrate の記録が DB を直接開いたときと同じ） |
| 1041811 | 1 | 3386 | code_defect | 取りこぼし | 範囲内 | code_defect の特則（run の差分の上の欠陥） |
| 1041811 | 2 | 3386 | code_defect | 取りこぼし | 範囲内 | code_defect の特則（run の差分の上の欠陥） |
| 336603 | 1 | 1526 | code_defect・acceptance_unmet | 取りこぼし | 範囲内 | 受け入れ条件（番号なし）の「古いproposal/receipt」 |
| 336603 | 2 | 1526 | acceptance_unmet | 直しが入れた | 範囲内 | context が読ませる ADR-t1521-2 決定 5 |
| 336603 | 3 | 1526 | code_defect | 取りこぼし | 範囲内 | code_defect の特則（run の差分の上の欠陥） |
| 339076 | 1 | 1526 | code_defect | 判じられない | 範囲内 | code_defect の特則（run の差分の上の欠陥） |
| 340466 | 1 | 1526 | code_defect | 判じられない | 範囲内 | code_defect の特則（run の差分の上の欠陥）（`goal_tasks` の置換先の追加は run が足した。箇所の行は差分の外） |
| 340466 | 2 | 1526 | code_defect | 取りこぼし | 範囲内 | code_defect の特則（run の差分の上の欠陥） |
| 343471 | 1 | 3125 | code_defect | 取りこぼし | 範囲内 | code_defect の特則（run の差分の上の欠陥） |
| 343802 | 1 | 3125 | code_defect・acceptance_unmet | 取りこぼし | 範囲内 | 受け入れ条件 (4)（既存の振る舞いが変わらない） |
| 414198 | 1 | 3125 | code_defect | 取りこぼし | 範囲内 | code_defect の特則（run の差分の上の欠陥） |
| 328752 | 1 | 1181 | acceptance_unmet | 判じられない | 範囲内 | 受け入れ条件 (1) |
| 328578 | 1 | 3107 | code_defect | 取りこぼし | 範囲内 | code_defect の特則（run の差分の上の欠陥） |
| 337995 | 1 | 3107 | acceptance_unmet | 判じられない | 範囲内 | 受け入れ条件 (1) |
| 330271 | 1 | 3336 | acceptance_unmet | 前の回が返さずにやり直し | 範囲内 | 受け入れ条件 (2) |
| 332662 | 1 | 3354 | acceptance_unmet | 取りこぼし | 範囲内 | 受け入れ条件 (2) |
| 339654 | 1 | 3294 | code_defect・acceptance_unmet | 取りこぼし | 範囲内 | 受け入れ条件 (3) |
| 340609 | 1 | 3294 | acceptance_unmet・code_defect | 判じられない | 範囲内 | 受け入れ条件 (3)・(5) |
| 340609 | 2 | 3294 | acceptance_unmet・code_defect | 判じられない | 範囲内 | 受け入れ条件 (1)・(4) |
| 415962 | 1 | 3109 | acceptance_unmet | 取りこぼし | 範囲内 | 受け入れ条件 (1) |
| 415962 | 2 | 3109 | code_defect | 判じられない | 範囲内 | code_defect の特則（run の差分の上の欠陥） |
| 419506 | 1 | 3439 | code_defect・acceptance_unmet | 取りこぼし | 範囲内 | 受け入れ条件 (2) |
| 1041712 | 1 | 1527 | code_defect | 取りこぼし | 範囲内 | code_defect の特則（run の差分の上の欠陥） |
| 1041712 | 2 | 1527 | code_defect | 取りこぼし | 範囲内 | code_defect の特則（run の差分の上の欠陥） |
| 1041712 | 3 | 1527 | code_defect | 取りこぼし | 範囲内 | code_defect の特則（run の差分の上の欠陥） |
| 1041712 | 4 | 1527 | code_defect | 取りこぼし | 範囲内 | code_defect の特則（run の差分の上の欠陥） |
| 1044349 | 1 | 1527 | code_defect | 判じられない | 範囲内 | code_defect の特則（run の差分の上の欠陥） |
| 1044349 | 2 | 1527 | code_defect | 判じられない | 範囲内 | code_defect の特則（run の差分の上の欠陥） |
| 341414 | 1 | 3388 | acceptance_unmet・code_defect | 直しが入れた | 範囲内 | 受け入れ条件 (2) |
| 341327 | 1 | 3331 | code_defect・acceptance_unmet | 判じられない | 範囲内 | 受け入れ条件 (6) |
| 341327 | 2 | 3331 | code_defect・acceptance_unmet | 直しが入れた | 範囲内 | 受け入れ条件 (6) |
| 341576 | 1 | 3331 | acceptance_unmet | 取りこぼし | 範囲内 | 受け入れ条件 (9) |
| 344574 | 1 | 2183 | acceptance_unmet | 直しが入れた | 範囲内 | 受け入れ条件 2 |
| 344574 | 2 | 2183 | acceptance_unmet | 判じられない | 範囲内 | 受け入れ条件 1・4（queue-broker.md と矛盾しない） |
| 344705 | 1 | 2183 | acceptance_unmet | 判じられない | 範囲内 | 受け入れ条件 1・11 |
| 344705 | 3 | 2183 | acceptance_unmet | 判じられない | 範囲内 | description の「stop・cancel 両経路の後片付け」 |
| 416505 | 1 | 3385 | acceptance_unmet | 取りこぼし | 範囲内 | 受け入れ条件（番号なし）の「test が新しい toolchain で通る」 |
| 417311 | 1 | 3414 | code_defect | 取りこぼし | 範囲内 | code_defect の特則（run の差分の上の欠陥） |

### 表 2（run 単位、30 run）

| 判じられた理由の区分の組 | run 数 | 割合（÷ 30） |
| --- | --- | --- |
| 取りこぼしだけ | 9 | 30.0% |
| 直しが入れただけ | 2 | 6.7% |
| 混在 | 3 | 10.0% |
| 判じられない理由だけ | 2 | 6.7% |
| 対象の理由なし | 14 | 46.7% |
| 計 | 30 | 100.0% |

判じられない理由だけの 2 run は task 1181（判じられない 1）と task 3336（前の回が返さずにやり直し 1）。対象の理由なしの 14 run は、2 回目以降の差し戻しが `docs_drift`・`test_gap`・`receipt_evidence` などの code だけか、subagent の理由だけだった。

### run ごとの表

版は 30 run とも「後」（2 回目以降の `review_started` が全て e4d3baad7 を含む）。行数・ファイル数は最後の `review_started` の差分、受け入れ条件は (n) の数え方 / 広げた数え方。人の send_back の回は表 A の回の列。

| task | run | attempt の最大 | 対象の理由 | 取りこぼし | 直しが入れた | 判じられない | 範囲外 | 行数 | ファイル数 | 受け入れ条件 | 人の send_back の回 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 3386 | fd9d53a4 | 6 | 5 | 4 | 1 | 0 | 0 | 3473 | 36 | 5 / 5 | 3・4・5 以上 |
| 1526 | 292abb37 | 5 | 6 | 3 | 1 | 2 | 0 | 4601 | 50 | 1 / 1 | 2・3・4 |
| 3125 | 7e2a2e37 | 5 | 3 | 3 | 0 | 0 | 0 | 7253 | 48 | 5 / 5 | 3・5 以上・5 以上 |
| 1181 | 2e252404 | 4 | 1 | 0 | 0 | 1 | 0 | 109 | 4 | 5 / 5 | 1 |
| 3107 | c42db599 | 4 | 2 | 1 | 0 | 1 | 0 | 6143 | 61 | 4 / 4 | 1・2 |
| 3336 | 52dd5155 | 4 | 1 | 0 | 0 | 1（やり直し） | 0 | 812 | 32 | 6 / 6 | - |
| 3354 | 0cc337f1 | 4 | 1 | 1 | 0 | 0 | 0 | 697 | 26 | 5 / 5 | - |
| 3294 | 5c7565f3 | 4 | 3 | 1 | 0 | 2 | 0 | 2522 | 27 | 6 / 6 | 2・3 |
| 3125 | 40e3b8b6 | 4 | 0 | 0 | 0 | 0 | 0 | 5376 | 46 | 5 / 5 | 1 |
| 3457 | f796dc7c | 4 | 0 | 0 | 0 | 0 | 0 | 132 | 9 | 3 / 3 | - |
| 3421 | 09035ad1 | 4 | 0 | 0 | 0 | 0 | 0 | 296 | 9 | 4 / 4 | - |
| 3109 | d9d4f9ac | 4 | 2 | 1 | 0 | 1 | 0 | 12543 | 110 | 4 / 4 | 3 |
| 3439 | 9c77412d | 4 | 1 | 1 | 0 | 0 | 0 | 1029 | 13 | 4 / 4 | 3 |
| 1527 | a5bd6fef | 4 | 6 | 4 | 0 | 2 | 0 | 4919 | 33 | 1 / 1 | 2・3 |
| 3346 | 79024219 | 3 | 0 | 0 | 0 | 0 | 0 | 322 | 4 | 4 / 4 | - |
| 3337 | 06a7461b | 3 | 0 | 0 | 0 | 0 | 0 | 2797 | 65 | 5 / 5 | - |
| 2324 | 1b6306bb | 3 | 0 | 0 | 0 | 0 | 0 | 310 | 4 | 6 / 6 | - |
| 3353 | 09652fdd | 3 | 0 | 0 | 0 | 0 | 0 | 653 | 12 | 5 / 5 | - |
| 3370 | 2c1d00dd | 3 | 0 | 0 | 0 | 0 | 0 | 120 | 6 | 1 / 5 | - |
| 3163 | 6153b690 | 3 | 0 | 0 | 0 | 0 | 0 | 146 | 3 | 1 / 6 | - |
| 3388 | cec9d767 | 3 | 1 | 0 | 1 | 0 | 0 | 1905 | 20 | 4 / 4 | 2 |
| 3331 | b534ae87 | 3 | 3 | 1 | 1 | 1 | 0 | 2392 | 24 | 9 / 9 | - |
| 3294 | 6e47ad60 | 3 | 0 | 0 | 0 | 0 | 0 | 2543 | 28 | 6 / 6 | - |
| 2183 | 5f0bb992 | 3 | 4 | 0 | 1 | 3 | 0 | 254 | 2 | 1 / 11 | - |
| 3391 | e61c4151 | 3 | 0 | 0 | 0 | 0 | 0 | 3691 | 46 | 1 / 11 | - |
| 3345 | 95597941 | 3 | 0 | 0 | 0 | 0 | 0 | 307 | 8 | 1 / 4 | - |
| 3460 | 398fa8b6 | 3 | 0 | 0 | 0 | 0 | 0 | 29 | 5 | 3 / 3 | - |
| 3385 | b8e6ba67 | 3 | 1 | 1 | 0 | 0 | 0 | 12 | 6 | 1 / 1 | - |
| 3414 | 45ffce6e | 3 | 1 | 1 | 0 | 0 | 0 | 707 | 16 | 1 / 1 | - |
| 1384 | 76d08396 | 3 | 0 | 0 | 0 | 0 | 0 | 509 | 14 | 5 / 5 | 3 |

取りこぼしは 12 task に散る一方、差分が 2,500 行を超える 11 run のうち 7 run（task 3386・1526・3125・3107・3294・3109・1527）に 22 件のうち 17 件が集まる（残りの 4 run の取りこぼしは 0）。

## 判定

| 仮説 | 判定 | 根拠（表の該当行） |
| --- | --- | --- |
| H1 取りこぼし | holds | 表 1 の取りこぼし 22 / J 27 = 81.5%（J ≥ 10、50% 以上） |
| H2 直しが入れた | does_not_hold | 表 1 の直しが入れた 5 / J 27 = 18.5%（25% 未満） |
| H3 大きい task に偏る | holds | 表 C の比: 行数 5.04・ファイル数 6.00 の 2 つが 1.5 以上（G3 151・G12 60 run） |
| H4 範囲の外を求める | does_not_hold | 表 1b の範囲外 0 / R 41 = 0%（R ≥ 10、10% 未満） |

- 主: holds は H1 と H3。task の記述は「holds の仮説のうちその割合（H1・H2 は ÷ J、H4 は ÷ R）が最も大きいもの」を主とし、H3 だけが holds のときに H3 を主とする。H3 は割合を持たないので、H1 を主とした。H1 の割合 81.5% と比べられる H3 の割合は無く、「差が 10 ポイント未満」の規則は当たらない。H3 も成り立つため、推奨には両方の打ち手を並べる。
- H3 の受け入れ条件の項目数は、(n) の数え方では比 0.29、広げた数え方でも 1.25 で、偏りは差分の行数とファイル数に出る。
- 3316 の後に残るか: J の中の取りこぼしは 81.5%（22 / 27）で、3314 の 73.3%（22 / 30）から下がっていない。3314 は 2026-10-16〜22 の親だけの差し戻しの全部、この Spike は指示の後の 3 回以上の run の上位 30 run で、期間・選び方・run の大きさが違う。効果の判定ではなく、指示の後にも取りこぼしが主のまま残ったことだけを言う。

## 後続の計画への推奨

goal 227 の planner が task にするかを決める。人の方針（request 117、ADR-t2599-1）と数える resume の定義（goal 213）は変えない前提で書く。

1. **H1（主）: 全体の review の入力を変える runtime の task**。1 回で出し切る指示を prompt の文に足すだけでは、大きい差分で取りこぼしが残った。打ち手の候補は、全体の review の入力に「前の回までの指摘と、前の回の reviewed commit からの差分」を渡し、前の回に見た範囲のうち指摘の無かった file を観点ごとに見直させること、観点（並行と競合・権限・記録の欠け・境界の検証）を prompt に列挙すること。取りこぼしの例の多く（control.rs:60 の再送の期限、queue_service.rs:1432 の旧版 token、replan_diagnosis.rs の askの記録の原子性）は、並行・停止・旧版の記録という同じ観点の欠陥だった。
2. **H3: task の大きさの目安と分け方を task-registration.md に足す docs の task**。3 回以上の run の差分は中央値 653 行・12 file で、2 回以下の 5 倍・6 倍、取りこぼしの約 8 割（17 / 22）は差分が 2,500 行を超える 11 run のうちの 7 run に集まった。受け入れ条件の項目数は偏りの指標にならなかったので、目安は差分の行数とファイル数で書く。
3. **H2・H4 は打ち手を足さない**: 直しが入れた指摘は J の 18.5%、範囲の外を求める指摘は 0 で、worker の revise・resume の prompt と review の範囲の規則は今のままでよい。
4. **resume の結末（表 B）**: 人の send_back の後の resume の 42.9% は、resume 自体は resolved で終わっても次の review が再び `revise` を返し、kpi の resolved が低い（W43 の 33 / 121）ことの大きな部分はこの種類だった（W43 の差 58 件の内訳は数えていない）。H1 の打ち手が効けばこの行が減るはずで、効果は H1 の task の着地後に表 B の同じ行で数え直す。resolved でない 27 件のうち 25 件は約 1 時間で receipt 無しに終わった（検証の流し直しの途中）。これは review の回とは別の原因で、扱うかは planner が決める。
5. **表 A の 1・2 回目の人の send_back（43 件）**: 既定の ask の経路（`concern` の `scope`・`discard`・`low`・推奨なし、review の失敗・打ち切り）のどれで人に回ったかの内訳は数えていない。回数を減らす打ち手の前提として内訳を数えるかは planner が決める。

## 使った時間と上限

作業は合わせて約 15 分（上限 3 時間）で、worker の session の記録（2026-10-27T18:26:22Z〜18:41:04Z、turn の `duration_ms` 879646）による。内訳は run の `$TMPDIR` の中間の file の更新時刻で区切った: 全集計（表 A・B・C と kpi の突き合わせ）約 6 分半（18:26:22〜18:32:48Z、上限 1 時間）、詳細調査（30 run・41 理由）約 3 分（〜18:35:45Z、上限 1.5 時間）、文書（subagent の点検と commit を含む）約 5 分（〜18:41:04Z、上限 0.5 時間）。どれも上限の内。review の差し戻しの後の手直しの turn はこの値に含めない。build・試作・test は流していない。数えた run は G3 151・G12 60・詳細調査 30（対象の理由を持つのは 16 run）。周回は 1 回（同じ入力を読み直しても値は変わらない測定のため）。

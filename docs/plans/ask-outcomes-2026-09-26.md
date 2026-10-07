---
id: plan-ask-outcomes-2026-09-26
type: plan
title: 2026-09-26以降の本番のaskのkindごとの件数と、answerが推奨・見立てどおりだった割合
status: completed
created: 2026-10-02
owners:
  - hisamekms
tags:
  - measurement
  - operations
related:
  - adr-t451-1
  - adr-0047
  - adr-0027
  - adr-t808-1
---

# 2026-09-26以降の本番のaskのkindごとの件数と、answerが推奨・見立てどおりだった割合

[ADR-t451-1](../adr/2026-10-02-t451-1-ai-decides-recommendable-asks-and-escalates-only-the-undecidable.md)のContextの根拠（goal 42、task 451）。

## 数え方

- 対象: この repository の本番の queue の ask のうち、ID 69（2026-09-26 00:35 JST）から ID 287（2026-10-02 06:29 JST）までの 219 件。2026-10-02 に固定バイナリの読み取り専用のコマンド `dagq asks --all`（JSON）を読み、`jq` で kind・`question`・`options`・`answer`・`created_at`・`answered_at` を数えた。queue の状態は変えていない。
- 「推奨どおり」は、ask の作り手が question か options に推奨を書いたときの、answer とその推奨の一致。
  - `planner_question`: question の「推奨: …」と answer の option。
  - `blocked`: observer は推奨の欄を持たないので、見立て（options の第 1 の選択肢）と answer の一致と、answer が「leave it / wait」（何もしない）だったかの両方を数えた。
  - `approve_landing`: review の verdict は推奨を持たないので、answer の `land` / `send_back` を ask の出どころ（concern、revise の上限超え、pass の後の merge-tree の衝突、review の agent が動かなかった `provider_disabled`）ごとに数えた。
- 答えまでの時間は `answered_at - created_at`（分）。人の答え待ちは inbox を経た時間を含む。

## kindごとの件数（ID 69〜287）

| kind | 件数 | 備考 |
| --- | --- | --- |
| `planner_question` | 62 | すべて runtime の planner の follow_up の draft の採否（options は `adopt` / `cancel` / `keep_draft`） |
| `approve_landing` | 50 | うち 24 件は `--no-claude` の運転で review の agent が動かなかった `provider_disabled`（人が review する） |
| `update_failed` | 21 | 自動更新の失敗。この ADR の対象外 |
| `blocked` | 19 | observer の finding の ask |
| `queue_hold` | 17 | ディスクの `cost`。どれも runtime が閉じた |
| `decide` | 15 | 復旧 job の escalate。対象外（ADR-0047 決定 39・40） |
| `worker_question` | 13 | 対象外 |
| `approve_update` | 12 | 対象外 |
| `stuck_exit` / `stalled` / `answer_prompt` | 5 / 2 / 1 | 復旧 job の escalate。対象外 |
| `approve_goal` | 2 | goal review の ask。対象外 |
| `approve_plan` | 0 | plan review の concern は本番の queue で一度も ask になっていない（全期間でも 0） |

## 推奨・見立てどおりだった割合

| kind | 母数 | 推奨・見立てどおり | 割合 | 外れたもの |
| --- | --- | --- | --- | --- |
| `planner_question` | 62 | 60 | 97% | ask 96（推奨 `cancel` に対し、人は方針「自動でフローが回る仕組みがほしい。AI でどうしても判断がつかない場合はエスカレーション」で答えた。この goal の起点）、ask 195（権限の policy を変える (A) の推奨に対し、人は docs だけの (B) を選んだ。ADR-t728-1 の決定に触れる `scope` の判断） |
| `blocked`（見立て = 第 1 の選択肢） | 19 | 14 | 74% | ask 77・91・99（leave it が第 1 だったが人は手を入れた）、185（第 4 の wait を選んだ）、239（自由文で recover を指示） |
| `blocked`（answer が leave it / wait） | 19 | 11 | 58% | 2026-09-26 の 13 件は 10 件が leave it。2026-09-27 以降の 6 件（`reason_category: recovery_failed`）は 5 件が人の手の recover などの操作を求めた |
| `approve_landing`（concern） | 21 | `land` 12 | 57% | `send_back` 9 件（ask 109・116・123・124・168・178・198・200・278。review が受け入れ条件や ADR からの外れを指摘したもの） |
| `approve_landing`（revise の上限超え） | 3 | `send_back` 3 | — | ask 218・282・285。3 件とも人も差し戻した |
| `approve_landing`（pass の後の merge-tree の衝突） | 2 | `land` 2 | — | ask 85・119 |
| `approve_landing`（`provider_disabled`） | 24 | `land` 20 | — | `send_back` 3、runtime が閉じた 1。AI の材料が無い ask なので、この ADR の対象外 |

`planner_question` の 62 件のうち 24 件は、question の文に goal なし・閉じた goal・深さのどれかを書いており、ADR-t808-1 の「人の `adopt` を経ずに submit できない follow_up」（深さ 3 以上と、goal が無いか閉じた goal の follow_up）に当たるために聞いたものと読める。

## 答えまでの時間（分）

| kind | 件数 | 中央値 | p90 | 合計 |
| --- | --- | --- | --- | --- |
| `planner_question` | 62 | 2 | 307 | 約 72 時間 |
| `approve_landing`（`provider_disabled` を除く） | 26 | 75 | 377 | 約 49 時間 |
| `blocked` | 19 | 159 | 380 | 約 46 時間 |

## 読み方

- `planner_question` は推奨がほぼそのまま答えになっている。外れた 2 件はどちらも方針や ADR の決定に触れるもので、ADR-0047 決定 41 の `scope` に当たる。
- observer の `blocked` のうち「待てば解ける」見立てのものは、人が答えても何も変わらない。人が手を入れたものは、runtime と復旧 job が拾わなかった止まった run の手の recover だった。
- review の concern は半分近くが差し戻されており、「concern はすべて land」ではない。差し戻されたものは受け入れ条件や ADR からの外れで、外れを受け入れて着地させるのは範囲の判断（`scope`）、受け入れ条件に合わせて直させるのは task の範囲の中の差し戻しになる。

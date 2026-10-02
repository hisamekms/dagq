---
id: adr-t451-1
type: adr
title: 推奨が出せる判断はAI（planner・job）が確信度つきで決めて進め、inboxに上げるのは人が要る理由に当たりAIの材料で決めきれないものと低い確信度のものだけにする（ADR-0047決定4・11・13・16・17・20・23・37とADR-0027決定1・2をamends）
status: accepted
created: 2026-10-02
updated: 2026-10-02
accepted_on: 2026-10-02
amends:
  - adr-0047 decision 4
  - adr-0047 decision 11
  - adr-0047 decision 13
  - adr-0047 decision 16
  - adr-0047 decision 17
  - adr-0047 decision 20
  - adr-0047 decision 23
  - adr-0047 decision 37
  - adr-0027 decision 1
  - adr-0027 decision 2
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - planner
  - observer
  - operations
related:
  - adr-0047
  - adr-0027
  - adr-t808-1
  - adr-t1233-2
  - adr-t609-1
  - adr-t598-1
  - adr-t1091-1
  - plan-ask-outcomes-2026-09-26
  - design-supervisor-lifecycle-ask
  - design-supervisor-lifecycle-review
  - design-supervisor-lifecycle-observer
  - design-supervisor-lifecycle-plan-review
  - design-supervisor-lifecycle-draft-planners
  - design-supervisor-lifecycle-finding-planners
---

# ADR-t451-1: 推奨が出せる判断はAI（planner・job）が確信度つきで決めて進め、inboxに上げるのは人が要る理由に当たりAIの材料で決めきれないものと低い確信度のものだけにする（ADR-0047決定4・11・13・16・17・20・23・37とADR-0027決定1・2をamends）

## Context

[ADR-0047](0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)は、inboxに出すものを決定41の人が要る理由（`authentication`・`cost`・`scope`・`discard`・`recovery_failed`）を持つものに絞った。しかし`planner_question`・reviewの`concern`（`approve_landing`、[ADR-0027](0027-keep-worker-session-through-review-revise-verdict-and-merge-tree-precheck.md)決定2）・plan reviewの`concern`（`approve_plan`）は、作り手が推奨を出せても種類ごとに一律に人に上がる（`approve_landing`と`approve_plan`は一律に`scope`）。observerの`blocked`は決定4で「待っても解けないもの」に限っていたが、実際には見立てが「leave it」のものも上がっていた。2026-09-26に人は「自動でフローが回る仕組みがほしい。AIでどうしても判断がつかない場合はエスカレーション」と答えた（ask 96、goal 42）。

2026-09-26 00:35〜2026-10-02 06:29（JST）の本番のask 219件（ID 69〜287）の数え方と表は[ask-outcomes-2026-09-26](../plans/ask-outcomes-2026-09-26.md)にある。要点:

| kind | 件数 | answerが推奨・見立てどおり |
| --- | --- | --- |
| `planner_question` | 62 | 60（97%）。外れた2件（ask 96・195）はどちらも方針・ADRの決定に触れる`scope`の判断。24件はquestionの文からADR-t808-1の自動で採用しない上限に当たるため聞いたものと読める |
| `blocked`（observer） | 19 | 見立て（第1の選択肢）どおり14（74%）。answerがleave it / waitは11（58%）。2026-09-26の13件は10件がleave itで、9-27以降の6件は5件が人の手のrecoverを求めた |
| `approve_landing`（reviewの`concern`） | 21 | `land` 12（57%）、`send_back` 9。差し戻しはどれも受け入れ条件やADRからの外れの指摘 |
| `approve_landing`（その他） | 29 | reviseの上限超え3（3件とも`send_back`）、passの後のmerge-treeの衝突2（`land`）、`--no-claude`でreviewのagentが動かなかった24（AIの材料が無いので対象外） |
| `approve_plan` | 0 | 全期間で0件 |

答えまでの中央値は`planner_question` 2分・`approve_landing`（`provider_disabled`を除く26件）75分・`blocked` 159分、合計は約72・49・46時間だった。`stuck_exit`・`answer_prompt`・`stalled`・`decide`は、既に復旧jobが選んで適用し、escalateか低い確信度のときだけaskにしている（ADR-0047決定39・40、[ADR-t609-1](2026-09-27-t609-1-failed-live-recovery-job-opens-the-alert-ask.md)）ので、この決定の対象にしない。

## Decision

1. **原則: 推奨が出せる判断はAIが決めて進め、理由を記録する。** planner・plan review・review・observerは、推奨と確信度（`high` / `low`）を出せる判断を自分で決め、runtimeがそれを適用し、判断と理由をevent・note・taskの`context`に残す。inboxに上げるのは次のどれかだけにする: (a) 決定41の`scope`・`discard`・`authentication`・`cost`に当たり、AIの材料（queueの記録・repositoryの文書・ADR・人の先例）で決めきれないもの、(b) 確信度が`low`のもの、(c) 決まった上限・柵が人の判断を求めるもの（ADR-t808-1のfollow_upの上限、reviseの上限、`recovery_failed`）。kindで一律にaskにする規則（ADR-0047決定17のinboxに届くもの、決定20の人に聞く基準、決定37のinboxの項のreviewとplan reviewの`concern`）をこれで改める。決定41の分類の集合と`dagq ask --because`の必須は変えない。
2. **observerの`blocked`は、人が要る見立てのときだけaskにする。** 見立てが「待てば解ける」「leave it」のものはaskにせず、findingの見立て（`detail`）と根拠に残す（ADR-0047決定4の出力のaskの項を改める）。askにするのは、見立てが人の判断（決定41の`scope`・`discard`。`authentication`・`cost`はqueueの控えのaskが持つ）か、runtimeと復旧jobが行えない人の手の操作（`recovery_failed`）を要るときだけで、askには見立てを推奨と確信度として載せる（決定4の「待っても解けないもの」を、この見立ての基準で読み替える）。決定23は、`blocked`のaskをfindingごとに作る前提を「人が要る見立てのfindingにだけ作る」と改める。openな1件の一意性は変えず、askの無いfindingを普通の状態とする。
3. **reviewの`concern`は、review jobの推奨と確信度でruntimeが進める（ADR-0027決定1・2を改める）。** 方式は2度目のjobではなく、今のreview jobのverdictに推奨（`land` / `send_back`）と確信度を持たせる。runtimeは`confidence: high`のとき推奨を適用する: `send_back`はreviseと同じく生きているsessionに返し（reviseの上限に数える）、`land`は着地へ進める。ただし次はAIで決めずに`approve_landing`のaskにする: 着地させると受け入れ条件・ADR・goalの決定からの外れを受け入れることになるもの（`scope`）、cancelや成果を捨てる判断（`discard`）、確信度`low`、reviseの上限超え、reviewのagentが動かなかったもの。AIが`land`を選んだrunも、reviewのpassと人の`land`と同じく、e2eが要るなら着地の前にruntimeがhostでe2eを流す工程（[ADR-t1233-2](2026-10-02-t1233-2-e2e-runs-on-the-host-after-review-passes.md)決定2）を通り、e2eを飛ばさない。`integrate`の検証も同じに流れる。workerのsessionを閉じるのは、passとAIの`land`では着地の直前、askにするときはaskを作る前にする（ADR-0027決定1）。
4. **plan reviewの`concern`も同じ原則にそろえる（ADR-0047決定11を改める）。** verdictに推奨（`ready` / `send_back`）と確信度を持たせ、`high`の`send_back`はreviseとして持ち主のplannerに返し（reviseの回数に数える）、`high`の`ready`はpassと同じに適用する。cancel（`discard`）、ADR・goalのconstraints・人の先例と矛盾するまま通す判断（`scope`）、`low`は`approve_plan`のaskにする。jobが自分でしてよい修正の4つは変えない。
5. **`planner_question`は、推奨が出せればplannerが決める（ADR-0047決定13・16・20を改める）。** 人が開いたplannerもruntimeが立てたplannerも、推奨が出せる判断（draftの採否、計画の細部）は自分で決めて進め、理由をnoteかproposalのtaskの`context`に残す。`planner_question`にするのは原則(a)(b)と、ADR-t808-1の自動で採用しない上限（深さ3以上と、goalが無いか閉じたgoalのfollow_up）に当たるdraftだけにする。上限は変えず、その問いにも推奨を載せる。

決める場所: verdictとaskの欄の名前、既定値、eventの名前は[docs/design](../design/)の[Review](../design/supervisor-lifecycle/review.md)・[Plan review](../design/supervisor-lifecycle/plan-review.md)・[Observer](../design/supervisor-lifecycle/observer.md)・[ask](../design/supervisor-lifecycle/ask.md)・[Draft planners](../design/supervisor-lifecycle/draft-planners.md)・[Finding planners](../design/supervisor-lifecycle/finding-planners.md)の「今後の姿」の節が持つ（いずれも未実装）。

## Alternatives

- **今のまま（kindごとに一律にask）**: 誤った自動の判断は起きないが、6日で`planner_question` 62件の97%が推奨どおりで、人の答え待ちが約72時間あった。人が足した判断の無いaskが人の注意を奪い、決めきれない問いが埋もれる。
- **askのkindごとに一律に自動にする（`concern`は常に`land`など）**: 実装は最も軽いが、reviewの`concern`は57%しか`land`でなく、外れを受け入れる`scope`の判断まで黙って着地する。kindでなく判断の中身（理由と確信度）で分ける。
- **確信度なしで自動にする**: 推奨を常に適用すると、AI自身が迷ったものも人に届かない。確信度`low`を人に上げる逃げ道を残す。
- **reviewの`concern`を2度目のjobで判断する**: 独立した目で確かめられるが、runごとにjobが1本増え、同じ材料を読み直す。今のreview jobに推奨と確信度を出させ、誤りの歯止めは下のConsequencesの柵に任せる。効果が足りなければ別に決める。

## Consequences

- 人の答え待ちが減る: 今の数え方では`planner_question`の大半（上限に当たる24件を除く）、`blocked`のleave it、`concern`のうち`scope`・`discard`に当たらないものがinboxに来なくなる。件数の変化は[Stats](../design/supervisor-lifecycle/stats.md)のaskの`reason_category`の集計と、AIが決めた判断の記録で確かめる。
- 誤った自動の`land`の危険が生まれる。歯止めは、外れを受け入れる`land`を人に残すこと、確信度`low`を人に上げること、AIの`land`も`integrate`の検証と着地の前のe2eを通ること、AIの`send_back`をreviseの上限に数えること、AIの判断をeventに残して後から数えられること。
- observerのleave itはfindingにだけ残るので、人はinboxでなく`dagq findings`で見る。
- 実装はこの後のtaskが行う（taskの一覧はtask 451のreceiptの`follow_ups`）。実装の前は今の動きのまま。

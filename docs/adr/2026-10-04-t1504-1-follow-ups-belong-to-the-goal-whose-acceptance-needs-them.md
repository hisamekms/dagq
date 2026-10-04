---
id: adr-t1504-1
type: adr
title: follow-upの所属を元のgoalの受け入れ条件の達成に必要かで決め、所属・採用・priorityを分け、goalは必須の作業の完了と発見済みのfollow-upの所属判断の完了でachievedで閉じ、workerは提案・plannerは判断と記録・plan reviewは対応づけからの検査・runtimeは汎用の契約を持ち、acceptanceを弱める変更とachievedの後の満たしていなかった誤分類は人が決める（ADR-0047決定16・43をamends）
status: accepted
created: 2026-10-04
updated: 2026-10-04
accepted_on: 2026-10-04
amends:
  - adr-0047 decision 16
  - adr-0047 decision 43
owners:
  - hisamekms
tags:
  - runtime
  - planner
  - follow-up
  - goal
related:
  - adr-t1504-2
  - adr-0009
  - adr-0047
  - adr-t808-1
  - adr-t451-1
  - adr-t947-3
  - adr-t1394-1
  - adr-t1453-2
  - design-supervisor-lifecycle-goal-review
  - design-supervisor-lifecycle-draft-planners
---

# ADR-t1504-1: follow-upの所属を元のgoalの受け入れ条件の達成に必要かで決め、goalは必須の作業の完了と発見済みのfollow-upの所属判断の完了でachievedで閉じる（ADR-0047決定16・43をamends）

## Context

receiptの`follow_ups`から`integrate`が登録するdraftは、無条件に元のtaskのgoalに入る（[ADR-0047](0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)決定27）。決定16の最後の項と決定43の起動の条件は、goalにdraft・submittedのtaskが1件でもあればgoal reviewを起動しないので、本質的な成果が着地しても小さな追加改善のfollow-upが残ってgoalが閉じない（2026-10-03にopenのgoal 65件のうち残り1件のものが13件）。所属の判断の記録も、判断したときのacceptanceの版も無い。

2026-10-03に人が方針を承認した（goal 97のdescriptionの(1)〜(7)）。このADRはその方針を決定にし、runtimeが強制する契約は[ADR-t1504-2](2026-10-04-t1504-2-runtime-records-and-enforces-follow-up-membership-judgements.md)が持つ。

## Decision

1. **所属は「そのfollow-upを実施しなくても元のgoalのacceptanceを満たしたと言えるか」で決める。** 言えなければ元のgoalに必須として残し、言えれば範囲外として別のgoalに置き、判断できなければ根拠（acceptance・receipt・source・ADR）を調べて決め、goalの意図が要るときだけ人に確かめる（[ADR-t451-1](2026-10-02-t451-1-ai-decides-recommendable-asks-and-escalates-only-the-undecidable.md)の推奨が出せればAIが決める方針のまま）。影響の大小・priority・元のgoalとの関連性・workerの種類（[ADR-t947-3](2026-09-28-t947-3-follow-ups-carry-category-codes.md)の`category`）では決めない。
2. **所属・採用・priorityは別の判断にする。** 範囲外として別のgoalに移しただけで採用（実施の価値がある）とも優先とも扱わない。採否は今までどおりruntimeのplannerが決め（ADR-0047決定16）、重複・実装済み・不要は根拠を残してcancelできる。所属を変えても出どころ（元のtask・run・goal）と深さは残す（[ADR-t808-1](2026-09-28-t808-1-runtime-planners-submit-follow-ups-up-to-depth-two.md)の上限は所属の変更で外れない。仕組みはADR-t1504-2）。
3. **範囲外の置き先は、既存の適切なgoalを先に探し、無ければ作る。** 無関係な大きな保守のgoalにまとめて詰めない（そこで同じ「閉じないgoal」が再び生まれ、所属の判断が形だけになるため）。
4. **goalをachievedで閉じる条件は、必須の作業が完了し、発見済みのfollow-upの所属判断が済んでいること。** 範囲外に分類したfollow-upの実行は待たない。abandonedは達成を言わないのでこの条件で妨げない。未判定のfollow-upをgoalから外しただけでは閉じない（判断の記録が要る）。ADR-0047決定16の最後の項の「所属goalのdraft（plannerの待ち、openな`planner_question`、`keep_draft`）があるgoalは起動の条件を満たさない」と決定43の起動の条件のうちfollow-upに関わる部分は、この条件に読み替える（必須と判定したfollow-upはgoalのtaskとして今までどおり完了を待つ。具体の検査はADR-t1504-2）。
5. **役割を分ける。**
   - **worker**: receiptのfollow-upに、問題・根拠・元のgoalのacceptanceとの関係の提案を書く。確定はしない。
   - **planner**（runtimeのplanner。人が開くplannerは[ADR-t1394-1](2026-10-03-t1394-1-abolish-person-planners-and-route-planning-through-inbox-requests.md)で廃止し、計画はinboxからの依頼のruntimeのplannerか人の自分のterminalで行う）: 意味を判断し、該当するacceptanceの項目・満たせない（満たせる）理由・証拠の参照・所属先を短く記録する。
   - **plan review**: plannerの対応づけを起点に検査し、疑わしいものは周辺の証拠（receipt・source・acceptanceの他の項目）も読む。同じ全面調査を独立に2回は強制しないが、欄がそろっているだけでpassにしない。
   - **runtime**: 分類・理由・所属先などの必須の欄、許す状態遷移、記録の保存、閉じる条件の検査を汎用の契約として強制する。意味の正しさはplannerとplan reviewが持ち、runtimeは判定しない。
6. **人が決めるもの。** (a) follow-upを外すためにacceptanceを弱める（項目を消す・緩める）変更は、plannerが自動で行わず人の意図を確かめる。単なる所属の訂正（分類の誤りを直す）とは区別し、訂正はplannerが記録して行う。(b) goalがachievedで閉じた後に見つかった誤分類は、達成の履歴を残したまま訂正の記録と修正のgoalを作るのを基本にし、元のacceptanceを実際には満たしていなかったときは、goalの再開か達成の判定の訂正かを人が決める。人への問いには解放済みの依存task（そのgoalを待っていたtask）の一覧を添え（再開は[ADR-0009](0009-goal-groups-tasks.md)の「閉じたgoalへの追加と付け替えを拒み、続きは新しいgoalにする」の例外になる。その範囲と変え方はADR-t1504-2決定9）、履歴の書き換えと走っている依存taskの停止は自動でしない。
7. **acceptanceの版と判断を結ぶ。** 判断にはそのときのacceptanceの版を残し、acceptanceが変われば関係する判断を確かめ直すまでgoalを閉じない。登録・所属の変更・goal reviewの並行・再起動・引き継ぎでも、未判定の必須の修正があるgoalを閉じない（検査の形はADR-t1504-2）。
8. **手順の置き場所は[ADR-t1453-2](2026-10-03-t1453-2-ownership-of-agents-md-plugin-development-docs-and-config.md)決定1に従う。** planner・plan review・inboxの共通の手順（分類の問い、記録の書き方、検査の仕方、人に上げる場合）はpluginのskill、このrepository固有の規則（どの文書を証拠に読むか等）は`docs/development/`、AGENTS.mdは短い参照だけを持つ。

## Alternatives

- **影響の大きさやpriorityで分ける**: 小さくても受け入れ条件の欠けは欠けで、大きくても範囲外の改善は範囲外。基準が判断者ごとに揺れ、閉じた後に欠けが見つかる。
- **範囲外のfollow-upを1つの保守goalに集める**: 判断が楽になる代わりに、保守goalが閉じなくなり、所属の判断が「とりあえず保守へ」に流れる。
- **runtimeが意味を判定する**（acceptanceの文とfollow-upの文の照合など）: 意味の判断はLLMのplannerとplan reviewの役で、runtimeが持つと誤判定を強制する。runtimeは欄と遷移と検査だけを持つ。
- **plan reviewに同じ全面調査を独立にやり直させる**: 毎回のtokenと時間が倍になる。plannerの対応づけを起点にし、疑わしいときだけ広げる。

## Consequences

- 範囲外のfollow-upが残っていてもgoalを閉じられる。代わりに、未判定のfollow-upは（元のgoalから外しても）goalを閉じさせない。
- plannerの仕事に所属の判断の記録が加わり、plan reviewの資料にその対応づけが載る。
- 既存のopenのgoalの所属の判断は自動で済ませない（ADR-t1504-2の移行の契約）。棚卸しの分類案は計画までで、適用は権限のあるplannerか人が行う。
- 評価の指標（分類の初回のpass率、採用の判断から承認までの総tokenと時間とreviseの周回、承認後に見つかった誤分類）はgoal 97の別のtaskが定義し測る。

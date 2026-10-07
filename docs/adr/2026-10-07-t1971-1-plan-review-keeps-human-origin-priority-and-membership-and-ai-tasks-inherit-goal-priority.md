---
id: adr-t1971-1
type: adr
title: plan reviewはproposalの由来（人かAIか）で優先度と所属の扱いを分け、由来は出し直しで切らず、人間由来は変えずにconcernにし、AI由来のtaskは個別の優先度を持たずgoalから継ぎ、AI由来の新しいgoalの優先度と既存のgoalへの所属を受け入れ条件と段の目安で見てreviseにする（ADR-0047決定11・ADR-0051決定26をamends）
status: accepted
created: 2026-10-07
updated: 2026-10-07
accepted_on: 2026-10-07
amends:
  - adr-0047 decision 11
  - adr-0051 decision 26
amended_by:
  - adr-t1975-1
owners:
  - hisamekms
tags:
  - runtime
  - planner
  - plan-review
  - priority
  - goal
related:
  - adr-0047
  - adr-0051
  - adr-t1639-1
  - adr-t1639-2
  - adr-t1504-1
  - adr-t1504-2
  - adr-t1394-1
  - adr-t451-1
  - adr-t1091-1
  - adr-t1453-2
  - development-task-registration
---

# ADR-t1971-1: plan reviewはproposalの由来で優先度と所属の扱いを分け、人間由来は変えず、AI由来のtaskはgoalから継がせる（ADR-0047決定11・ADR-0051決定26をamends）

## Context

plan reviewの自分でしてよい修正の`lower_priority`（[ADR-0047](0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)決定11）はtaskに個別の優先度を置き、taskの由来を見ない。findingの改善のproposalの引き下げ（[ADR-0051](0051-kpi-time-series-report-and-push.md)決定26）も個別の`normal`を置く。これは「goalの優先度を正本にし、taskは個別の指定が無ければgoalから継ぐ」（[ADR-t1639-1](2026-10-04-t1639-1-goal-priority-is-the-source-tasks-inherit-and-goals-carry-tags.md)）に反し、人の決定を黙って変える事故を2回起こした: 2026-10-04に人がhighと決めたgoal 118・119のtaskが下げられ、2026-10-06に人がinterruptを指示したrequest 44のgoal 159のtaskにpassと同時に個別の`normal`が置かれ、どちらも人が手で戻した。

2026-10-06に人がrequest 47で方針を決めた: 人間由来のgoal・taskは指示どおり、AI由来はreviewする。新しいgoalは従来の方針で優先度を付け、taskは基本的に優先度を入れず、既存のgoalに入れるかはreviewの対象で、受け入れ条件と関係なければ新しいgoalか単独のtaskにする。このADRはそれを決定にする。

由来の記録には穴がある。requestとproposalの結び付けは、submitしたplannerのworkspaceがrequestを持つときだけ付く。request 44のproposal 734は結ばれたが、取り下げと出し直しを経て、requestを持たないreviseのplannerが同じtaskをproposal 737で出し直してacceptedになり、737はrequest 44に結ばれていない。runtimeが立てたplannerのproposalの持ち主は、人の依頼から立ったplannerでもそうでなくても同じに記録され、plan reviewの材料の出どころも人かAIかを表さない。今の結び付けだけで判定すると、出し直しで人間由来がAI由来になる。

## Decision

1. **由来はproposal単位で判定する。** requestに結ばれたproposalと、持ち主が人のproposal（submitしたactorが人、または人が持ち主として出したproposal）は人間由来、それ以外（findingに結ばれたproposal、follow_up・goal_gapのdraftのproposal、draftのplannerのproposalなど）はAI由来とする。人間由来のproposalの中でplannerが自分で足したtaskも人間由来に数える（人のrequestから作ったものだから）。taskやgoalのcontext・descriptionの「from request N」などの文面は判定に使わない（plannerが書く文で、守る規則にならない）。記録から人間由来と言えないproposalはAI由来として扱う。その場合も、goalの優先度はplan reviewが変えず（決定5）、人が置いたtaskの個別の指定は外さない（決定4）ので、人の決定が黙って変わる経路は残らない。
2. **由来は出し直しで切らない。** submitのとき、出すtaskかgoalがその時点で結ばれているproposal（取り下げ・reviseの後もtaskとgoalは前のproposalを指したまま）が、あるrequestに結ばれていれば、新しいproposalもそのrequestに結ぶ。別のplannerが同じtask・goalを別のproposalで出し直しても、出し直しが何度続いても同じにする。requestのproposalの一覧に出し直しのproposalも並ぶ形で、schemaは変えない（今のtaskとgoalのproposalの参照とrequestとproposalの結び付けで判定しきれるため）。taskとgoalに由来の欄を足す案は採らない（決定1の判定がproposal単位なので、同じことを2か所に持つことになる）。この結び付けが入る前に切れたproposal（737など）を遡って結び直すことはしない（既存の値を変えない）。
3. **人間由来のproposalでは、plan reviewはtaskとgoalの優先度と所属を自分で変えない。** verdictの`lower_priority`はpassで適用せず、適用しなかったactionと理由を記録し、verdict全体は失敗させない（ADR-0047決定11の「1つでも適用できなければverdict全体をjobの失敗として扱う」の例外）。優先度・所属に疑いがあれば`concern`で人に上げる（AIが決めきれないconcernで、[ADR-t451-1](2026-10-02-t451-1-ai-decides-recommendable-asks-and-escalates-only-the-undecidable.md)の理由の分類は`scope`）。人の言葉に優先度の無い新しいgoalの優先度はplannerが段の目安で付け、そのことをgoalのdescriptionかtaskのcontextに書く。plan reviewはそれをAI由来の新しいgoal（決定5）と同じ目安で見てよいが、自分では変えず、外れていれば`revise`でplannerに直させる。
4. **AI由来のtaskは個別の優先度を持たず、所属のgoalから継ぐ（goalの無いtaskは`normal`）。** AI由来のproposalのpassで、runtimeは`submitted`のtaskの個別の指定を外してgoalから継がせる。plan reviewの`lower_priority`は、AI由来のtaskには値を置かず個別の指定を外す形で適用する（verdictの形とactionの種類は変えない）。findingの改善のproposalの自動の引き下げ（ADR-0051決定26）もこの形に改め、個別の`normal`を置かない。goalから継いだ優先度が高すぎるなら、それは所属（決定6）かgoalの優先度（決定5）の問題として扱う。人が`set-priority`で置いた個別の指定は人の決定なので、reopenなどで`submitted`に戻ったtaskでも外さない（誰が置いたかが記録から分からなければ外さない側に倒す）。そのtaskへの`lower_priority`は決定3と同じく適用せず理由を記録し、verdict全体は失敗させない。ADR-0051決定26のうち、改善のproposalのtaskを`normal`以下にする上限は、taskの値ではなく所属で守る: plannerは改善のtaskを`normal`以下のgoalか単独のtaskに置き、plan reviewは`high`以上のgoalに入った改善のtaskを決定6の所属の検査で見る。改善のgoalを他のgoalより前に置かないことは変えない。
5. **AI由来の新しいgoalの優先度は、plannerが従来の段の目安（このrepositoryでは`docs/development/`）で付け、plan reviewが見る。** 外れていれば`revise`にし、goalの優先度はplan reviewが自分で変えない（goalの変更は個別の指定の無い他のtaskにも波及し、人が決めたgoalとの区別も要らないreviseの方が安全なため）。
6. **AI由来のtaskを既存のgoalに入れるかは、plan reviewがそのgoalの受け入れ条件で見る。** 関係なければ`revise`にし、plannerが新しいgoalを作るか、goalの無い単独のtaskにする。後回しの受け皿は[ADR-t1639-2](2026-10-04-t1639-2-defer-improvements-outside-acceptance-to-a-low-goal-per-tag.md)のまま。follow_upは[ADR-t1504-1](2026-10-04-t1504-1-follow-ups-belong-to-the-goal-whose-acceptance-needs-them.md)・[ADR-t1504-2](2026-10-04-t1504-2-runtime-records-and-enforces-follow-up-membership-judgements.md)の判定と記録のまま。follow_up以外のAI由来のtaskには所属の記録を求めない（goalを閉じる条件に関わる未判定の発見はfollow_upだけで、それ以外はplan reviewの判定とplannerのcontextで足りる）。
7. **ADR-t1504-1とは`related`にし、amendsしない。** このADRはADR-t1504-1の決定（follow_upの所属の基準・役割・閉じる条件）を変えず、同じ「受け入れ条件に要るか」の見方をfollow_up以外のAI由来のtaskのplan reviewに新しい決定として当てはめるため（[ADR-t1091-1](2026-09-30-t1091-1-amend-or-replace-by-number-of-decisions.md)の選び方で、変える決定が無い）。ADR-t1639-1も変えず、その継承をplan reviewに当てはめる。

## Alternatives

- **由来を問わず今の`lower_priority`を保つ**: 人の決定が黙って変わる事故が続き、人の手で戻すコストが残る。
- **requestの高い優先度だけを守る（goal 124の案）**: 人がnormalやlowを明示したときと所属の変更を守れず、AI由来のtaskに個別の値が置かれ続ける。
- **contextの文面で由来を判定する**: plannerが書く文で、書き漏れや書き換えで判定が変わる。
- **taskとgoalに由来の欄を足す（schemaの変更）**: 判定がproposal単位なので二重に持つことになり、今の参照で足りる。
- **人間由来の`lower_priority`を失敗として扱う**: verdict全体が失敗し、人のrequestから作った計画がpassしても`ready`にならない。適用せず記録する方が流れを止めない。
- **AI由来のgoalの優先度をplan reviewが直接下げる**: 波及が大きく、goalの判断はplannerに戻す方が理由が残る。

## Consequences

- 人の明示した優先度と所属は、plan reviewとrunの自動の修正で変わらない。疑いは`concern`で人に届く。
- AI由来のtaskの優先度はgoalで一括して決まり、goalの優先度と所属がplan reviewの検査の対象になる。
- runtime（由来の判定と出し直しの結び付け、passでの個別の指定の扱い、適用しなかったactionの記録、plan reviewの材料での由来の提示）、`docs/design/`のplan reviewとfinding plannerの今の姿、pluginのplannerとregisterの手順は、後続のtaskが合わせる。このADRと同じ変更では`docs/development/task-registration.md`だけを合わせる。

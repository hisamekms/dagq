---
id: adr-t1985-1
type: adr
title: taskの依存は中身の前提に限り、同じファイルの衝突を避けるためだけの依存はplannerもplan reviewも付けずclaimの控えに任せ、依存の種類と理由を記録し、優先度の高いgoalのtaskが低いgoalのtaskを中身で待ちplannerの手で解けないときだけ人に聞き、plan reviewは優先度の下がる向きの衝突回避だけの依存をreviseにする（ADR-0047決定10・11をamends）
status: accepted
created: 2026-10-07
updated: 2026-10-07
accepted_on: 2026-10-07
amends:
  - adr-0047 decision 10
  - adr-0047 decision 11
owners:
  - hisamekms
tags:
  - runtime
  - planner
  - plan-review
  - priority
related:
  - adr-0047
  - adr-0080
  - adr-t1484-1
  - adr-t1850-1
  - adr-t1566-1
  - adr-t451-1
  - adr-t1091-1
  - adr-t1453-2
  - design-supervisor-lifecycle-claim-defer
  - design-supervisor-lifecycle-plan-review
  - development-task-registration
---

# ADR-t1985-1: taskの依存は中身の前提に限り、衝突回避だけの依存は付けずclaimの控えに任せ、優先度の高いgoalが低いgoalのtaskを中身で待つときだけ人に聞く（ADR-0047決定10・11をamends）

## Context

[ADR-0047](0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)決定10はplan reviewの意味の検査に「同じファイルを触るtaskの間の依存の提案」を置き、決定11はそのための`add_dependency`をjobが自分でしてよい修正にした。plan reviewのpromptは、衝突の多いファイル（hotspot）をproposalのtaskと`ready` / `in_progress`のtaskが共に触ると予想されるとき、依存を足すか要らない理由を書くよう求めていた。plannerも同じ考えで、同じファイルを触るtaskの間に依存を付けてきた。

一方、同じファイルの衝突はruntimeのclaimの控え（[ADR-0080](0080-supervisor-rereads-conflicts-config.md)決定3〜7。ADR-0069を統合し、人のaskだけを待つrunを数えない[ADR-t1484-1](2026-10-04-t1484-1-runs-waiting-only-for-a-person-stop-holding-claims-past-a-grace.md)のamendを含む）が、進行中のrunと重なるtaskをそのpassでclaimせずに次の候補へ進むことで扱う。依存はそれより強く、着地まで待たせ、しかも[ADR-t1850-1](2026-10-06-t1850-1-resumes-recovery-jobs-and-claims-share-one-line-by-effective-priority.md)の効く優先度（effective_priority）で待たれるtaskに待つtaskの優先度を継がせる。

2026-10-06のrequest 54で、interruptのgoal 159のtask 1949・1951が、normalのgoal 71のtask 1908・1909を同じファイルの衝突を避けるためだけに待った。継承で1438→1690→1664のnormalの鎖がinterruptに上がり、interruptのslotが中身の関係ないtaskに使われた。人はrequest 55で、優先度の高いgoalのtaskから低いgoalのtaskへ衝突回避だけの依存を付けず、どうしても要るときは聞けるようにすると指示した。

## Decision

1. **taskの依存は中身の前提（先に着地しないと作業が成り立たない）に限る。** 同じファイルの衝突を避けるためだけの依存は、plannerもplan reviewも付けず、askにもしない。同じファイルの衝突はADR-0080のclaimの控えに任せる。
2. **依存を付けるとき、plannerはnoteかtaskのcontextに種類（中身の前提か衝突回避か）と理由を書く。** 決定1により衝突回避は付けない側だが、種類を書くことでplan reviewと後から読む人が確かめられる。種類は文面の判断で機械に読めないので、`lint`の検査は足さない。
3. **人に聞く（`planner_question`、理由の分類は`scope`）のは、次の全部を満たすときだけにする。** (a) 中身の前提の依存で、(b) その依存で低い優先度のgoalのtaskが効く優先度を継いで上がり、(c) 順番の変更・taskのgoalへの取り込み・分割などplannerの手で解けない。問いは「低いgoalのtaskを（優先度ごと）前に出すか、高いgoalが待つか」にする。どれかを満たさなければplannerが自分で決めて進め、理由をnoteかcontextに残す（[ADR-t451-1](2026-10-02-t451-1-ai-decides-recommendable-asks-and-escalates-only-the-undecidable.md)）。
4. **plan reviewは、優先度が下がる向き（高いgoalのtask → 低いgoalのtask）の依存の理由を見て、衝突回避だけなら`revise`にし、理由がnoteにもcontextにも無いときも`revise`にする。**
5. **ADR-0047決定10を改める。** 意味の検査の「同じファイルを触るtaskの間の依存の提案」を「中身の前提の依存の提案」に改め、同じファイルの衝突はADR-0080の控えに任せる。hotspotの材料は、重複と一部の重なりの検出のために残す。[ADR-t1566-1](2026-10-03-t1566-1-headless-job-prompts-carry-decision-material-within-limits.md)が決定10のpromptの材料を改めた部分は変えない。
6. **ADR-0047決定11を改める。** `actions`の`add_dependency`は残すが、中身の前提の依存にだけ使い、同じファイル・hotspotのためだけには使わない。verdictの形と`actions`の種類は変えない。

## Alternatives

- **衝突回避の依存は付けてよく、優先度の継承だけを止める**: effective_priorityの計算を依存の種類で分けることになり、種類が機械に読めないので守れない。claimの控えで足りる衝突のために着地まで待たせる損も残る。
- **優先度の下がる向きの依存を全てaskにする**: 中身の前提で取り込みや分割で解けるものまで人に上がる。ADR-t451-1の、推奨が出せる判断はAIが決める方針に反する。
- **依存の種類を欄にしてlintで検査する**: 種類は文面の判断で、欄にしても書いた者の判断を写すだけになる。
- **ADR-0047を置き換える**: 変えるのは決定10・11の一部で、他の決定は残るのでamendsにする（[ADR-t1091-1](2026-09-30-t1091-1-amend-or-replace-by-number-of-decisions.md)）。

## Consequences

- 衝突回避だけの依存で低い優先度のtaskが上がり、高い優先度のslotが中身の関係ないtaskに使われることが無くなる。同じファイルの衝突はclaimの控えが扱い、控えた分の待ちはstatusとstatsに出る。
- claimの控えはADR-0080のとおり衝突の多いファイル（alertのhotspot）だけを、上限の時間まで控え、効く優先度が`interrupt`のtaskは控えない。それ以外の同じファイルの重なりは控えられずに並んで走り、rebaseか着地の衝突として解く。依存で着地まで待たせるより、この衝突の手間を受け入れる（衝突の多いファイルの扱いを強めるなら、依存ではなくclaimの控えを変える）。
- plannerは依存ごとに種類と理由を書く手間が増え、plan reviewは優先度の下がる向きの依存の理由を読む。
- このrepositoryの規則（`docs/development/task-registration.md`）、plan reviewのpromptとそのtest、`docs/design/`のplan reviewの今の姿をこのADRと同じ変更で合わせる。runtimeのpromptとpluginの句は汎用に保ち、このrepositoryに固有の規則は`docs/development/`に書く（[ADR-t1453-2](2026-10-03-t1453-2-ownership-of-agents-md-plugin-development-docs-and-config.md)）。pluginのplannerとregisterの手順は同じgoalの別のtaskが合わせる。claimの控え・effective_priorityの計算・`lint`は変えない。既存のtaskの依存は変えない。

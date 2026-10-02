---
id: adr-t1300-1
type: adr
title: 人が開いたplannerも、agentの終了が記録されたら猶予の後にruntimeがworkspaceと行を閉じ、閉じたことをeventに残す
status: accepted
created: 2026-10-02
updated: 2026-10-02
accepted_on: 2026-10-02
amended_by:
  - adr-t1394-1
owners:
  - hisamekms
tags:
  - runtime
  - planner
related:
  - adr-0044
  - adr-0047
  - adr-t1228-1
  - adr-t1228-2
  - design-supervisor-lifecycle-plan-planners
---

# ADR-t1300-1: 人が開いたplannerも、agentの終了が記録されたら猶予の後にruntimeがworkspaceと行を閉じ、閉じたことをeventに残す

## Context

runtimeがplannerのworkspaceを閉じるのは、runtimeが立てたplannerだけだった（[Plan review (supervisor)](../design/supervisor-lifecycle/plan-review.md)の10）。人が`dagq plan`で開いたplanner（origin `person`）は、agentが終わってもworkspaceが残り、行の片付けはworkspaceがcmuxの一覧から消えた行を後から閉じるだけだった（[`plan` / `planners`](../design/supervisor-lifecycle/plan-planners.md)）。これは2026-09-27の人の決定（task 695のdescriptionの「runtimeが勝手には閉じない」）による。

2026-10-02の朝、inboxがplanner #593・#595・#597・#634を`dagq planner send <ID> --key exit`で終えたが、workspaceは4つとも残り、人の了承でcmuxを直接打って閉じた。task 1232がinboxとplannerの`Bash(cmux:*)`を拒む（[ADR-t1228-2](2026-10-02-t1228-2-deny-raw-cmux-to-inbox-and-planner-as-a-guardrail.md)）と、終わったplannerのworkspaceを閉じられるのは人だけになる。人は同じ朝、人が開いたplannerもsessionが終わったらruntimeが閉じると決めた。猶予の長さと閉じる状態はplannerに任された。

また、runtimeがplannerを閉じたことはlogにしか残らず、originに依らずplannerの終わりをqueueから追えなかった。

## Decision

1. **人が開いたplannerも、agentの終了が記録された後、猶予を置いてruntimeがworkspaceと行を閉じる。** 対象はagentの終了が記録された（状態が`exited`の）plannerだけで、wrapperが死んだ`lost`のplannerと生きているplannerは今までどおり閉じない。猶予は人が最後の画面を読むためのもので、短くてよい。inboxが終えたplannerは誰も画面を見ず、人が自分で`/exit`したときは最後の画面を読む時間があればよく、debug logとtranscriptはplannerのディレクトリとClaudeの記録に残るためである。`lost`を閉じないのは原因を見るために画面を残すためで、workspaceが消えた`lost`の行は従来の行の片付けが拾う。supervisorが居ないときは`plan`の実行時の片付けが同じ条件で閉じる。wrapperは閉じる予告をterminalに出すが、自分では閉じない。
2. **runtimeがplannerを閉じたことを、originに依らずqueueのeventに残す。** 人のplannerの猶予切れ、runtimeのplannerの終了（exit・wrapperの消失・sessionの消失・`/exit`の時間切れ）、workspaceとwrapperが消えた行の片付けのどれも、閉じた理由とagentの終了を持つ同じeventを1回残す。画面の中身は載せない。

この決定は2026-09-27の人の決定（runtimeは人が開いたplannerを勝手には閉じない）を置き換える。[ADR-0044](0044-findings-proposals-from-findings-and-quiet-observer.md)決定13（[ADR-0047](0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)が引き継ぐ）の「期限を過ぎても閉じない」は生きているplannerのreviseの期限の話で、変えない。猶予の値（60秒）と名前、eventのkindとpayload、wrapperの予告の文面は[`plan` / `planners`](../design/supervisor-lifecycle/plan-planners.md)が書く。

## Alternatives

- **人のplannerは閉じず、inboxが`planner close`で閉じる**（task 695のCLI）: inboxが`planner send --key exit`の後にもう1手打つことになり、人が自分で`/exit`したplannerは誰も閉じない。手で閉じる経路はtask 695に残る。
- **`lost`も閉じる**: wrapperが死んだ原因を画面で見られなくなる。
- **猶予を長く（数分以上）とる**: 終わったworkspaceがcmuxの一覧に残り続け、読む材料はlogとtranscriptに残るので得るものが少ない。

## Consequences

- 人が開いたplannerのworkspaceは、agentが終わって猶予が過ぎると消える。画面を長く見たいときはagentを終わらせずに開いておく。
- cmuxの一覧が取れない・closeが失敗したときは行を閉じず、次のpassか次の`plan`で再試行する。
- `dagq events --kind planner_closed`で、どのplannerがなぜ閉じたかを読める。

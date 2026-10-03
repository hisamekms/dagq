---
id: adr-t906-1
type: adr
title: inboxのwatchの生存を、watcherの記録と可視化・pluginのSessionStart / Stop hook・supervisorによるidleのinboxへの知らせの3層で保証する（ADR-0016決定3とADR-0022決定2をamends）
status: superseded
created: 2026-09-28
updated: 2026-09-28
accepted_on: 2026-09-28
superseded_by: adr-t1433-5
superseded_on: 2026-10-03
amends:
  - adr-0016 decision 3
  - adr-0022 decision 2
owners:
  - hisamekms
tags:
  - runtime
  - plugin
  - inbox
  - operations
related:
  - adr-0016
  - adr-0022
  - adr-0044
  - adr-t803-1
  - design-supervisor-lifecycle-events-watch
  - design-supervisor-lifecycle-status
  - design-supervisor-lifecycle-doctor
  - design-supervisor-lifecycle-notification-route
  - design-supervisor-lifecycle-cmux-notify
  - design-plugin-integration
---

# ADR-t906-1: inboxのwatchの生存を3層で保証する（ADR-0016決定3とADR-0022決定2をamends）

> **置き換え済み（2026-10-03）**: このADRの決定は現在有効ではない。現行の決定は[ADR-t1433-5](2026-10-03-t1433-5-inbox-watch-without-typing-into-the-inbox.md)を読む。

## Context

inboxのClaude sessionが起きるのは、backgroundで回す`watch --role inbox`の終了だけ（[ADR-0016](0016-maintainer-notification-and-compact-output.md)の決定3、[ADR-0022](0022-ask-answer-inbox-planner-and-landing-on-doubt.md)）。askの`cmux notify`は人への知らせで、sessionを起こさない。watchを張り直すかどうかはmodelの振る舞い次第で、SessionStart hook（`compact|clear`）は`status --role inbox`を出すだけだった。

2026-09-28、inboxを`/clear`した後にsessionがwatchを張り直さず、ask 162〜175が約3時間人に届かなかった。task 828のworkerはask 165で3時間待ち、待ちのslotを占めた。watchが居ないことはどこにも記録されず、`status`からも分からなかった。

## Decision

1. **inboxのwatchの生存を、次の3層の仕組みで保証する。** どれか1つが効かなくても次の層が拾う。
   - (1) **記録と可視化**: `watch --role inbox`は実行中の自分の生存（pid・開始・heartbeat・終了）をqueueのディレクトリのファイルに残す。queue DBには書かず、watchとstatusはqueueを読むだけのまま。`status`と`doctor`はinboxのwatcherの有無（alive / absent）・最後に居た時刻・居ない秒数を出す。生存はprocessの有無ではなくheartbeatの新しさで判定し（固まった・queueを読めていないwatchも居ないと数える）、判定はapplicationの1か所に置いてhookとsupervisorが共有する。watchが返ってから次を張るまでの短い切れ目は猶予にして居ないと数えない。
   - (2) **pluginのhook**: SessionStart hookはinboxの`startup`・`resume`・`clear`・`compact`の全部で、watchを張ることを最初の一手として命じる行と`status`を出す。Stop hookは、watchが1つもwatchingでないinboxのsessionのturnの終わりをblockし、watchを張るコマンドをreasonで渡す。hookのblockで続いたturn（`stop_hook_active`）は止めない。hookは失敗してもsessionを止めない。
   - (3) **runtimeの後ろ盾**: watcherが居ないまま閾値を超えてaskが開いているとき、supervisorはinboxのworkspaceが画面でidleのとき（[ADR-t803-1](2026-09-27-t803-1-infer-idle-from-the-screen-when-the-idle-marker-is-missing-or-stale.md)の画面の推定）に1行の知らせを1回打ち込み、sessionを起こし、eventに残す。plannerへのreviseの配送と同じ経路を使い、人が入力中のinboxには打ち込まない。実装は後続のtask（907）が行う。

2. **runtimeはinboxのterminalに、決定1の(3)の知らせに限って打ち込んでよい。** ADR-0016の決定3（「runtimeはmaintainer（今のinbox）のterminalに文字を打ち込まない」）と、ADR-0022の決定2とその原則の「runtimeはどのsessionのterminalにも打ち込まない。例外はworkerへのanswerの送信だけ」を、inboxについてこの1つの例外で改める。ADR-0016が打ち込みを退けた理由のうち、(a) UIの状態が分からない点は画面のidleの推定と人の入力中を避ける規則で、(b) 送達確認が無い点はeventに残すことと`status`のwatcherの記録で後から確かめられることで補う。answerやattentionの中身は打ち込まず、知らせは「watchを張り直してaskを見る」ことを促す1行にとどめる。pull型の`watch`がattentionを運ぶことは変えない。

## Alternatives

- **hookだけ（(3)なし）**: SessionStartとStopのhookはmodelがturnを動かしているときにしか効かない。`/clear`の後に人が何も打たなければturnが始まらず、Stop hookも走らないので、askは届かないまま残る。今回の事故はまさにこの形だった。
- **watchをやめてaskごとに直接inboxへ送る**: watchはaskだけでなくattention全般（回答済みのask、失敗したreview、supervisorの停止など）とsupervisorの健全性の変化を運ぶので、置き換えは大きい。askのたびに打ち込むと、人がinboxで入力・対話している最中との衝突も増える。(3)は「watchが居ないときの起こし役」に限るので、打ち込みは稀で1回きり。
- **UserPromptSubmit hookで状態を差し込む**: 人がpromptを打つまで効かず、人が見ていないあいだに開いたaskを届けるという目的に届かない。

## Consequences

- `status --role inbox`と`doctor`で、inboxにwatchが張られているか、どれだけ居ないかが見える。KPI（askを開いてからinboxが見るまでの時間）は、この記録が入った後の別の段にする。
- inboxのsessionはwatchを張らずにturnを終えられなくなる（Stop hookが1回止める）。
- 閾値・猶予の値、記録のファイルの場所と形、hookの出力の文面は`docs/design/`（[`events` / `watch`](../design/supervisor-lifecycle/events-watch.md)、[`status`](../design/supervisor-lifecycle/status.md)、[`doctor`](../design/supervisor-lifecycle/doctor.md)、[plugin integration](../design/plugin-integration.md)）が持つ。
- ADR-0016とADR-0022の他の決定（pull型の`watch`、`cmux notify`はaskのときだけinbox宛て、workerへのanswerの送信）は変えない。

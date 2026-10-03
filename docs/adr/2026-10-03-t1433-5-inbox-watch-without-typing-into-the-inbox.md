---
id: adr-t1433-5
type: adr
title: inboxのwatchの生存を、watcherの記録と可視化・pluginのSessionStart / Stop hookで保証し、runtimeの後ろ盾はinboxの画面を読まず打ち込まず、watcherが居ないまま閾値を超えてaskが開いていればhost.tomlの[push]で送り、無ければeventだけ残す。runtimeはどのsessionのterminalにも打ち込まない（ADR-t906-1を置き換え、ADR-0016決定3・ADR-0022決定2・ADR-t1233-4決定3をamends）
status: accepted
created: 2026-10-03
updated: 2026-10-03
accepted_on: 2026-10-03
supersedes:
  - adr-t906-1
amends:
  - adr-0016 decision 3
  - adr-0022 decision 2
  - adr-t1233-4 decision 3
owners:
  - hisamekms
tags:
  - runtime
  - plugin
  - inbox
  - operations
related:
  - adr-t906-1
  - adr-0016
  - adr-0022
  - adr-t1233-4
  - adr-t1418-1
  - adr-0051
  - adr-t1091-1
  - adr-t1433-1
  - adr-t1433-2
  - design-supervisor-lifecycle-events-watch
  - design-supervisor-lifecycle-notification-route
  - design-plugin-integration
---

# ADR-t1433-5: inboxのwatchの生存を、runtimeの打ち込みなしで保証する

## Context

[ADR-t906-1](2026-09-28-t906-1-guarantee-the-inbox-watch.md)は、2026-09-28にinboxが`/clear`の後にwatchを張り直さずask 162〜175が約3時間人に届かなかったことから、inboxのwatchの生存を (1) watcherの記録と可視化、(2) pluginのSessionStart / Stop hook、(3) supervisorがinboxの画面のidleを見て1行を打ち込む後ろ盾、の3層で保証するとした（決定1）。決定2は(3)のために、runtimeはどのsessionのterminalにも打ち込まないという原則（[ADR-0016](0016-maintainer-notification-and-compact-output.md)決定3、[ADR-0022](0022-ask-answer-inbox-planner-and-landing-on-doubt.md)決定2）にinboxの例外を足した。

2026-10-03の人の決定（goal 92、[ADR-t1433-1](2026-10-03-t1433-1-cmux-is-used-only-by-the-inbox.md)）で、supervisorはcmuxを呼ばない。(3)は画面を読むことと打ち込むことの両方でcmuxを使うので残せない。決定1の3層のうち1つと決定2の全部が変わり、決定の大半を変えるので丸ごと置き換える。[ADR-t1433-2](2026-10-03-t1433-2-abolish-the-interactive-route.md)でworkerへのanswerの打ち込み（ADR-0022決定2の例外）も対象が無くなる。

## Decision

1. **inboxのwatchの生存を、次の層で保証する。** ADR-t906-1決定1の(1)(2)を引き継ぎ、(3)を改める。
   - (1) **記録と可視化**（引き継ぐ）: `watch --role inbox`は実行中の自分の生存（pid・開始・heartbeat・終了）をqueueのディレクトリのファイルに残し、queue DBには書かない。`status`と`doctor`はinboxのwatcherの有無（alive / absent）・最後に居た時刻・居ない秒数を出す。生存はprocessの有無ではなくheartbeatの新しさで判定し、判定はapplicationの1か所に置いてhookとsupervisorが共有する。watchが返ってから次を張るまでの短い切れ目は猶予にして居ないと数えない。
   - (2) **pluginのhook**（引き継ぐ）: SessionStart hookはinboxの`startup`・`resume`・`clear`・`compact`の全部で、watchを張ることを最初の一手として命じる行と`status`を出す。Stop hookは、watchが1つもwatchingでないinboxのsessionのturnの終わりをblockし、watchを張るコマンドをreasonで渡す。hookのblockで続いたturnは止めない。hookは失敗してもsessionを止めない。
   - (3) **runtimeの後ろ盾**（改める）: supervisorはinboxの画面を読まず、inboxのterminalに打ち込まない。watcherが居ないまま閾値を超えてaskが開いているとき、supervisorはそのことをeventに残し、`host.toml`の`[push]`が設定されていればそれで人に1回送る。無ければeventだけ残す。人に届けるのは「inboxのwatchが居ないまま開いているaskがある」ことと件数だけで、askやattentionの中身は送らない。同じ不在のあいだは繰り返し送らない。
2. **runtimeはどのsessionのterminalにも打ち込まない（ADR-0016決定3、ADR-0022決定2をamends）。** ADR-t906-1決定2のinboxの例外を無くし、ADR-0016決定3とADR-0022決定2の原則に戻す。ADR-0022決定2の「workerへのanswerの送信だけは例外」も、workerが非対話だけになる（ADR-t1433-2）ので、answerを次のturnの依頼として届けることに置き換わり、例外は無い。ADR-0016決定3の「workerへの`/exit`は従来どおり送る」も対象が無い。pull型の`watch`がattentionを運ぶことは変えない。
3. **queue serviceに届かないときの後ろ盾（ADR-t1233-4決定3をamends）。** 段(5)でinboxの`watch`と`status`がservice経由になったとき、serviceに届かないことの後ろ盾は、inboxのterminalへの1行ではなく、決定1の(3)と同じ`[push]`とeventにする。watchとstatusがserviceに届かないこと自体を知らせとして返すことは変えない。

実装はgoal 92の後続のtaskが行う。閾値・eventのkind・pushの文面は[`events` / `watch`](../design/supervisor-lifecycle/events-watch.md)と[通知経路](../design/supervisor-lifecycle/notification-route.md)に書く。

## Alternatives

- **inboxへの打ち込みを後ろ盾として残す**: supervisorがinboxの画面を読み、terminalに打つためにcmuxを呼び続け、supervisorをcmuxから外せない（ADR-t1433-4）。人の入力中との衝突の避け方も画面の推定に頼り続ける。
- **hookだけにする（後ろ盾なし）**: ADR-t906-1のAlternativesのとおり、`/clear`の後に人が何も打たなければhookは走らず、askが届かない。後ろ盾を人の端末に届く経路で残す。
- **inboxのwatchがcmux notifyを出す（ADR-t1433-1決定2）だけに頼る**: watchが居ないときは通知も出ないので、ちょうど守りたい場面で効かない。
- **supervisorから`cmux notify`だけを残す**: 打ち込みより穏当だが、launchdのsupervisorがcmuxのsocketに接続する前提（ADR-0011）が戻る。`[push]`は人が設定した経路で、cmuxを要らない。

## Consequences

- runtimeはどのsessionのterminalにも打ち込まなくなり、inboxへの打ち込みの処理（画面の推定・入力中の判定・打ち込みの記録）とそのtestを消せる。
- `[push]`を設定していないhostでは、watchが居ないあいだのaskはeventと`status` / `doctor`のwatcherの記録でしか分からない。KPIの`ask_seen_wait`（[ADR-0051](0051-kpi-time-series-report-and-push.md)）がその遅れを数える。
- ADR-0016とADR-0022の他の決定（pull型の`watch`、通知はaskのときだけ）は変えない。通知を出すのはinboxのwatch（ADR-t1433-1決定2）になる。

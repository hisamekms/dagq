---
id: adr-t697-1
type: adr
title: supervisorのsweepは、閉じたplanner・終わったplan review・observationのdirを終わってから保持期間（dagq.tomlで変えられる）を過ぎたら消し、backups/は新しい方から決めた世代（dagq.tomlで変えられる）だけ残す。run dirの本体は残す
status: accepted
created: 2026-10-24
updated: 2026-10-24
accepted_on: 2026-10-24
owners:
  - hisamekms
tags:
  - persistence
  - supervisor
related:
  - adr-0045
  - design-persistence
  - design-supervisor-lifecycle-run-environment
---

# ADR-t697-1: 終わったplanner・plan review・observationのdirを保持期間で消し、backups/を世代で保つ

## Context

queueのdirには、終わった後に読むコードが無いまま消されないものが溜まる（goal 54、2026-09-27の調査で`planners/`に264個）。
runner（バイナリの写し）は終わったら消すようになったが、`planners/<id>`の残り（log・prompt）、`plan-reviews/<id>`、`observer/<started_at>`、`backups/`（非互換の`migrate`の前の写し、[ADR-0045](0045-build-identifier-explicit-migrate-schema-compat-handoff-and-auto-update.md)決定9）は残る。
終わった直後は人が調べに読むことがあるので、すぐには消さない。

人の決定（2026-09-27）: 閉じたplanner・終わったplan review・observerのrunごとのdirは日数の保持期間で消して`dagq.toml`で変えられ、`backups/`は新しい方から決めた世代だけ残し、run dirの本体は残す。

## Decision

1. **保持期間を過ぎたdirを消す。**
   supervisorのsweepが、閉じたplannerのdir、終わったplan reviewのdir、observationのdirを、閉じた・終わった時刻から保持期間を過ぎたら消す。
   plannerは行の閉じた時刻、plan reviewは行の終わった時刻で測る。
   observationには行が無いので、そのdirと中のファイルが最後に書かれた時刻を終わった時刻とする。
2. **生きているものは消さない。**
   閉じていないplanner、wrapperかagentのprocessがまだ動いているplanner、終わっていないplan reviewのdirは期間に関わらず残す。
   plannerの終わりの記録や古いheartbeatは、runnerを消す根拠にはなるが、dirを消す根拠にはしない。
   process（記録したpid）が動いているか終わりの記録が無いobservationのdirも、最後に書かれた時刻に関わらず残す。開始の記録も終わりの記録も無いdirは、終わりの記録が無いものに数える。observationのtimeoutは保持期間より長くできるので、書かれた時刻だけでは生きているかを判じられない。
   observerのcursorは消さない。
3. **backups/は世代で保つ。**
   `migrate`が作った写しを、作った時刻の新しい方から決めた数だけ残し、ほかを消す。
   `migrate`の名前でないファイルは残す。
4. **期間と世代は`dagq.toml`の1つの表で変えられる。**
   既定値はこのADRに書かず、実装と[Run environment](../design/supervisor-lifecycle/run-environment.md)が持つ。
   logの保持期間は変えられない値のままで、この表に入れない。
5. **消すのは上の対象だけ。**
   run dirの本体（receipt・verifyのlog）、plannersの行は消さない（後のtaskのprompt・resume・reviseの宛先が読む）。
6. **消したものはtracingに残す。**
   eventにはしない。消えたdirは後から読まれないので、queueの記録に載せる要が無い。

## Alternatives

- **数で保つ**: 動いた量でdirの数が変わるので、忙しい日に新しいものまで消えうる。backups/は非互換の`migrate`のたびに1つで量が動かないので数で保つ。
- **終わったら即消す**: 終わった直後の調べに読めなくなる。
- **eventに記録する**: 消したことを後で読む使い手が無い。

## Consequences

- `planners/`・`plan-reviews/`・`observer/`の大きさは、保持期間のあいだに終わった数で頭打ちになる。
  ただし終わりを記録する前に落ちたobservationのdirと、どの記録も名指さないdirは、決定2により消えずに残る。
- 保持期間を短くしたい・長くしたいqueueは`dagq.toml`で変えられる。表を知らない旧バイナリはその`dagq.toml`を読めないので、表は固定バイナリが対応してから足す。
- goal reviewなど、ここに挙げないjobのdirは今までどおり残る。

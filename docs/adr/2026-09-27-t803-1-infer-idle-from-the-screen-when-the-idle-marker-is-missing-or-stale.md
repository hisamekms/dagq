---
id: adr-t803-1
type: adr
title: idle の印（Stop hook）を主な信号のまま残し、印が無いか最後の入力より古いときだけ画面から idle を推定する
status: superseded
created: 2026-09-27
updated: 2026-09-27
accepted_on: 2026-09-27
superseded_by: adr-t1433-2
superseded_on: 2026-10-03
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - planner
  - session
related:
  - adr-0016
  - adr-0043
  - adr-0071
  - adr-t598-1
  - design-supervisor-lifecycle-receipt-and-session-exit
  - design-supervisor-lifecycle-plan-planners
  - design-supervisor-lifecycle-draft-planners
  - design-provider-lifecycle
---

# ADR-t803-1: idle の印を主な信号のまま残し、印が無いか最後の入力より古いときだけ画面から idle を推定する

> **置き換え済み（2026-10-03）**: このADRの決定は現在有効ではない。現行の決定は[ADR-t1433-2](2026-10-03-t1433-2-abolish-the-interactive-route.md)を読む。

## Context

runtime は session（planner・worker・resume・revise）が turn を閉じたことを、agent の Stop hook が書く idle の印（`idle.json`。[ADR-0016](0016-maintainer-notification-and-compact-output.md)）だけで知る。`/exit` を送るのも、stall の検知（[ADR-0043](0043-detect-stalled-worker-sessions-nudge-once-then-ask.md)）も、待ちの終わり（[ADR-0071](0071-runs-waiting-in-revise-and-resume-leave-the-slot.md)）も、この印を前提にしている。

2026-09-27、host のディスクが満杯になり、Stop hook の書き込みが `Hook Stop (Stop) error: No space left on device` で失敗した（planners/263/claude.log）。runtime が draft 661 のために立てた planner#263 は仕事を終えた後も印が無いまま `state: working` で約 13 時間残り、`/exit` も alert も起きなかった。hook の失敗は Claude Code の debug log にしか残らず、runtime は記録も再試行もしなかった。同じ時間帯に worker の印も書けず、supervisor が動きの無い session を誤認した。印は agent の側の 1 回の書き込みで、失敗すると代わりの信号が無い。

## Decision

1. idle の印を、session が turn を閉じたことの主な信号のまま残す。印が最後の入力より新しい間は、今までどおり印（と印の後に画面が作業中かどうか）で判断し、画面からの推定は使わない。
2. 印が無いとき、または印が session の最後の入力（agent の入力の印 `prompt-submit.json`、supervisor が最後に打った文、session を開いた時刻の遅い方）より古いときだけ、cmux の capture の画面で idle を推定する。推定の条件は、画面で入力欄が入力を受けられ、作業中の表示が無く、ダイアログも無い状態が、同じ transcript のまま、間を空けた 2 回以上の capture で一定時間（設定できる閾値）続いたこと。推定した idle の始まりは、その区間の最初の capture の時刻とする。
3. 画面が読めない（capture の失敗）ときは推定しない。作業中・ダイアログあり・入力欄が無い画面は区間を終わらせる。最後の入力が区間の始まりより後なら区間をやり直す。
4. 推定は provider にも session の種類にも依らない 1 つの共通の判定にし、planner に使う。worker・resume・revise の session への適用は後続の task が同じ判定で行う。
5. 推定で idle にしたときは、session ごと・区間ごとに 1 回 event に残し、印の状態（無い / 古い）と、agent の debug log の末尾に idle の hook の失敗があればその行の抜粋を持たせる。印が書けなかったことを runtime の記録から見えるようにするため。
6. runtime が立てた planner は、推定の idle でも印の idle と同じく `/exit` の対象にする。人が開いた planner には今までどおり `/exit` を送らず、state の表示だけが推定で直る。

## Consequences

- 印が書けない障害（ディスク満杯、hook の timeout など）でも、runtime の planner は仕事を終えれば `/exit` されて行が閉じ、`planners` の state も正しく idle を示す。
- 推定には閾値ぶんの遅れがあり、印があるときより `/exit` が遅れる。印が主な信号なので、正常時の挙動と遅れは変わらない。
- 画面の読み取りは agent の TUI の形に依存する。形が変わって入力欄を読めなくなると、推定は起きない側（今までどおり working）に倒れる。
- 印が古いだけで画面がダイアログを出している session は、以前は idle と判断されることがあったが、今は working のままになる。
- 画面も読めないまま動かない runtime の planner への backstop（時間切れと inbox への通知）は、この決定の外で後続の task が扱う。

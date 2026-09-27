---
id: design-supervisor-lifecycle-integrate-errors
type: design
title: "errorと復旧"
status: current
created: 2026-09-26
updated: 2026-09-27
last_verified: 2026-09-27
scope: runtime
related:
  - design-supervisor-lifecycle
  - adr-0008
  - adr-t768-1
---

# errorと復旧

mainを進める前のGit・ファイル・DBのerror（worktreeがない、mainのcheckoutにローカル変更があって`--ff-only`が失敗する、など）は`abort_integration`で`integration_error`イベントと`last_error`を書き、runを着地開始時のstatus（`awaiting_integration` / `needs_session`）に戻してleaseを解放する。`integrate`は非0で終わり、原因を直して再実行する。検証コマンドがコマンド全体の上限（30分）を過ぎてkillされたものはこのerrorではなく、`timeout`の分類の検証の失敗として扱う（task 639。[`integrate`](integrate.md#integrate)の5）。

検証コマンドの失敗はこのerrorではなく、runの判定になる（[`integrate`](integrate.md#integrate)の5）。どれがworkerに返るかは分類で決まる: コードの分類は`verification_failed`の`needs_session`でresumeする。hostの分類（`disk_full`・`killed`・`timeout`）は同じ試行で1回やり直し、なお落ちれば`integration_held`で人に知らせる（task 639）。落ちたtestが全てnextestの流し直しで通った`flaky`は、resumeせずに`integration_retried`を記録して着地をrunごとに1回だけやり直し、2回目の失敗は`verification_failed`の`needs_session`でresumeする（task 768、[ADR-t768-1](../../adr/2026-09-27-t768-1-rerun-failed-tests-once-and-land-again-on-flaky-only.md)）。やり直しの途中で`integrate`が死んだときも上と同じく`recover`で戻し、次の`integrate`は記録済みの`integration_retried`を見て、もうやり直さない。

`integrate`プロセスが途中で死ぬとrunは`integrating`のまま、leaseはstaleになる。`doctor`が`integrating`のrunをlease付きで報告し、leaseのPIDが死んでheartbeatが30秒以上古ければ`recover RUN_ID`が`awaiting_integration`に戻す（`run_recovered`の`previous_status: integrating`）。次の`integrate`は途中のrebaseをabortしてやり直す。mainを進めた後にDBの更新が失敗した場合はrunを戻さず、error messageに着地commitを含める（[ADR-0008](../../adr/0008-merge-queue-squash-landing.md)のConsequences）。

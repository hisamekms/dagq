---
id: adr-t3807-1
type: adr
title: CIの見張りは`[ci_watch]`が宣言した追加のworkflowの定時と手動の実行も着地先のbranchで読み、落ちた実行をpushのCIと別の種類のfindingにしてruntimeのplannerに直すtaskを作らせ、既に落ちているtestの一覧・赤と緑・claimと着地の保留には効かせない（ADR-t1920-1決定2・4をamends）
status: accepted
created: 2026-10-31
updated: 2026-10-31
accepted_on: 2026-10-31
amends:
  - adr-t1920-1 decision 2
  - adr-t1920-1 decision 4
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - ci
related:
  - adr-t1920-1
  - adr-t2928-1
  - adr-t2268-1
  - design-supervisor-lifecycle-ci-watch
  - design-supervisor-lifecycle-finding-planners
  - design-supervisor-lifecycle-run-environment
---

# ADR-t3807-1: CIの見張りは宣言した定時のworkflowの失敗をfindingにし、一覧と着地には効かせない（ADR-t1920-1決定2・4をamends）

## Context

[ADR-t1920-1](2026-10-06-t1920-1-supervisor-watches-main-ci-keeps-known-failures-and-files-fixes-through-findings.md)の見張りは、`[ci_watch]`の1つのworkflowの着地先のbranchへのpushの実行だけを読む（決定2）。定時の実行（GitHub Actionsの`schedule`）の失敗は、ci-failureのissueかGitHubの画面でしか人に届かず、queueに届かない。

goal 239は、testの本数と秒の予算をCIが毎晩測り、超えればfindingにしてplannerが直すtaskを作り、着地は止めない形を求める（request 194の方針B.4）。人の言葉は「observerのfinding」だが、observerはGitHubを読まず、CIの結果をqueueに届ける経路はsupervisorのCIの見張りだけなので、その汎用の拡張にする。runtimeはこのrepositoryのworkflowの名前もtestの構成も持たない（[ADR-t2268-1](2026-10-12-t2268-1-toolchain-knowledge-behind-declared-profiles.md)）。

ADR-t1920-1は番号付きの決定を8つ持ち、足すのは決定2（何を読むか）と決定4（findingの記録）の範囲なので、amendsにする。

## Decision

1. **projectが宣言したworkflowだけを足して読む。** `[ci_watch]`に追加のworkflowの名前の並びを宣言でき、宣言の無いprojectは今と何も変わらない。runtimeは宣言された名前だけを受け、どのworkflowが何を測るかを知らない。
2. **読む実行**: 宣言したworkflowの、着地先のbranchの、定時（`schedule`）と手動（`workflow_dispatch`）の実行のうち終わったもの。pushの実行と同じ確かめの周回で、作られた順に、実行のIDとattemptごとに1度だけ記録する。宣言の後の最初の確かめは最新の1件だけを読み、過去の履歴を遡らない。遅れて終わった古い実行は記録するが判断を変えない。
3. **失敗はpushのCIと別の種類のfindingにする。** 落ちた（`failure`・`timed_out`）実行は、落ちたjobとstep・実行のURLを根拠に、proposalを求める印付きのfindingを記録する。subjectはworkflowごとに1つで、続けて落ちれば同じfindingを更新する。後の`success`の実行は、そのfindingが`open`でplannerが立っていなければ`resolved`にする。cancelなど成否の決まらない実行は記録だけする。
   - **別の種類（kind）にする理由**: `ci_failure`のkindは、既に落ちているtestの一覧の項目・修正taskのrunで外さない項目（決定5）・改善の上限の外の修理（[ADR-t2928-1](2026-10-20-t2928-1-only-the-ci-watchs-ci-failure-findings-are-repairs-outside-the-improvement-limit.md)）の判定に使われている。同じkindでsubjectだけ変えると、そのどれもがsubjectを見分ける規則を要し、見落とせば定時の失敗が一覧や修理の枠に紛れる。kindを分ければ、既存の判定は1つも変えずに済む。
   - **改善として数える**: 定時のworkflowの失敗（予算の超過・重い繰り返しのtestの揺れなど）は着地先のbranchの壊れではないので、ADR-t2928-1の修理の枠に入れず、改善の上限の中で待つ。この種類も`finding record`は拒み、記録できるのは見張りだけにする（見張りが閉じるfindingに他の役割の行が混ざらないため）。
4. **効かせないもの**: 定時のworkflowの実行は、既に落ちているtestの一覧、赤と緑の状態と転換、着地の検証、claimと着地の保留を変えない。記録はpushの実行の記録と別のeventにし、一覧の畳み込みに入れない。定時のworkflowを読めない（`gh`の失敗、名前の変わったworkflow）ときは見張りの一時の失敗に数えず、logに残して他の宣言したworkflowを読み、次の確かめで読み直す。pushの見張りの答えと保留を、宣言した名前の誤りで揺らさないため。
5. 変えないもの: ADR-t1920-1の他の決定（読む手段が無いときの止め方、一覧の持ち方、修正taskの扱い）と、pushの実行の読み方。

## Alternatives

- **`ci_failure`のままsubjectで分ける**: 3のとおり、一覧・修正taskのrun・修理の枠の判定がsubjectの規則を要する。
- **observerがGitHubを読む**: observerにGitHubを読む手段と認証を足すことになり、見張りと同じ読み方が2つになる。
- **定時の失敗を修理（上限の外）にする**: 着地先のbranchの壊れではなく、修理の枠を予算の超過のような改善が取ると、本当の赤の修理が待つ。
- **workflowを読めないときに見張り全体の一時の失敗に数える**: 宣言の名前の誤りでpushの見張りの失敗の記録と数え方が揺れる。

## Consequences

- このrepositoryで使うには、固定バイナリがこの設定を読めるようになってから`dagq.toml`に宣言を足す（別のtask）。
- 定時の失敗の修正taskは改善の上限で待つことがある。急ぐときは人かinboxが優先度を上げる。
- 設定のkeyと型と既定、eventの種類と欄、findingの種類とsubjectは[CI watch](../design/supervisor-lifecycle/ci-watch.md)の地図から辿るコードのdoc commentが持つ。

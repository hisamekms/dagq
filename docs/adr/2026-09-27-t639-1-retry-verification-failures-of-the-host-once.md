---
id: adr-t639-1
type: adr
title: 着地の検証コマンドの失敗のうちhostの分類（disk_full・killed・timeout）はworkerのsessionをresumeせず同じ試行で1回やり直し、なお落ちればrunを着地待ちに戻して原因付きで人に知らせる
status: accepted
created: 2026-09-27
updated: 2026-09-27
accepted_on: 2026-09-27
owners:
  - hisamekms
tags:
  - runtime
  - integrate
  - verification
related:
  - adr-0023
  - adr-0047
  - adr-0049
  - adr-t598-1
  - design-supervisor-lifecycle-integrate
  - design-supervisor-lifecycle-needs-session
  - design-supervisor-lifecycle-disk-space
  - design-supervisor-lifecycle-stats
---

# ADR-t639-1: 着地の検証コマンドの失敗のうちhostの分類はresumeせず1回やり直し、なお落ちれば人に知らせる

## Context

`integrate`はrebase後に検証コマンドを1回だけ実行し、非0ならrunを`needs_session`にしてsupervisorがworkerのsessionをresumeする（[ADR-0023](0023-verify-once-review-in-supervisor-run-env-graph-and-stats.md)の決定1、ADR-0049の決定1）。goal 37は、コードではなくhostが原因の失敗（ディスク満杯、負荷の下のkillや時間切れ）でも同じくresumeされ、1回に14〜17分のsessionを使っていたことを記録した（notes 6785・7442・8303）。goal 37のacceptanceは「ディスクや負荷の下のtimingのtestで落ちた検証はworkerのsessionをresumeせず、やり直すか原因付きで知らせ、statsかrunの記録に原因が出る」。

task 467は失敗を分類（`domain::verify_failure::classify`）して記録するようにしたが、扱いは分類によらずresumeのままにし、goal 37の後続に回した。また、コマンド全体の上限（30分）で打ち切られた検証は分類なしの着地処理のerrorになっていた。

## Decision

1. **hostの分類を`disk_full`・`killed`・`timeout`の3つとし、それ以外（`build_error`・`lint`・`test_failure`・`format`・`coverage_below`・`unknown`）をコードの分類とする。** コードの分類は今までどおり`needs_session`にしてresumeする。`unknown`はコードの側に置く（原因が分からないものを人に上げ続けないため、workerに見せる）。
2. **hostの分類で落ちたコマンドは、workerのsessionをresumeせず、同じ`integrate`の試行の中で1回だけやり直す。** やり直しも検証の記録として残し、やり直しであることと最初の失敗、別のlogが後から分かるようにする。通れば着地へ進む。やり直しがコードの分類で落ちたらそれをコードの失敗として扱う。`disk_full`のやり直しは、空き容量の確かめ（ADR-0047の決定44）を先に通し、足りなければやり直さない。
3. **やり直してもhostの分類で落ちたら（容量が足りずやり直さなかったときも）、runをresumeせずに着地待ちに戻し、原因（分類、根拠の行、logの場所）付きでinboxに1件知らせる。** resumeの回数は使わない。hostを直した後の着地のやり直しは人の指示（`integrate`）で行い、supervisorはleaseの無い着地待ちのrunを自分では着地させない（同じ負荷の下で重い検証を繰り返さないため）。
4. **コマンド全体の上限で打ち切られた検証は、着地処理のerrorではなく`timeout`の分類の検証の失敗として扱う。**
5. **hostの分類の集合は、人の判断なしに広げない。** 分類を足すと、その失敗はworkerに見せられずに人に上がるので、足すかどうかは人が決める（この決定を置き換えるかamendsするADRで）。分類の印（logのどの行を見るか）の調整はこの決定の外で、designが持つ。

## Alternatives

- **hostの分類もresumeする（これまで）**: sessionを使って直すものが無く、goal 37が数えた時間を空費する。
- **やり直さずにすぐ人に知らせる**: 一過性の負荷やkillの多くは1回のやり直しで通る見込みで、そのたびに人を呼ぶことになる。
- **通るまで何度もやり直す／supervisorが時間をおいて自動でやり直す**: 同じ負荷の下で30分の検証を繰り返し、slotとhostを占有する。回数と間隔の設計が要り、原因が続くときは結局人が直す。
- **`unknown`もhostの側に入れる**: 空のlogなどが人に上がり続け、コードの失敗を見逃す。

## Consequences

- hostが原因の検証の失敗でworkerのsessionが開かなくなり、resumeの回数も使わない。
- やり直しの件数と結果が`stats`に出るので、hostの失敗の頻度と、1回のやり直しで足りているかを前後で比べられる。
- なお落ちたrunは人の`review and integrate`になり、人がhost（空き容量、負荷）を直してから着地をやり直す。
- eventの名前・payloadの欄・logの名前・reasonのcode・statsの欄・testは[`integrate`](../design/supervisor-lifecycle/integrate.md)、[`needs_session`](../design/supervisor-lifecycle/needs-session.md)、[空き容量](../design/supervisor-lifecycle/disk-space.md)、[`stats`](../design/supervisor-lifecycle/stats.md)、[Domain model](../design/domain-model.md)が持つ。

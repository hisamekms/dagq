---
id: adr-t4950-1
type: adr
title: 着地の検証のコマンドが上限で殺されても、ログに時間切れでない落ちたtestがあればhostのtimeoutでなくコードのtest_failureとしてworkerに返す（ADR-t639-1決定4をamends）
status: accepted
created: 2026-11-16
updated: 2026-11-16
accepted_on: 2026-11-16
amends:
  - adr-t639-1 decision 4
owners:
  - hisamekms
tags:
  - runtime
  - integrate
  - verification
related:
  - adr-t639-1
  - adr-t2268-1
  - adr-t3749-1
  - design-supervisor-lifecycle-integrate
  - design-toolchain-profiles
---

# ADR-t4950-1: 上限で殺された着地の検証も、時間切れでない落ちたtestがあればコードの失敗にする

## Context

[ADR-t639-1](2026-09-27-t639-1-retry-verification-failures-of-the-host-once.md)決定4は、コマンド全体の上限で打ち切られた着地の検証を`timeout`（hostの分類）とした。
hostの分類は決定2・3のとおり同じ試行で1回やり直し、なお同じならresumeせずに人を待つ。
task 4849のrun c715915fは、着地の検証が3600秒の上限で2回殺され、hostの失敗として人を待った。
ログでは各回ITが75件落ちていた。新しい認可の規則によるコードの失敗で、落ちたtestのやり直しと重いtestが重なって1時間を超えていた。
人はask 1759で手でsend_backした。時間切れの前にコードが壊れていたのに、hostのせいにして人を待たせたことになる。

## Decision

1. **上限で殺された着地の検証のコマンドも、ログを宣言したtoolchainのprofileとprojectの印で読み、時間切れでない落ちたtestが1件以上あれば、コードの分類の`test_failure`にする。**
   - 時間切れでない落ちたtestとは、profileか印が落ちたと名指したtestから、どこかの行で時間切れかkillの印と一緒に名指されたものと、道具の流し直しで通ったものを除いたもの。
   - コードの分類なので、やり直さず`integration_held`にもせず、今までどおり`needs_session`にしてworkerのsessionをresumeする。
   - 証拠は最初の落ちたtestの行にする。落ちたtestの名前（件数の上限まで、残りは件数）と、上限で殺されたこととその秒数は、検証のコマンドの記録とworkerへの知らせに残す。
2. **落ちたtestが無い、時間切れかkillのtestだけ、profileを宣言していない、どの宣言もログを読めないとき、ログに容量の不足・道具のkill・testを名指さないhostの印があるときは、今どおり`timeout`にする。** hostの原因はbuildとtestの失敗も残すので、`classify`と同じくhostの印を先に見る。 決定1〜3のhostの分類と1回のやり直しはそのまま働く。
3. **やり直しの試行も同じに読む。** 1回目が`timeout`でやり直しのログに時間切れでない落ちたtestがあれば、やり直しのコードの失敗としてworkerに返す（ADR-t639-1決定2）。
4. **判定はruntimeの共通の分類にRustやnextestの文言を書かず、profileが読む落ちたtestと時間切れの印だけで行う。** dagqは外部のprojectでも動くため（[ADR-t2268-1](2026-10-12-t2268-1-toolchain-knowledge-behind-declared-profiles.md)）。
5. **reviewのプログラムの時間切れ（[ADR-t3749-1](2026-10-31-t3749-1-integrate-runs-the-review-programs-of-the-landing-diff-before-its-commands.md)決定4）とlanding recheckの扱いはこの決定の外で、変えない。**

## Alternatives

- **上限を延ばす**: 着地の数の上限を下げ、コードの失敗を待つ時間も延びる。
- **上限で殺されたら常にresumeする**: 負荷で遅れただけの検証でもsessionを開き、ADR-t639-1が避けた空費に戻る。
- **時間切れのtestも含めて落ちたtestがあればコードの失敗にする**: 時間切れのtestは負荷のせいのことが多く、hostの分類と一貫しない。
- **落ちたtestの割合や件数のしきい値で決める**: 根拠のある値が無く、1件でも時間切れでない落ちたtestがあればworkerが直すものがある。

## Consequences

- 時間切れの前にコードが壊れていた着地の検証は、人を待たずにworkerに返り、workerは落ちたtestの名前を知らせから読める。
- 落ちたtestを読めないprojectと、時間切れだけのコマンドの扱いは変わらない。
- profileが印を誤って読めば、hostの失敗がworkerに返りうる。印は今の`test_failure`と同じものなので、誤りの範囲は今の分類と同じ。
- 判定の関数・欄の名前・testは[`integrate`](../design/supervisor-lifecycle/integrate.md)と`domain::verify_failure`のdoc commentが持つ。

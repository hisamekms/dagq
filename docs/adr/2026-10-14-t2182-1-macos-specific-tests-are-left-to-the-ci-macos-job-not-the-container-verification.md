---
id: adr-t2182-1
type: adr
title: このrepositoryのmacOSに固有のtestは、containerの着地の検証でもhostでも流さず、mainのCIのmacOSのjobに任せ、hostの絶対pathを名指すverifyはimageの道具の名前で書く
status: accepted
created: 2026-10-14
updated: 2026-10-14
accepted_on: 2026-10-14
owners:
  - hisamekms
tags:
  - testing
  - security
  - ci
related:
  - adr-t2113-3
  - adr-t2257-1
  - adr-t1925-1
  - adr-t1920-1
  - adr-t2114-4
  - development-testing
  - development-local-checks
  - development-task-registration
  - design-container-runtime
---

# ADR-t2182-1: このrepositoryのmacOSに固有のtestはmainのCIのmacOSのjobに任せる

## Context

[ADR-t2113-3](2026-10-08-t2113-3-only-workers-resume-and-integrate-verification-run-in-containers.md)決定1は、integrateのverificationをrunごとのcontainerに移した。
containerはdagq専用のPodman machineのLinuxなので、`#[cfg(target_os = "macos")]`で分けたtest（launchd・実物のcmuxなど、[testの制約](../development/testing.md)の「macOSに固有のtest」）はbuildされず、着地の検証で流れない。
ADR-t2113-3のConsequencesは、それを流す場所を実装のgoalに回した。
[ADR-t2257-1](2026-10-12-t2257-1-container-runtime-takes-project-and-host-values-from-config.md)決定4は、runtimeが「macOSに固有のtest」を知らず、hostで流すのは`dagq.toml`が宣言したものだけとし、このrepositoryの場所は別のADRが決めるとした。
mainのCIは`checks`（macOS）でtestを全部流し、着地の検証の後の最終関門である（[ADR-t1925-1](2026-10-07-t1925-1-landing-verifies-unit-tests-and-selected-integration-tests-and-ci-is-the-final-gate.md)）。
mainのCIで落ちたtestは、CIの見張りが修正のtaskにする（[ADR-t1920-1](2026-10-06-t1920-1-supervisor-watches-main-ci-keeps-known-failures-and-files-fixes-through-findings.md)）。

taskのverifyには、hostの絶対pathの道具を名指すものがある（[taskの登録](../development/task-registration.md)の「推奨の組み合わせ」のconfigの行）。
containerにはhostのpathが無い。

## Decision

1. **macOSに固有のtestは、着地の検証で流さず、mainのCIのmacOSのjobに任せる。**
   containerの着地の検証では、Linuxでbuildされるtestだけが流れる。
   macOSに固有のtestをhostで流し直すことはせず、`dagq.toml`の準備のコマンドにもreviewの後にhostで流すコマンドにも入れない。
   mainのCIの`checks`（macOS）が流し、落ちればCIの見張りが修正のtaskにする。
   これはこのrepositoryの決定で、runtimeにmacOSのtestの扱いや設定を足さない（ADR-t2257-1決定4）。
2. **workerの手元のtestも、containerではLinuxになる。**
   containerのworker（resumeを含む）が手元で流すtestもLinuxでbuildされ、macOSに固有のtestは流れない。
   macOSに固有のtestを変えたworkerは、手元で流せなかったことをreceiptに書き、確かめはmainのCIの`checks`に任せる。
3. **hostの絶対pathを名指すverifyは、containerではimageの道具の名前で書く。**
   verificationがcontainerのときのverifyは、hostの絶対pathではなく、imageの`PATH`にある道具の名前で名指す。
   要る道具はprojectのimage（`dagq.toml`の`[container.image]`が名指すContainerfile）に入れる。
   そのtaskのverificationだけをhostのruntimeに回すことはしない。

## Alternatives

- **macOSに固有のtestをhostで流し直す**（reviewのpassの後のe2eと同じ形で、制御側が`dagq.toml`の宣言でhostに流す）: workerが書いたコードをhostのユーザー権限・秘密情報・無制限のネットワークで走らせ、containerの隔離を破る。
  e2eはhostに残す例外だが、macOSに固有のtestまで広げると、隔離の外で走るコードが増える。
- **macOSのcontainerかVMで流す**: dagq専用のmachineはLinuxで、macOSのguestを動かす手段と資源がhostに無い。
- **hostの絶対pathと同じpathに道具をimageに入れる**: pathは人のhostのhomeとmiseの版に依り、別のhostのimageで合わない。
- **hostの絶対pathのverifyだけをhostのruntimeに回す**: 決定1の理由と同じく、そのtaskのコードがhostで走り、隔離を破る。

## Consequences

- macOSに固有のtestを壊す変更は、着地の後にmainのCIで見つかる。
  mainが一時的に赤くなりうることは、ADR-t1925-1が受け入れた事故の範囲で、直しはCIの見張りのtaskが受け持つ。
- `dagq.toml`のverificationをcontainerに切り替えるときは、登録済みのtaskのverifyのうちhostの絶対pathを名指すものを書き直し、要る道具をprojectのimageに入れておく。
- verificationがhostのままの間は、今のverifyと着地の検証のままで変わらない。

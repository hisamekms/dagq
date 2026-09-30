---
id: adr-t1162-1
type: adr
title: 固定バイナリを入れ替える前のe2eの関門で、上限つきで待ってもpodmanに繋がらないときだけpodmanに頼るe2eを流さずに残りで判定し、流さなかったことを記録してinboxに届ける（ADR-t963-1決定1をamends）
status: accepted
created: 2026-09-30
updated: 2026-09-30
accepted_on: 2026-09-30
amends:
  - adr-t963-1 decision 1
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - testing
  - operations
related:
  - adr-t963-1
  - adr-t827-3
  - adr-t827-4
  - adr-0073
  - design-supervisor-lifecycle-auto-update
  - design-supervisor-lifecycle-install
  - design-broker
---

# ADR-t1162-1: 固定バイナリを入れ替える前のe2eの関門で、上限つきで待ってもpodmanに繋がらないときだけpodmanに頼るe2eを流さずに残りで判定し、流さなかったことを記録してinboxに届ける（ADR-t963-1決定1をamends）

## Context

[ADR-t963-1](2026-09-29-t963-1-e2e-required-by-diff-and-run-in-full-before-auto-update.md)決定1は、固定バイナリを入れ替える前に全部のe2eを流し、1件でも落ちれば入れ替えず、cmuxが使えずe2eを流せないときも通さないと決めた。全部のe2eには、cmuxに加えてdagqのPodman machineを要るbrokerのe2e（[ADR-t827-4](2026-09-28-t827-4-worker-mcp-tools-audit-mode-and-relations.md)決定1）が入っている。

2026-09-30の05:12と06:45、自動更新の関門でbrokerのe2eだけが、podmanの接続の失敗（`ssh: handshake failed ... connection reset by peer`と、imageのbuildの途中の`server probably quit: unexpected EOF`）で落ち、runtime全体の自動更新が2回見送られた（ask 227・231）。同じ時刻に、workerのe2eのbrokerのtestも同じdagqのmachineを使っていた。machineのhost全体のlock（[ADR-t827-3](2026-09-28-t827-3-supervisor-runs-the-broker-container-on-a-dedicated-podman-machine.md)決定5）が`XDG_DATA_HOME`の下にあり、e2eのfixtureは`XDG_DATA_HOME`を使い捨ての場所に向けるので、2つのe2eが互いを待たず、一方の後始末（`broker stop`）が他方のbuildの途中でmachineを止めていた。lockの場所の誤りと、接続が一時的に切れたときの再試行はこの ADR と同じtaskで直す（場所と回数は[Broker](../design/broker.md)）。

それでも、podmanのmachineはhostの資源（人の別のmachine、VMの起動、hostの高負荷）に左右され、runtimeのcommitが直せない理由で繋がらないことが残る。brokerはgoal 58の途中で、本番のrunはまだbrokerを使っていない。podmanの一時的な状態でruntime全体の自動更新が止まる代償の方が、brokerのe2eを1回流さない代償より大きい。

## Decision

1. **関門は、e2eを流す前にdagqのmachineを用意して、podmanの接続が答えることを確かめる。** 接続が一時的に切れていれば上限つきで待つ。上限つきで待ってもpodmanに繋がらないとき（podmanが無い、人の別のmachineが動いている、machineの接続が答えない）だけ、podmanに頼るe2e（brokerのe2e）を落ちとせずに流さず、残りのe2eで判定する。残りが全部通れば入れ替える。
2. **流さなかったe2eを黙って通さない。** 流さなかったtestと理由を、関門のlog、e2eが通ったeventと入れ替えの報告（自動更新の`update_installed`と、人の`install`の結果）に残し、`update_installed`をinboxが人に伝えるときに一緒に届ける。
3. **それ以外は ADR-t963-1 決定1のまま。** cmuxが使えずe2eを流せないときは今までどおり通さない。podmanに繋がって流したbrokerのe2eが落ちたときも、今までどおり落ちとして入れ替えない。

## Alternatives

- **今のまま（podmanに繋がらなくても全部流して落ちとする）**: runtimeのcommitが直せないhostの状態で自動更新が止まり、そのたびに人が`retry` / `skip`を答える。本番のrunがbrokerを使っていない今は代償が見合わない。
- **brokerのe2eを関門から外す**: 繋がるときまで流さなくなり、brokerとruntimeの組み合わせを壊す変更が本番のバイナリに入る。繋がるときは流す方がよい。
- **machineを起動し直せば必ず直るので、流さない扱いは要らない**: 調べた範囲では、machineの接続が切れた原因は別のe2eの後始末がmachineを止めたことで、起動し直しでは防げない。lockの場所を直した後も、人の別のmachineやVMの起動の失敗は起動し直しでは直らない。
- **流さなかったときに`update_failed`のaskを開いて人に決めさせる**: 人の答えを待つ間は自動更新が止まり、今のaskと同じ代償になる。記録してinboxに知らせれば、人は後から気づける。

## Consequences

- podmanに繋がらない間の入れ替えでは、brokerの経路は実物で確かめないまま本番に入る。流さなかったことは`update_installed`とそのinboxへの知らせに出るので、続くときは人とplannerがhostのpodmanを直す。
- 本番のrunがbrokerを使い始めたら（goal 58の後段）、brokerのe2eを流さずに入れ替えてよいかをこのADRを直して決め直す。
- 待つ回数と秒数、podmanに頼るe2eの選び方（testの名前のfilter）、eventの欄名、lockの場所は[Auto-update](../design/supervisor-lifecycle/auto-update.md)・[install](../design/supervisor-lifecycle/install.md)・[Broker](../design/broker.md)が持つ。

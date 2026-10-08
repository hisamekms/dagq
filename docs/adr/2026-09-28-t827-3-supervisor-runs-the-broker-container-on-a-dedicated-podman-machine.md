---
id: adr-t827-3
type: adr
title: brokerのcontainerはqueueごとに1つで、supervisorが起動・health・停止の責任を持ち、dagq専用の最小のPodman machineを必要なときにruntimeが冪等にinit・startして、使われなくなれば止め、人の既定のmachineには触らない
status: superseded
created: 2026-09-28
updated: 2026-09-28
accepted_on: 2026-09-28
superseded_by: adr-t2113-1
superseded_on: 2026-10-08
owners:
  - hisamekms
tags:
  - runtime
  - broker
  - operations
related:
  - adr-t827-1
  - adr-t827-2
  - adr-t827-4
  - adr-0011
  - adr-0047
  - design-broker
---

# ADR-t827-3: brokerのcontainerはqueueごとに1つで、supervisorが起動・health・停止の責任を持ち、dagq専用の最小のPodman machineを必要なときにruntimeが冪等にinit・startして、使われなくなれば止め、人の既定のmachineには触らない

> **置き換え済み（2026-10-08）**: このADRの決定は現在有効ではない。現行の決定は[ADR-t2113-1](2026-10-08-t2113-1-remove-the-resource-broker.md)を読む。

## Context

brokerはPodmanのcontainerで常駐する（人の決定、2026-09-27）。人は2026-09-28にhostへpodmanを入れた（runのPATHにある）がmachineはまだ無く、machineは必要になったとき（brokerを起動するとき、podmanを要る`#[ignore]`のtestとスモーク）にruntimeが初期化して起動すると決めた。hostは8コア / 16GBで資源が不足しがちなので、machineは最小の資源にする（目安: CPU 1・メモリ1 GiB前後・disk 10 GiB前後。実際の値はimageのbuildとbrokerの実行が通る最小を測って決める）。macOSのPodmanは同時に1つのmachineしか動かせない。Linux向けのRustのbuildは重い。brokerにPodman / Dockerのsocketを渡さず、brokerにworkerのcontainerを作らせない（goal 58のconstraints）。

## Decision

1. **queueごとに1つのcontainer。** mount（[ADR-t827-2](2026-09-28-t827-2-broker-transport-run-token-and-workspace-confinement.md)決定5）がqueueのrunsとrepositoryのgitの共通dirで決まるので、brokerのcontainerはqueueごとに1つにし、portもqueueごとに持つ。
2. **supervisorが責任を持つ。** brokerのmodeが`disabled`でないqueueでは、supervisorが起動時とclaimの前にbrokerを冪等に用意し（machine・image・container・health）、自分のtickでhealthを見る。`down`はdrainの後にbrokerのcontainerを止める。brokerの用意（特に長いimageのbuild）はclaimを待たせず、終わるまでの間は使えないものとして扱う。exec（installとauto-update）の引き継ぎではcontainerを止めない。build識別子が変わっても、古いbrokerは有効なtokenを持つrunが残る間はそのrunのために動かし続け（そのrunのworkerは古いclientを持っているので、同じ版どうしで話し続ける）、新しいclaimには新しいbrokerが用意できるまでtokenを出さない。有効なtokenが無くなってから新しいimageで起動し直す（走っているexecを途中で殺さないため）。人とtestとスモークには同じ処理を呼ぶ`dagq`の管理のコマンドを用意する。`up`はpreflightでpodmanの有無を確かめる。brokerはworkerの起動に要る前提で、`dagq`の外で動く常駐のserviceにはしない（supervisorが居ないときに動いても使う者がいない）。
3. **不健康なときの通知。** healthが続けて失敗したら、supervisorはinbox宛てのattentionで人に知らせる。`preferred`ではclaimを止めず、workerはbrokerの道具なしで動く（[ADR-t827-4](2026-09-28-t827-4-worker-mcp-tools-audit-mode-and-relations.md)）。runtimeが決まった規則で直せるもの（止まったcontainerの起動し直し）はADR-0047の1層目として自動で直して記録する。
4. **dagq専用のmachine。** dagqは専用の名前のmachineだけを使い、podmanのコマンドはそのmachineの接続を明示して打つ。人の既定のmachineと既定の接続は変えず、作らず、止めない。人のmachineが動いていて専用のmachineを起動できないときは、brokerは使えないものとして扱い、人に知らせる（人のmachineを止めない）。
5. **init・start・stopは冪等でlockする。** machineが無ければ最小の資源でinitし、止まっていればstartする。これをbrokerの起動の前とpodmanを要るtestとスモークの前に行い、host全体のlockで直列にして、並行するtestやrunが同時に呼んでも壊れないようにする。queueのどのbrokerも要らなくなったら（最後のbrokerのcontainerを止めたとき）machineを止める。止める判定と停止も同じlockの中で行う。podmanを要るtestとスモークも、終わりに同じ判定で止める。
6. **machineのvolume。** macOSのPodman machineは既定でhostの`$HOME`をVMにmountする。queue dirとrepositoryの場所はqueueごとに違い、machineのvolumeはinitのときにしか決められないので、Phase 1は既定のvolumeのままにし、containerのmountで絞る。containerから抜け出せば`~/.ssh`などが見えることを既知の制限として記録し、workerのcontainer化の段で見直す。
7. **資源は最小を測って決める。** machineはCPU 1・メモリ1 GiB・disk 10 GiBから始め、imageのbuildとbrokerの実行が通らなければ、通る最小まで上げる。決めた値はdesignに書き、host.tomlで上書きできる。containerはmemory・cpu・pidsの上限を持ち、非rootで、root filesystemを読み取り専用にし、capabilityを落とし、特権の昇格を禁じる。
8. **imageは最小で、buildは作り直しのときだけ。** imageはContainerfileのmulti-stageで作り、実行のimageはbrokerとgitとexecのallowlistに要る最小の道具だけを持つ。Rustのbuildはmachineの中のbuildのstageで、tagが無いときだけ（[ADR-t827-1](2026-09-28-t827-1-broker-crates-binaries-and-version-alignment.md)決定6）、buildの並列度を絞って行う。hostでLinux向けに作る案はhostに無い道具を要るので採らない。
9. **`process.exec`は軽いものに限る。** containerの上限の中で動くので、execが走らせてよいのは軽いコマンドで、言語のtoolchain（cargoなど）はimageに入れない。使い捨てのrepositoryの代表のtaskは軽いもの（shellの道具とファイルの読み書き）にする。

machineとcontainerとimageの名前、資源と上限の値、lockの場所、healthの間隔と回数、attentionとeventのkind、管理のコマンドの綴り、Containerfileの形は[Broker](../design/broker.md)に書く。

## Alternatives

- **`up`だけがbrokerを起こす**: supervisorのexecの引き継ぎやcontainerの停止に追従できず、healthを見る者がいない。
- **launchdなどの別のserviceにする**: supervisorが居ないときに動いても使う者がおらず、in-cmux modeの運用（ADR-0011）と責任が分かれる。
- **人の既定のmachineを使う**: 人の資源と設定を変えてしまい、人が止めるとbrokerが止まる。
- **machineを常に動かしておく**: 資源の不足しがちなhostで、使わない間もメモリを取る。
- **hostでLinux向けにcross buildする**: cross用のtoolchainやlinkerをhostに入れる必要があり、workerはhostにツールを入れない。
- **toolchainを含む大きなimage**: 最小のmachineでbuildも実行も通らず、Phase 1の契約の証明には要らない。

## Consequences

- supervisorの起動とclaimの前にpodmanの呼び出しが増える（disabledのqueueでは何もしない）。
- 最初のbrokerの起動はmachineのinitとimageのbuildで長くかかる。
- 人が既定のmachineを使っている間は、dagqのbrokerは起動できない。
- auto-updateの後、古いbrokerと新しいbrokerの切り替えは古いtokenのrunが終わるまで遅れ、その間の新しいrunはbrokerの道具なしで動く。

---
id: adr-t1487-2
type: adr
title: Spikeはworkerの総枠（parallel）を実装と共有し、その中で同時のSpikeの数を上限で絞り、枠を予約せず実行中のrunを止めず、区分をまたぐagingで両方の区分の餓死を防ぎ、控えは区分に依らず全てに効かせ、candidatesとgraphを実際のclaimと同じ規則で並べる（ADR-0049決定4、ADR-t1639-1決定8をamends）
status: accepted
created: 2026-10-04
updated: 2026-10-04
accepted_on: 2026-10-04
amends:
  - adr-0049 decision 4
  - adr-t1639-1 decision 8
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - capacity
related:
  - adr-0049
  - adr-0080
  - adr-t1479-1
  - adr-t1484-1
  - adr-t1591-1
  - adr-t1639-1
  - adr-t1487-1
  - design-supervisor-lifecycle-supervise
  - design-supervisor-lifecycle-claim-defer
  - design-supervisor-lifecycle-claim-hold
---

# ADR-t1487-2: Spikeはworkerの総枠（parallel）を実装と共有し、その中で同時のSpikeの数を上限で絞り、枠を予約せず実行中のrunを止めず、区分をまたぐagingで両方の区分の餓死を防ぎ、控えは区分に依らず全てに効かせ、candidatesとgraphを実際のclaimと同じ規則で並べる（ADR-0049決定4、ADR-t1639-1決定8をamends）

## Context

[ADR-t1487-1](2026-10-04-t1487-1-spike-is-an-execution-class-with-a-judged-result-and-a-durable-replanning-request.md)でtaskに実行区分（`implementation` / `spike`）を足した。claimの順は「効く優先度の降順 → 解放数の降順 → IDの昇順」（[ADR-0049](0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)決定4、[ADR-t1639-1](2026-10-04-t1639-1-goal-priority-is-the-source-tasks-inherit-and-goals-carry-tags.md)決定8）で、区分を見ない。このままでは、Spikeが並ぶと実装の枠を埋め、実装が並び続けるとSpikeがclaimされない。Spike専用の枠を作るとSpikeが無いときに枠が遊び、`parallel`と`[run.env]`の並列度の前提（同時に走る重いbuildの数）が崩れる。claimの関門（queue全体のloadとディスクの控え、claimの間隔（[ADR-t1479-1](2026-10-04-t1479-1-space-new-claims-while-the-load-hold-is-on.md)）、hotspotの控え（[ADR-0080](0080-supervisor-rereads-conflicts-config.md)、[ADR-t1484-1](2026-10-04-t1484-1-runs-waiting-only-for-a-person-stop-holding-claims-past-a-grace.md)）、providerの控え、着地待ちの軽い枠（[ADR-t1591-1](2026-10-04-t1591-1-landing-queue-leaves-room-for-light-changes.md)））はすでにある。

## Decision

1. **Spikeは総枠の中で上限を持つ。** Spikeのrunは`parallel`に上乗せせず、同じslotの数え方で数える（人の答えを待ってslotの外にいるrunは数えない）。同時に走るSpikeの数を`[supervisor]`の上限で絞り、値はrepositoryの`dagq.toml`に置き、runtimeはrepositoryの値を既定として持たない。設定が無ければSpikeの上限は無い（今の振る舞い）。枠を予約しないので、Spikeが無ければ実装が全ての枠を使う。上限に達しても実行中のrunは止めず、Spikeのために実装のrunを止めることもない。上限は正の整数で、`parallel`以上の値（`parallel` 1で上限1を含む）は区分の上限としては効かず、総枠だけが効く（その場合も決定2のagingが両方の区分の餓死を防ぐ）。着地待ちが空けた軽い枠のclaimにも同じ上限が効く。
2. **区分をまたぐagingで餓死を防ぐ。** 各taskの待ちは、claimできる候補になった時刻（readyで依存が全て解けた時刻。queueの記録から求めるので再起動・execの引き継ぎで数え直さない）から測る。claimできる候補の順（決定3(c)）の先頭と違う区分に、待ちが閾値を超えた候補があれば、その区分の候補のうち順で最初に来る閾値超えのものを先頭に入れる。agingは区分の間でだけ順を入れ替え、同じ区分の中の順は今のまま（候補は1つに決まる）。待ちは最後に候補になった時刻から測り、reopen・失敗の後の戻り・依存の追加で候補から外れて戻ったら測り直す。控えや上限で除かれていた時間は待ちに数える。閾値は`[supervisor]`の設定で、agingを切る値は持たない（餓死の防止を設定で外せないように。既定はruntimeの汎用の値で、repositoryの値を埋め込まない）。Spikeの上限（決定1）が新しいclaimでは実装の枠を`parallel`から上限を引いた数だけ残し（slotの外で待っていたSpikeが戻るときは一時的に上限を超えうるが、その間は新しいSpikeをclaimしない）、agingが実装の並びの中のSpikeを上限の内で前に出す。上限が`parallel`以上で実装が待ち続けるときは、同じagingが実装を前に出す。
3. **関門と順の関係。** 1つのclaimを次の順で決める。(a) queue全体の控え（ログイン切れ・利用上限・ディスク・load）とclaimの間隔と空きslot（軽い枠を含む）が許すか。(b) candidates（readyで依存が解けたtask）から、claimできないtaskを除く: providerの控え、hotspotの控え、調査中の計画の上限（ADR-t1487-1決定7）、Spikeの上限、軽い枠ではADR-t1591-1の条件。(c) 残りを効く優先度の降順 → 解放数の降順 → IDの昇順に並べ、決定2のagingを当てる。ただしagingはinterruptの候補より前には入れない。控えは区分に依らず全てのtaskに効き、Spikeも迂回しない（hotspotの控えをinterruptが越える既存の例外は、区分に依らず同じに効く）。interruptはSpikeの上限と調査中の計画の上限を越えない。interruptをSpikeを急がせる代わりに使わない（Spikeを待たせないのはagingの役目）。agingは(b)の後の候補にだけ当てるので、控えや上限で除かれたtaskは前に出さず（除かれていた時間も待ちには数える）、空いた枠は次のclaimできるtaskに回す（claimできないtaskのために枠を空けて待たない）。人のpriority・効く優先度（inherited）・解放数の意味は変えない。
4. **candidatesとgraphは実際のclaimと同じ規則で並べる。** candidatesとgraphは(b)と(c)を同じ関数で求めて並べ、各taskの区分と、上限（Spikeの上限・調査中の計画の上限）で控えた理由とagingで前に出たことを出す。graphは今の順を同じ規則で示し、過去のclaimの理由はclaimの記録（既存のclaimのevent）に区分とagingで前に出たかを足して残す。新しいeventのkindは足さない。

ADR-0049決定4のclaimの順（ADR-t1639-1決定8で「効く優先度の降順 → 解放数の降順 → IDの昇順」に改めたもの。両方をamends）に、区分をまたぐaging（決定2）と、区分と調査の上限による除外（決定3(b)）を足す。設定の名前・既定値・閾値の値・表示の欄・eventのkindは`docs/design/`に書く。

## Alternatives

- **Spike専用の枠を予約する（総枠に上乗せ、または総枠から切り出す）**: 上乗せは重いbuildの同時数の前提を崩し、切り出しはSpikeが無いときに枠が遊ぶ。
- **交互にclaimする**: 実装とSpikeの数の比が偏ると、少ない側を無理に前に出すか、交互の約束が空振りして枠が待つ。
- **比率で配る**: 比率の値の決め方に根拠が無く、総枠が小さいと端数で偏る。実際には上限とagingで足りる。
- **区分ごとの別の待ち行列**: 行列の間の選び方がまた要り、優先度・解放数の比べが行列をまたげなくなる。
- **全てのtaskにagingを当てる**: 人のpriorityと効く優先度の意味を変える。区分の間だけで餓死は防げる。
- **Spikeを自動でinterruptにする・interruptでSpikeを急がせる**: interruptは人の割り込みの印で、上限と控えを越える例外を広げる。
- **agingで先頭になったSpikeが控えで取れないとき、枠を空けて待つ**: 予約と同じく枠が遊び、控えが解けるまで実装も止まる。
- **上限を`parallel`以上にしたら拒む**: `parallel`は各passで読み直して変わりうるので、設定の組で拒むと起動と読み直しが壊れる。効かないだけにする。

## Consequences

- Spikeが無ければ今と同じに振る舞い、Spikeが並んでも実装は上限の外の枠で進む。どちらの区分も閾値を超えて待てば次の空き枠の先頭に入る。ただしagingはinterruptより前に入らないので、interruptが途切れず並ぶあいだは餓死の防止は効かない（interruptは人の割り込みとしてそれを許す）。
- 公平の規則は、候補の時刻・区分・上限・閾値から決まるので、integration testで決定的に検査できる。
- claimの関門が1つ増える（Spikeの上限と調査中の計画の上限）。表示とclaimの不一致を避けるため、candidates・graph・claimが同じ判定を使う実装の制約がつく。

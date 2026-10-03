---
id: adr-t1479-1
type: adr
title: loadの保留が有効なsupervisorは新しいclaimの間を空け、間隔をqueueの最新のclaimから測り、間隔が過ぎたpassでloadを判定し直してから次をclaimする
status: accepted
created: 2026-10-04
updated: 2026-10-04
accepted_on: 2026-10-04
owners:
  - hisamekms
tags:
  - supervisor
  - capacity
related:
  - adr-0049
  - design-supervisor-lifecycle-claim-hold
  - design-supervisor-lifecycle-run-environment
  - design-supervisor-lifecycle-status
---

# ADR-t1479-1: loadの保留が有効なsupervisorは新しいclaimの間を空け、間隔をqueueの最新のclaimから測り、間隔が過ぎたpassでloadを判定し直してから次をclaimする

## Context

supervisorは新しいclaimの前にhostの1分のload averageを`--max-load`と比べ、越えていればそのpassのclaimを控える（task 327、[claimを控える](../design/supervisor-lifecycle/claim-hold.md)）。判定はpassごとに1回で、控えなければ空いたslotの数だけ続けてclaimする。claimの時点の1分のloadは直前にclaimしたrunのbuildをまだ含まないので、loadの低い瞬間に2〜3本を数秒のうちにclaimすると、数分後にloadが`--max-load`を大きく越える（finding 36）。loadの保留は「claimの時点のload」しか見ていない。

記録（固定バイナリのevents、queueのdirの`host/metrics-*.csv`）では、2026-10-01〜02に1本だけのclaimの後6分のload1の最大の中央値は17.7、10秒以内に2本続けたclaimでは26.9で、claimから約2分でcargoとrustcのCPUがload1に出る。

## Decision

1. **loadの保留が有効なsupervisorは新しいclaimの間を空ける。** 新しいclaimの後、決まった間隔が過ぎるまで次の新しいclaimをせず、間隔が過ぎたpassでloadを判定し直してからclaimする。1つのpassで新しくclaimするのは1本で、空いたslotが複数あっても続けて埋めない。間隔のための待ちはloadの保留（控え）とは別のもので、控えとしては記録しない。
2. **間隔はqueueの最新のclaimから測る。** どのsupervisorのclaimかを問わず、queueに記録された最新のclaimの時刻から数える。supervisorの起動し直し・execの引き継ぎ・同じqueueの別のsupervisorのclaimで、間隔が始めから数え直しにならない。
3. **loadの保留を切ったsupervisorは間を空けない。** `--max-load`を切ったsupervisor（libraryの既定を含む）と、間隔を0にしたsupervisorは、今までどおり1つのpassで空いたslotを埋める。
4. **対象は新しいclaimだけ。** parkしたrunのresume・人の待ちからの戻り・着地・headless jobは変えない。

間隔の値・設定の置き場所と名前・既定値の根拠・記録の欄は[claimを控える](../design/supervisor-lifecycle/claim-hold.md)が持つ（ADR-t598-1決定3）。

## Alternatives

- **`--max-load`を下げる、またはclaimごとにloadの余裕（headroom）を見込む**: いつでも並列を減らすので、スループットとの兼ね合いを人が決めることになる。続けてのclaimだけが問題で、1つのslotが空くたびに1本claimする普段の流れは変えたくない。
- **間隔をsupervisorのprocessの中だけで測る**: 起動し直しや引き継ぎのたびに数え直しになり、同じqueueの別のsupervisorのclaimも見えない。queueの記録から測れば、どのprocessでも同じ答えになる。
- **間隔の待ちを控え（`claim_held`）として記録する**: 控えの数え方（observerとstatsのloadの控え）に混ざり、loadが高くて控えた時間と区別できなくなる。
- **workerの手元のtestの並列度を分ける・loadの高いあいだ重いtestを待たせる**: goal 36の測定が決め直すもので、この決定の範囲の外。

## Consequences

- 3つのslotが同時に空いたとき、2本目・3本目のclaimは間隔の分ずつ遅れる。1つのslotが空くたびに1本claimする流れはほとんど変わらない。
- 次のclaimは直前のrunのloadが1分のload averageに出た後に判定されるので、続けてのclaimによる`--max-load`の大きな越えが減る見込み。効果は`stats`の`claim_holds`とclaim後6分のload1の最大を前後で比べて見る。
- `--once`のsupervisorは、1本claimしたrunが終わって次のclaimが間隔を待つときに終わる（控えているpassで終わるのと同じ扱い）。
- e2eと統合testで複数のtaskを続けてclaimさせるものは、loadの保留を切るか間隔を0にする。

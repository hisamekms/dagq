---
id: adr-t2105-1
type: adr
title: e2eの関門で、cmuxが答えないときだけ実cmuxを要るe2eを流さずに残りで判定し、流さなかったことを記録してinboxに届け、cmuxの後始末は次の関門に残す（ADR-t963-1決定1、ADR-t1162-1決定3、ADR-t1233-2決定2・3をamends）
status: superseded
created: 2026-10-08
updated: 2026-10-08
accepted_on: 2026-10-08
superseded_by: adr-t2159-1
superseded_on: 2026-10-09
amends:
  - adr-t963-1 decision 1
  - adr-t1162-1 decision 3
  - adr-t1233-2 decision 2
  - adr-t1233-2 decision 3
amended_by:
  - adr-t2125-1
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - testing
  - cmux
related:
  - adr-t963-1
  - adr-t1162-1
  - adr-t1233-2
  - adr-t1433-1
  - adr-t1433-4
  - design-supervisor-lifecycle-validation
  - design-supervisor-lifecycle-auto-update
  - design-supervisor-lifecycle-install
---

# ADR-t2105-1: e2eの関門で、cmuxが答えないときだけ実cmuxを要るe2eを流さずに残りで判定し、流さなかったことを記録してinboxに届け、cmuxの後始末は次の関門に残す

> **置き換え済み（2026-10-09）**: このADRの決定は現在有効ではない。現行の決定は[ADR-t2159-1](2026-10-09-t2159-1-dagq-does-not-use-cmux-and-the-person-opens-the-inbox.md)を読む。

## Context

e2eの関門は2つある。固定バイナリを入れ替える前の自動更新と`install`の関門（[ADR-t963-1](2026-09-29-t963-1-e2e-required-by-diff-and-run-in-full-before-auto-update.md)決定1）と、reviewのpassの後・着地の前にruntimeがhostで流すe2e（[ADR-t1233-2](2026-10-02-t1233-2-e2e-runs-on-the-host-after-review-passes.md)決定2・3）。どちらも全部のe2eを流し、cmuxが使えずe2eを流せないときは通さない（後者は待たせて後で流し直す）と決めている。関門はe2eの前に`cmux ping`を打ち、答えなければe2eを1本も流さない。

[ADR-t1433-1](2026-10-03-t1433-1-cmux-is-used-only-by-the-inbox.md)の後、runtimeはcmuxを呼ばず、実cmuxを要るe2eはinboxのworkspaceを開く`up` / `down`のe2eだけになった。一方、socketがpasswordを求めるcmux（やcmuxの外のprocessを拒む設定のcmux）は、cmuxの端末の外からの呼び出しを拒む。[ADR-t1433-4](2026-10-03-t1433-4-supervisor-resides-without-cmux.md)の後はsupervisorがlaunchdで常駐し、socketのpasswordを案内しない（同決定1）ので、hostのcmuxの設定しだいで関門が始まらず、自動更新とe2eの要るrunの着地が全部止まる。直し方は、socketのpasswordを案内するか、関門のcmuxへの届き方を変えるかで、前者はADR-t1433-4決定1が退けている。

[ADR-t1162-1](2026-09-30-t1162-1-e2e-gate-skips-podman-e2e-only-when-podman-is-unreachable.md)はpodmanについて、繋がらないときだけ頼るe2eを流さずに残りで判定し、記録してinboxに届けると決めた。cmuxも同じ形にできる。

## Decision

1. **cmuxが答えないときだけ、実cmuxを要るe2eを流さず残りで判定する。** 関門はe2eを流す前に`cmux ping`でcmuxが答えるかを確かめる。答えないとき（cmuxが無い、socketが拒む）だけ、実cmuxを要るe2e（inboxのworkspaceを開く`up` / `down`のe2e）を落ちとせずに流さず、残りのe2eで判定する。cmuxを要らないe2eはcmuxが答えなくても流れる。cmuxが答えないことは、着地の前のe2eを待たせて後で流し直す理由（ADR-t1233-2決定3）にも、入れ替えの関門を通さない理由（ADR-t963-1決定1、ADR-t1162-1決定3）にもしない。
2. **流さなかったe2eを黙って通さない。** ADR-t1162-1決定2と同じく、流さなかったtestと理由（`ping`の失敗の文）を関門のlog、e2eが通ったevent（着地の前のe2eのrunのeventを含む）、入れ替えの報告（自動更新の`update_installed`と`install`の結果）に残し、`update_installed`をinboxが人に伝えるときに一緒に届ける。podmanとcmuxの両方を流さなかったときは両方を並べる。
3. **cmuxの後始末は答えるcmuxにだけ頼む。** cmuxが答えるときは今までどおり、e2eが残したcmuxのworkspaceとgroupを閉じる。答えないときはcmuxを1度も呼ばず、関門のdirectoryの中のprocessを止め、fixtureのqueueのhash（cmuxのgroupを消す手がかり）を覚えてdirectoryを残し、cmuxの後始末を残したことと理由を結果に載せる。次にcmuxが答える関門が、残ったdirectoryを見つけて覚えたgroupとその中を指すworkspaceを閉じ、directoryを消す。
4. **それ以外は変えない。** cmuxが答えて流したe2eが落ちれば落ち。podmanの扱い（ADR-t1162-1決定1・2）は変えない。socketのpasswordは案内しない（ADR-t1433-4決定1）。人が端末から`install`を打つときに、端末が持つcmuxのsocketのpasswordをe2eに渡すことは残してよい（その関門ではcmuxが答えて全部が流れる）。

## Alternatives

- **socketのpasswordをsupervisorのlaunchdのjobに載せる**: ADR-t1433-4決定1が退けた。資格情報をplistに置き、hostの設定を人に求めることになる。
- **今のまま（cmuxが答えなければ関門ごと失敗・待ち）**: launchdに移った後、hostのcmuxの設定しだいで自動更新とe2eの要る着地が全部止まる。止まる原因はruntimeのcommitで直せない。
- **実cmuxを要るe2eを関門から外す**: cmuxが答えるhost（人が端末から打つ`install`、cmuxの中の関門）でも流さなくなり、inboxを開く`up`の経路を本番の前に確かめる機会が無くなる。答えるときは流す方がよい。
- **cmuxが答えないときも後始末でcmuxを呼ぶ**: 答えないcmuxを呼んでも失敗するだけで、呼ぶたびに上限まで待つ。directoryを残して次の関門に任せれば、cmuxのworkspaceとgroupは後から閉じられる。

## Consequences

- cmuxが答えない関門を経た入れ替えと着地では、inboxを開く`up` / `down`の経路は実物で確かめないまま進む。流さなかったことは`update_installed`とそのinboxへの知らせ、runの`run_e2e_finished`に出るので、続くときは人とplannerがhostのcmuxの設定を見直す。
- cmuxが答えない関門のdirectoryは、次にcmuxが答える関門まで残る。
- 実cmuxを要るe2eの選び方（testの名前のfilter）、eventの欄名、後始末の結果の欄は[Validation](../design/supervisor-lifecycle/validation.md)・[Auto-update](../design/supervisor-lifecycle/auto-update.md)・[install](../design/supervisor-lifecycle/install.md)とコードのdoc commentが持つ。

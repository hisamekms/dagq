---
id: adr-t3102-3
type: adr
title: 段(5)でserviceが落ちたら、supervisorはDBの外のqueue dirのファイルに不在を記録して`[push]`で1回送り、戻ったらserviceを通してeventに残し、inboxの`watch`と`status`は届かないこと自体をその記録とともに返す。supervisor・integrate・session wrapperの多段のDB操作は、今1つのtransactionで行う操作ごとに1つのユースケースにし、leaseの検査と前提の再検査をserviceの中の同じtransactionで行い、Gitとプロセスの操作はユースケースの間で制御側が行う（ADR-t1233-4決定3をamends）
status: accepted
created: 2026-10-22
updated: 2026-10-22
accepted_on: 2026-10-22
amends:
  - adr-t1233-4 decision 3
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - architecture
related:
  - adr-t1233-1
  - adr-t1233-4
  - adr-t1433-5
  - adr-t2159-1
  - adr-t3102-1
  - adr-t3102-2
  - adr-t728-2
  - adr-0007
  - design-queue-service
  - design-persistence
  - design-supervisor-lifecycle-ask-push
---

# ADR-t3102-3: 段(5)のserviceが落ちたときの知らせと、制御側の多段の操作のユースケースの単位

## Context

[ADR-t1233-4](2026-10-02-t1233-4-queue-service-lifecycle-outage-notice-and-principal-tokens.md)決定3は、段(2)〜(4)ではsupervisorがserviceの停止をDBにattentionとして直接書き、段(5)では`watch`と`status`が届かないこと自体を返すとした。その後ろ盾は[ADR-t1433-5](2026-10-03-t1433-5-inbox-watch-without-typing-into-the-inbox.md)決定3がinboxのterminalへの1行から`[push]`とeventに改め、[ADR-t2159-1](2026-10-09-t2159-1-dagq-does-not-use-cmux-and-the-person-opens-the-inbox.md)でdagqはcmuxを使わなくなった。段(5)ではsupervisorもservice経由になるので、serviceが落ちている間はattentionもeventもDBに書けない。

[ADR-t1233-1](2026-10-02-t1233-1-control-and-execution-sides-queue-service-broker-and-client-mode.md)決定3は、APIをユースケース単位にし、leaseの検査を含むtransactionをserviceの中で完結させるとした。supervisor・integrate・wrapperは、claimとlease、runの状態とevent、着地の記録、jobの起動と結果の記録を、今はDBを開いて行う（[SQLite persistence](../design/persistence.md)の「Runtime ownership」）。

goal 182の決定の分け方は[ADR-t3102-1](2026-10-22-t3102-1-stage-5-principals-user-by-socket-peer-and-control-side-tokens.md)のContext。goal 182の他のtaskが「ADR-t3102-2」の(b)(c)として名指すもの（落ちたときの知らせ、ユースケースの単位とtransaction）はこのADRに置く。

## Decision

1. **serviceの不在はsupervisorがDBの外に記録し、`[push]`で送り、戻ったらeventに残す。** supervisorはserviceが居ない・起動し直せない・上限に達したことを、queue dirの中でDBの外のファイルに記録する（inboxのwatcherの生存の記録と同じくDBに頼らない置き場）。`host.toml`の`[push]`が設定されていれば、1つの不在につき1回だけ人に送り、中身は不在と時刻と次の手だけにする（ADR-t1433-5決定1の(3)・決定3と同じ経路と抑え方）。serviceが戻ったら、supervisorは不在と起動し直しをserviceを通してeventに残し、ファイルの記録を閉じる。どのterminalにも打ち込まない（ADR-t1433-5決定2）。
2. **`status`と`watch`は届かないこと自体を返す。** `status`はserviceに届かないとき、空の答えや待ちに替えず、届かないことと理由、決定1の記録（あれば）、次の手を出し、失敗の終了で返す。`watch`は届かないとき、時間切れや空の返りと区別できる「届かない」の返りで直ちに返り、inboxはそれを中身として扱う。同じ不在の記録のあいだの次の`watch`は同じ知らせを繰り返さず、serviceが戻るか新しい不在の記録ができるまで待つ。決定1の記録は読み取りであってDBを開かないので、ADR-t3102-2決定1の例外に入れない。supervisorも居ないときは記録も`[push]`も無いが、`status`と`watch`は届かないことを自分で判定して返し、人が`up`で起動し直す。
3. **多段のDB操作は、今1つのtransactionで行う操作ごとに1つのユースケースにする。** 単位はapplicationの操作（行の読み書きではない）で、次を1つずつ持つ。
   - supervisor: claim（候補の選択・runとleaseの作成・tokenの発行・eventを1つ）、leaseの更新、runの状態の遷移とそのevent（遷移ごと）、引き継ぎ、復旧、jobの起動の記録、jobのverdictの適用、plannerを開く・閉じる
   - integrate: 着地の依頼の承認の記録（依頼者の`landing.request`の検査）、slotを取って始める（integratorのtokenの発行を含む）、着地を終える（着地の記録とleaseの解放）、着地をやめて戻す
   - session wrapperとhook: sessionの始まりと終わり、sessionのeventの記録、receiptの取り込み
   候補の選択のようにqueueを読んで決めて書く判断は、serviceの中で同じbuildのapplicationのコードが行う。hostの状態（slot・資源・ディスク・providerの可否）の判断は制御側が行い、その結果をユースケースの入力として渡す。
4. **leaseと前提はserviceの中の同じtransactionで確かめる。** runを変えるユースケースは呼び出し元のleaseを同じtransactionで確かめて更新し、持たなければ何も書かずに断る（[ADR-0007](0007-run-level-leases-parallel-execution.md)とSQLite persistenceの規則のまま）。クライアントが前に読んだ状態で決めた書き込みは、書くユースケースが前提（状態・lease・期待する版）を同じtransactionで確かめ直し、違えば何も書かずに断る。transactionを開く・閉じるユースケースや、表の行を読み書きするユースケースは持たない。
5. **Gitとプロセスの操作はユースケースの間で制御側が行う。** 外の副作用（mainを進める・pushする・プロセスを起動する）は、それを始める記録のユースケースが通った後に行い、結果を記録するユースケースは同じ結果を2回送っても1回だけ記録する形にする。serviceに届かない間、制御側は新しい副作用を始めず（claim・jobの起動・着地を始めない）、始めた副作用の結果の記録は届くまでやり直し、DBを直接開かない。走っているagentのturnは止めない（ADR-t1233-4決定2）。着地の権限とpushの資格情報はIntegratorの側に残し（[ADR-t728-2](2026-09-27-t728-2-landing-only-by-the-trusted-integrator.md)）、serviceに渡さない。
6. **ADR-t1233-4決定3を変える範囲。** 段(5)の形を決定1・2に置き換える。段(2)〜(4)のDBに直接書くattentionと、どの段でもクライアントモードがDBを直接開くことへ戻らないことは変えない。後ろ盾が`[push]`とeventであること（ADR-t1433-5決定3）は保ち、eventはserviceが戻った後に書く。

記録のファイルの置き場と形、返りの欄と終了コード、ユースケースの名前とparams、やり直しの間隔は実装のtaskが[docs/design/](../design/)（[Queue service](../design/queue-service.md)・[人への通知](../design/supervisor-lifecycle/ask-push.md)）に書く。

## Alternatives

- **serviceが落ちたときだけsupervisorがDBを直接開いてattentionを書く**: DBを開くプロセスをserviceだけにする段(5)の目的（ADR-t1233-1決定2）が崩れ、開くプロセスの例外が増える。
- **serviceの外に知らせ専用の別のプロセスを置く**: 起動・停止・見張りの手順がもう1つ増え、それが落ちたときの知らせがまた要る。
- **`watch`を届かない間も待ち続けさせる**: 「何も来ない」と「届かない」を見分けられない（ADR-t1233-4のConsequences）。
- **不在のあいだ`watch`が毎回届かないことを返す**: inboxが同じ知らせを繰り返し人に上げる。
- **行の読み書きのユースケースか、transactionを開くユースケース**: leaseの検査とtransactionがクライアントとの往復に割れる（ADR-t1233-1のAlternatives）。
- **supervisorの1回のpassを1つのユースケースにする**: hostの状態の判断とプロセスの起動がtransactionの中に入り、長いtransactionがほかの呼び出しを止める。

## Consequences

- serviceが落ちている間の記録はファイルに残り、eventは戻った後に書かれるので、時系列のeventの時刻は戻った時刻になり、不在の時刻はpayloadが持つ。
- 制御側のクライアントに、届かない間のやり直しと、結果の記録の重複を除く仕組みが要る。
- ユースケースの数は今のapplicationの操作の数に沿って増える。

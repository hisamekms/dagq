---
id: adr-t838-1
type: adr
title: brokerのmodeがrequiredのworkerとresumeは非対話のturnだけで動かし、組み込みのファイルの道具を拒んでBashはdagqだけを通し、receiptはclientのMCPの道具で書き、brokerが使えなければclaimもresumeもせずinboxに知らせて直接の実行に戻らない（guardrailで、enforcementではない）
status: superseded
created: 2026-10-05
updated: 2026-10-05
accepted_on: 2026-10-05
superseded_by: adr-t2113-1
superseded_on: 2026-10-08
owners:
  - hisamekms
tags:
  - runtime
  - security
  - broker
related:
  - adr-t827-4
  - adr-t827-2
  - adr-t728-1
  - adr-t813-1
  - adr-t1433-2
  - design-broker
  - design-security
---

# ADR-t838-1: brokerのmodeがrequiredのworkerとresumeは非対話のturnだけで動かし、組み込みのファイルの道具を拒んでBashはdagqだけを通し、receiptはclientのMCPの道具で書き、brokerが使えなければclaimもresumeもせずinboxに知らせて直接の実行に戻らない（guardrailで、enforcementではない）

> **置き換え済み（2026-10-08）**: このADRの決定は現在有効ではない。現行の決定は[ADR-t2113-1](2026-10-08-t2113-1-remove-the-resource-broker.md)を読む。

## Context

[ADR-t827-4](2026-09-28-t827-4-worker-mcp-tools-audit-mode-and-relations.md)決定3は`required`を「組み込みの道具を拒み、brokerが使えなければclaimしない」形とし、Phase 2まで起動を拒んでいた。goal 59がそれを実装する。workerはhostのClaude Codeのプロセスのまま（ADR-t728-1決定6）で、制御側の操作（`dagq ask`などqueueへのコマンド、receiptの書き込み）はbrokerの外に残す（goal 59、containerのworkerのqueueの操作は範囲外）。計画書のI8・I9は、`required`で黙ってhostの直接の実行に戻る経路を作らないことを求める。

Claude Codeのpermissionの規則はdenyがallowに勝つ。`Bash`をdenyに入れると`Bash(dagq:*)`をallowに書いても`dagq ask`は打てない。receiptは今run dirのファイルにworkerが書き（tmpとrename）、組み込みのWriteかBashを使う。workerのrunは全て非対話のturnで（ADR-t813-1の経路。対話の経路は[ADR-t1433-2](2026-10-03-t1433-2-abolish-the-interactive-route.md)が廃止した）、turnは`Stop`のhookを持たない（idleの印はwrapperが書く）。brokerの道具を渡したrunのturnが持つhookは、組み込みの道具の操作を数える`PreToolUse`のhook（task 839）だけで、`required`のrunのturnにも付く。

## Decision

1. **道具。** `required`のrunのturn（新しいturnもresumeも）は、組み込みのファイルの道具（Read・Edit・Write・MultiEdit・NotebookEdit・Glob・Grep・LS）をsettingsの`permissions.deny`で拒む。Bashはdenyに入れず、permission modeを問わずに拒む形（Claude Codeの`dontAsk`）にして、allowにはbrokerのMCP serverと`dagq`のコマンドだけを置く。MCPはbrokerのserverだけを読む（利用者・projectのserverを読まない）。roleの`permissions.deny`（打てないdagqのコマンドなど）はそのまま重ねる。
2. **receipt。** receiptはbrokerのclientのMCP serverの道具（`write_receipt`）で書く。clientはhostのプロセスなので、brokerを通さずにrun dirのreceiptをtmpとrenameで書く。supervisorは`required`のrunのMCPの設定にだけreceiptのファイルを名指し、`preferred`のworkerには道具を出さない。brokerが落ちていても失敗のreceiptを書ける。
3. **fail closed。** supervisorは`required`のrunのdirに印を置いてからworkerとresumeを起こす。executorは印があってMCPの設定が無いrunのagentの起動の引数を作らずに拒み、対話のsessionの起動も拒む（settingsで組み込みの道具を拒めない）。brokerの道具を渡せないprovider（今のCodex）のturnも拒む。印が置けない・道具を渡せないrunは起こさず失敗にし、`preferred`の形に落とさない。
4. **claimとresumeの前。** `required`のsupervisorは各passのclaimとresumeの前に、workerに今道具を渡せるか（brokerが用意できてhealthが答え、dagqのbuildで、clientがあり、tokenが発行できる）を確かめる。できなければclaimもresumeもせず、queueのattentionでinboxに知らせ、できるようになれば知らせを閉じる。runの途中で落ちたbrokerは道具が構造化のerrorを返し、workerはpromptに従って失敗のreceiptかaskにする。
5. **guardrailであってenforcementではない。** `permissions.deny`・permission mode・allowはClaude Codeの設定で、hostのプロセスを隔離しない（別のpathのコマンド・scriptを通せる）。host実行は助言的のまま（ADR-t728-1決定6、ADR-t827-4決定5）で、`status`と`doctor`の`backend: host`・`enforcement: advisory`は変えない。

settingsの中身・印のfileの名前・attentionのeventのkindとreason・道具の入力・promptの文は[Broker](../design/broker.md)に書く。

## Alternatives

- **Bashをdenyし、dagqの操作もMCPの道具にする**: queueの操作をbrokerのclientに持たせることになり、ADR-t827-4決定6（queueの操作はgoal 38か後のgoal）を越える。
- **Writeを拒みつつreceiptのpathだけallowする**: denyがallowに勝つので成り立たない。
- **receiptを書く`dagq`のコマンドを足す**: workerの`dagq`はqueue serviceのclient modeで動き、ファイルを書くコマンドの権限と経路を新しく決める必要がある。clientのMCP serverは既にhostで動き、runごとの設定を持つ。
- **permission modeを`auto`のままallowだけ足す**: 分類器が他のBashのコマンドを通しうる。
- **brokerが使えないときは`preferred`の形で動かす**: I8・I9とADR-t827-4決定3（黙って弱い形に戻さない）に反する。

## Consequences

- `required`のqueueではbrokerの`exec`のallowlistにないcheckをworkerが流せない。流せないcheckはreceiptに理由を書く。
- Codexのworkerは`required`では動かない（道具の渡し方が決まるまで）。
- Claude Codeの`dontAsk`とdenyがallowに勝つ規則に依存する。版で変われば設定を直す。

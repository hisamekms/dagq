---
id: adr-t1857-1
type: adr
title: 使えないproviderからもう一方への切り替えを、人が設定でworkerとjobで別々に止められるようにし、止めたときは切り替えずに控えが解けるのを待って同じproviderで続ける。設定しなければ今のまま、`--no-claude`による行き先と能力による選択は止めず、控えと人への届け方は変えない（ADR-t813-2決定2・4・6、ADR-t1063-1決定4・5をamends）
status: accepted
created: 2026-10-06
updated: 2026-10-06
accepted_on: 2026-10-06
amends:
  - adr-t813-2 decision 2
  - adr-t813-2 decision 4
  - adr-t813-2 decision 6
  - adr-t1063-1 decision 4
  - adr-t1063-1 decision 5
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - provider
related:
  - adr-t813-2
  - adr-t1063-1
  - adr-t1204-1
  - adr-t1453-1
  - design-provider-lifecycle
---

# ADR-t1857-1: 使えないproviderからの切り替えを、workerとjobで別々に設定で止められるようにする

## Context

[ADR-t813-2](2026-09-28-t813-2-provider-per-task-and-mutual-fallback.md)（worker）と[ADR-t1063-1](2026-09-29-t1063-1-headless-job-provider-per-role-with-intent-permissions.md)（`[roles.<role>]`にproviderを書いたheadlessのjob）は、providerが使えない（実行ファイルが無い・起動できない・認証・利用上限）と、常にもう一方のproviderへ切り替えると決めた。切り替えを止める手は`--no-claude`（[ADR-t1204-1](2026-09-30-t1204-1-explicit-no-claude-operation.md)）しか無く、それはClaudeそのものを禁じる。

人は「providerが動作しなかった時に別のプロバイダーにフォールバックする機能を設定でon/offできるようにしておきたい。急ぐのはworker」と頼んだ（request 32、goal 148）。providerごとに得意な作業や費用が違い、使えないあいだだけ別のproviderで進めるより、戻るのを待って同じproviderで続けたいことがある。workerとjobでは急ぎ方が違う。

## Decision

1. **人は設定で、使えないproviderからの切り替えをworkerとjobで別々に止められる。** 設定しなければ今の振る舞いのまま（切り替える）。
2. **止めたときは切り替えずに、使えないproviderの控えが解けるのを待って同じproviderで続ける。** workerはclaimの時点ではもう一方で始めずに控え、turnの途中では失敗したturnの呼び出しを同じproviderにもう一度送る。jobはもう一方で起動せず、控えのあいだ止まる。これはADR-t813-2決定2・4（両方向の切り替えと切り替え先の新しいsession）と決定6の「使えるproviderがあれば止めない」、ADR-t1063-1決定4（jobの切り替え）と決定5の「両方が控えられたときだけ止める」の例外になる。待つ経路は切り替えの上限に達したrunが待つ今の経路と同じで、新しい失敗の扱いを足さない。
3. **止めるのはproviderが使えないことによる切り替えだけにする。** 人の明示的な禁止（`--no-claude`）による行き先と、能力による選択（必須のsubagentを動かせないproviderからの切り替え、[ADR-t1453-1](2026-10-03-t1453-1-review-subagents-named-by-path-run-inside-the-review-job.md)決定8）は止めない。`--no-claude`の下でClaudeを頼んだtaskは、止めていてもCodexで動く。
4. **控えと人への届け方は変えない。** providerごとの控え、Claudeの認証・利用上限の控えのask、使えないことの記録はon/offで同じ。止めたことで切り替えなかった理由は記録（claimを控えた理由・runの待ち）に残す。

設定の表とkeyの綴り・型・既定値、判定の関数と記録の欄は[provider-lifecycle](../design/provider-lifecycle.md)の「使えないproviderからの切り替え」が書く。

## Alternatives

- **`--no-claude`をoffより後にする（offならClaudeのtaskも経路なし）**: `--no-claude`はClaudeを使わない人の明示の運転で、その意味（ClaudeのtaskもCodexで進む）をoffが変えてしまう。goalの制約が`--no-claude`の意味を変えないと言う。
- **offでは控えずに失敗にする**: 人が復旧を判断する手間が増え、待てば解ける壁を失敗として扱うことになる。今の待ちの経路で足りる。
- **workerとjobを1つの設定にする**: 人の優先度がworkerとjobで違い、片方だけ止めたいことがある。
- **providerごとに切り替えの可否を書く**: 今の要望は向きを分けておらず、表が増えるだけ。要れば後で足せる。

## Consequences

- offのとき、使えないproviderのtaskとrunは控えが解けるまで進まない。利用上限ならその窓のあいだ着地が止まり、claimの順と1回のpassで走るrunの本数が変わりうる。
- Claudeの認証・利用上限では今どおり控えのaskが開き、人が`done`を答えるまで待つ。Codexの壁はClaudeが使えてもaskを開かず、控えの時刻の後に同じproviderへもう一度送る。
- workerの実装はtask 1857、jobの実装は後続のtask 1858が行う。

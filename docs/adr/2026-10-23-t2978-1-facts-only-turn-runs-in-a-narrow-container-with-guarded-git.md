---
id: adr-t2978-1
type: adr
title: 止まったcontainerのrunの事実を取り直す事実だけのturnは、queueのbrokerへの経路もrun tokenも無い狭いcontainerで、hook・fsmonitor・driverを切ったgitだけを走らせる
status: accepted
created: 2026-10-23
updated: 2026-10-23
accepted_on: 2026-10-23
owners:
  - hisamekms
tags:
  - runtime
  - security
  - git
related:
  - adr-t2179-1
  - adr-t2113-3
  - design-container-runtime
---

# ADR-t2978-1: 事実だけのturnは狭いcontainerで、hook・fsmonitor・driverを切ったgitだけを走らせる

## Context

containerのrunの事実（bundleと状態のファイル）は、containerの中のexecutorがturnの終わりに書く（[ADR-t2179-1](2026-10-12-t2179-1-control-side-runs-no-git-in-a-container-runs-clone-and-lands-in-its-own-worktree.md)決定2）。
turnがcontainerごと切られると、事実は最後のturnの終わりのものでなくなる。そのままでは、holdの解除と`ready --inherit`は拒まれ続け、triageは古いheadを読む。
制御側はcloneでgitを走らせないので（同決定1）、事実はcontainerの中で書き直すしかない。そのため、agentを走らせずに事実だけを書き直すturnを足した。
このturnが動くのは、runが止まった後（`failed`・`interrupted`、holdの中）で、起こすのは人の解除か`ready --inherit`、またはsupervisorのtriageと引き継ぎである。
初版は通常のturnと同じ準備と引数を使ったので、runのbrokerへのrelayが開き、そのsocketとrun dirがmountされた。
さらにexecutorのgitがcloneのconfigのまま走るので、workerがcloneに残したhook・fsmonitor・driverのコードが、runが止まった後にbrokerへの経路を持ったcontainerで動きえた。
run tokenの写しは、runのcontainerを消す経路でしか消えない。そのため、切られたturnの写しがrun dirに残りうる。
これはsandboxの範囲（止まったrunのcontainerを誰が、何を持たせて動かせるか）を広げるので、人が決めた（ask 1102）。

## Decision

1. **事実だけのturnの範囲。**
   引き継ぐheadや自分のcommitを事実から読む判断の前に、事実が最後のturnの終わりのものでないときだけ動かす。
   agentは走らせず、forwarderも起こさない。executorがするのは、制御側が渡した復元の適用と、bundle・作業・状態の書き直しだけである。
2. **containerは狭くする。**
   runのturnと同じhardeningと名前を使う。runのcontainerが残っていれば起動しない。時間切れのときは、turnのcontainerと同じく消す。
   mountするのはcloneとrun dirだけで、brokerへのrelayを開かず、そのsocketもmountしない。buildの出力とworkerのcacheのvolumeも、envも渡さない。
   準備はdagqのmachineとimageだけにする。
3. **始める前に、run dirに残ったrun tokenの写しを消す。**
4. **executorのgitは、workerがcloneに残したプログラムを走らせない。**
   hookを無いpathに向け、fsmonitorを切り、cloneのconfigの全てのscopeが定義するfilter・diff・mergeのdriverをプログラムを走らせないものにする。これらはcommand scopeで上書きする（ADR-t2179-1決定8の上書きにfsmonitorを足したもの）。
   gitが読むconfigで上書きが効いていることを確かめ、確かめられなければ事実を書かない。この場合は取り直しが失敗するので、事実を読む判断は拒む。
   中身のあるsubmoduleを持つcloneも、同じく事実を書かずに拒む。`status`と`diff`が入れ子のrepositoryで走らせるgitは、そのrepositoryのconfigのdriverを読み、上書きはそこに届かないため。
   workerのturnのexecutorのgitは変えない。

## Alternatives

- **通常のturnと同じ準備と引数（初版）**: 止まったrunのcontainerに、brokerへの経路とworkerのコードの実行を戻す。
- **executorの全てのturnのgitに上書きを掛ける**: filter（例 git-lfs）に頼るprojectで、workerのturnの作業の書き出しと状態が変わる。workerのturnは、生きたrunのsandboxの中でworkerのものなので、今のままにした。
- **事実を取り直さずに拒み続ける、または人に古いheadを受け入れさせる**: 前者は解除もcancelもできないtaskを残し、後者は成果を失う側の逃げ道になる。

## Consequences

- filterに頼るprojectでは、事実だけのturnの状態はfilterを掛けない中身との差を未commitとする。そのため解除と引き継ぎは拒まれる（成果は失わない）。中身のあるsubmoduleを持つcloneでも同じく拒まれる。
- 上書きは、gitの版が増やす設定をすべては覆わない。残った設定が動いても届くのはこのcontainerの中のcloneとrun dirだけで、それはworkerが元から書けた範囲である。
- queueのbrokerが止まっていても、事実だけのturnは動く。

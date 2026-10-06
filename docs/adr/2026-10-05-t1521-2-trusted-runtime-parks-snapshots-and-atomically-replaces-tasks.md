---
id: adr-t1521-2
type: adr
title: trusted runtimeが安全に保留し成果を保存して、plan reviewを経た置換を依存保護とともに原子的に適用する（ADR-0027決定1、ADR-0047決定8・43・44、ADR-t813-1決定2、ADR-t1394-1決定2・4・5・6をamends）
status: accepted
created: 2026-10-05
updated: 2026-10-06 # task 1521 revise 1: replacement sources in goal review and close
accepted_on: 2026-10-05
amends:
  - adr-0027 decision 1
  - adr-0047 decision 8
  - adr-0047 decision 43
  - adr-0047 decision 44
  - adr-t813-1 decision 2
  - adr-t1394-1 decision 2
  - adr-t1394-1 decision 4
  - adr-t1394-1 decision 5
  - adr-t1394-1 decision 6
owners:
  - hisamekms
tags:
  - runtime
  - replanning
related:
  - adr-t451-1
  - adr-t1487-1
  - design-supervisor-lifecycle-task-replanning
---

# ADR-t1521-2: trusted runtimeが安全に保留し成果を保存して、plan reviewを経た置換を依存保護とともに原子的に適用する（ADR-0027決定1、ADR-0047決定8・43・44、ADR-t813-1決定2、ADR-t1394-1決定2・4・5・6をamends）

## Context

実行中のtaskをworkerやplannerが直接編集すると、旧workerのreceipt、着地、後続のclaimと競合する。分割前の成果と未達条件を保つには、停止・保存・置換を同じtrusted runtimeが引き受ける必要がある。goal 96のSpike再計画（ADR-t1487-1）は既存の永続した計画依頼とruntime plannerを再利用すると決めている。

## Decision

1. **安全な境界で保留する。** trusted runtimeだけが通常の続行と着地を隔離し、書き手の停止を確認してrunを保留する。ADR-0027決定1に再計画のための保留を加え、ADR-t813-1決定2の続行配送は保留中は止める。停止を確かめられない場合は置換に進まない。遅れたreceipt・旧worker・古いlease・重複配送が新しい計画を動かせない柵を持つ。
2. **成果を保存してから依頼する。** commitだけでなく未commitの変更・証拠・会話と診断の参照・条件の写しを復元できるsnapshotに保存し、その完全性を確かめてからplannerへ渡す。ADR-0047決定44の掃除に、引き継ぎが確定するまで保存したソースと証拠を削除しない例外を加える。失敗・差し戻し・依頼の使い切りでは旧成果を保留したまま修正・継続・案の撤回へ進める。成果の破棄を暗黙の後始末にしない。
3. **既存の計画依頼を使う。** ADR-t1394-1決定2・4・5を拡張し、runtime起点の依頼に診断・snapshot・元条件と依存の参照を載せ、同じruntime plannerと起動上限・復旧を使う。workerは提案するだけで、計画依頼の登録や実行中taskの編集権限を得ない。goal 96と同じgoalの再計画を直列化し、別のplanner engineは作らない。
4. **置換proposalをplan reviewに通す。** 元条件から子への対応表・成果の引き継ぎ方・全後続の依存先・権限ある変更の根拠を検査する。ADR-0047決定8のready化は、置換proposalではtrusted runtimeの原子的適用まで遅らせる。ADR-t1394-1決定6の通常proposalの結末は維持し、置換依頼では一つの有効な置換proposalに結び、submitの記録とは別に適用・継続・撤回の結末を追う。
5. **未達をcompletedにしない。** 適用は元taskを未達の置換元として終了し、子の登録とready化、元条件の対応、全後続依存の付け替え、旧runとclaimの封鎖を一括で確定する。後続は必要な子が全て着地するまで開始できない。元taskの完了扱いや通常のcancelで依存を満たさない。ADR-0047決定8のgoal closeの拒否条件と決定43の起動・閉じる条件は、置換元を、その置換先が再置換も辿って全て完了か取消になったときだけ解決済みとして扱うよう改める。置換元自身は達成taskに数えず、goal reviewは子の着地と元条件の対応を根拠にgoal acceptanceを判断する。他の起動・close条件とfollow-upの所属判断は維持する。適用直前に版と所有権と依存を再検査し、競合なら全体を適用せず保留する。

## Alternatives

- 元taskをcompletedにして子を登録する: 未達を成功と記録し、後続が子より先に開始する。
- plannerが実行中taskを編集する: 信頼境界を越え、停止・着地・claimの競合を防げない。
- 保存と依存変更を個別に行う: 再起動で成果や依存の一部が欠ける。
- Spikeとは別の再計画engineを作る: 依頼の永続・plannerの上限・復旧を二重に持つ。

## Consequences

保存とqueueの確定の間は再開可能な段階として扱い、適用は冪等にする。詳細な状態・停止境界・順序・versionとlease・ask・競合・回数・実装の責務は[予定の設計](../design/supervisor-lifecycle/task-replanning.md)が持つ。通常のrequestとSpikeの完了起点の契約は維持し、未達の実装taskをSpikeの成功とは扱わない。実装は後続taskが行う。

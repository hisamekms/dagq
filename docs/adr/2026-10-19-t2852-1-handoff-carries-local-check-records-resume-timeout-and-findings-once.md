---
id: adr-t2852-1
type: adr
title: 新しいsessionの引き継ぎのpromptに、今のHEADのtreeの手元の検証の記録（ほかは数だけ）・resumeの時間の上限・文書を読み直さないこと・task が触るファイルに絞ったknown failuresを載せ、reviewの指摘は依頼の中の1回だけにする（ADR-t2080-1決定2をamends）
status: accepted
created: 2026-10-19
updated: 2026-10-19
accepted_on: 2026-10-19
amends:
  - adr-t2080-1 decision 2
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - worker
related:
  - adr-t2080-1
  - adr-t2589-1
  - adr-t920-1
  - adr-t1707-1
  - adr-t2072-1
  - adr-t2257-1
  - adr-t1920-1
  - design-provider-lifecycle
---

# ADR-t2852-1: 新しいsessionの引き継ぎは検査の記録と時間の上限を渡し、読み直しとreviewの指摘の重複をやめる（ADR-t2080-1決定2をamends）

## Context

[ADR-t2080-1](2026-10-08-t2080-1-fresh-session-on-send-back-and-resume-when-peak-context-exceeds-threshold.md)決定2は、新しいsessionの最初の呼び出しをtaskのpromptと引き継ぎで始め、引き継ぎにcommit・未commitの変更・reviewの理由・receiptの要約・送るはずだった依頼を載せると決めた。
2026-10-19、閾値を超えて新しいsessionに切り替えたreviseとresumeの27件中9件がresumeの時間の上限で打ち切られた。
新しいsessionはtaskのpromptの「最初に文書を読む」から始め直し、run dirにある手元の検証の記録（[ADR-t2589-1](2026-10-17-t2589-1-local-check-runs-are-recorded-by-project-scripts-and-matched-to-the-receipt-tree.md)）を知らされず、時間の上限も知らないので、stress・subagent review・clippyを一からやり直し、2周目のstressの途中で打ち切られた例がある。
reviewの指摘は、引き継ぎのreviewの節と送るはずだったreviseの依頼の両方に入っていた。
known failuresはlanding branchで落ちるtestの全部で、taskに関わらないものが多い。

## Decision

1. **引き継ぎは今のHEADのtreeの手元の検証の記録だけを載せ、ほかは数だけを書く。** runの記録のdirを、reviewの材料と同じ読み方と除外（ADR-t2589-1決定4: linkを辿らない、上限を超えたものと壊れたものは数だけ、treeがHEADのtreeでないものと実行の間にtreeが変わったものは載せない）でHEADのtreeに照らし、当たる記録のコマンド・結果・時間・開始の時刻を新しいものから上限の内に載せる。上限の件数を超えたときは名前の順の最後（新しいもの）を読む。runtimeは検査の名前を知らず、記録のコマンドをそのまま載せる（[ADR-t2257-1](2026-10-12-t2257-1-container-runtime-takes-project-and-host-values-from-config.md)）。コマンドは同じrunのworkerが打ち同じproviderが既に見たものなので載せてよいが、別のtreeの記録の中身はpromptに入れない（人の判断、ask 1003）。receiptの根拠の規則はADR-t2589-1決定3・9のまま変えない。引き継ぎは、載せた記録はそれから何も変えていなければそのまま根拠になること、別のtreeの記録は根拠にならず記録のdirにある再開の手がかり（何が通り・落ち・どれだけかかったか）であること、何か直したら最終のtreeで要る検査を流し直すこと（pjの関門の要否はADR-t2589-1決定6の判定のまま）を書く。差分に当たる検査だけを流せばよいとは書かない。
2. **引き継ぎは文書を最初に読み直さないことを書く。** taskのpromptは前に置いたまま（claimの時の写しで書き換えない）で、引き継ぎがその読む指示を上書きし、文書は作業が要るときだけ開くと書く。taskのpromptから読む指示を外す案は、promptを書き換えない約束と、providerの切り替えの新しいsessionとで扱いが分かれることから採らない。
3. **引き継ぎはresumeの時間の上限を設定の秒で書く。** 依頼からその秒の内にreceiptを書き直さないとsessionが止められること、直して最終のtreeで要る検査を流したら、subagent reviewのやり直しなどほかの作業より先にreceiptを書き直すことを書く。
4. **reviewの指摘は1回だけ載せる。** 送るはずだったreviseの依頼が指摘を持つので、引き継ぎはreviewの節を持たない。
5. **known failuresはrunが触るファイルに当たるものに絞る。** runのcommitと未commitの変更が触るpathとtaskが宣言したpathから名前を取り、testのmoduleのpathがそれに当たるものとtask自身が直すものだけを載せ、外した件数と全部を持つファイルを書く。runtimeはpjのtestの置き方を知らないので、名前の一致だけで決める。どのtestも共有するtest binaryやpackageの名前と、test自身の名前では一致させない（それに当たるファイルを触っただけで全部が残らないように）。名前の取り方と照らし方は定義のそばのdoc commentが持つ。新しいsessionに送るresumeの依頼はknown failuresの節を持たず、taskのpromptのknown failuresの節はclaimの時の写しのまま残る。

ADR-t2080-1決定2のうち、taskのprompt・commit・未commitの変更・receiptの要約・送るはずだった依頼を載せること、会話のtranscriptを渡さないこと、上限（ADR-t2072-1）の内に収めることは変えない。決定1・3・4・5も変えない。
[ADR-t920-1](2026-09-28-t920-1-light-worker-stress-and-heavy-repetition-in-scheduled-ci.md)・[ADR-t1707-1](2026-10-05-t1707-1-time-gate-for-added-or-changed-integration-tests.md)の手元の検証の規則も変えない。

## Alternatives

- **今のtreeと一致しない記録でも、差分に当たらない検査は流し直さなくてよいとする**: ADR-t2589-1は最終のtreeと一致する記録だけを根拠にし、古いtreeの記録の再利用を退けている。plan reviewの指摘で外した。
- **新しいsessionのためにtaskのpromptを書き直す（読む指示とknown failuresを外す）**: workerのpromptはclaimの時の写しで書き換えない約束があり、providerの切り替えも同じファイルを読む。引き継ぎの側で上書きする方が変える範囲が小さい。
- **reviewの節を残し、依頼の指摘を外す**: 依頼は同じsessionに送るときと同じ文で、切り替えたときだけ形を変えると、adoptや送り直しで持つ文が分かれる。

## Consequences

- 切り替えた後のsessionは、今のtreeと一致する記録の流し直しを避けられ、別のtreeの記録は自分で記録のdirを読めば計画の手がかりにできる。時間切れの率の比較は着地の後にinboxと人が行う。
- 新しいsessionは文書を読まずに始めるので、作業に要る規則を読み落とすことがありうる。taskのpromptと依頼が名指すものは残る。
- known failuresの絞り込みは名前の一致による推定で、taskのファイルに当たる失敗を外すことも、関係の薄いものを残すこともある。外したものの在りかは引き継ぎが書く。

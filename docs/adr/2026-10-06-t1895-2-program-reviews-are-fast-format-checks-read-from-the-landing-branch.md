---
id: adr-t1895-2
type: adr
title: programのreviewはtestを含まない速い形式の検査に限り、プログラムと設定はlanding branchのcommitから読み、落ちたらagentを起動せず差し戻し、資格情報を渡さない絞ったenvでreviewのactorのbackendで動かす
status: accepted
created: 2026-10-06
updated: 2026-10-06
accepted_on: 2026-10-06
owners:
  - hisamekms
tags:
  - runtime
  - review
  - security
related:
  - adr-t1895-1
  - adr-t1453-1
  - adr-0040
  - adr-t963-1
  - adr-t1233-2
  - adr-t728-1
  - design-supervisor-lifecycle-review
  - design-supervisor-lifecycle-validation
---

# ADR-t1895-2: programのreviewはtestを含まない速い形式の検査に限り、プログラムと設定はlanding branchのcommitから読み、落ちたらagentを起動せず差し戻し、資格情報を渡さない絞ったenvでreviewのactorのbackendで動かす

## Context

[ADR-t1895-1](2026-10-06-t1895-1-review-stage-runs-agent-and-program-jobs-in-a-fixed-shape.md)は、runのreviewの段の最初にprogramのreviewを順に流し、落ちたらagentを動かさずworkerに返すと決めた。programのjobはworkerが書いたcommitに対して、AIでなく決まったプログラムをhostで動かすので、何を流してよいか、どこから読むか、何を渡すかを決める。

検証は`integrate`のrebase後の1回だけ（[ADR-0040](0040-verify-once-review-run-env-graph-stats-and-task-priority-in-claim-order.md)決定1、[Validation](../design/supervisor-lifecycle/validation.md)）で、e2eはreviewのpassの後にruntimeがhostで流す（[ADR-t1233-2](2026-10-02-t1233-2-e2e-runs-on-the-host-after-review-passes.md)）。e2eの関門（`src/infrastructure/e2e_gate.rs`）は、名前の許可の一覧とprefixで起動元のenvを絞り、credentialの名前を除く。ただし今の`passed`は`CMUX_SOCKET_PASSWORD`をcredentialの除外より先に許し、`CMUX_*`もprefixで許す（e2eの実cmuxの呼び出しに要る）。そのまま共用するとprogramのjobにsecretを渡す。

task 1728のADR-t1728-2（未着地）は、(a) agentの定義が道具を宣言することと、(b) agentの定義が名指す決まった検査をsupervisorがreviewの前に流し、違反ならagentを起動したうえで結果を`revise`に置き換えることを予定していた。(b)はこのADRのprogramのreviewと同じ目的なので、ここに一本化する（plan reviewの差し戻しでplannerが決めた。proposal 705）。

## Decision

1. **範囲。** programのreviewは、testを含まない速い形式の検査（読むだけの、書式・構造・参照の検査）に限る。build・unit test・integration test・e2eは流さない。検証は`integrate`の1回のまま変えず、programのreviewはその前に形式の誤りを安く返すためのものとする。
2. **読む元。** 実行するプログラムと設定（programの一覧・引数）は、run worktreeでもmain checkoutの作業ファイルでもなく、workerが変えられないlanding branchの着地したcommitのtreeから、reviewの試行ごとに読む（[ADR-t1453-1](2026-10-03-t1453-1-review-subagents-named-by-path-run-inside-the-review-job.md)決定4と同じ）。workerがrun branchで変えたプログラムと設定は、着地した後のreviewから効く。検査の対象はreviewするcommitのrun worktreeである。
3. **落ちたとき。** プログラムが失敗で終われば、agentのjobを1本も起動せず、落ちたプログラムの名前と出力を理由にworkerへ差し戻す。差し戻しはreviewの`revise`と同じく、reviseの上限に1回と数える（上限を超えれば人の判断）。
4. **起動の失敗と時間切れ。** プログラムを起動できない・landing branchから読めない・時間の上限を超えたときは、workerの変更のせいとせず、reviewの失敗（`review_failed`と手動review）にする。passにも差し戻しにもしない。時間の上限を超えたプログラムはprocess groupごと止める。
5. **流す場所。** programのjobはreviewのactorのbackendで動かす。今はhostである。backendが`podman`に決まっていて実装が無いときは、起動しない誤りにし、hostに黙って戻さない（[Roles](../design/supervisor-lifecycle/roles.md)の「実行のbackendとenforcement」とgoal 38のfail closedに揃える）。hostの外のbackendのportの実装はtask 1874が持つ。
6. **env。** e2eの関門の名前の許可の一覧・prefix・credentialの除外を共通の部品にする。credentialの除外は常に効かせ、呼び出し側が選べるのは名前の許可の一覧、足すprefix、除外の例外の3つだけにする。
   - e2eの関門は今の例外（`CMUX_SOCKET_PASSWORD`）とprefix（`CMUX_*`）を保ち、渡すenvを変えない。
   - programのjobには資格情報を一切渡さない。`CMUX_SOCKET_PASSWORD`も`CMUX_*`も、queue serviceとbrokerに届く変数（`DAGQ_SERVICE_SOCKET`・`DAGQ_SERVICE_CREDENTIAL_FILE`・`DAGQ_BROKER_URL`・`DAGQ_BROKER_TOKEN_FILE`・`DAGQ_QUEUE`）も渡さない。programは読むだけの形式の検査で、cmuxもqueueも呼ばない。
7. **ADR-t1728-2との分担。** ADR-t1728-2(b)が予定したreviewの前の決まった検査は、このADRのprogramのreviewに一本化する。検査の宣言はagentの定義のfrontmatterでなく、`dagq.toml`のprogramの一覧に置く。ADR-t1728-2に残るのは(a)のagentの道具の宣言だけで、eval（ADR-t1728-1）のケースにprogramのreviewをどう当てるかはADR-t1728-1の側（task 1728・1874）が決める。

## Alternatives

- **ADR-t1728-2(b)のとおりagentを起動したうえで結果を`revise`に置き換える**: 差し戻すと分かっているcommitにAIを動かし、時間とtokenを使う。検査の宣言がagentの定義と`dagq.toml`の2か所に分かれる。差し戻しの前にAIを動かさず、宣言の置き場を1つにする。
- **programのreviewでtestやbuildを流す**: 検証が`integrate`と2回になり、reviewの時間が延びる。workerのcodeを動かす範囲も広がる。
- **プログラムをrun worktreeから読む**: workerが検査を消せる・変えられる。
- **e2eの関門の`passed`をそのまま使う**: `CMUX_SOCKET_PASSWORD`と`CMUX_*`をprogramのjobに渡す。
- **起動の失敗と時間切れを差し戻しにする**: hostやプログラムの問題でworkerを動かし、直せないreviseが上限まで続く。
- **Podmanが無ければhostで流す**: 隔離を求めた設定が黙って弱まる。

## Consequences

- 共通のenvの部品、programの一覧の設定、landing branchからの読み込み、process groupごとの停止の実装と、unit testとintegration testは、goal 152の後続のtaskが行う。fail-fastの段への接続と設定・定義の整理はgoal 153の後続のtaskが行う。設定のkeyの綴り・時間の上限の値・eventの形は[Review](../design/supervisor-lifecycle/review.md)が持つ。
- e2eの関門の渡すenvは変わらない。
- このrepositoryの`dagq.toml`にprogramの一覧のtableを足すのは、固定バイナリが読めるようになった後にする（ADR-t1453-1決定10）。

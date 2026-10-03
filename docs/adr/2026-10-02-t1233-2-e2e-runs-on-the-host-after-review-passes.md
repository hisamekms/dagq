---
id: adr-t1233-2
type: adr
title: e2eをworkerから外し、e2eが要るrunはreviewがpassした後にruntimeがhostで1本ずつ流す工程にする。落ちたe2eは1回流し直し、印はruntimeが効かせ、残った失敗はneeds_sessionのresumeでworkerに返す（ADR-t963-1決定2・5とADR-t1165-1決定6をamends）
status: accepted
created: 2026-10-02
updated: 2026-10-02
accepted_on: 2026-10-02
amended_by:
  - adr-t1433-1
amends:
  - adr-t963-1 decision 2
  - adr-t963-1 decision 5
  - adr-t1165-1 decision 6
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - testing
related:
  - adr-t1233-1
  - adr-t963-1
  - adr-t1165-1
  - adr-t1162-1
  - adr-t768-1
  - adr-0027
  - adr-0047
  - design-supervisor-lifecycle-validation
  - design-supervisor-lifecycle-auto-update
---

# ADR-t1233-2: e2eをreviewのpassの後にruntimeがhostで流す工程に移す（ADR-t963-1決定2・5とADR-t1165-1決定6をamends）

## Context

[ADR-t963-1](2026-09-29-t963-1-e2e-required-by-diff-and-run-in-full-before-auto-update.md)決定2は、runの差分が`dagq.toml`の`[e2e] paths`に触れるrunと`--evidence e2e`のtaskで、workerがe2e（`tests/e2e.rs`）を流してreceiptのevidenceにすると決めた。決定5はCodexのworkerのsandboxからhostの権限に依るe2eを名前で除外する例外、[ADR-t1165-1](2026-09-30-t1165-1-e2e-gate-reruns-failed-e2e-once-and-records-quarantined-failures.md)決定6はworkerの手元のe2eに関門の印を効かせる規則である。

e2eは実cmux・実Git・実プロセスを操作するので、実行側を隔離環境に置く（[ADR-t1233-1](2026-10-02-t1233-1-control-and-execution-sides-queue-service-broker-and-client-mode.md)決定1）とコンテナの中では走らない。Codexのsandboxでも一部が走らず、除外の規則（決定5）と、それをこのrepositoryに限る案（task 1209）が要った。2026-10-01に人は、goal 38の未決の案A（reviewのpassの後にruntimeがhostで流す工程にする）を選んだ。

## Decision

1. **workerはe2eを流さない。** e2eが要るrunの決め方（差分が`[e2e] paths`に触れるか、taskが`--evidence e2e`を持つ。ADR-t963-1決定2・3）は変えないが、流すのはworkerではなくruntimeにする。validatingはreceiptの`e2e`のevidenceを求めない。ADR-t963-1決定2のうち「workerが流し、receiptのevidenceにし、欠ければ`evidence_missing`でresumeする」部分をこの決定で置き換える。
2. **reviewのpassの後、着地の前にruntimeがhostで流す。** e2eが要るrunは、reviewがpassした後（と、`approve_landing`に人が`land`と答えた後）、着地を依頼する前に、制御側（supervisor）がhostで、reviewしたcommitのrunのworktreeで全部のe2eを流す。通れば着地へ進む。流したcommit・結果・logをrunの記録に残し、receiptの代わりのevidenceにする。着地のrebaseの後には流し直さない（ADR-t963-1決定4のとおりintegrateでは流さない）。本番のバイナリはADR-t963-1決定1の関門が守る。
3. **落ちたときの行き先。** 落ちたe2eは名前で絞って1回だけ流し直す（[ADR-t768-1](2026-09-27-t768-1-rerun-failed-tests-once-and-land-again-on-flaky-only.md)・ADR-t1165-1決定1と同じ考え方）。流し直しで通ったtestはflakyとして記録して通す。流し直しでも落ちたtestが残れば、runを`needs_session`にしてresumeし、落ちたtestの名前とlogの場所をworkerに渡して直させる。直したrunはもう一度validating・reviewを経てこの工程に戻る。このresumeは今の`needs_session`のresumeの上限と復旧の層（[ADR-0047](0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)）に乗る。cmuxが動いていない・上限切れなど、変更のせいでなくe2eを流せないときはworkerに返さず、runを待たせて後で流し直し、続くならinboxにattentionで知らせる。新しいaskのkindは作らない。
4. **同時に流すのは1本。** 実cmuxのworkspaceと実プロセスを使い、hostの負荷も大きいので、e2eはhostで同時に1本だけ流す。自動更新と`install`の関門のe2e（ADR-t963-1決定1）とも重ねず、hostで1つのlockを先に取ったものが流し、後のもの（人の`install`を含む）はlockが空くまで待つ。待つrunはslotとsessionを持ったまま順に待つ（待ちを数えて見えるようにする）。本数を増やすのは人の判断で、別に決める。
5. **関門の印はruntimeが効かせる（ADR-t1165-1決定6をamends）。** runのworktreeの印（ADR-t1165-1決定2）を、workerではなくこの工程が読み、流し直しでも落ちたtestが全て効いている印を持てば通して印で通したと記録する。ただし、runの差分がその印のtestを変えるとき、taskがその印を直すtaskのときは効かせない（決定6の条件のまま）。期限・上限・続けての失敗の歯止め（決定3）も同じに効かせ、続けての失敗はこの工程と関門の記録を合わせて同じtestごとに数える。podmanに繋がらないときの扱い（ADR-t1162-1）もこの工程に同じに当てはめる。
6. **Codexのworkerの除外は要らなくなる（ADR-t963-1決定5をamends）。** e2eはsandboxの外のhostで全件を流すので、providerに依らず除外は無い。決定5の除外とevidenceの書式は、この工程が着地するまでの間だけ使う。
7. **移行。** この工程の実装が着地するまでは、ADR-t963-1決定2・5とADR-t1165-1決定6の今の動き（workerが流す）を続ける。実装が着地したら、workerのpromptとresumeの依頼、AGENTS.mdとpluginのskillのe2eの規則を同じ変更で改める。

ADR-t963-1の決定1（自動更新と`install`の関門）・決定3（`[e2e] paths`の範囲）・決定4（integrateでは流さない）と、ADR-t1165-1の決定1〜5は変えない。工程の名前・eventのkind・欄名・待ちの数え方・logの場所・知らせの閾値は実装のtaskが[Validation](../design/supervisor-lifecycle/validation.md)などの`docs/design/`に書く。

## Alternatives

- **案B: 人が承認してから流す**: 着地のたびに人の判断が要り、待ちが人の応答に縛られる。
- **workerが流し続ける**: コンテナとsandboxの中で走らず、隔離に進めない。Codexの除外の規則も残る。
- **integrateで流す**: 直列の着地の上限が下がり、実cmuxの不安定な失敗が全部の着地を止める（ADR-t963-1決定4の理由）。
- **関門だけにしてrunごとには流さない**: 狭い範囲の不具合が関門で初めて見つかり、どのcommitかの切り分けに時間がかかり自動更新が止まる（ADR-t963-1のAlternatives）。
- **e2eもコンテナや実行側で流す**: e2eは実cmuxと実プロセスを操作するのでコンテナの中では走らない。
- **複数本を並べて流す**: cmuxのworkspaceと負荷の取り合いで不安定な失敗が増える。測ってから決める。

## Consequences

- workerの時間からe2eが抜け、Codexのworkerも除外なしで同じ規則になる。task 1209（除外をこのrepositoryに限る）は要らなくなる。
- e2eが要るrunはreviewのpassの後に待ちとe2eの分だけ着地が遅れ、そのあいだslotを持つ。1本ずつなので、要るrunが重なると待ちが伸びる。
- e2eの失敗がreviewの後に見つかると、resumeの後にもう一度reviewを通る。
- この工程はworkerが書いたコード（build.rs・proc-macro・test）を制御側のhostで動かす。実行側の隔離（ADR-t1233-1決定1、goal 38の段(4)〜(6)）の後もここだけはhostに残る例外で、reviewのpassの後に限ることで受け入れる。hostの秘密やpushの権限に届きうる危険は残り、e2eの環境を絞る（envや資格情報を渡さない）ことは実装のtaskが扱う。

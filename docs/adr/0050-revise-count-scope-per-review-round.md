---
id: adr-0050
type: adr
title: reviseの回数をresume・send_back・衝突の解消で区切ったreviewの回ごとに数え、run全体の往復はresumeの上限で抑える（ADR-0027決定2をamends）
status: accepted
created: 2026-09-27
updated: 2026-09-27
accepted_on: 2026-09-27
amends:
  - adr-0027 decision 2
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
related:
  - adr-0027
  - adr-0044
  - adr-0047
  - design-supervisor-lifecycle-review
  - design-supervisor-lifecycle-needs-session
  - design-supervisor-lifecycle-plan-review
---

# ADR-0050: reviseの回数をresume・send_back・衝突の解消で区切ったreviewの回ごとに数え、run全体の往復はresumeの上限で抑える（ADR-0027決定2をamends）

## Context

[ADR-0027](0027-keep-worker-session-through-review-revise-verdict-and-merge-tree-precheck.md)の決定2は「reviseはrunごとに2回まで。3回目のreviewがpassでなければconcernとして扱う」とだけ書き、resumeや人の`send_back`の後にreviewをやり直すとき数え直すかを書いていない。task 107の実装は、runの全期間の`revise_requested`（送れずに取り消した`revise_unsent`を引く）を数える（`domain::decide_revise`）。

reviewがやり直されるのは、reviseの往復の他に次の経路がある。

- 人が`approve_landing`に`send_back`と答え、runが`needs_session`になってresumeする。
- 着地のrebaseの衝突・rebase後の検証の失敗・landing recheck・復旧jobの`resume`で`needs_session`からresumeする。
- passの後の衝突の事前判定（ADR-0027の決定4）が生きているsessionに解消依頼を送り、sessionがrebaseしてreceiptを書き直す。

どれもworkerがreview済みの差分の外で新しい変更（人の指摘への対応、rebaseと衝突の解消、検証の修正）を加え、その変更を新しくreviewする。前の回で2回reviseしたrunでは、新しい変更への機械的な指摘（fmt、test不足、receiptの食い違い）にも1回もreviseできず、すぐ`approve_landing`のaskになる。人が`send_back`した直後に同じrunのaskがもう一度来ることもあり、人の判断が要らない指摘で人を待たせる。これはADR-0027の原則（sessionで直せる指摘はsessionで直す）と合わない。

一方、reviseの上限は、reviewとworkerが同じ指摘で往復し続けないための柵でもある。数え直すなら、run全体の往復が有限であることを別の仕組みで保つ必要がある。

## Decision

1. **reviseの回数はreviewの回ごとに数える。** reviewの回は、runの最初のreviewか、下の区切りの後の最初のreviewから始まり、次の区切りまで続く。回の中で送ったrevise（取り消したものを除く）が2回に達していれば、その回の次のreviewがpassでないとき（reviseでも）concernとして扱う。回の中の数え方と上限の値（2）はADR-0027の決定2のまま変えない。
2. **区切りは、reviseの往復の外でworkerに新しい変更を求めた時点にする。** 次の3つのどれかがrunのイベントにあれば、それより前の`revise_requested`は数えない。
   - resumeの開始（`resume_started`）。理由を問わない（人の`send_back`、着地の衝突・検証の失敗、landing recheck、復旧jobの`resume`）。
   - 人の`send_back`の適用（`status: needs_session`の`landing_decided`）。直後のresumeでも区切られるが、resumeが始まる前に数える経路でも同じ結果になるよう区切りに含める。
   - 衝突の事前判定の解消依頼の完了（`conflict_resolved`）。passの後にsessionの中でrebaseしたもので、resumeを経ない。
   resumeが解決せずに`validating`へ戻す経路（resumeの見送り）は、その前に始まったresumeで既に区切られている。reviseの往復そのもの（`revise_finished`の後のreview）と、reviewの失敗・読めないverdictのやり直しは区切りにしない。区切りはrunのイベントだけから決めるので、supervisorの引き継ぎでも同じ数になる。
3. **run全体の往復に別の上限は置かず、resumeの上限で抑える。** 区切りを作る経路はどれも既に上限を持つ: 数えるresume（`send_back`・検証の失敗・復旧jobのresumeなど）は`MAX_RESUME_ATTEMPTS`、衝突だけの試行（衝突だけのresumeと事前判定の解消依頼の和）は`[resume] conflict_only_limit`（[ADR-0047](0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)の決定24とその後の設定）。reviewの回の数はこれらの和に1を足したもので抑えられ、reviseの総数は回の数の2倍を超えない。`send_back`の区切りは人の答えを経るので、人の知らないまま回り続けることはない。
4. **plan reviewの`MAX_PLAN_REVISES`は変えない。** proposalのreviseは今までどおりproposalの全期間で数える。[ADR-0044](0044-findings-proposals-from-findings-and-quiet-observer.md)の決定11が「runのreviewの`MAX_REVISE_ATTEMPTS`と同じ」としたのは上限の値（2）で、この決定はその値も`MAX_PLAN_REVISES`との結び付きも変えない。

## Alternatives

- **run全体で数える（今の実装のまま）**: 上限が単純で往復の総数も小さいが、Contextの問題（新しい変更への機械的な指摘がすぐaskになる）が残る。人の`send_back`の後に続けてaskが来るのは、人の判断を増やすだけである。
- **区切りを人の`send_back`だけにする**: 人が関わった後だけ数え直す案。着地の衝突や検証の失敗のresumeは人を経ずに起き、その後のreviewが最もよく新しい指摘を出す（rebaseで入ったmainの変更と合わせた差分を見る）ので、問題の大半が残る。
- **run全体の上限を別に置く（例: run全体で4回）**: 往復の総数を小さく保てるが、上限に達した後は区切りの後でもすぐaskになり、同じ問題が遅れて起きる。区切りを作る経路は既に上限を持つので、2つ目の柵は要らない。
- **plan reviewも回ごとに数える**: proposalには自動のやり直し（resumeや衝突の解消）が無く、上限の後にreviewがやり直されるのは人が`approve_plan`に`send_back`と答えたときだけである。その後のconcernは、差し戻した人が次の判断をする場になる。plannerの書き直しは計画の意図がずれやすく、reviseの往復を短く保つ方を選ぶ。plan reviewで同じ問題が数字で見えたら、別のADRで決める。

## Consequences

- 今の実装（`domain::decide_revise`が`revise_requested`をrunの全期間で数える）はこの決定と違う。区切りの後の`revise_requested`だけを数える実装は後続のruntimeのtaskが行う。それまでのruntimeはrun全体で数える。
- `revise_requested`の`attempt`は、依頼の文面のファイル（`revise-<attempt>.txt`）と引き継ぎの錨に使われているので、run全体で一意なままにする。回の中の何回目かは別に求める（実装の詳細は[Review](../design/supervisor-lifecycle/review.md)に置く）。
- reviseの総数は増えうる（既定の上限では、区切りの数によって1つのrunで2回より多くなる）。reviewとworkerの往復が長引くrunは`dagq events --run <run> --kind revise_requested`で見る。
- ADR-0027の決定2のうち「runごとに2回まで」の範囲だけをこのADRが変え、他の部分（verdictの3値、reviseの内容、concernの扱い）はADR-0027のまま有効である。

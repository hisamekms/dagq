---
id: adr-t2080-1
type: adr
title: workerの差し戻し（revise・send_back）・needs_sessionのresume・triageのresumeで、直前のturnのpeak_contextが設定の閾値を超えていたら、同じworktree・branch・runのまま会話を引き継がない新しいsessionを引き継ぎのpromptで始める（ADR-0027決定2・3、ADR-t813-2決定4をamends）
status: accepted
created: 2026-10-08
updated: 2026-10-08
accepted_on: 2026-10-08
amends:
  - adr-0027 decision 2
  - adr-0027 decision 3
  - adr-t813-2 decision 4
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - worker
related:
  - adr-0027
  - adr-t813-2
  - adr-t947-1
  - adr-t2072-1
  - adr-t1521-1
  - adr-t598-1
  - adr-t1942-1
  - design-provider-lifecycle
---

# ADR-t2080-1: 差し戻しとresumeで直前のturnのpeak_contextが閾値を超えていたら新しいsessionで引き継いで続ける（ADR-0027決定2・3、ADR-t813-2決定4をamends）

## Context

[ADR-0027](0027-keep-worker-session-through-review-revise-verdict-and-merge-tree-precheck.md)決定2・3は、reviewのreviseを生きているworkerのsessionに返し、自動resumeで開き直したsessionも同じ扱いにすると決めた。triageのresumeも同じsessionに足す。[ADR-t813-2](2026-09-28-t813-2-provider-per-task-and-mutual-fallback.md)決定4は、同じworktreeで会話を引き継がない新しいsessionをresumeのpromptで始める仕組みを持つが、それを使うのはproviderが使えないときの切り替えだけという前提だった。

この結果、差し戻しとresumeのたびに会話が足され続け、差し戻し後の呼び出しのcontextは平均321kに膨らむ。人のsession（2026-10-07）が10/01〜10/07に着地したClaudeのworkerのrun 380件のtranscriptを見ると、差し戻しのあるrunは194件で、workerの入力token 8.6Bのうち差し戻し後が3.3B（38%）を占めた。

判定の入力には、Execution（呼び出し）ごとに記録するpeak_context（task 1889）を使える。contextを減らす取り組みの段1（goal 172、[ADR-t2072-1](2026-10-08-t2072-1-worker-prompts-and-next-turns-carry-decision-material-within-limits.md)）は、workerのpromptと次のturnの文にbyteの上限を付けた。この決定は段2-bで、人は2026-10-07に推奨の方向で進め、Lのtaskの分割は一旦しないと決めた。

## Decision

1. **差し戻しとresumeで直前のturnのpeak_contextが閾値を超えていたら、新しいsessionで続ける（ADR-0027決定2・3をamends）。** 対象はworkerのrunの差し戻し（reviewのrevise・send_back）、`needs_session`のresume、triageのresume。そのとき、そのrunのworkerの直前のturnのpeak_contextが閾値を超えていれば、生きている（か開き直す）sessionに足さず、同じworktree・branch・runのまま会話を引き継がない新しいsessionを始める。閾値以下なら今までどおり同じsessionに返す。ADR-0027決定2・3のうち、reviseを生きているsessionに返すこと・自動resumeで開き直したsessionも同じ扱いにすることを、この条件の下でだけ変える。reviseの回数の上限、`/exit`の位置、merge-treeの判定などの他の決定は変えない。
2. **新しいsessionの最初の呼び出しは引き継ぎのpromptで始める。** 引き継ぎのpromptは、taskのprompt、今までのcommit、worktreeの未commitの変更、reviewの理由、receiptの要約、送るはずだった依頼（reviseの理由・resumeの依頼・triageの指示）を持ち、段1の上限（ADR-t2072-1）の内に収める。会話のtranscriptは渡さない（worktreeとcommitが作業の状態を持つ）。
3. **閾値は`dagq.toml`の設定で決める。** 閾値の値はADRにもdesignにも書かず、設定の定義のそばのdoc commentとこのrepositoryの`dagq.toml`が持つ（[ADR-t598-1](2026-09-26-t598-1-adr-id-is-task-id-small-adrs-and-design-holds-current-state.md)決定2・3、[ADR-t1942-1](2026-10-07-t1942-1-design-docs-in-four-layers-with-size-budgets.md)決定2・3）。
4. **判定の入力は直前のturnのpeak_contextだけにする。** peak_contextが未計測（null）なら切り替えない。reviewのreason code・差し戻しの分類などの分類コードを判定に使わない（[ADR-t947-1](2026-09-28-t947-1-review-verdicts-carry-reason-codes.md)決定3は変えない）。判定はruntimeのdomainの関数が行う。
5. **sessionの区切りはeventに残し、providerの切り替えの新しいsessionと同じ仕組みの延長にする（ADR-t813-2決定4をamends）。** ADR-t813-2決定4が置く、新しいsessionを始めるのはproviderが使えないときの切り替えだけという前提を外し、この決定の条件でも同じworktreeとbranchで新しいsessionを始める。区切りと理由、新しいsessionの名前をeventに記録する。providerは変えない（providerの切り替えは今までどおりADR-t813-2決定2〜4）。ADR-t813-2の他の決定は変えない。

eventの種類と欄、設定のkeyの名前、引き継ぎのpromptの節の形と上限は、後続の実装のtaskがコードのdoc commentと`docs/design/`（provider-lifecycle・review・triage・needs-sessionなど）に挙動と一緒に書く。

## Alternatives

- **差し戻しをlocal・moderate・fundamentalに分類し、分類で扱いを決める**: 分類はLLMの判断で揺れ、分類コードを判定に使わない約束（ADR-t947-1決定3）に反する。膨らんだcontextの費用は分類に関係なくかかり、peak_contextで直接判定できる。
- **「仮説」を独立した対象にして、誤りの差し戻しで捨てる**: 根本の誤りは再計画（[ADR-t1521-1](2026-10-05-t1521-1-diagnose-before-replanning-and-preserve-acceptance.md)・ADR-t1521-2）が扱う。新しい対象と状態を足すほどの得が見えない。
- **同じ指摘の繰り返しを判定して切り替える**: 「同じ」の判定に分類か文の比較が要り、揺れる。contextの膨らみは繰り返しが無くても起きる。
- **shadow executionでA/Bを取ってから決める**: workerを2重に動かす費用が大きく、worktreeとslotの扱いも要る。閾値の効果は切り替えた後のpeak_contextと入力tokenで測れる。
- **Lのtaskを一律に分割する**: taskが大きいほど行あたりの費用は安く、一律の分割は総費用を増やす見込み。人が一旦しないと決めた。

## Consequences

- 人の試算（上のrun 380件、ある閾値で133 runが切り替わる）では、workerの入力tokenは13〜20%減（費用で1割強）、差し戻し後のpeak>200kの呼び出しは102件から10〜12件に減る。品質の得（膨らんだcontextでの取り違えが減ること）は兆しだけで、期待値に入れない。
- 新しいsessionは会話の文脈を失うので、turnが増えうる。切り替えの回数と、切り替えた後のpeak_context・入力token・差し戻しの回数をeventで見る。
- 効果を測ってから、`needs_session`のresumeの回数やwrong_premiseの差し戻しの後を切り替えの条件に足すか、peak_contextと差し戻しの関係を見直すかを決める。それまで条件はpeak_contextだけにする。
- reviseの送信・resumeの開始・triageへの差し込みと、sessionのidleとreceiptの待ち・世代の柵との組み合わせは実装のtaskがtestで確かめる。

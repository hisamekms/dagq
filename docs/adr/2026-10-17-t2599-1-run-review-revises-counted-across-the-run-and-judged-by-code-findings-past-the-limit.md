---
id: adr-t2599-1
type: adr
title: runのreviewの自動の手直しをrunを通して数え、上限の後に差し戻すreviewは理由のコードでコードの指摘が無ければ着地させ残りをfollow_upのdraftに、あれば人のapprove_landingにする（ADR-0050・ADR-t2256-1を置き換え、ADR-0027決定2・ADR-t947-1決定3・6・ADR-t451-1決定1・3・ADR-t1453-1決定7・ADR-0047決定11をamends）
status: accepted
created: 2026-10-17
updated: 2026-10-17
accepted_on: 2026-10-17
amended_by:
  - adr-t3033-1
supersedes:
  - adr-0050
  - adr-t2256-1
amends:
  - adr-0027 decision 2
  - adr-t947-1 decision 3
  - adr-t947-1 decision 6
  - adr-t451-1 decision 1
  - adr-t451-1 decision 3
  - adr-t1453-1 decision 7
  - adr-0047 decision 11
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - review
related:
  - adr-0027
  - adr-0050
  - adr-t2256-1
  - adr-t947-1
  - adr-t947-3
  - adr-t451-1
  - adr-t1453-1
  - adr-t1906-1
  - adr-t2352-1
  - adr-t1942-1
  - adr-t2257-1
  - design-supervisor-lifecycle-review
---

# ADR-t2599-1: runのreviewの自動の手直しをrunを通して数え、上限の後はコードの指摘の有無で着地か人の判断かを決める

## Context

runのreviewは、`revise`と上限に数える差し戻しをreviewの回（resume・人の`send_back`・衝突の解消で区切る、[ADR-0050](0050-revise-count-scope-per-review-round.md)）ごとに上限まで生きているsessionに返し（上限の値は[ADR-t2256-1](2026-10-11-t2256-1-raise-the-run-review-revise-limit-and-decouple-plan-review.md)）、超えると`approve_landing`のaskで人に上げていた。回が変わるたびに数え直すので、run全体の往復は長い。

inboxの数字（2026-10-12以降の手直しの直後のreview 189回）では、回の中の手直しの後に通る率は1回目13%・2回目55%・3回目52%・4回目44%で、2回を過ぎると上がらない。inboxは手直しの後に残る指摘の多くを書式と文書の消し残しと見て、上限のaskに`land`と答えていた（goal 194向けのinboxの自動の回答）。inboxはrunを通して数えた3回目以降のreview 131回にこの規則を当てはめて人に示した。

inboxの内訳と当てはめの結果はqueueに残っていないので、inboxが示した131回の母集団で数え直した。期間は2026-10-12T00:00Zから2026-10-16T13:00Z（この窓で3回目以降のreviewがちょうど131回になる。最後の1回は10-16T10:33Z）。出どころは`dagq events`の`review_finished`・`revise_requested`・`resume_started`で、回の区切りはresume。数え方は回ごとの通る率（13%・55%・52%）を再現するが、この窓の手直しの直後のreviewは175回で、inboxの189回は再現しない。

- 回の中の手直しの直後のreview 175回のうち、着地に進まなかったものは84回。うち21回はagentが行き先を決めたもので、agentの理由のコードは記録されていなかった。残る全体のreviewの63回の理由（コード付き106項目、1項目が複数のコードを持つ）のコードは、`acceptance_unmet` 51、`docs_drift` 44、`test_gap` 27、`code_defect` 22、`repo_rule_violation` 17、`local_slip` 4、`adr_design_mismatch` 4、`out_of_scope_change` 1、`other` 1。63回のうち45回がコードの指摘を持っていた。
- runを通して3回目以降のreview 131回は、着地64回、人の判断3回、差し戻し64回。差し戻し64回にこの規則を当てはめると、収束して着地するのは11回（コードは`docs_drift` 10・`repo_rule_violation` 4・`local_slip` 1）。ほかの53回は人のaskになる: agentが決めた18回（コードが記録されていない）と、全体のreviewの理由にコードの指摘がある35回（`acceptance_unmet` 25・`test_gap` 18・`code_defect` 16・`adr_design_mismatch` 4）。`acceptance_unmet`の多くが文書の理由なら、pathで分けることで着地に回る数が増える。

人の言葉（request 117）:「2回目まで全ての指摘を直す。3回目はコードの指摘が収束していれば承認、そうでない場合は人が承認した上で4回目に回す。以降は同じ」。人はinboxの推奨（収束はその回のコードの指摘が0件、判定は理由の分類、ADRとの食い違いとsecurityも収束していない側、残ったコード以外の指摘はfollow_upのdraft、回数はrunを通す、人の承認は今の`approve_landing`の形）をokした。

理由の分類コード（[ADR-t947-1](2026-09-28-t947-1-review-verdicts-carry-reason-codes.md)）は集計のためだけにあり、判定と適用に使わないとしていた（決定3・6）。

## Decision

1. **自動の手直しはrunを通して数え、上限まで生きているsessionに返す。** 数えるのは自動の手直しの全部（reviewの`revise`、concernの`high`・`reason_category`無しの`send_back`、プログラムのreviewの差し戻し、着地の前のagent evalの差し戻し）。resume（着地の検証の失敗・rebaseの衝突・人の`send_back`）と衝突の解消で数え直さない（ADR-0050決定1・2の回の区切りを廃する）。resumeの後のreviewも、自動の手直しが残っていれば今どおり手直しに返す。上限の値は人の言葉に従い、その置き場は定数のdoc comment（[ADR-t1942-1](2026-10-07-t1942-1-design-docs-in-four-layers-with-size-budgets.md)）。ADR-0027決定2の上限の数え方をこれに改める。
2. **run全体の往復の柵は、自動の手直しの上限、上限の後は判定（決定3〜6）を経てしか差し戻さないこと、resumeの上限が持つ（ADR-0050決定3の後継）。** 上限の後の差し戻しは人の`send_back`の答えを経るか、着地に進むかのどちらかで、人の知らないまま往復が続くことはない。
3. **上限に達した後にreviewが差し戻す（`revise`か、上限に数えるconcernの`send_back`）とき、runtimeはその回の理由の分類で「コードの指摘が収束したか」を決める。** 収束したと数えるのは、全ての理由の全てのコード（主のコードに限らない）が収束した側として列挙したコード（`local_slip`・`docs_drift`・`acceptance_ambiguous`・`acceptance_conflict`・`acceptance_infeasible`・`out_of_scope_change`・`repo_rule_violation`）のどれかで、securityの役のreviewのagent（built-inのsecurityと、projectがsecurityの名で置くagent）の指摘を含まないときだけ。それ以外は収束していない: `code_defect`・`test_gap`・`adr_conflict`・`adr_design_mismatch`・`acceptance_unmet`・`other`、コードの無い（unlabeled）理由、どの一覧にも無いコード（誤記を含む）を1つでも持つ理由（既知の非コードのコードと混ざっていても）、理由の無い差し戻し。迷うものは収束していない側に倒す。securityのagentの指摘を緩めて扱わないこと（[ADR-t1906-1](2026-10-14-t1906-1-security-review-agent-findings-follow-the-concern-rules.md)）は変えない。
4. **`acceptance_unmet`はpathで分ける。** 理由がpathを持ち、repositoryが設定で名指したコードのpathに当たる`acceptance_unmet`だけをコードの指摘にし、それ以外の`acceptance_unmet`は収束した側に数える。設定を書かないprojectとpathの無い理由では、いつもコードの指摘に数える。コードのpathはruntimeに固定しない（[ADR-t2257-1](2026-10-12-t2257-1-container-runtime-takes-project-and-host-values-from-config.md)）。設定のkeyの名前・書式・既定は設定の型のdoc commentが、読む時点と誤りの扱いはdesignが持つ。
5. **収束したら、runtimeはrunを承認して着地へ進め、その回の理由をfollow_upのdraftにする。** 着地はreviewのpassと`concern`の`land`と同じ道で、衝突の事前判定・e2e・`integrate`の検証を省かない。draftはreviewのverdictの`follow_ups`をdraftにする道（[ADR-t2352-1](2026-10-16-t2352-1-review-sends-back-only-documents-the-diff-touched-and-follows-up-the-rest.md)）で作り、分類は理由のコードから[ADR-t947-3](2026-09-28-t947-3-follow-ups-carry-category-codes.md)のcategoryに対応づける。判定は記録に残し、その記録がrunを着地させてよいreviewに数える。
6. **収束していなければ、今の`approve_landing`のask（`land` / `send_back` / `cancel`）にする。** 問いにはコードの指摘が残っていることと、残ったコードの指摘を書く。`send_back`の後の次のreviewも決定3〜6で判定し、自動の手直しは増やさない。
7. **コードを読むのは決定3の判定だけにする（ADR-t947-1決定3・6をamends）。** 欠けたコードを「未分類」に、知らない値をその値のまま記録することは変えない。コードの一覧への追加・削除は今どおりdesignと実装の変更でよいが、新しいコードは収束した側の列挙に入るまで収束していない側に数える（fail closed）。収束した側の列挙を変えるのはADR（このADRのamends）で行う。verdictのpass・revise・concernの判定、concernの適用、plan reviewの判定はコードを読まない。
8. **上限の後の扱いを改める範囲。** [ADR-t451-1](2026-10-02-t451-1-ai-decides-recommendable-asks-and-escalates-only-the-undecidable.md)決定1の(c)の「reviseの上限」と決定3の「reviseの上限超えは`approve_landing`のask」、[ADR-t1453-1](2026-10-03-t1453-1-review-subagents-named-by-path-run-inside-the-review-job.md)決定7の「上限を超えれば人の判断」を、決定3〜6に改める。ADR-t451-1決定3の`scope`・`discard`・`low`・agentが動かなかったものを人に回すこと、ADR-t1453-1決定7の最も重い行き先の選び方とagentの判定を親の集約で弱めないことは変えない。
9. **ADR-0050とADR-t2256-1のうち有効な決定を引き継ぐ。** concernの`land`・`scope`・`discard`・`low`・推奨なしの判断はreviseの残数に依らず（`high`・`reason_category`無しの`land`は上限の後も適用する）、verdictは`pass` / `revise` / `concern`の3値のまま。plan reviewの差し戻しの上限は、値もproposalの全期間で数えることも変えず、runのreviewの上限と連動させない（ADR-0050決定4とADR-t2256-1決定3。ADR-0047決定11の「runのreviewの上限と同じ」を外したamendsもこの形で引き継ぐ）。

## Alternatives

- **回ごとの数え方のまま上限を下げる**: resumeのたびに数え直すので、run全体の往復はresumeの数だけ長いまま残る。通る率が上がらないのは回の中でもrunの通しでも同じで、数え直す理由が無い。
- **上限の後をいつも人に上げる（今のまま）**: 書式と文書の消し残しだけの回で人を待たせ、inboxが自動で`land`と答える運用が要る。答えが決まっているものは人に上げない（ADR-t451-1決定1）。
- **上限の後をいつも着地させる**: コードの欠陥・testの不足・ADRとの食い違い・securityの指摘を人の知らないまま着地させる。goal 199の制約に反する。
- **収束していない側を列挙する（denylist）**: 新しいコードや誤記、`other`・unlabeledが収束した側に倒れ、コードの指摘を自動で着地させうる。ADR-t947-1決定6で一覧はADRなしに増えるので、列挙する側を収束した側にしてfail closedにする。
- **`other`を収束した側に入れる**: 意味が自由文にしか無く、runtimeがコードの指摘か分けられない。

## Consequences

- runの往復は短くなる: 自動の手直しはrunを通した上限までで、その後は着地か人の判断のどちらかになる。
- 上限の後に残った書式・文書・受け入れ条件の読み方の指摘は、着地を止めずにfollow_upのdraftとしてplannerに渡る。draftが増える分、plannerの仕分けが増える。
- goal 194向けにinboxが`approve_landing`に自動で答えている運用は、この仕組みに吸収される（inboxがこの決定の着地の後に廃止する）。
- `acceptance_unmet`をpathで分ける実装と設定が入るまでは、`acceptance_unmet`はいつも収束していない側に数える。迷う側に倒すので、誤って着地させることはない。
- jobが付けるコードの誤りが着地の判断に効くようになる。コードを付け忘れた理由と誤記は収束していない側に倒れ、人に上がる。

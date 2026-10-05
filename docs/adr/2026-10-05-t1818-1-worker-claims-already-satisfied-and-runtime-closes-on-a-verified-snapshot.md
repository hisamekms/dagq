---
id: adr-t1818-1
type: adr
title: workerは「baseがすでに受け入れ条件を満たす」を根拠つきの正式な結果で返し、runtimeは着地せずmainのheadのsnapshotでverifyとreviewの判定を行い、highのpassか人の答えでcommitなしのcompletedとして閉じ、着地と別に数える
status: accepted
created: 2026-10-05
updated: 2026-10-05
accepted_on: 2026-10-05
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - measurement
related:
  - adr-0047
  - adr-t451-1
  - adr-t947-4
  - adr-t1818-2
  - design-supervisor-lifecycle-integrate
  - design-supervisor-lifecycle-validation
  - design-supervisor-lifecycle-review
  - design-supervisor-lifecycle-triage
  - design-domain-model
---

# ADR-t1818-1: workerは「baseがすでに受け入れ条件を満たす」を根拠つきの正式な結果で返し、runtimeは着地せずmainのheadのsnapshotでverifyとreviewの判定を行い、highのpassか人の答えでcommitなしのcompletedとして閉じ、着地と別に数える

## Context

task 1217のworkerは、着手したらbaseのmainがすでに受け入れ条件を満たしていると気づいた。receiptの結果は`succeeded`と`failed`だけで、commitの無いreceiptは受理の検査（[Domain model](../design/domain-model.md)のReceiptの判定の順序）で`commit_mismatch`になり、runは`failed`で復旧jobに回った。復旧jobはruntimeが適用できないoption「Mark task 1217 done (or cancel it as obsolete)」を足し（ask 436）、inboxがそれを選んでも何も動かなかった（[Triage](../design/supervisor-lifecycle/triage.md)の6・9。optionの扱いは[ADR-t1818-2](2026-10-05-t1818-2-recovery-job-options-map-to-runtime-options-or-pass-back.md)）。

人はrequest 27で、この主張を正式な結果にし（goal 141のA）、閉じ方をADRで決めると求めた。制約は次のとおり。

- workerは全体の`cargo test`と`cargo llvm-cov`（coverageの関門）を手元で流さない（[local-checks](../development/local-checks.md)の「workerの手元の検証」）。taskのverifyの全体はruntimeが流す。
- 主張の判定は推奨と確信度を出せるのでAIが決め、決めきれないものだけを人に上げる（[ADR-t451-1](2026-10-02-t451-1-ai-decides-recommendable-asks-and-escalates-only-the-undecidable.md)）。
- mainは人の答えを待つ数時間の間に何度も進む。古いheadで判定した主張を新しいheadで閉じると、間に入った変更で受け入れ条件が崩れていても気づけない。一方で、headが進むたびに人のaskを開き直すと閉じられなくなる。

## Decision

1. **workerは「差分が要らない」を根拠つきの正式な結果としてreceiptに書ける。根拠の無い主張は受け付けない。** 主張には、baseで行った受け入れ条件の確かめの根拠を必須にする。根拠はlocal-checksの「workerの手元の検証」でworkerに許された確かめ（fmt・clippy・関係するtest・taskのverifyのうちcoverageの関門と全体の`cargo test`を除くもの、受け入れ条件の対応づけ）の結果に限る。workerに禁じた関門（`cargo llvm-cov`・全体の`cargo test`）の実行結果は受理の条件にしない。根拠の無い主張はこの経路に乗せず、sessionに返すかfailedにする（どちらにするかと、結果の名前・綴り・receiptの形は実装のtask 1820がdesignで決める）。例外は、workerが主張を書けずにfailedになったrunを復旧jobがこの経路に渡すとき（[ADR-t1818-2](2026-10-05-t1818-2-recovery-job-options-map-to-runtime-options-or-pass-back.md)決定3）で、そのときはreceiptに残った確かめとjobの見立てを根拠に代える。どちらの入口でも、決定2〜4のsnapshotのverifyとreviewの判定を経ずには閉じない。
2. **runtimeは着地せず、検証の時点のmainのheadを1つ決めて（snapshot）eventに残し、そのheadを検証と判定の対象に固定する。** runtimeはsnapshotでtaskのverification_commandsの全体（coverageの関門を含む）を流す。reviewの判定（決定4）と人のask（決定5）も同じsnapshotを対象にし、askとeventにそのheadを載せる。snapshotはeventに残すので、supervisorが再起動しても同じ比較ができる。差分が無いのでrebase・e2e・mainへの書き込みは無い。
3. **閉じる時点でmainのheadがsnapshotと同じかを確かめ直し、進んでいれば古い判定では閉じない。** reviewの間・人の答えの前・supervisorの再起動の後のどれで進んだときも同じに扱う。やり直すものは閉じる経路で分ける。
   - verifyは、閉じる時点のheadを新しいsnapshotにして1回流し直す。通ればその新しいsnapshotを確かめたheadとして閉じてよく、流し直しの間にmainがさらに進んでも、もう一度は流さない（流し直したverifyが最も新しい機械の確かめで、毎回の再確認を求めるとcoverageの関門を含むverifyの間に進むmainに追いつけず閉じられない）。落ちれば閉じず、人のaskに落ちたことを書いて上げる（既にaskが開いていればそこに書く）。
   - reviewのhighのpassで閉じる経路では、新しいsnapshotでverifyを流し直した後にreviewもやり直し、そのpassでまた閉じる時点の確かめに戻る。reviewのやり直しには小さな上限を置き（値はdesign）、超えたら閉じずに人のaskに上げる。
   - 人の答えの「閉じる」は、上のverifyの流し直しが通れば適用してよい。人の判断は受け入れ条件の意味の判断で、機械の確かめだけを新しいheadでやり直す。reviewはやり直さず、askも開き直さない。
   - 閉じたときは、どのheadで何（verify・review・人の答え）を確かめて閉じたかをeventに残す。
4. **review jobが「差分なしで受け入れ条件を満たす」主張を判定し、`high`のpassならruntimeが閉じ、それ以外は人に上げる。** `low`のpass・revise・concern・確信度が無いか読めない・jobの失敗・snapshotのverifyの失敗は、optionsと推奨を持つ人のaskにする（人が要る理由は決定41の分類で、主張の判定は`scope`、jobの失敗は`recovery_failed`）。concernを`high`の推奨でもAIに適用させないのは、差分の無いtaskを閉じることは依存するtaskを進め、誤りが後ろに広がるので、passの`high`だけを閉じる条件にするためである。passの確信度はこの判定に限って読み、通常のrunのreviewのpassが確信度を読まないこと（[Review](../design/supervisor-lifecycle/review.md)の「verdictの欄」）は変えない。
5. **閉じ方はcommitなしの`completed`にする。** 依存するtaskは満たされて進める。commitの代わりに、receiptの根拠と、決定2〜4のsnapshot・verify・判定・人の答えをeventに残す。`cancel --reason already_done`（[ADR-t947-4](2026-09-28-t947-4-cancel-carries-a-reason-code.md)）は使わない。
6. **集計では着地に入れず、別に数える。** 着地数・`stats`・`kpi`は、この閉じ方を着地（commitを持つ`run_integrated`）と分けて数え、runの結果としても別に見えるようにする。欄の名前と出し方はtask 1820がdesignに書く。
7. **既存のADRの番号付きの決定は変えない。** receiptの受理の検査の順序はdesign（Domain model・[integrate](../design/supervisor-lifecycle/integrate.md)の3・[Validation](../design/supervisor-lifecycle/validation.md)）が持ち、この経路の分岐はそこに足す。workerが主張を書いたrunは`failed`にならないので、ADR-0047決定3（failedのrunを復旧jobにかける）は変えない。主張を書けずにfailedになったrunを復旧jobからこの経路に渡す操作は、決定40と[ADR-t813-1](2026-09-28-t813-1-headless-worker-path.md)決定9をamendsするADR-t1818-2が決める。askの理由の分類（決定41）、ADR-t451-1決定3（reviewのconcern）、ADR-t947-4（cancelの理由）も変えない。

## Alternatives

- **`cancel --reason already_done`で閉じる**: ADR-t947-4は`already_done`に中身を受け持つtaskを`--duplicate-of`で指すことを求めるが、受け入れ条件を満たした変更がどのtaskの着地かは分からないことが多い。cancelは依存するtaskを満たさないので、後ろのtaskが止まる。
- **workerの根拠だけで閉じる**: workerはcoverageの関門と全体のtestを流さず、baseはclaimの時点のheadで古い。runtimeのverifyとreviewの判定を経ずに閉じると、間違った主張がそのまま依存を外す。
- **workerにcoverageの関門と全体のtestを流させて根拠にする**: local-checksの規則（workerは流さない）と食い違い、hostの負荷が増える。runtimeがsnapshotで1回流せば足りる。
- **headが進むたびに人のaskを開き直す**: mainは答えを待つ間に何度も進むので閉じられなくなる。人の判断は受け入れ条件の意味の判断で、headに依らない部分は保てる。
- **最初のsnapshotの判定のまま閉じる**: 間に入った変更で受け入れ条件が崩れても気づけない。
- **常に人のaskにする**: 推奨と確信度が出せる判断を人に上げることになり、ADR-t451-1に反する。

## Consequences

- workerは差分の要らないtaskを`failed`にせず閉じられ、復旧jobと人のaskを経る回り道が減る。
- runtimeはsnapshotのverifyを流す分のslotと時間を使い、mainが速く進むとやり直しが増える。上限で人に上げるので止まらない。
- `completed`のtaskにcommitの無いものができる。読み手（`show`・依存の判定・集計）はcommitの有無で着地と区別する。
- 実装はgoal 141のtask 1819〜1822が行い、receiptの形・結果の綴り・eventの種類と欄・askのoptions・CLIの表示・上限の値はdesignに書く。

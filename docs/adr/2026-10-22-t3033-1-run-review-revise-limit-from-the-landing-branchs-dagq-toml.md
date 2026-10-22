---
id: adr-t3033-1
type: adr
title: runのreviewの自動の手直しのrunを通した上限を、reviewの試行ごとにlanding branchのcommitのdagq.tomlの[review] revise_limitから読み、書かなければ今の既定のままにする（ADR-t2599-1決定1をamends）
status: accepted
created: 2026-10-22
updated: 2026-10-22
accepted_on: 2026-10-22
amends:
  - adr-t2599-1 decision 1
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - review
  - config
related:
  - adr-t2599-1
  - adr-t1453-1
  - adr-t1942-1
  - adr-t2687-1
  - adr-0047
  - design-supervisor-lifecycle-review
  - design-supervisor-lifecycle-run-environment
---

# ADR-t3033-1: runのreviewの自動の手直しの上限を、reviewの試行ごとにlanding branchのcommitのdagq.tomlから読む

## Context

[ADR-t2599-1](2026-10-17-t2599-1-run-review-revises-counted-across-the-run-and-judged-by-code-findings-past-the-limit.md)は、runのreviewの自動の手直しをrunを通して数え、上限の後は理由のコードで収束を判定すると決めた。
上限の値は人の言葉に従い、その置き場は定数のdoc commentとした（決定1）ので、projectは値を変えられない。

人はrequest 94で、dagqにbuilt-inのreviewを同梱し、自動の手直しの上限を設定で変えられるようにしたいと言った。
dagqは外部のprojectで使う前提で（request 93）、往復の長さとreviewの厳しさの釣り合いはprojectごとに違う。

task 2267は同じ設定をreviewの回ごとの上限として作る予定だったが、request 117で数え方がrunを通すものに変わったため取り消され、そのnoteがADR-t2599-1の着地の後にrunを通した上限をlanding branchのcommitの`dagq.toml`から読む形で作り直すと決めた。
このADRはその後継である。

## Decision

1. **runを通した自動の手直しの上限は、`dagq.toml`の`[review] revise_limit`で与える。** 値は1以上の整数で、0・負・整数でない値は設定の誤り。書かなければ今の既定（`MAX_REVISE_ATTEMPTS`、値は定義のdoc commentが持つ）。ADR-t2599-1決定1の「上限の値は人の言葉に従い、その置き場は定数のdoc comment」をこれに改め、定数は既定の置き場として残す。
2. **数えるもの・数え方・上限の後の扱いは変えない。** 数えるのはreviewの`revise`、concernの`high`・`reason_category`無しの`send_back`、プログラムのreviewの差し戻し（[ADR-t2687-1](2026-10-18-t2687-1-program-review-send-backs-follow-the-run-wide-revise-limit-and-its-judgement.md)）、着地の前のagent evalの差し戻しで、runを通して数える。上限の後の収束の判定と`approve_landing`（ADR-t2599-1決定2〜8）もそのまま。plan reviewの差し戻しの上限（ADR-t2599-1決定9）は設定に依らず、この値と連動しない。
3. **値はreviewの試行ごとに、その試行が読むlanding branchのcommitの`dagq.toml`から読む。** reviewのagentと`[review.code]`と同じく、workerが変えられない場所から読む（[ADR-t1453-1](2026-10-03-t1453-1-review-subagents-named-by-path-run-inside-the-review-job.md)決定4）。main checkoutの作業ファイル・run branch・worktreeの`dagq.toml`は読まない。supervisorの起動時に固定せず、landing branchのcommitで値を変えれば次の試行から効く。試行はその試行で読んだ値を記録し、その試行の差し戻しをreviseにするか上限の後の判定に入れるかはその値で決める。
4. **読めない・解釈できない値はその試行を失敗にする。** 読み出しか解釈の誤り（0・負・整数でない値を含む）は`[review.code]`の読み出しの誤りと同じく、その試行をpassにせずreviewの失敗にする。既定に黙って戻さない（ADR-t1453-1決定6）。
5. **値を下げたrunは次の差し戻しから上限の後の判定に入る。** 既に送った手直しの数が新しい上限以上になったrunは、次のreviewの差し戻しからADR-t2599-1決定3〜6の判定に入る。送った手直しは取り消さない。

## Alternatives

- **`[resume] conflict_only_limit`と同じく、supervisorの起動時にmain checkoutの作業ファイルを読む**: workerが変えられないlanding branchのcommitから読むというgoal 188の制約に反する。未commitの値がreviewの判断（手直しを返すか、上限の後の判定で着地させるか）を変えうるうえ、どの値で判断したかがcommitから辿れない。
- **runのworktree（run branch）の`dagq.toml`を読む**: 判断されるrun自身が上限を上げ下げでき、reviewの柵を自分で外せる。
- **既定に黙って戻す**: 書き誤りに気づかないまま、意図と違う回数で着地か人の判断に進む。

## Consequences

- projectは自動の手直しの回数を`dagq.toml`で選べる。上げれば人の判断の前の往復が増え、下げれば早く上限の後の判定に入る。
- 値の変更は着地したcommitでだけ効くので、どの上限で判断したかは試行の記録とcommitから辿れる。
- 古いdagqは知らないkeyを誤りにするので、このrepositoryの`dagq.toml`にkeyを書くのは固定バイナリが読めるようになってから。

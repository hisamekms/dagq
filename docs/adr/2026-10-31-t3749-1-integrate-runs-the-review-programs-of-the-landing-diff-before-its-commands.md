---
id: adr-t3749-1
type: adr
title: integrateはrebaseの後・検証のコマンドの前に、着地する差分にpathsが当たるreviewのプログラムを流し、落ちたら重い検証の前にworkerへ返す（ADR-t1895-2決定1・3・4をamends）
status: accepted
created: 2026-10-31
updated: 2026-10-31
accepted_on: 2026-10-31
amends:
  - adr-t1895-2 decision 1
  - adr-t1895-2 decision 3
  - adr-t1895-2 decision 4
owners:
  - hisamekms
tags:
  - runtime
  - review
  - integrate
related:
  - adr-t1895-2
  - adr-t1925-1
  - adr-t2259-1
  - adr-t639-1
  - adr-t3344-1
  - adr-t2268-1
  - adr-t2257-1
  - design-supervisor-lifecycle-integrate
  - design-supervisor-lifecycle-review-programs
  - design-supervisor-lifecycle-validation
---

# ADR-t3749-1: integrateはrebaseの後・検証のコマンドの前に、着地する差分にpathsが当たるreviewのプログラムを流し、落ちたら重い検証の前にworkerへ返す（ADR-t1895-2決定1・3・4をamends）

## Context

reviewのプログラム（[ADR-t1895-2](2026-10-06-t1895-2-program-reviews-are-fast-format-checks-read-from-the-landing-branch.md)、built-inは[ADR-t2259-1](2026-10-12-t2259-1-builtin-review-agents-and-programs-configured-by-builtin-names.md)）は、reviewの段でrebaseの前のrunのcommitに流れる。`integrate`はrebaseの後にtaskの検証のコマンドを登録の順に流し、1本落ちたら止まる。推奨の順はfmt → clippy → coverage → 軽いscriptなので、rebaseでmainの変更と合わさって初めて落ちる軽い検査（文書の予算超えなど）は、重い検証を数十分流した後にしか分からない。rebaseの後に軽い関門が無いのが穴である。

軽さはprojectが`[review.programs]`に宣言済み（速い・buildとtestを流さない・今のmainで通る）なので、新しい設定もコマンドの文字列のパターンも道具の知識も要らない（[ADR-t2268-1](2026-10-12-t2268-1-toolchain-knowledge-behind-declared-profiles.md)・[ADR-t2257-1](2026-10-12-t2257-1-container-runtime-takes-project-and-host-values-from-config.md)決定1に合う）。

## Decision

1. **流す所（決定1の適用先を広げる）。** 検証は`integrate`の1回のまま変えない。`integrate`はrebase・migrationの番号の振り直し・pathsの検査の後、taskの検証のコマンド（`[landing_verification]`の置き換えを含む、[ADR-t1925-1](2026-10-07-t1925-1-landing-verifies-unit-tests-and-selected-integration-tests-and-ci-is-the-final-gate.md)決定1・4）の前に、reviewのプログラムを流す。プログラムは検証のコマンドではなく、その前の安い関門である。
2. **選びと読む元。** landing branchの今のcommit（rebaseの先のmain）から、プログラム（built-inを先に）とそのscriptを読み（ADR-t1895-2決定2を保つ）、着地する差分`main..rebased`の変えたpathで選ぶ。宣言の順に1本ずつ最後まで流し、working directoryは着地のtree、envの絞り（決定6）と時間の上限はreviewの段と同じにする。built-inには範囲`main..rebased`を渡す。当たるプログラムが無ければ何も流さず、何も記録しない。
3. **落ちたとき（決定3の適用先を広げる）。** 1本が非0で終われば、残りのプログラムと検証のコマンドを流さず、`needs_session`でworkerに返す。理由は落ちたプログラムの名前と出力の末尾（reviewの差し戻しと同じ材料）。reviewのreviseの上限には数えず、検証のコマンドの失敗（workerに返すもの）と同じに扱う。宣言は今のmainで通るものなので、rebaseの後に落ちればrunの変更（とmainとの合わさり）のせいである。
4. **起動の失敗・読めない・時間切れ（決定4の適用先を広げる）。** workerの変更のせいとしない。検証のコマンドの環境の失敗（[ADR-t639-1](2026-09-27-t639-1-retry-verification-failures-of-the-host-once.md)）と同じく、同じ試行でそのプログラム（読めないときは読み込み）を1回だけ流し直し、また起動できない・読めない・時間切れなら`integration_held`（`verification_environment`、理由にプログラムの名前とlog）で人を待ち、sessionは再開しない。handoffのdrain中は流し直さず、`integration_handed_over`にする（[ADR-t3344-1](2026-10-25-t3344-1-handoff-drain-hands-over-a-landing-before-its-verification-is-retried.md)）。
5. **backendが流せないとき。** プログラムはreviewのactorのbackendで流し、流せない（containerなど）ときはhostに戻さない（決定5のfail closedを保つ）。このときはプログラムを1本も流さず、記録に`skipped`（理由`backend_unavailable`とbackendの名前）を残して検証のコマンドへ進む。heldにしないのは、backendの設定は着地ごとに変わらない恒久の条件で、heldにするとそのprojectの当たるプログラムがある着地が人の手でも通らなくなるため。reviewの段は同じ条件で既にfail closed（reviewの失敗）にしており、検証のコマンドの関門は残る。
6. **1つのheadに1回。** 同じrebased headでプログラムが全部通った後の検証のやり直し（flakyの再実行・引き継ぎの次の着地・同じheadのreceiptでのやり直し）では流し直さない。heldや差し戻しの後に人が`integrate`し直したときは流し直す。
7. **記録。** 1試行に1つ、新しいkind `integrate_programs_finished`を記録する（main・head・プログラムごとの名前・結果（`passed` / `failed` / `start_failed` / `timed_out`）・時間・log、または`skipped`と理由）。reviewの段の`review_programs_started` / `review_programs_finished`に段を足さない。reviewの段はその数で回を数え、adoptは最後の`review_programs_finished`を読むので、混ぜるとreviewの段の振る舞いが変わる。

ADR-t1895-2決定2・5・6とADR-t1925-1決定1・4・7は変えない。

## Alternatives

- **登録の推奨の順を変える（軽いscriptを先に）**: 既存のtaskと他のprojectに効かず、順を守るのは登録の規則に頼る。
- **projectの設定にコマンドの文字列のパターンで軽いものを書き写す・runtimeに道具の知識を入れる**: ADR-t2268-1・ADR-t2257-1決定1に反し、宣言が2か所に分かれる。
- **`duration_secs`の中央値で検証のコマンドを軽重に分けて並べ替える（案2）**: 今回は頼まれていない。案1の様子を見て人が決める。
- **backendが流せないときにheldにする**: 恒久の条件で着地が通らなくなる（決定5）。
- **`review_programs_*`に段を足す**: reviewの段の回の数え方とadoptの読み方が変わる（決定7）。

## Consequences

- rebaseで合わさって初めて落ちる軽い検査は、重い検証の前に数秒でworkerに返る。プログラムを宣言していない軽い検査は前に来ない（宣言すればよい）。
- 着地でもmainの旧いscriptが流れるので、プログラムの読む形を変えるtaskはmainの旧いscriptで通る差分にする必要がある（[taskの登録](../development/task-registration.md)の「reviewのプログラムが読む形を変えるtask」）。
- landing recheck（goal 169）で流すこと、プログラムと同じscriptが`--verify`にもあるとき検証のコマンドの側を省くことは、この決定の外である。

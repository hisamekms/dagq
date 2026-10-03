---
id: adr-t963-1
type: adr
title: e2eを、差分が狭い範囲に触れるrunだけworkerで必須にし、全部のe2eは固定バイナリを入れ替える前の関門で流す（ADR-0047決定28、ADR-0073決定12・14・17をamends）
status: accepted
created: 2026-09-29
updated: 2026-09-29
accepted_on: 2026-09-29
amends:
  - adr-0047 decision 28
  - adr-0073 decision 12
  - adr-0073 decision 14
  - adr-0073 decision 17
amended_by:
  - adr-t1162-1
  - adr-t1165-1
  - adr-t1233-2
  - adr-t1433-1
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - testing
  - operations
related:
  - adr-0047
  - adr-0073
  - adr-0029
  - adr-t598-1
  - design-supervisor-lifecycle-auto-update
  - design-supervisor-lifecycle-validation
  - design-supervisor-lifecycle-install
---

# ADR-t963-1: e2eを、差分が狭い範囲に触れるrunだけworkerで必須にし、全部のe2eは固定バイナリを入れ替える前の関門で流す（ADR-0047決定28、ADR-0073決定12・14・17をamends）

## Context

[ADR-0047](0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)の決定28（ADR-0019の決定5を引き継いだもの）は、taskが要求するevidenceを`add --evidence`で固定して持ち、validatingで確かめると決めている。この repository はAGENTS.mdの規則でruntimeのtaskに一律に`--evidence e2e`を付けるので、`src/`を変えた全てのrunのworkerが実cmux・実Gitのe2e（`cargo test --locked --test e2e -- --ignored`）を流す。

2026-09-28の分析（goal 66）では、e2eは1本あたり平均2.9分、26日以降の126本で計6時間、runtimeのrunがslotを使う時間の約6.5%だった。一方、26日以降の`src/`のcommit 247本のうち、e2eが実物で確かめる範囲（cmuxのadapter・process・launchd・lifecycle・integrate・install・update・actorの起動）に触れるのは38%で、残りの62%はfakeのcmuxを使うin-processのtestで足りる変更にもe2eを流していた。

また、今の自動更新（[ADR-0073](0073-kind-additions-are-compatible.md)決定17）が固定バイナリを入れ替える前に確かめるのは`--version`と使い捨てのqueueでの起動だけで、e2eの無いrunが着地しても本番のバイナリを守る関門が無い。人はplannerと案F5を選んだ。

## Decision

1. **全部のe2eを、固定バイナリを入れ替える前の関門にする。** 自動更新のjobは、headをbuildした後、`install`の確認と入れ替えの前に、同じcheckoutと同じtargetで全部のe2eを流す。1件でも落ちれば入れ替えず（非互換のmigrationのbuildも`staged`に置かず`approve_update`を開かない）、今の`update_failed`のask（`retry` / `skip`）で人に知らせる。新しいaskのkindは作らない（人が選ぶことはbuildの失敗と同じで、段の名前だけが違う）。cmuxが使えずe2eを流せないときも、流さずに通さず失敗にする。人が`dagq install`をdagqのソースのcheckoutから打つときも同じ関門を既定で通し、急ぎのときだけ人が明示して飛ばせる。`--rollback`（前に動いていたバイナリへ戻す）と、リリースのバイナリの入れ替え（CIとリリースの手順が確かめたもの）には関門を置かない。
2. **workerのe2eの要否を、runの差分とrepositoryが宣言するpathから決める。** repositoryは`dagq.toml`にe2eが要るpathをglobで書き（言語に依らない。書式はADR-0029の`--paths`と同じ）、validatingは、宣言パスの検査（ADR-0029）と同じrunの差分がどれかのglobに触れるときだけ、receiptの`e2e`を要求したevidenceとして扱う。taskに`--evidence e2e`を明示したものは今までどおり必須。`dagq.toml`に書かなければ既定は空で、差分からは何も要求しない（今までの`--evidence`だけの動き）。
3. **この repository で置く範囲は、実cmux・実プロセスの境目だけにする。** cmuxのadapter、process、launchd、lifecycle（`up` / `down`・引き継ぎ）、integrate、install、update、actor（worker・planner・inbox・jobのsession）の起動。`supervise`の判断の部分は含めない。理由: runtimeのcommitの多くが触るので含めると狭める効果がほぼ消え、その判断はfakeのcmuxのin-processのtest（`tests/it/runtime_*`）が確かめ、実物との組み合わせは決定1の関門が本番の前に確かめる。具体のglobは`docs/design`に書く。
4. **e2eは着地の処理（integrate）では流さない。** integrateの検証は1本ずつ直列なので、1件ごとに約3分を足すと着地の上限が下がり、実cmuxに依る不安定な失敗が全部の着地を止める。本番を守るのは決定1の関門で、着地ごとに流す必要は無い。
5. **Codex worker の sandbox に限り、host 権限に依存する名前の決まった e2e を除外する。** workspace-write sandbox から Podman machine や cmux が起動した supervisor の生存確認・signal を扱えないので、worker prompt が列挙する該当 test の完全な名前だけを `--skip` で除外する。残りは決定 2 の差分ベースの要件どおり最後の変更の後に流す。receipt の `e2e` はコマンド、結果、除外した名前と理由を構造化した evidence で示し、validating は Codex の必須 e2e について名前のない・承認されていない省略を受理しない。除外した test を passed と数えない。Claude worker にはこの例外を適用しない。決定 1 の `install` と auto-update の関門は worker の prompt を使わず、同じ除外を継承せず全件を流す。（2026-09-30、task 1206）

ADR-0047の決定28のうち「taskが要求するevidenceは`add --evidence`で固定して持つ」部分を、e2eについては差分からも要求しうるように改める。判定の順序、`evidence_missing`の`needs_session`とresume、`tests`・`subagent_review`の扱いは変えない。

決定1は[ADR-0073](0073-kind-additions-are-compatible.md)の3つの決定を改める。決定12の入れ替えの前の確認（`--version`と使い捨てのqueueでの起動）に、ソースからのbuildでは全部のe2eを足す。決定14の`install`は、ソースのcheckoutからのbuildで確認の前にe2eを流し、人が明示したときだけ`--skip-e2e`で飛ばす。決定17の自動更新は、buildの後にe2eを流し、落ちたbuildは非互換のmigrationを含んでも`approve_update`を開かずに`update_failed`にする。ADR-0073のその他（build識別子、migrationの互換、差し替え、引き継ぎ、見張り）は変えない。

## Alternatives

- **今のまま（runtimeのtaskは全部`--evidence e2e`）**: slotの時間の約6.5%を、62%のe2eの要らない変更に使い続ける。本番のバイナリを入れ替える前の関門も無いまま。
- **plannerが登録のときにe2eの要否を判断する**: 登録のときには実際の差分が分からず、見落としか付けすぎに寄る。plan reviewの判断も増える。差分で決める方が機械的で言語に依らない。
- **integrateで流す**: 決定4の理由（直列の着地の上限と、不安定な失敗が全部の着地を止めること）。
- **workerから完全に外し、関門だけにする**: 狭い範囲の不具合が関門で初めて見つかると、どのcommitかを切り分けるのに時間がかかり、その間は自動更新が止まる。狭い範囲に触れるrunは着地の前に止める。

## Consequences

- 範囲の外のrunはe2eなしで着地できる。範囲の外の変更が実cmuxとの組み合わせで壊す不具合はmainに着地しうるが、決定1の関門が入れ替えの前に止めるので、本番のバイナリは守られる。止まった間は前のバイナリで動き続け、`update_failed`のaskで人とplannerが直すtaskを作る。
- 自動更新はe2eの分（約3分と、並列で動くrunへの負荷）遅くなる。buildと同じく同時に1つで、着地が続けば間の着地はまとめて1回のbuildとe2eになる。
- 順序: 決定1の関門を先に着地させてから、決定2・3でworkerのe2eを狭める（goal 66の制約）。狭めた後、AGENTS.mdとpluginのskillの「runtimeのtaskは`--evidence e2e`」を改め、導入の前後でruntimeのrunのe2eの時間とworkの時間を比べる。
- 欄名・既定値・globの一覧・段の名前・flagの綴りは[Auto-update](../design/supervisor-lifecycle/auto-update.md)・[Validation](../design/supervisor-lifecycle/validation.md)・[install](../design/supervisor-lifecycle/install.md)が持つ。どれもこのADRの時点では未実装。

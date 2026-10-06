---
id: adr-t1728-1
type: adr
title: agentの定義とケースを.dagq/agents/<name>/に置き、evalをqueue serviceのユースケースとして本番のagentのjobと同じ経路でsupervisorが実行し、依頼の認可・費用の上限・専用の枠を持ち、定義を変えるrunは着地の前にlanding branchのケースで採用を判定する
status: accepted
created: 2026-10-06
updated: 2026-10-06
accepted_on: 2026-10-06
amends:
  - adr-t1453-1 decision 2
owners:
  - hisamekms
tags:
  - runtime
  - review
  - eval
related:
  - adr-t1453-1
  - adr-t1728-2
  - adr-t1895-1
  - adr-t1895-2
  - adr-t728-1
  - adr-t598-1
  - adr-t1091-1
  - plan-review-agent-eval-spike
  - design-agent-eval
---

# ADR-t1728-1: agentの定義とケースを.dagq/agents/<name>/に置き、evalをqueue serviceのユースケースとして本番のagentのjobと同じ経路でsupervisorが実行し、依頼の認可・費用の上限・専用の枠を持ち、定義を変えるrunは着地の前にlanding branchのケースで採用を判定する

## Context

reviewのagent（[ADR-t1453-1](2026-10-03-t1453-1-review-subagents-named-by-path-run-inside-the-review-job.md)）が正しく判定しているかを、規則コードごとの陽性・陰性のケースと閾値で測る作り方を、dagqの外のSpike（dagq-agent-eval、commit `cfb8a4c`）で試した。結果と所見は[review-agent-evalのSpike](../plans/review-agent-eval-spike.md)。本番の定義はコードを束ねて規則コードのprecisionが0.25〜0.27しかなく、短く詰めた定義は作ったケースで満点でも本番の差分の違反の3分の2を見落とし、本番の履歴から作ったケースが過学習を最もよく捕まえた。人はrequest 35で、着地の前のdevの判定とその費用（1周$20〜24・10〜20分）、`.dagq/agents/<name>/`の置き場と役割の区分のディレクトリを作らないこと、ケースの共通の欄と役割ごとのinput・expected、定義の長さを懸念にしないことを決めた（goal 125）。

Spikeのケースはpatchを含めてadr-rulesが8.8MB、migration-rulesが4.1MBで、128個のpatchのうち29個が重複し、重複を除くと約6.6MBになる。runのreviewのagentは[ADR-t1895-1](2026-10-06-t1895-1-review-stage-runs-agent-and-program-jobs-in-a-fixed-shape.md)で親のjobの中のsubagentから、agentごとに1本の独立のheadless jobになり、その前にprogramのreview（[ADR-t1895-2](2026-10-06-t1895-2-program-reviews-are-fast-format-checks-read-from-the-landing-branch.md)）が流れる。workerはLLMのCLIを直接打たない（資格情報・設定の注入・自己採点・費用・prompt injection）。

## Decision

1. **定義の置き場（ADR-t1453-1決定2の置き場を改める）。** agentの定義は`.dagq/agents/<name>/`の下に置き、形は今のreviewの定義と同じfrontmatterと本文にする。同じディレクトリにそのagentのケースを置く。役割の区分のディレクトリは作らず、1つのagentは1つの役割で使う。どこで走るかは今の`dagq.toml`のreviewのagentの設定（ADR-t1453-1決定1）のままで形を変えない。providerに依らない形式は保ち、道具の扱いは[ADR-t1728-2](2026-10-06-t1728-2-agents-declare-their-tools-from-a-runtime-list.md)が改める。
2. **設定の検査。** 定義の無いagentを名指す設定と、同じagentを複数の役割で名指す設定を誤りにする。今の役割はreviewだけだが、役割が増える前提の規則とする。
3. **ケースの一覧。** split（dev・holdout・production）はケースの欄でなくファイルで区分する。各ケースは役割に依らない共通の欄と、役割ごとのinput・expectedに分ける。起動・形の検査・採点・productionのケースの作り方は役割ごとのharnessが持ち、今作るharnessはreviewだけにする。
4. **ケースの大きさ。** patchはagentをまたいで内容のhashで1か所に置き、ケースから参照する。版ごとの成績の記録はrepositoryに置かずqueue側（evalのevent）に残す。
5. **evalはqueue serviceのユースケース。** 依頼をqueueに記録し、実行は常にsupervisorが行い、成績をeventに記録する。evalは本番のrunのreviewのagentのjobと同じ起動経路（ADR-t1895-1の独立のagentのjob、その共通のjobの経路での起動・時間の上限・記録、snapshotが返した定義をそのまま持たせるpromptの組み立て、ADR-t1728-2の道具の変換、行き先のproviderはreviewの役割の設定）で、ケースごとに測るagentの1本のjobだけを起動する。全体のreviewのjobと親のjobの中のsubagent（Claudeの`--agents`、Codexの`runs_review_subagents`、能力による切り替えの`subagents_unsupported`）は使わず、eval専用の起動の組み立ては作らない。
6. **依頼の認可と実行を分ける。** devの依頼はworker（自分のrunの分）・人・inbox・plannerができ、hold-outとproductionの依頼は人・inbox・plannerだけができる。supervisorは認可された依頼と決定10の着地の前のdevを実行するだけで、自分ではhold-out・productionを依頼しない。workerのcapabilityはdevの依頼と結果（成績・失敗したケースのidとagentの理由）の読み取りだけにする。ケースのラベルの変更と争いの印の解除は人とinbox・plannerに限る。
7. **hold-outの1回の制限。** キーは（agent、定義のdigest、ケースの集合のdigest）で、集合のdigestは流すケースの一覧と参照するpatchの内容から作る（repositoryのhold-outでもqueue側のproductionの集合でも同じ）。同じキーの初回はhold-outを依頼できるactorが依頼でき、2回目以降は人の明示の再実行の指定（人と、人の言葉を受けたinboxだけ。planner・supervisor・workerは拒む）でだけ流せる。制限の判定はevalのeventから読み、別の状態を持たない。
8. **費用の上限。** 1周の実行の回数と金額に上限を置き、起動の前に見積もる（予定の実行の数 × 1回の見込み。見込みは同じagentとproviderの直近の実績の最大、無ければproviderごとの既定の見込み）。回数か見積もりが上限を超える周は起動せず理由を記録する。起動した周は見積もりを予約として記録し、実行中は次の実行を起動する前に、使った額と実行中の実行の見込みと次の1回の見込みの和が上限を超えるなら新しい実行を起動せず、実行中のものを待って周をincomplete（`cost_limit`）で閉じる。使った額は実行ごとに確定できなければならない: 金額を返すproviderはその実績、金額を返さないproviderはproviderごとのtokenの単価での換算（出所を換算と記録）にする。金額もtokenの単価も無いproviderの周は、既定の見込みがあっても使った額を確定できないので起動せず`cost_unknown`を記録し、1回の見込み（実績も既定も）が無い周も見積もれないので同じく`cost_unknown`で起動しない。上限なしで流す経路は持たない。金額もtokenも返さずに終わった実行（異常終了など）は、その実行の見込みを保守的な課金額として使った額に数え、出所を見込みと記録する。
9. **実行の枠。** evalはrunのslotを使わず（claim・着地の順・superviseの回ごとのrunの本数を変えない）、本番のreviewは今のままrunのslotの中で動いてevalを待たない。evalはqueueごとに同時に1周だけ流し、1周の中のproviderのprocessの同時数に上限を置く。待っている周は、決定10の着地の前のdevを先に、依頼は古い順にし、流れている周は止めない。
10. **採用の判定。** 定義を変えるrunの着地の前に、supervisorがlanding branchのcommitのケースでdevを流し、判定と規則コードのrecall・precisionのどれかが閾値を下回れば着地させない。run branchのケースの追加・変更はそのrunの判定に使わない（自分のケースで自分を採点させない）。evalの周を待つ間と流す間、runは着地の前のe2eと同じく自分のslotとleaseを持ったまま待つ。
11. **productionのケースと見張り。** 本番のreviewのeventと差分からagentごとにケースを作るユースケースをruntimeに置く（依頼は人・inbox・planner、ラベルのjobの起動と集合の作成はsupervisor）。ラベルは改善する側と別のproviderが規則の本文だけで付け、本番の判定と食い違うものは人に回す。作ったケースは使うまでqueue側に置き、hold-outとして1回流した後にrepositoryの一覧へ足すのはtaskで行う（supervisorとplannerはrepositoryを直接変えない）。採用した定義の見張りでは、supervisorは前回の見張りの後に新しい本番のreviewが一定の件数たまったか一定の間隔が過ぎたかを判断して、時期が来たことをfindingにするだけで、productionのケースの作成とhold-outの依頼は、そのfindingで開かれたruntimeのplannerが依頼者として行う。閾値を下回ったhold-out・productionの成績はfindingにする。
12. **定義の長さは懸念にしない。** 実測で、長くした定義の1回あたりの費用は約20%下がった。
13. **programのreviewをケースに当てる。** evalは各ケースに本番のreviewと同じ段を当て、landing branchのcommitのprogramをケースのtreeに対して先に流す（envとbackendはADR-t1895-2のまま）。programが落ちたケースは本番でもagentが起動しないので、agentに渡さず、agentの成績（判定と規則コードのrecall・precision）の分母から外し、「programで止まったケース」として数とidを記録する。programの起動の失敗・時間切れのケースは違反の有無が分からないので周をincompleteにする。incompleteの周（決定8を含む）は閾値との比較でpassに数えない。

## Amendsの判断

ADR-t1453-1は番号付きの決定を10持ち、ここで変えるのは決定2の定義の置き場（今のreviewの定義のファイルから`.dagq/agents/<name>/`へ）だけなので、丸ごと置き換えずamendsにする（[ADR-t1091-1](2026-09-30-t1091-1-amend-or-replace-by-number-of-decisions.md)）。決定2のうちproviderに依らない形式と`.claude/agents`を採らないことは保ち、道具の許可はADR-t1728-2が同じ決定2をamendsして改める。決定1（設定の形）・3〜10は変えない（決定5・8はADR-t1895-1がすでにamendsした）。決定4の信頼するsnapshot（landing branchのcommitから読む）は、決定10の採用の判定のケースにも同じ考えで当てる。

## Alternatives

- **ケースがbase・headのcommitを指す（Spikeの(a)案）**: runのcommitがどのrefからも辿れないことがあり（Spikeの所見5.8）、cloneごとにrefが要る。
- **大きな差分をqueue側に置く（(c)案）**: 採用の判定をlanding branchのケースだけで行う信頼の模型（ADR-t1453-1決定4と同じ）に合わない。
- **patchをケースごとに持つ（Spikeのまま）**: 重複が全体の約4分の1あり、repositoryが膨らむ。
- **workerがLLMのCLIを打ってevalを流す**: 資格情報と設定の注入、自己採点、費用、prompt injectionを防げない。
- **evalのための別の起動の組み立て（Spikeの実行器のように親のjobのargvを組む）**: 本番と測るものがずれ、削除予定の親のjobの中のsubagentの経路に依る。
- **supervisorがhold-outを自分で依頼して見張る**: goal 125の制約（hold-outとproductionは人・inbox・planner）を変える。人の記録が無いので権限を広げない。
- **evalにrunのslotを使う**: 本番のrunの流れとclaimの順を変える。
- **費用を確定できないproviderでも既定の見込みで流す**: 使った額が分からず上限が効かない。
- **run branchのケースで採用を判定する**: workerが自分のケースで自分を採点できる。

## Consequences

- 定義とケースの移動、設定の検査、ケースの読み手とpatchの共有、evalのユースケース・CLI・capability・event、費用の上限と枠、着地の前の判定、productionのケースと見張りの実装は、goal 125の後続のtask（1866〜1874）が行う。programのreviewの当て方とhostの外のbackendへの接続はtask 1874が持つ。programが受け持った規則コードをagentのexpectedから外す定義とケースの整理は、定義を移す後続のtask（goal 153の1901とgoal 125の定義の取り込み）が行う。
- ケースの欄・ファイルの名前・CLIの名前とflag・eventの種類と欄・既定値と閾値・設定のkeyの予定は[agent eval](../design/agent-eval.md)が持つ。[Review](../design/supervisor-lifecycle/review.md)・[Run environment](../design/supervisor-lifecycle/run-environment.md)の今の姿は実装のtaskが直す。
- 定義を変えるrunは着地の前に1周の時間（10〜20分）と費用を待ち、その間slotを持つ。

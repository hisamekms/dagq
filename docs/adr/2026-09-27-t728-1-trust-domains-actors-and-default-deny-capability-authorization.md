---
id: adr-t728-1
type: adr
title: 信頼する制御側と信頼しないAI actorを分け、actorを型で表して、状態変更をapplicationの境界でdefault denyの静的なcapabilityのpolicyで認可する（host実行は助言的で隔離ではない）
status: accepted
created: 2026-09-27
updated: 2026-09-27
accepted_on: 2026-09-27
amended_by:
  - adr-t883-1
owners:
  - hisamekms
tags:
  - runtime
  - security
related:
  - adr-0034
  - adr-0044
  - adr-0047
  - adr-t598-1
  - adr-t728-2
  - adr-t728-3
---

# ADR-t728-1: 信頼する制御側と信頼しないAI actorを分け、actorを型で表して、状態変更をapplicationの境界でdefault denyの静的なcapabilityのpolicyで認可する（host実行は助言的で隔離ではない）

## Context

企業で使える安全性に向けたセキュリティ再設計の第1段（goal 55。2026-09-27に人がplannerに渡した計画のPhase 1〜2）。2026-09-27の調査では、roleはenvの`DAGQ_ROLE`の生の文字列で、判定はCLIのparseの近くの2か所（observerとheadless jobの環境）だけだった。worker・planner・inbox・envなしは着地・answer・review無しのready・goal close・recover・installを含む全コマンドを打て、`ask --run`は呼び出し元がrunの持ち主かを見ない。eventに実行者の欄が無く（[ADR-0034](0034-domain-events-carry-reason-codes-actor-and-configuration-changes.md)決定4はproposedのまま）、状態変更の多くはCLIからqueueを直接呼んでapplicationの境界が無い。headlessのjob（review・復旧・plan review・goal review）は型付きのverdictをsupervisorが適用し、壊れた出力はfail closedになる一方、observerだけは自分でCLIを打って状態を変える。

計画の原則: AI actorは提案・生成・評価をしてよいが、自分に権限を与えること、特権の状態遷移を確定すること、割り当ての外の外部副作用を起こすことはできない。

## Decision

1. **信頼の区分。** actorを2つに分ける。
   - 信頼しないAI actor: inbox・desk・planner・worker・review-job・recovery-job・plan-review-job・goal-review-job・observer。
   - update-jobは予約の名前で、今のupdateの処理はAIを起動しない。AI actorとして起動するときに信頼しない側に入れる。
   - 信頼する制御側: supervisor・wrapper・integrator・user。
   AI actorの出力（verdict・receipt・CLIの引数・answerの文）はデータで、制御側が決定的なRustのコードで状態遷移に写す。LLMの出力や判断で認可を決めない。promptの指示はUXでenforcementではない。
2. **actorの名前。** goal 48のconstraintsにある人の決定（2026-09-26）の名前の表をそのまま使う。この段で新しく足す信頼する制御側の**integrator**（[ADR-t728-2](2026-09-27-t728-2-landing-only-by-the-trusted-integrator.md)）だけをこのADRが名付ける。

   | actor | 区分 | `DAGQ_ROLE`の値・eventのroleの綴り | 備考 |
   | --- | --- | --- | --- |
   | user | 信頼する | なし（envが無い） | host実行の互換のため（決定6） |
   | inbox | 信頼しない | `inbox` | |
   | desk | 信頼しない | `desk` | goal 48のtask 504が作るまでActorRoleに無くてよい。それまで人が開くplannerは`planner` |
   | planner | 信頼しない | `planner` | |
   | worker | 信頼しない | `worker` | |
   | review-job | 信頼しない | `review-job` | |
   | recovery-job | 信頼しない | `recovery-job` | triage jobを広げたもの |
   | plan-review-job | 信頼しない | `plan-review-job` | |
   | goal-review-job | 信頼しない | `goal-review-job` | |
   | update-job | （予約） | `update-job` | 今のupdateの処理はAIを起動しないので予約の名前 |
   | observer | 信頼しない | `observer` | |
   | supervisor | 信頼する | `supervisor` | |
   | wrapper | 信頼する | `wrapper` | |
   | integrator | 信頼する | `integrator` | このADRが名付ける |

   ActorRoleの値とこの名前は1対1にする。jobは1つにまとめず、jobごとに別のactorにする。今のreview・復旧・plan review・goal reviewに共通の`reviewer`はこの段でjobごとの名前に分け、入れ替え前のバイナリが起動したjobのために、移行の間だけ読み取りだけのjobとして読む（書き込みは拒む。fail closed）。
3. **名前の優先。** 名前はgoal 48の人の決定が正で、goal 48の統合ADR（task 500の書き直し）はこの表を参照する。後のADRが名前を変えるなら、そのADRがこのADRを`amends`で直し、コードはそれに合わせる。askの問いの主の欄の値（jobのaskをそのjobの名前にし、humanをuserにする改名）はgoal 48のtask 502が持ち、goal 55では変えない。eventのactorはその欄とは別の欄として記録する。
4. **actorは型で表す。** actorはActorRoleとTrustLevelの型（とactor idを持つ文脈）で表し、promptの名前や任意の文字列から信頼を推し量らない。`DAGQ_ROLE`の未知の値はfail closedで扱う。runtimeが起動するAI actorは全てroleとactor idをenvに持ち、状態を変えるeventはそのactor（roleとid）を記録する。
5. **認可はdefault denyの静的なpolicy。** 認可はCapability × Resourceの静的なpolicy（Rustのコードとdata）で、明示して許したもの以外は拒む。外部のpolicy engineは入れない。判定はCLIのparseではなくapplicationの境界で、mutationの前に行う。resourceの持ち主が曖昧なら拒む。子の作業（worker・job）は親（それを起動した制御側や割り当て）の権限を越えない方針とし、goalの単位の権限の包みは後のgoalにする。
6. **host実行は助言的（advisory）で、sandboxでも隔離でもない。** 今のactorは全て同じユーザーとしてhostで動く。敵対的なプロセスは`DAGQ_ROLE`などのenvの偽装やqueue DBの直接の操作で認可を迂回できる。この段の認可は、誤りと事故を止め、誰が何をしたかを記録するための論理的な境界で、security boundaryではなく移行の途中の状態である。`DAGQ_ROLE`が無い呼び出しをuser（人）と扱うのはhost実行の互換のためで、これも助言的である。sandbox（Podmanなど。draftのgoal 38）は同じcapabilityの模型の上に強制を足すだけで、模型を作り直さない。
7. **この段では各roleの今の運用の権限を変えない。** 境界を明示して強制と記録を足すだけにする。policyは今の運用が使う権限を全て許す（plannerはgoal closeと、readyまでのtaskのcancel・set-*・依存・goal editを持ち続ける。inboxの権限は[ADR-t728-3](2026-09-27-t728-3-answer-and-delegated-authority-of-the-inbox.md)）。足す強制は、今は誰でも打てるが運用ではどのroleも使っていない操作（workerが自分のrun以外のaskを開くこと、workerとjobとobserverの着地・answer・ready・cancel、plannerの着地とrunの操作など）をdefault denyが拒むことで、運用の権限を狭めることではない。運用で使っている権限を狭めるのは後のgoalにする。
8. **ADR-0034決定4との関係。** ADR-0034決定4（proposed）の「すべてのeventにactorを持たせ、今の問いの主やanswerの主の欄は残して項目を足す」「envが無ければ人とする」はこのADRが取り込み、roleの名前はこのADRの表に置き換える（ADR-0034のreviewerとperson、triageをsupervisorの工程とする扱いは採らない）。マシンのIDとbinaryのversionの記録は決定4に残し、この段では扱わない。ADR-0034は書き換えない。

eventのkind・欄名・flagの綴り、policyの表の中身、testの一覧は[docs/design/](../design/)に書く。

## Alternatives

- **CLIのparseで判定を続ける**: application層を通らない書き込み（supervisorの中の呼び出しや将来のqueue service）が素通りする。境界をmutationの前に置く。
- **LLMにpromptで権限を守らせる**: promptはenforcementにならず、出力の揺れで判断が変わる。認可は決定的なコードにする。
- **reviewerのまま1つのjobのroleにする**: job間で権限が違う（plan reviewとreviewでは触るresourceが違う）のに区別できず、記録も読めない。
- **最初から権限を狭める**: 運用が壊れ、境界の導入と権限の変更の影響が混ざる。まず境界と記録を入れ、狭めるのは後のgoalにする。
- **OPAなど外部のpolicy engine**: 依存と運用が増え、今の規模の静的な表には要らない。

## Consequences

- 状態変更が全てapplicationの境界を通り、誰が起こしたかがeventに残る。後のsandboxやqueue serviceは同じ境界を強制の点にできる。
- host実行のままでは敵対的なプロセスを止められないことを文書が明記し、隔離を約束しない。
- `reviewer`を読む移行の期間が要り、job名の分割の後に起動したjobから新しい名前になる。

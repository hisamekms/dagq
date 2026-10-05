---
id: adr-t1632-1
type: adr
title: 固定バイナリが依存先の着地を含むことを要するtaskは登録で宣言し、supervisorは自分のbuildが依存先の着地commitを全て含むまでclaimしない
status: accepted
created: 2026-10-05
updated: 2026-10-05
accepted_on: 2026-10-05
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
related:
  - adr-0038
  - adr-0073
  - adr-0080
  - adr-t813-2
  - design-supervisor-lifecycle-claim-defer
  - design-supervisor-lifecycle-auto-update
  - design-domain-model
---

# ADR-t1632-1: 固定バイナリが依存先の着地を含むことを要するtaskは登録で宣言し、supervisorは自分のbuildが依存先の着地commitを全て含むまでclaimしない

## Context

taskの依存は、依存先が`completed`になれば満たされ、taskはclaimの候補になる（依存の満たし方。goalへの依存はADR-0038）。依存先の着地から、自動更新（[ADR-0073](0073-kind-additions-are-compatible.md)）がそれをbuildしてe2eを流し、固定バイナリを入れ替えてsupervisorを引き継がせるまでには数分かかる。

この間に、固定バイナリが依存先を含むことを前提にするtask（固定バイナリで新しいflagを打つ、新しい挙動を本番のqueueで確かめる、など）がclaimされると、そのrunは古いバイナリで動いて落ちる。2026-09-29、task 1067はverifyの1本目に「固定バイナリのbuild識別子のcommitがtask 1065の着地を含む」の関門を手で書いていたが、1065の着地の10秒後、`update_installed`の約4分前にclaimされ、関門で`failed`になった（finding 76、goal 105）。verifyの関門はclaimの後にしか効かないので、claimを止められない。同じ形の関門は1208・1219・1221・1402などで繰り返された。

## Decision

1. **taskの宣言**: taskは「固定バイナリが依存先（`--depends-on`の直接の依存先）の着地を全て含むこと」をclaimの条件として持てる。登録で宣言し、draft / submittedの間は宣言し直し・外せる。既定は宣言しないで、宣言しないtaskのclaimは今と変わらない。依存の満たし方（いつ候補になるか）は変えず、候補を選ぶときの条件として足す。
2. **判定はsupervisor自身のbuild**: supervisorは候補を選ぶとき、自分のbuild識別子が名乗るcommitが、依存先の着地commit（`run_integrated`のcommit）を全てgitの祖先として含むかを見て、含まなければclaimせずに待つ。queueの記録（`update_installed`）ではなく自分のbuildを見るのは、自動更新の引き継ぎで入れ替わったsupervisorのbuildは固定バイナリと同じで、記録の有無や引き継ぎの成否に依らず確かな値だから。
3. **1つのtaskの控えとして記録する**: 待ちはhotspotの控え（ADR-0080）・workerを動かせないtask（ADR-t813-2）と同じ、候補を順に見て1つのtaskだけを飛ばす控えとし、同じ始まりと終わりのeventに理由を分けて記録する。`status`と`show`は待っているtaskと、含まない依存先の着地を示す。上限は無く（入れ替わるまで待つのが目的）、`interrupt`の優先度でも待つ（急いでも同じに落ちる）。
4. **判定できないbuild**: `.dirty`のbuildはそのcommitで判定する（そのcommitに手元の変更を足して作ったもの）。commitを名乗らないbuild（リリース、`+unknown`）は判定できないので、待たずにclaimし、logに残す（来ないbuildを待ち続けない。dagqのソースでないrepositoryでは宣言に意味が無い）。gitが祖先かを答えられない着地は、含むと分からないので待つ。
5. **見直し**: build識別子はprocessの間変わらない。引き継ぎ（`supervisor_handed_off`）でexecした新しいprocessの最初のpassで、全ての待ちを新しいbuildで判定し直し、含めばclaimする。

## Alternatives

- **verifyの手書きの関門を続ける**: claimの後にしか効かず、落ちたrunがtriage・復旧・askと人の時間を使う（finding 76では約1時間）。採らない。
- **queueの`update_installed`を見る**: 固定バイナリのfileが入れ替わっても、そのsupervisorが引き継いだとは限らない（`up --auto-update`でない、引き継ぎに失敗した）。走っているbuildが着地を含むかは自分のbuild識別子が直接言う。採らない。
- **依存の満たし方を変える（着地を含むbuildが入るまで`completed`を依存の充足と見ない）**: 全てのtaskの依存に効き、宣言しないtaskまで遅らせる。必要なのは一部のtaskだけ。採らない。
- **判定できないbuildでは待つ**: リリースのバイナリや`+unknown`のbuildでは待ちが終わらず、taskが黙って止まる。採らない。
- **待ちを専用のevent kindで記録する**: 1つのtaskを飛ばす控えの記録・`status`・`stats`・起動時の組み立て直しを二重に持つことになる。理由で分ければ足りる。採らない。

## Consequences

- 固定バイナリの関門を要するtaskは、verifyの手書きのscriptでなく宣言で書ける（登録の案内はgoal 105のconstraintsにより、宣言を知るバイナリが固定バイナリに入ってから別のtaskが書く）。
- 宣言したtaskは、依存先の着地の後、自動更新の引き継ぎまでclaimされない。自動更新が止まっている間（ビルドやe2eの関門の失敗、`approve_update`待ち）は待ち続け、`status`の待ちの理由に出る。
- 自動更新はruntimeを変える着地でだけbuildする（ADR-0073）。依存先の着地がruntimeを変えない（docsだけなど）なら、その着地を含むbuildは、後でruntimeを変える別の着地がbuildされて引き継がれるまで来ず、宣言したtaskはそれまで上限なく待つ。runtimeを変えない依存先だけを待つtaskは宣言しない（宣言の要らない場合）。
- 宣言を知らない古いバイナリは列を読まないので、宣言したtaskを待たずにclaimする（互換のmigration）。
- 判定は依存先の着地commitがsupervisorのrepositoryにあることを前提にする。別のcloneのbuildなどで祖先を答えられなければ待ち、人が宣言を外すか入れ替えるまで続く。

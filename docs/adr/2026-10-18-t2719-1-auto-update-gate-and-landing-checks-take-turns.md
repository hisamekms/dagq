---
id: adr-t2719-1
type: adr
title: 自動更新のjobのbuildとe2eの関門と、着地の検証（着地の検証のコマンドとlanding recheck）を同時に走らせず、queueのlock fileで順番を決めて交互に流す
status: accepted
created: 2026-10-18
updated: 2026-10-18
accepted_on: 2026-10-18
owners:
  - hisamekms
tags:
  - runtime
  - auto-update
  - landing
related:
  - adr-0045
  - adr-t1233-2
  - adr-t2257-1
  - adr-0068
  - design-supervisor-lifecycle-auto-update
  - design-supervisor-lifecycle-integrate
---

# ADR-t2719-1: 自動更新のjobのbuildとe2eの関門と、着地の検証（着地の検証のコマンドとlanding recheck）を同時に走らせず、queueのlock fileで順番を決めて交互に流す

## Context

`up --auto-update`のsupervisorは、mainがruntimeを変える着地で進むたびに自動更新のjob（releaseのbuildと全部のe2e）を始める（[ADR-0045](0045-build-identifier-explicit-migrate-schema-compat-handoff-and-auto-update.md)決定17）。
着地の検証（`integrate`の検証と[landing recheck](0068-recheck-waiting-runs-after-each-landing.md)）が走っていても始めるので、着地が30〜40分おきに続くとbuildと検証が途切れず重なり、hostのloadが上がってloadによるclaimの控えでclaimが何時間も止まった。
e2eどうしはhostのlockで1本ずつ（[ADR-t1233-2](2026-10-02-t1233-2-e2e-runs-on-the-host-after-review-passes.md)決定4）だが、buildと着地の検証の排他は無い。
人は`max_load`を上げず、重い処理を重ねずに順に流すことを選んだ。

## Decision

1. **自動更新のjobの「関門」と「着地の検証」は同時に走らない。** 関門はjobのbuildの前からe2eの関門の判定までで、その後の入れ替え・引き継ぎ・見張りは含めない。着地の検証は、supervisorの着地と人が直接打つ`integrate`の検証のコマンド（コマンドの無いtaskは含めない）と、landing recheckである。queueのどのprocess（どのsupervisor・人のCLI・jobのprocess）のものでも同じに扱う。
2. **順番はqueueのdirの2つのlock file（OSのファイルlockの共有と排他）で決める。** 「番」のlockを関門は排他で、着地の検証は共有で持つので、検証どうしは今どおり並んで走り、関門とは重ならない。jobは関門を始める前に「待ち」のlockを排他で持ってから番を待ち、着地の検証は待ちのlockが持たれていないことを確かめてから番を取るまで、その確かめ（待ちのlockの共有）を手放さない。だから、jobが待ち始めた後に始まる検証は無い。lockはprocessが終われば消えるので、殺されたjobや検証が番を持ち続けることは無い。どちらを待たせるかの判断は副作用の無い関数1つにする。
3. **待つ側。** supervisorはjobを今どおり起動し、jobのprocessが番を待つ。supervisorの着地とlanding recheckは始める前に番を待たずに取ろうとし、取れなければ始めずに次のpassで見直す（着地の枠を持ったまま待たない）。人の`integrate`は検証のコマンドの前で番を取れるまで待つ。supervisorが見た後に始まった関門と重なるときも、着地は検証の前に同じく待つ。
4. **飢えない決まり。** 関門は待ち始めた時に走っている着地の検証の終わりまでしか待たない（待ちのlockの間は新しい検証が始まらないので）。着地の検証は1回の関門の終わりまで待ち、次の関門はmainが進む（次の着地）まで来ないので、着地が続いても両者は交互に進む。
5. **待っていることはlogに出す。** jobは自分のlogに、supervisorの着地とlanding recheckはそれぞれ待ち始めに1回、人の`integrate`は待ち始めに、何を待つかを書く。
6. **重い処理の種類はruntimeのjobの種類で扱う。** 対象は「自動更新のjobの関門」と「着地の検証」という種類で、buildやe2eのコマンド・Rust・cargoの値をこの判断に持ち込まない（[ADR-t2257-1](2026-10-12-t2257-1-container-runtime-takes-project-and-host-values-from-config.md)）。
7. **含めないもの。**
   - workerのrunのbuildとtest: claimの控え（`max_load`とloadによる控え）が扱い、その規則と値は変えない。
   - runtimeが流すrunの着地の前のe2eと`install`の関門: e2eどうしは今のhostのlockで1本ずつのままで、runのe2eはそのlockで自動更新のe2eと重ならない。
   - release更新のjobと`install --release`: dagqのソースでないrepositoryか人のコマンドでだけ動き、自動更新のbuildの出力のlockで自動更新のjobとは重ならない。jobの種類として後から足せる。
   - reviewのjob: agentのjobで、buildではない。

## 他のADRとの関係

- ADR-0045決定17は置き換えない。きっかけ（runtimeを変える着地で始める、同時に1つ、着地中の次の着地は積まずに最新のheadを1回）はそのままで、jobのbuildの前に「走っている着地の検証の終わり」という待ちを足す。
- ADR-t1233-2決定4は置き換えない。e2eを1本ずつにするhostのlockはそのままで、この決定は別の種類（自動更新の関門と着地の検証）の間の順番である。

## Alternatives

- **supervisorがqueueの記録（runの状態・jobの段）を見て決める**: 読んでから始めるまでの間に別のsupervisorや人の`integrate`が始めうり、待っていることがprocessの中にしか無いので、別のprocessの着地が待つ関門を追い越す。
- **番のlockを待って取るだけ（待ちのlockなし）**: OSのファイルlockは排他を待つ者がいても共有を通すので、着地が続くと関門が飢える。
- **待ちのlockを確かめてすぐ手放し、それから番を取る**: 確かめと番の間にjobが待ち始めると、その後に検証が番を取って始まり、待ち始めたjobを追い越す。
- **runtimeの重い処理を全部（runのe2e、release更新、workerのbuild）同じ排他に入れる**: workerのbuildはclaimの控えが扱い、e2eはlockが1本ずつにしている。重なって困ったのは自動更新の関門と着地の検証で、範囲を広げると着地のスループットを落とす。

## Consequences

- 関門が走る約10分の間、着地の検証とlanding recheckは始まらず、着地の待ちがその分伸びる。代わりに検証とbuildが重ならず、loadが`max_load`を超えにくい。
- jobは着地の検証の終わりを待つ間も始まったjobとして見え、何を待つかはjobのlogが持つ。
- 種類を足すとき（release更新など）は、jobの種類として足し、同じlock fileを取らせる。

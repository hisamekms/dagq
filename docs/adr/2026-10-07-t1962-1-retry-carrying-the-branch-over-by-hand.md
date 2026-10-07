---
id: adr-t1962-1
type: adr
title: 人とinboxが終わったrunのbranchを引き継ぐretryを手で打つ
status: accepted
created: 2026-10-07
updated: 2026-10-07
accepted_on: 2026-10-07
related:
  - adr-0047
  - adr-t883-1
  - adr-t1818-2
owners:
  - hisamekms
tags:
  - runtime
  - recovery
  - authorization
---

# ADR-t1962-1: 人とinboxが終わったrunのbranchを引き継ぐretryを手で打つ

## Context

復旧jobのdecideのaskに人が`retry_inherit`を選んでも、答えが選択肢の原文と一致しなければ自由な答えとして閉じ、runtimeは何も適用しない。
commit済みの成果を持ったまま`failed`で止まったrunを、人とinboxが成果ごと進める手段が無かった。
素の`dagq ready`のretryは今のmainからやり直して成果を捨て、hold・provider・modelの解除を含むreleaseはまだ無い。
decideの答えに`retry_inherit`を足す案は、ADR-0047決定3の答えの集合を変え、ADR-t1818-2が退けた案（引き渡しをdecideのruntimeの答えにする）に近い。

## Decision

1. userとinbox（人の言葉で）は、`in_progress`のtaskの最新runが`failed`か`interrupted`のとき、`ready --inherit`（理由を添える）でそのrunのbranchの独自のcommitを次のrunに引き継ぎ、taskを`ready`に戻せる。
   既存の`ready`の形に足し、認可は`ready --bypass-review`と同じuserとinboxだけのcapabilityにする（plan reviewを通らない、人の判断の`ready`だから）。
2. 前提はstoreの1つの確定の中で確かめ、崩れていれば理由つきで断って何も変えない: actorがuserかinbox、taskが`in_progress`、最新runが`failed`か`interrupted`、そのrunに生きたprocessもstaleでないleaseも無い、runのbranchにbaseより先のcommitがある（無ければ素の`ready`を案内する）。
3. 引き継ぎの処理は自動の`retry_inherit`と共有する: 同じ選び方のcommitを`refs/dagq/runs/<run-id>`に残し、taskをplan reviewなしで`ready`に戻し、自動のものと同じ引き継ぎの記録を残し、次のrunは同じ引き継ぎ（記録とpromptの引き継ぎの節）で始まる。
   refは確定が通った後にだけ書き、断ったときはrefも変えない。
4. 自動の`retry_inherit`のtaskごとに1回の制限は掛けず、手での引き継ぎはその回数を使わない。
   人とinboxの明示の判断で、自動の制限が止めたい繰り返しとは違うから。
   記録に誰が打ったか（userかinbox）を持たせて自動のものと分ける。
5. その確定で、runの復旧jobの閉じていないdecideのaskを閉じ、誰が、なぜ、どのrunのどのcommitを引き継いだかを引き継ぎの記録に残す。
6. 復旧jobのラウンドとは、ラウンドの開始と同じく即時の確定の中でrunのleaseを見て競合を解く。
   staleでないlease（ラウンドが生きている）があれば断り、task・ask・refを変えない。
   staleなleaseは同じ確定で取り除き、止まっていたラウンドの適用権（その古い候補を含む）を失わせる。
   手での確定の後は、新しいラウンドはそのrunを取らず、閉じたaskへの答えも適用されない。
   どちらの順でも、runの引き継ぎは高々1回になる。

## Alternatives

- decideの答えに`retry_inherit`を足す: ADR-0047決定3の答えの集合を変え、ADR-t1818-2が退けた形に近いので採らない。
- 別の名前のコマンド: 今の`ready`は既に「`in_progress`のtaskをretryで`ready`に戻す」操作で、引き継ぐかどうかはその変種なので、flagにした。
- 手での引き継ぎも自動の1回に数える: 人の明示の判断を自動の制限で止めることになり、自動の`retry_inherit`の後に人が成果を進められなくなるので採らない。
- staleなleaseのあるrunも断る: 死んだラウンドのleaseが残るとrunを誰も進められず、人に待たせるだけになるので、取り除いて確定する。
- refを確定の前に書く（自動の経路の順）: 断ったときにrefだけが変わるので、手での経路では確定の後に書く。

## Consequences

- inboxは人の言葉（decideのaskで選んだ`retry_inherit`）に従い、止まったrunの成果を次のrunに進められる。
- releaseは作らず、後のreleaseはこの引き継ぎを再利用できる。
- 確定の後にrefを書けなかったときは、taskは`ready`のままrefが無い。
  次のrunのpromptはbranchとheadを名指すので、branchが残っていれば引き継げる。
- 関係: [ADR-t883-1](2026-09-30-t883-1-edit-ended-run-verification-before-inherited-retry.md)（verifyを直してから`retry_inherit`）のverifyの修正の後にも、人はこの操作で引き継げる。
  ADR-0047決定3とADR-t1818-2は変えない。

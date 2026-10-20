---
id: adr-t2141-1
type: adr
title: landing recheckでrunのcommandがコードの分類で落ちたpassはmainの木だけでも流し、mainも落ちればrunをresumeせずinboxに知らせる（ADR-0068決定3・5をamends）
status: accepted
created: 2026-10-20
updated: 2026-10-20
accepted_on: 2026-10-20
amends:
  - adr-0068 decision 3
  - adr-0068 decision 5
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - operations
related:
  - adr-0068
  - adr-t639-1
  - adr-0049
  - adr-t2424-1
  - design-supervisor-lifecycle-integrate
---

# ADR-t2141-1: landing recheckでrunのcommandがコードの分類で落ちたpassはmainの木だけでも流し、mainも落ちればrunをresumeせずinboxに知らせる（ADR-0068決定3・5をamends）

## Context

[ADR-0068](0068-recheck-waiting-runs-after-each-landing.md)決定3は、recheckのcommandが落ちたrunをその場でresumeへ回し、決定5はそのresumeを着地の検証の失敗と同じく数える。[ADR-t639-1](2026-09-27-t639-1-retry-verification-failures-of-the-host-once.md)のhostの分類（disk_full・killed・timeout）で落ちたものだけはresumeしない。

2026-09-30の着地の直後、着地待ちの19 runが12msのうちに同じ出力で落ちた。commandはrunのコードをcompileする前にrecheckのbuild dirへ書けず、出力は`could not compile`の印でbuild_errorに分類され、6本のresumeが始まり1本はworker_failedになった。原因はsupervisorの起動環境で、どのrunのworkerも直せない。mainが壊れたときも同じで、各runのresumeはmainを直さない。

初めの案は「2本以上のrunが正規化した同じ印で落ちたら共通の原因とみなす」だった。plan reviewが、着地した変更に合わない呼び出しを複数のrunが持つ場合も同じ印で落ちることを指摘し、判定をmainの木で引くことにして、人がask 634で承認した（人に上げる失敗を足すので、ADR-t639-1決定5の趣旨に当たる）。

ADR-0068は番号付きの決定を6つ持ち、変えるのは決定3と5の一部なのでamendsにする。

## Decision

1. **runのcommandがコードの分類で落ちたpassでは、mainの木だけでcommandを1回流す。**
   1回のrecheckで、hostで流したrunのcommandがhostの分類でない分類（build_error・lint・test_failure・format・coverage_below・unknownなど）で落ちたrunが1本以上あれば、そのpassで1回、mainの木だけ（runをmergeしない）で同じcommandを、同じscratch worktreeとbuild dirで流す。
   containerで流したrunの失敗と、hostの分類で既に何も判定しなかったrunは引き金に数えない（mainの確かめはhostで流れ、containerのrunのcommandと同じ環境ではない）。commandを流さなかったpassでは流さない。
2. **mainが通れば、runの失敗は今までどおりresumeする。**
   複数のrunが同じ診断で落ちても、mainが通るならrunのコードの不具合として扱う。これが既存のコードの失敗の扱いとの境界である。
3. **mainも落ちれば、そのpassで落ちたrunはresumeしない。**
   引き金になるrunは`needs_session`にも`held`にもせず`awaiting_integration`に残し、resumeの回数も使わない。記録はhostの分類の失敗と同じ「何も判定しなかった」形にし、mainの失敗とmainの出力の場所を添える。queueにはinboxへのattentionを1件残す。mainが落ちたままの次のrecheckでは足さず、mainの確かめが通ったときに終わりの印を残す。attentionが残る間は、hostでcommandが通ったrunのあるpassでもmainを確かめる（落ちたrunが無いままmainが直ると終わりの印が残らず、次にmainが壊れても知らせられないため）。次にmainが動いたrecheckで同じrunが落ちmainが通れば、今までどおりresumeする。
4. **mainの確かめが判定できなければ、今までどおりresumeする。**
   gitやcommandが起動できなかったとき、時間切れ、hostの分類で落ちたときは、mainについて何も言わず、runの失敗を決定3（ADR-0068）どおり扱う。判定できないものを人に上げない。
5. 変えないもの: hostの分類（ADR-t639-1）の集合と分類の仕方。人に上がるのは「mainの木だけでも落ちる」と確かめた失敗だけ。merge-treeの衝突の扱い。ADR-0068決定5のうち、mainが通ったpassのcommandの失敗のresumeを数えること。

## Alternatives

- **2本以上のrunが正規化した同じ印で落ちたら共通とみなす（初めの案）**: 着地した変更に合わない呼び出しを複数のrunが持つときのように、runのコードの不具合でも条件を満たし、各runが直すべき失敗を人に上げてしまう。閾値の本数にも根拠が無い。
- **build dirの書き込みの失敗の印をhostの分類に足す**: 人に上げる集合を広げる変更で（ADR-t639-1決定5）、mainの破損は拾えない。書き込みの確かめは別のtaskがcommandの前に行う。
- **mainの確かめを毎回のrecheckで流す**: 落ちたrunが無いpassでも1回分のcommandの時間を使う。

## Consequences

- mainか環境の破損で全ての待つrunが落ちても、resumeのslotと回数を使わず、inboxに1回だけ知らせる。
- runのcommandがコードの分類で落ちたpassは、commandを1回多く流す。
- mainが壊れている間、待つrunは着地の検証まで直されない。mainを直す変更が着地してmainが動けば、次のrecheckが改めて確かめる。

---
id: adr-t4027-1
type: adr
title: 同じrunの新しいsessionの引き継ぎのpromptに、そのrunで人が下した判断（send_backの理由・worker_question・decideの答え・stalledへの指示の答え）を届いたかを添えて本文のまま必須の節として入れ、必須の節とdagqの読むコマンドの例外を人の判断の節の2つに広げる（ADR-t2080-1決定2、ADR-t2072-1決定2・決定3をamends）
status: accepted
created: 2026-11-03
updated: 2026-11-03
accepted_on: 2026-11-03
amends:
  - adr-t2080-1 decision 2
  - adr-t2072-1 decision 2
  - adr-t2072-1 decision 3
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - worker
related:
  - adr-t4018-1
  - adr-t2852-1
  - adr-t2080-1
  - adr-t2072-1
  - design-provider-lifecycle
  - design-supervisor-lifecycle-prompt
---

# ADR-t4027-1: 新しいsessionの引き継ぎにそのrunの人の判断を必須の節として入れ、必須の節と読むコマンドの例外を人の判断の節の2つに広げる（ADR-t2080-1決定2、ADR-t2072-1決定2・決定3をamends）

## Context

[ADR-t2080-1](2026-10-08-t2080-1-fresh-session-on-send-back-and-resume-when-peak-context-exceeds-threshold.md)決定1は、直前のturnのpeak_contextが閾値を超えていれば、同じrunのまま会話を引き継がない新しいsessionで続けると決めた。
決定2（[ADR-t2852-1](2026-10-19-t2852-1-handoff-carries-local-check-records-resume-timeout-and-findings-once.md)がamendsした今の形）は、引き継ぎのpromptにcommit・未commitの変更・今のHEADのtreeの手元の検証の記録・known failures・receiptの要約・送るはずだった依頼を載せる。
そのrunがそれまでに受けた人の判断（approve_landingの`send_back`の理由、worker_question・decideの答え、stalledのaskへの指示の答え）は、届いた先の会話とともに落ち、送るはずだった依頼が運ぶ最新の差し戻しの理由のほかは新しいsessionに渡らなかった。
30日の`session_renewed`の82件のうち54件が同じrunの人の判断の後に起き、人が同じ判断を言い直したrunがあった（docs/plans/human-decision-delivery-spike.mdの「G」）。

[ADR-t4018-1](2026-11-02-t4018-1-worker-prompt-carries-unapplied-person-decisions-of-the-previous-run.md)は、retryで作り直したrunのpromptの「前のrunの未適用の人の判断」の節に限って、一覧から外さない必須の節と、切るときの`dagq events --full --task <ID> --kind <KIND>`の読むコマンドを許した（[ADR-t2072-1](2026-10-08-t2072-1-worker-prompts-and-next-turns-carry-decision-material-within-limits.md)決定2・3の例外）。
ほかの節には決定2の制限（読む方法はworktree・git・goalのdocに限る）が残る。

## Decision

1. **新しいsessionの引き継ぎのpromptに、そのrunで人が下した判断の節を足す（ADR-t2080-1決定2に足す）。**
   そのrunのeventsから、approve_landingの`send_back`の理由、そのrunのworker_question・decideのaskへの答え、stalledのaskへの指示の文の答えを集め、本文をaskのid・種類・時刻とともに、人の判断として必ず従うものとして入れる。
   各判断には、前のsessionに届いたかを添える。
   届いたかは、そのsessionに実際に送った記録だけで決める（記録の無いものは届いていないとみなす）。
   `send_back`とworker_question・decideの答えはADR-t4018-1と同じ判定を共有し、stalledの指示の答えは、その答えを運んだ依頼の記録かその送信を記した記録で決める。
   stalledのaskの答えのうち`wait`・`intervene`・`stop`と復旧のjobの選択肢（ラベルの有無に依らない）は指示ではないので入れない。
   送るはずだった依頼がそのものとして運ぶ`send_back`は、この節に重ねて書かない（同じ言葉を含むだけの依頼では省かない）。
   判断が無ければ節を出さない。
   ADR-t2080-1の決定1・3・4・5と、ADR-t2852-1のほかの中身は変えない。
2. **必須の節と`dagq`の読むコマンドの例外を、人の判断の節の2つに広げる（ADR-t2072-1決定2・決定3をamends、ADR-t4018-1の例外を広げる）。**
   ADR-t4018-1が「前のrunの未適用の人の判断」の節に限って許した、一覧から外さない必須の節と、切るときにaskのidと`dagq events --full --task <ID> --kind <KIND>`の読むコマンドを名指すことを、次の2つの節に当てる。
   (i) 新しいsessionの引き継ぎのpromptの「このrunで人が下した判断」の節（決定1）。
   (ii) 新しいrunのpromptの前のrunの判断の節を、届いていない判断に加えて前のrunのsessionに届いた判断も含むものに広げた節。
   上限と切り方はADR-t4018-1と同じにする: 全体の上限で外さず、自分の上限を越えるときだけ本文を切るか判断を節から外し、切ったことを記録に書き、askのidと読むコマンドを名指す。
   この2つの節のほかの節の読む方法は、決定2のままworktree・git・goalのdocに限る。

引き継ぎの全体の上限は、この節の自分の上限の分だけ大きくなる。
届いたかを決める記録の種類と照合の条件・節の上限の値・読むコマンドの形・promptの文面はコードのdoc commentと[Provider lifecycle](../design/provider-lifecycle.md)・[Prompt](../design/supervisor-lifecycle/prompt.md)が持つ。

## Alternatives

- **この節は読むコマンドを書かず、切った残りを読む方法が無いと書く**: 人の判断の本文はqueueのeventsにだけあり、worktreeにもgitにも無い。読む方法を書かなければ、上限で切った判断をworkerが読めず、人が同じ判断を言い直す穴が残る。workerのtokenはqueueを読めるので、ADR-t4018-1と同じ読むコマンドを名指す。
- **判断の節を必須にせず、全体の上限で外れてよい節にする**: 大きな引き継ぎ（長いcommitの一覧・検査の記録・依頼）のときに節ごと落ち、落とした判断をworkerが読めず人が言い直す穴が残る。節には自分の上限があるので、必須にしても全体の上限は守れる。
- **届いた判断を入れず、届いていないものだけを入れる**: 新しいsessionは会話を引き継がないので、前のsessionに届いた判断も新しいsessionには届いていない。届いたかは、workerがどの判断を前の作業に既に反映したかの手がかりとして添える。

## Consequences

- 同じrunの新しいsessionは、そのrunで人が下した判断を本文で受け取り、従う。
- 引き継ぎのpromptに`dagq`の読むコマンドが現れるのは、この節が切られたときだけ。
- 新しいrunのpromptの前のrunの判断の節を届いた判断にも広げる実装は後続のtaskが行い、この決定に従う。
- 届いた記録が書けなかった判断は届いていないと添えられ、前のsessionで反映済みのものが重ねて示されることがある。

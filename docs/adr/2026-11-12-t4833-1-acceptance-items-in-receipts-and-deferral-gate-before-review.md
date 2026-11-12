---
id: adr-t4833-1
type: adr
title: receiptに受け入れ条件の項目ごとの状態と根拠を持たせ、答えの無い先送りをreviewの前にworkerへ戻す
status: accepted
created: 2026-11-12
updated: 2026-11-12
accepted_on: 2026-11-12
amends:
  - adr-t1420-1 decision 3
owners:
  - hisamekms
tags:
  - runtime
  - worker
  - review
related:
  - adr-t1420-1
  - adr-t2599-1
  - adr-t3033-1
  - plan-acceptance-precheck-spike
---

# ADR-t4833-1: receiptに受け入れ条件の項目ごとの状態と根拠を持たせ、答えの無い先送りをreviewの前にworkerへ戻す

## Context

[ADR-t1420-1](2026-10-03-t1420-1-worker-maps-each-acceptance-criterion-before-the-receipt.md)は、receiptの前に受け入れ条件を項目ごとに根拠へ対応づけ、満たせない条件はfollow_upへ回さずworker_questionかfailedのreceiptにすると決めた。
しかし文のsummaryだけでは、workerが先送りを開示したままrunのreviewへ進むことを機械で止められない。
[Spike](../plans/acceptance-precheck-spike.md#推奨)では先送りの理由51件全てがreceiptの文に現れ、標本5件全てでreviewの前に見つけられた。
受け入れ条件の被覆だけを安く検査するため、対応づけを形のある欄として受け取り、答えの無い先送りをreceiptの受理で止める。

## Decision

1. **receiptに受け入れ条件の項目ごとの欄を設ける。**
   各要素は項目の番号、状態`met` / `deferred`、空でない根拠を持つ。
   根拠は変えたファイル・文書の節、testの名前、測る・確かめるコマンドやreceiptのevidenceを指し、testが該当しない項目ではその理由を書く。
   新しいtestや重い検査をworkerに要求せず、既存の手元の検証の範囲を守る。
   `met`は満たしたというworkerの申告、`deferred`はまだ満たしていないという申告で、根拠には未達の内容と参照するworker_questionも記す。
   対象の各番号にちょうど1要素を要求し、抜け・重複・存在しない番号・未知の状態・空の根拠は欄の不備とする。
   ADR-t1420-1決定3の「schemaは変えない」を改めるが、summaryの項目ごとの短い句は残し、決定1・2・4の対応づけと修正の手順は、この欄を書く手順として保つ。
   初回・resume・reviseで書くreceiptに同じ規則を当てる。

2. **runtimeは成功を申告するreceiptを受理する際、欄の不備と答えの無い`deferred`を、runのreviewの全体とsubagentのjobを1本も起動する前にworkerへ返す。**
   `succeeded`と`already_satisfied`を対象にし、`failed`のreceiptは失敗を報告する既存の経路を妨げない。
   `deferred`ごとに、同じrunに属し、その受け入れ条件の番号を明示したworker_questionへの参照と、queueに記録された答えの存在を照合する。
   別runの質問、別項目だけを扱う質問、未回答、receipt内だけの「承認済み」という文では通さない。
   欠けた番号と理由を示し、要求evidenceの欠けと同じく`needs_session`から既存の自動resumeで同じworkerに書き直させる。
   これはreviewの差し戻しではなくreceiptの受理のやり直しなので、reviewの自動の手直しの回数には数えず、実際に開始するresumeを既存のresumeの回数と上限に数える。
   上限に達したら既存のresume上限後の復旧経路へ渡し、検査を飛ばしてreviewや着地へ進めない。
   [ADR-t2599-1](2026-10-17-t2599-1-run-review-revises-counted-across-the-run-and-judged-by-code-findings-past-the-limit.md)の数えるreviewの手直しと上限後の判定は変えない。
   その決定1の上限の置き場はamended_byを辿った[ADR-t3033-1](2026-10-22-t3033-1-run-review-revise-limit-from-the-landing-branchs-dagq-toml.md)が現行であり、定数への固定には戻さない。

3. **当該taskの受け入れ条件の番号を名指すfollow_upは、その番号の`deferred`と同じに扱う。**
   欄が`met`でも番号を名指すfollow_upがあれば、同じ項目を扱うworker_questionの答え無しには通さない。
   番号との対応は機械で読める参照として持たせ、欄とfollow_upの両方から照合する。
   自由文から未達の意味を推測する検査は足さず、workerには番号の参照を省いて先送りを隠すことを許さない。
   番号を名指さない改善提案の存在だけでは止めない。

4. **古いreceiptも、次に成功のreceiptとして受理するときに同じ検査を当てる。**
   欄が無ければ全項目を`met`と補完せず、欄の不備としてworkerへ返し、新しい欄を書かせる。
   既に着地したrunのreceiptを書き換えたり、review済みのrunを遡って再reviewしたりはしない。
   taskの受け入れ条件に明示の項目番号があればそれを用い、番号が無いtaskでは受け入れ条件の全文を1項目（番号1）としてworkerへの依頼と検査の両方で扱う。
   自由文を意味で分割したり、queueのtask本文を編集して採番したりしない。

5. **機械の検査は欄の形、`deferred`（番号を名指すfollow_upを含む）の有無と質問の答えの存在だけを確かめる。**
   `met`の根拠が真に条件を満たすか、testが形だけか、実装が入口へ接続されているかは判定しない。
   答えがあることも、先送りの許可や受け入れ条件の達成を意味せず、答えの内容との整合はrunのreviewが判定する。
   根拠の真偽と被覆の意味の判定はrunのreviewと、後に設ける確認のjobの責務であり、この検査を通ったことを根拠にrunのreviewの判定の基準を弱めない。

## Alternatives

- summaryだけを読み続ける: 機械で番号・状態を照合できず、開示された先送りもreviewの費用を払う。
- `met`の根拠の真偽までプログラムで判じる: 形だけのtestや接続の不足は意味の判断を要し、被覆だけの安い検査という境界を越える。
- 欄の無いreceiptを通す: 古い形を使うだけで先送りの検査を回避できる。

## Consequences

実装は後続のtask 4834が行う。
このADRでは実装前の方針として欄の形と検査の分岐を置き、docs/design/を変えない。
実装時にdesignへ入口と変わる不変条件を足し、具体的な欄の構造・名前・意味は定義のそばのdoc commentが持つ。
workerのprompt・resume・reviseとlocal-checksの対応づけの規則も実装と合わせ、実装の着地までは現行のreceiptを使う。
番号を名指す先送りはreviewより前に止まるが、虚偽の`met`や自由文だけの隠れた先送りをこの機械の検査だけでは見分けられない。

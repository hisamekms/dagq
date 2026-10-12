---
id: adr-t2177-1
type: adr
title: queueのbrokerはrun tokenを、host.tomlで人が選んだClaudeの資格情報（claude setup-tokenの長期tokenかAPI key）に差し替え、既定は置かずに未選択と欠けはfail closedにする（ADR-t2114-2決定2・3・5・6をamends）
status: accepted
created: 2026-10-12
updated: 2026-10-12
accepted_on: 2026-10-12
amends:
  - adr-t2114-2 decision 2
  - adr-t2114-2 decision 3
  - adr-t2114-2 decision 5
  - adr-t2114-2 decision 6
owners:
  - hisamekms
tags:
  - runtime
  - security
  - provider
related:
  - adr-t2114-1
  - adr-t2114-2
  - adr-t2113-3
  - design-queue-broker
  - design-security
---

# ADR-t2177-1: brokerはrun tokenをhost.tomlで人が選んだClaudeの資格情報に差し替える

## Context

[ADR-t2114-2](2026-10-08-t2114-2-broker-swaps-the-run-token-for-the-claude-credential.md)は、queueのbrokerがrun tokenを`claude setup-token`の長期tokenに差し替え（決定2・3）、前提（規約・SSE・認証切れと上限の分類、決定5）がだめならAPI keyに替える（決定6）と決めた。
Spikeの(a)（[記録の表](../plans/container-runtime-spike.md)の(a)の行）で、人は2026-10-11に規約を「個人利用なら適合しそう、brokerによる代理は明示的には保証されない」と判断した（goal 38のnote 151554）。
実際のclientは公式のClaude Codeで、第三者製のclientによるOAuthの流用とは違うが、公式の文書は代理を保証していない。
人は長期tokenとAPI keyの両方に対応し、設定で選ぶ形にすることを頼んだ（request 92）。

## Decision

1. **2種類の資格情報に対応する。**
   brokerは、`claude setup-token`の長期token（`Authorization`のbearerに差し替える）と、人がAnthropic Consoleで発行したAPI key（`Authorization`と受けた`x-api-key`を除き、`x-api-key`に付ける）の両方に対応する。
   containerの側（`ANTHROPIC_BASE_URL`にbroker、`ANTHROPIC_AUTH_TOKEN`にrun token）はどちらでも同じで、run tokenはbrokerの外へ送らない。
2. **どちらを使うかはhostの`host.toml`で人が選ぶ。**
   queueのdirの`host.toml`かhost全体の`host.toml`で選び、資格情報のファイルの場所も同じ表に置く。
   `dagq.toml`には置かない。
3. **既定は置かない。**
   containerのruntimeを選んだworkerがあるのに、種類が選ばれていないか、選んだ種類の資格情報が無いか0600でないときは、containerを使えないものとしてfail closedにする（container runtimeのportに理由つきで返し、claimを控える）。
   brokerも、選ばれていない・使えない資格情報で送らず、要求を拒む。
4. **保管と読み手は[ADR-t2114-2](2026-10-08-t2114-2-broker-swaps-the-run-token-for-the-claude-credential.md)決定3の形を両方の種類に当てる。**
   hostの0600のファイルかkeychainに置き、queueのbrokerだけが中身を読む。
   発行と置き換えは人の操作で、AI actorに発行も読み取りもさせず、人のkeychainのClaude Codeのログインは使わない。
   brokerは選ばれた種類だけを読み、認証の失敗や上限で別の種類に切り替えない（決定4を引き継ぐ）。
   API keyの認証の失敗（401）・利用上限・残高の不足の応答も、そのまま実行側に返し、今のClaudeの認証切れと上限の分類と控え（新しいclaimを控えてinboxにask）に乗せる。
5. **利用者への案内。**
   自分のサブスクリプションを個人で使うなら長期tokenを選べる。
   組織・チーム・共有のhost・業務での利用ではAPI keyを勧める。
   `doctor`の未設定の案内と利用者向けの文書がこれを出す。
6. **前提の確認（ADR-t2114-2決定5）の扱い。**
   規約（決定5の1）は上のContextの人の判断（note 151554）で扱う。
   SSE（決定5の2）と認証切れと上限の分類（決定5の3）は、どちらの種類にも共通の素通しの性質で、実tokenでの確認（Spike (a)の手順2〜7）を2026-10-11に人が流した（goal 38のnote 152330）。
   通常の応答・長いSSE・無効なtokenの分類（`TurnFailure::Authentication`）・tokenがファイルに無いことは合で、brokerの側の手当ては要らなかった。
   利用上限は上限の応答を出せず未確認で、確かめたのは応答のheader `anthropic-ratelimit-unified-*`が転送を素通しすることだけである。
   人の決定（note 152336・152337、2026-10-11）: 利用上限の応答は未確認のまま実装に進める。
   手順2〜7の結果と人の決定がSpikeの記録の表に取り込まれた時点（task 2171の着地）で、この実装の着地の条件（ADR-t2114-2のConsequencesの「前提の確認が終わるまで実装を着地させない」）を満たしたと扱う。
   未確認の利用上限は、本番で初めて利用上限に当たったときに、runtimeがそれを`TurnFailure::UsageLimit`（控え）に分類したかを確かめ、合わなければ分類を直す（直しは別のtask）。
7. **[ADR-t2114-2](2026-10-08-t2114-2-broker-swaps-the-run-token-for-the-claude-credential.md)決定6（前提がだめならAPI keyに替える）は、上の1〜3と6に置き換わる。**
   API keyは前提が崩れたときの替え先ではなく、人がいつでも選べる種類の1つになる。

## Alternatives

- **`dagq.toml`に置く**: 種類は契約と費用（サブスクリプションか従量か）と規約の当てはまりを負う人ごと・hostごとの選択で、commitされてcloneした全員が継ぐ`dagq.toml`には合わない。
- **既定を長期tokenにする**: 規約の当てはまりが使い方（個人利用か）に依り、組織や共有のhostで黙って当てはまらない使い方になる。
- **既定をAPI keyにする**: 費用の形がサブスクリプションから従量に黙って変わる。
  どちらも人が選ぶもので、黙って片方にしない（ADR-t2114-2決定6の「替えるかどうかは人が決める」を引き継ぐ）。
- **認証の失敗で別の種類に切り替える**: 人が選んだ契約と費用の形を、brokerが勝手に変えることになる。
  認証切れは人に知らせて人が直す（ADR-t2114-2決定4）。

## Consequences

- 種類を選ぶまで、containerのworkerは動かず、`doctor`と控えの理由が何を選ぶかを案内する。
  integrateのverificationはClaudeのAPIを呼ばないので、この選択に依らない。
- 長期tokenを選んだ利用者は、規約の当てはまりを自分の使い方で判断する。
- 利用上限の分類は本番で初めて当たったときに確かめる項目として残る（決定6）。

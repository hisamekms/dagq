---
id: adr-t4840-1
type: adr
title: 監査は誰が・何の権限で何をしたか・mainに入ったコードの来歴・権限境界・記録の改ざんに答える追記だけの記録で、eventsから導出せず同じuse caseが両方を書き、守る相手a〜eと配置ごとの保証の段階を明記し、記録する分類A〜Jを段0〜3で順に広げ、secretの値を残さない
status: accepted
created: 2026-11-12
updated: 2026-11-12
accepted_on: 2026-11-12
owners:
  - hisamekms
tags:
  - architecture
  - security
  - audit
related:
  - adr-t4840-2
  - adr-t4840-3
  - adr-t728-1
  - adr-t728-3
  - adr-t3985-1
  - adr-t3981-1
  - adr-t2114-1
  - adr-t2113-1
  - adr-t3102-1
  - design-audit
  - design-security
  - plan-audit-log
---

# ADR-t4840-1: 監査の目的と範囲（線引き・脅威モデル・保証の段階・記録する分類・段階案）

## Context

dagqは、AI actorがcodeを書き、reviewし、着地させる。
いま残る記録はevents（運用の状態で、整理して消える）・計測・debug logで、どれも「誰が・何の権限で何をしたか」「mainに入ったコードはどの承認を経たか」「権限境界は守られたか」「記録は書き換えられていないか」に答える形をしていない。
eventのactorと`AnswerAuthority`は人自身とinboxの代行を区別する（[ADR-t728-3](2026-09-27-t728-3-answer-and-delegated-authority-of-the-inbox.md)決定2）が、eventは消え、書き換えを検出できない。
host実行は助言的で、AI actorは人と同じOSのユーザーとして動く（[ADR-t728-1](2026-09-27-t728-1-trust-domains-actors-and-default-deny-capability-authorization.md)決定6）。

人は2026-11-12に、何を記録すべきかから監査を作り直すことを求め、inboxと議論した素案（request 302）と3つの決定（段階案A・守る path A・人が直接答える経路A）を出した。
この決定は素案の目的・脅威モデル・保証の段階・分類・段階案を決定にする。
記録の仕組みは[ADR-t4840-2](2026-11-12-t4840-2-one-audit-service-hash-chained-append-only-records-and-pluggable-stores.md)、守る path と人が直接答える経路は[ADR-t4840-3](2026-11-12-t4840-3-protected-paths-and-weakening-operations-need-a-direct-human-answer.md)が持つ。
欄・action・送り手・path・数値の具体は設計資料の[監査ログ 段0の設計資料](../plans/audit-log.md)が持つ。

## Decision

1. **目的と線引き。**
   監査は次の4つに答える: 誰が・何の権限で何をしたか、mainに入ったコードの来歴、権限境界が守られたか、記録が改ざんされていないか。
   events・計測・debug logとは別の記録にし、eventsから導出しない。
   状態を変えるuse caseが、同じドメインの事実からeventと監査の記録の両方を書く。
   監査は追記だけで、taskやrunを消しても残る。
   過去分は遡らず、監査を始めた時点の記録を最初の記録にする。
2. **守る相手。**
   (a) prompt injectionされた・乗っ取られたagent（worker・review・planner）、(b) 汚染された依存・image・自動更新、(c) 人の承認を誤って中継する・捏造するinbox（AI）、(d) チームの版の内部者、(e) hostのUIDを握った攻撃者。
   外への出口は、今動いているqueueのbroker（[ADR-t2114-1](2026-10-08-t2114-1-queue-broker-is-a-host-process-with-three-destinations.md)）で、containerのrunはbrokerの許可の範囲の外に出られない。
   hostで動くrunはbrokerを通らず、OSのユーザーの届く所に全て届くので、(a)(b)のhostのrunからの外への通信はこの段の監査では捕まえられない。
   削除したresource broker（[ADR-t2113-1](2026-10-08-t2113-1-remove-the-resource-broker.md)）はこの脅威モデルの部品ではない。
3. **配置ごとの保証の段階。**
   hostのadvisoryの配置では、agentは監査の置き場にも人と同じ権限で届くので、改ざんは防げず、守れるのは事後の検出（chainの検証・外のanchor・mainとGitHubとの突き合わせ）までにとどまる。
   防止はcontainer化と別のOSのユーザー（段1）、チームの版のIAM（段2）から得る。
   `doctor`と`status`は今の配置の保証の段階を出す。
   文書と出力は「無効化できない」と書かず、「設定では切れず、切った事実がchainに残る」と書く。
4. **記録する分類。**
   A 主体と認証、B 認可（拒否は全件、許可は特権の操作だけ）、C 計画の変更、D 人の判断（答えた本人と経路）、E 実行の来歴、F コードの来歴と着地（最重要）、G 管理と設定、H 機微データの読み出し、I 監査自体の操作、J 外部アクセス（J1 外の状態を変える操作・J2 資格情報の利用・J3 LLMの呼び出し・J4 取り込み・J5 情報の取得・J6 拒否された接続・J7 許可の範囲の変更）。
   各分類の中身は設計資料が持つ。
5. **段階案と各段で記録するもの。**
   - 段0（今、host advisory）: F（承認を差分が触るfileの変更後のblob hashの組に結ぶ照合を含む）・D・Bの拒否の全件とADR-t4840-3の弱める操作の許可・G・Iのうちverifyと突き合わせの結果と書けなかった件。push（J1のうちmainのpush）のattempted/completedもFとして記録する。
   - 段1（container化と同時）: 監査サービスの分離・別のOSのユーザー・別のDB・outbox、brokerがJ1・J2・J4・J6をenforcedで記録し、tokenをrunとprocessに結ぶ（A）、J5の1件ごとの記録と指示の来歴（task・ask・reviewのcommentを誰が書いたか。C）、自動更新の署名とbuild provenanceの検証。brokerのJの記録は段1で、段0のbrokerは監査を書かない。
   - 段2（チームの版）: Postgres・chainの先頭の行ロック・IAM・S3 Object Lock・KMSの署名・OIDCと仮名化・GitHubのaudit logの取り込み・見直しの証跡・年限とlegal hold・OCSFのexport。
   - 段3（Enterprise）: Hを必須、SIEM、tool call単位のobserved。
   段1以降はこの決定では実装せず、containerとチームの版の実装の後の別のgoalが持つ。
6. **secretの値を残さない。**
   資格情報は名前・scope・宛先だけを残し、値を残さない。
   redactionは送り手が行い、どの名前が資格情報かの判定は`passed_env::credential`と同じものを共有する（判定を二つ持たない）。
7. **LLMに送った本文は保留。**
   J3は段1でもrun・model・文脈のhashまでで、本文を監査に保存するかは人が後で決める。
   量の集計は計測に置き、監査に入れない。

## Alternatives

- **eventsから監査を導出する**: eventsは整理して消え、形も運用の都合で変わるので、導出した監査は欠けと書き換えを区別できない。同じuse caseが両方を書く（決定1）。
- **過去分をeventsから遡って作る**: 遡った記録は検証の根拠を持たず、正本に見えて誤った安心を与える。始めた時点を最初にする（決定1）。
- **host advisoryのまま防止を謳う**: 同じUIDのagentはDBもtriggerも書き換えられる。段階を明記し、防止は段1以降とする（決定3）。
- **段0で全分類を記録する**: 送り手の大半（broker・executor）は段1で形が変わり、二度作ることになる。着地の来歴と人の判断から始める（決定5）。
- **redactionを監査サービスで行う**: 値が一度は監査サービスまで流れる。送り手で落とす（決定6）。

## Consequences

- hostの配置では、監査は事後の検出の材料で、agentの改ざんは外のanchorが無ければ検知もできない。利用者はそれを`doctor`と`status`で読める。
- [ADR-t3985-1](2026-11-03-t3985-1-team-version-identity-by-oidc-human-roles-on-staticpolicy-runner-registration-per-person-inbox-and-no-delegated-landing-approval.md)決定8の監査の不変条件（チームの版の認証した呼び出しの追跡）は保ち、その追跡の主体をこの監査のprincipalの元にする。
- [ADR-t728-3](2026-09-27-t728-3-answer-and-delegated-authority-of-the-inbox.md)決定2の`AnswerAuthority`の区別は保ち、Dの答えの経路の元にする。
- [ADR-t3981-1](2026-11-02-t3981-1-team-version-control-side-on-containers-postgres-and-object-store-with-app-side-write-serialization.md)決定3（ローカルのSQLiteの既定の動き）は保つ。
- 段0はhostのrunの外への通信を記録しないので、(a)(b)の外への持ち出しの追跡はcontainerのrunだけが段1から持つ。

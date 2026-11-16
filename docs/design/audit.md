---
id: design-audit
type: design
title: 監査
status: draft
created: 2026-11-12
scope: runtime
tags:
  - security
  - audit
related:
  - adr-t4840-1
  - adr-t4840-2
  - adr-t4840-3
  - design-security
  - design-security-enforcement
  - design-queue-service
  - plan-audit-log
---

# 監査

誰が・何の権限で何をしたか、mainに入ったコードの来歴、権限境界が守られたか、記録が改ざんされていないかに答える記録の地図。
目的・守る相手・記録する分類・段階案の理由は[ADR-t4840-1](../adr/2026-11-12-t4840-1-audit-purpose-threat-model-guarantee-levels-and-record-categories.md)、監査サービスと記録の形は[ADR-t4840-2](../adr/2026-11-12-t4840-2-one-audit-service-hash-chained-append-only-records-and-pluggable-stores.md)、守るpathと人が直接答える経路は[ADR-t4840-3](../adr/2026-11-12-t4840-3-protected-paths-and-weakening-operations-need-a-direct-human-answer.md)が持つ。
記録の欄・actionの一覧・送り手・守るpathの具体と数値は、実装の前は[監査ログ 段0の設計資料](../plans/audit-log.md)が持ち、実装の後は定義のそばのdoc commentが持つ。
記録の型は`src/domain/audit/record.rs`の`AuditRecord`、chainの検証は`src/domain/audit/chain.rs`の`verify`、use caseとactionの対応表とqueue serviceの入口ごとの特権のuse caseは`src/application/audit.rs`の`USE_CASES`と`SERVICE_ENTRIES`が入口。

一次の置き場と`dagq audit verify`の入口は`src/infrastructure/audit.rs`。
SQLiteの監査は状態の変更と同じtransactionで確定し、UPDATE・DELETEを拒む。

## eventsとの線引き

監査はeventsと別の記録で、eventsから導出しない。
状態を変えるuse caseが、同じドメインの事実からeventと監査の記録の両方を書く。
eventsは運用の状態で整理して消えるが、監査は追記だけで、taskやrunを消しても残る。
計測とdebug logも監査ではなく、量の集計は計測に置く。
監査を始める前の過去分は遡って作らない。

## 流れ

```text
送り手（queue serviceのuse case・supervisor・integrate・updater）
  │  状態の変更と同じtransaction（段0）
  ▼
監査サービス ── 連番・時刻・前の記録のhash・checkpointを付ける
  │
  ▼
一次の置き場（検索できる正本。SQLite、段2からPostgres）
  │  一次の連番を読むカーソルで非同期に複製
  ▼
二次の置き場（追記だけ。最初はJSONLのファイル1つ）
```

送り手は制御側の部品だけで、AI actorは監査サービスに直接送らない。
記録の主体は、送り手が受けた認証の結果から監査サービスが付け、送り手の申告を使わない。
段0の監査サービスはqueue serviceの中のtraitとして動き、段1で別のプロセス・別のOSのユーザー・別のDBに分かれる。
chainと連番は監査サービスが付けるので、置き場を替えても検証の形は変わらない。

着地の来歴は、着地を許した判断（reviewのpass・AIのland・上限の後の収束・人のapprove_landing・直接のintegrate）の記録と、merge後に差分が触る各fileのblob hashの組を照合して結ぶ。
照合は着地の直前の1か所で、経路ごとに置かない（[integrate](supervisor-lifecycle/integrate.md#判断との照合)）。
判断・照合・mergeの監査の記録は、それを述べるrunのeventを書くstoreが同じtransactionで書く（`infrastructure::audit::record_landing`）。
守るpathに当たる差分は、この照合でhuman_attestedの承認が要る。

askを開くときに問いのhashと判断の前提を固定し、回答では認証から決まる本人と経路を記録する。
inboxが人の原文を渡したときはそのhashだけを残し、渡さなかった場合も区別できる。
適用の成功は回答を閉じる変更とともに確定し、失敗した試行は回答を未適用のまま記録する。
弱める操作と守るpathの承認では中継を拒み、hostの人の直接の経路は制御端末で対象のhashを打ち返したことを確かめる。
その記録はhuman_attestedとし、経路の材料は`domain::direct_human::Evidence`のdoc commentが持つ。
同じUIDのagentはこの経路を偽れるので、材料は防止の保証ではなく事後の照合のために保持する。
普通の着地の承認の中継はrelayed_by_inboxとして残す。

認可の拒否は認証できない呼び出しも含めて記録し、書けなかった呼び出しは件数を数えて拒否を返す。
特権の許可は拒否と同じ認可のpolicyの版に結び、通常の許可は記録しない。

管理の操作（binaryの入れ替え・migrate・`up` / `down`）は送り手の部品が認証の主体で記録し、記録の欄と組み立ては`src/domain/audit/administration.rs`が入口。
コマンドの記録は、serviceが答えない間は呼び手が同じidentityで送り直して一度だけ入り、それでも書けなければ操作は済んでもコマンドは失敗を返す（記録を知らない前の版のserviceだけは警告にとどめる）。
downはserviceを止められる前に依頼を記録し、その後の失敗はログへ残す。
管理の失敗理由は送り手が固定のcodeへ変え、詳しいエラーは監査へ送らずログへ残す。
検証情報も送り手が許可した項目と固定のcodeに絞り、詳しい報告を監査へ写さない。
設定の変更はCLIを通らないので、supervisorが周回で観測し、前の記録から変わったときだけobservedとして残す。
CLIを通らないDBの変更は、supervisorの定期のchainの検証が事後に見つけ、inboxのattentionに出す。
検証はsupervisorの周回とserviceの書き込み列の外で進め、短い結果の確定だけを書き込み列へ戻す。

## 保証の段階と境界

| 配置 | 守れること | 守れないこと |
| --- | --- | --- |
| hostのadvisory（段0） | chainを計算し直さない行の書き換え・欠け・入れ替えの事後の検出と、mainとGitHubとの突き合わせ（外のanchorをonにしたときはchain全体の書き直しも） | 同じUIDのagentによる改ざんの防止、外のanchorがoffのときのchain全体の書き直しの検知、hostのrunの外への通信の記録 |
| container・別のOSのユーザー（段1） | agentから監査の置き場への到達の防止、brokerを通る外への通信のenforcedな記録 | hostのUIDを握った攻撃者 |
| チームの版（段2） | IAMによる置き場の隔離、署名したcheckpoint、OIDCで識別した人の記録 | 置き場と鍵を管理する権限を持つ内部者 |

`doctor`と`status`は今の配置の段階を出し、外のanchorがoffなら事後の検知もできないことを出す。
Postgresのstoreの配置は段0では監査を書かず、そのことを同じく出す。
外への出口は今動いているqueueのbroker（[Queue broker](queue-broker.md)）で、containerのrunだけがその許可の範囲に閉じる。

## 不変条件

- eventsから導出しない。
  同じuse caseが両方を書く。
- 状態の変更の監査は同じtransactionで書き、書けなければ状態の変更もcommitしない。
- 一次の記録はUPDATEとDELETEをDBが拒み、訂正は打ち消しの記録を足す。
- actorは送らない。
  主体は監査サービスが認証から付ける。
- 取り消せない外への副作用の前だけattemptedの記録のackを待ち、拒否の記録は書けなくても拒否を止めない。
- secretの値は残さない。
  redactionは送り手が行い、資格情報の名前の判定は渡すenvの判定と同じものを使う。
- 監査は設定では切れず、範囲や保持を変えた事実がchainに残る。
- 読む権限はuserとinboxだけで、inboxに見せるAIの書いた文字列はデータとして囲む。

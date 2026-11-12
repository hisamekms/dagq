---
id: plan-audit-log
type: plan
title: 監査ログ 段0の設計資料（1件の形の欄・action enum v1・送り手・守るpathの最初の一覧・周期と閾値・二次の置き場の候補）
status: active
created: 2026-11-12
owners:
  - hisamekms
tags:
  - architecture
  - security
  - audit
related:
  - adr-t4840-1
  - adr-t4840-2
  - adr-t4840-3
  - design-audit
---

# 監査ログ 段0の設計資料

[ADR-t4840-1](../adr/2026-11-12-t4840-1-audit-purpose-threat-model-guarantee-levels-and-record-categories.md)・[ADR-t4840-2](../adr/2026-11-12-t4840-2-one-audit-service-hash-chained-append-only-records-and-pluggable-stores.md)・[ADR-t4840-3](../adr/2026-11-12-t4840-3-protected-paths-and-weakening-operations-need-a-direct-human-answer.md)の決定を実装する前の具体の仕様。
地図は[監査](../design/audit.md)が持つ。
実装のtaskは、それぞれの節を定義のそばのdoc comment（記録の型・actionのenum・送り手の型・守るpathの設定のkeyと組み込みの規則・周期と閾値の定数）に移し、この資料の該当の節をコードへの入口の1行に置き換える。
全ての節が入口の1行になったら、この資料を`completed`にする。

## 1件の形

| 欄 | 意味 |
| --- | --- |
| `schema_version` | recordの形の版。actionの版とは別 |
| `seq` | 監査サービスが付ける取り込みの順の連番。chainの順 |
| `prev_hash` | 一つ前の記録の`hash`。最初の記録は版ごとの固定の値 |
| `hash` | この欄を除いた記録のJCS（RFC 8785）の正規化のSHA-256 |
| `chain_version` | 正規化と算法の組の版（最初は`v1` = JCS＋SHA-256） |
| `sender_id` | 送り手の識別（部品の名前とprocessの実体）。監査サービスが認証から付ける |
| `sender_seq` | 送り手ごとの連番。`(sender_id, sender_seq)`が冪等の鍵で、欠けと切り詰めを見つける |
| `at` | 事実が起きた時刻（UTC） |
| `received_at` | 監査サービスが受けた時刻（UTC） |
| `clock_source` | 時刻の出どころ（段0はhostの時計） |
| `principal` | 主体。段0は今のactorの役割とid、段2から仮名のID |
| `on_behalf_of` | 代行の連鎖（inboxが人の代わりに、など）。配列 |
| `auth` | 認証の手段（socketのpeer・token・OIDC） |
| `action` | action enumの値と版（下の「action enum v1」） |
| `resource` | 対象（task・run・ask・commit・ref・設定のpath） |
| `authority` | 許した・拒んだ根拠（capability・policyの規則・policyの版） |
| `outcome` | 成功・失敗・拒否と理由 |
| `change` | before/afterか、大きければそのhash |
| `correlation` | request_id・goal・task・run_id・attempt・providerのsession id・reviewのjob id・causation（因果の元の`seq`）・W3C traceparent |
| `attestation` | 送り手の経路から監査サービスが導く確かさ（enforced・observed・verified）と、その証拠。self_reportedはchainに入れずtelemetryの別の流れにする |
| `answered_via` | Dの経路。`human_attested`か`relayed_by_inbox`。`AnswerAuthority`から導く |
| `approved_blobs` / `diff_hash` / `tree_hash` | Fの判断と照合で、照合に使う差分が触る各fileのpathと変更後のblob hash（消したfileは削除の印）の組と、記録と表示にだけ使うdiff hash（ADR-t4840-2決定7の正規化とSHA-256）とtree hash |
| `destination` / `direction` / `credential_ref` / `content_digest` / `via` / `policy_rule` | 外部アクセス（J）の記録だけ。段0のbrokerは書かない |

照合を実装するtaskは、同じfileの中の同じ文脈の箇所を入れ替える手直し（diff hashが同じになるもの）が再承認になるtestを足す（ADR-t4840-2決定7）。

人が直接答えた（`human_attested`）記録の`attestation`には、経路の証拠（制御端末の名前・呼び出しの親のprocessの連鎖・socketのpeer・答えるまでの時間）を持つ（ADR-t4840-3決定6）。
中継の答えは人の原文のhashを持つ。

## action enum v1

OCSFのclassはexportを作る段2で確かめる候補で、段0の実装はこの対応を表として持つだけにする。

| action | 分類 | 送り手 | OCSFのclass |
| --- | --- | --- | --- |
| `landing.judged` | F | supervisor（reviewのpass・AIのland・上限の後の収束の判断。blob hashの組・diff hash・tree hash） | API Activity (6003) |
| `landing.approved` | D・F | queue service（approve_landingのlandの答えと直接の`dagq integrate`の承認。blob hashの組と経路） | API Activity (6003) |
| `landing.compared` | F | integrate（merge後の照合。組の一致・不一致と違ったfile） | API Activity (6003) |
| `landing.merged` | F | integrate（mainのbefore/afterのsha・tree hash・diff hash・blob hashの組・rebaseと衝突の自動解消の有無） | API Activity (6003) |
| `push.attempted` | F・J1 | integrate（remoteとrefのold sha。ackを待ってからpush） | API Activity (6003) |
| `push.completed` | F・J1 | integrate（completedかfailed、remote refのold/newのsha） | API Activity (6003) |
| `ask.answered` | D | queue service（askの本文のhash・選択肢・推奨と確信度・答え・経路） | API Activity (6003) |
| `answer.applied` | D | queue service（答えが適用されたか、されなかった理由） | API Activity (6003) |
| `authz.denied` | B | queue service（拒んだcapability・規則・policyの版。fail-open） | API Activity (6003) |
| `authz.privileged` | B | queue service（弱める操作と特権の操作の許可: guardrailの無効化・plan reviewのbypass・force・手での復旧） | API Activity (6003) |
| `config.changed` | G | integrate（着地した差分が`dagq.toml`・policy・`.dagq/agents`・守るpathの定義を変えたこと） | Device Config State Change (5019) |
| `binary.installed` | G | updater（`install`と自動更新の前後の版とdigest、間の守るpathの変更の分類） | Application Lifecycle (6002) |
| `schema.migrated` | G | queue service（migrateの前後のschemaの版） | Application Lifecycle (6002) |
| `service.lifecycle` | G | supervisor・queue service（up・down・execの引き継ぎ） | Application Lifecycle (6002) |
| `audit.checked` | I | 突き合わせのjob（verifyの結果、mainの全commitにFの記録があるか、pushの記録とremoteのrefが合うか、書けなかった拒否の件数） | API Activity (6003) |
| `audit.policy_changed` | I | queue service（監査の範囲や保持の変更。人の直接の答えだけ） | API Activity (6003) |
| `audit.corrected` | I | queue service（打ち消しの記録。元の`seq`を指す） | API Activity (6003) |

## 送り手

| 送り手 | 段0での位置 | 書くaction |
| --- | --- | --- |
| queue service | 状態を変えるuse caseの同じtransaction | `landing.approved`・`ask.answered`・`answer.applied`・`authz.*`・`schema.migrated`・`audit.policy_changed`・`audit.corrected`・`service.lifecycle` |
| supervisor | queue serviceへの呼び出しの中 | `landing.judged`・`service.lifecycle` |
| integrate（Integrator） | queue serviceへの呼び出しの中。pushの前はackを待つ | `landing.compared`・`landing.merged`・`push.*`・`config.changed` |
| updater（`install`と自動更新） | queue serviceへの呼び出しの中 | `binary.installed` |
| 突き合わせのjob | 制御側のjob（AI actorではない決定的なコード）で、queue serviceの呼び出しの中 | `audit.checked` |

brokerとexecutorのtelemetryは段1から送り手になる。
AI actor（worker・review・planner・inbox）は送り手にならない。

## 守るpathの最初の一覧

この repository の`dagq.toml`に置く最初の一覧で、設定のkeyの名前は実装のtaskが決める。

| 分類 | path |
| --- | --- |
| P1 監査 | 監査サービス・記録の型・chainとverify・照合を足すmodule（実装のtaskが足したpathを同じ変更で一覧に足す） |
| P2 認可のpolicy | `src/domain/authorization.rs`、`src/domain/landing_approval.rs`、`src/application/integrate.rs`の着地の拒否の判定（実装のtaskがその判定と照合を別のmoduleに切り出し、そのmoduleだけを挙げる。integrateの全体を挙げると変わる回数が多く承認が形だけになる）、`.dagq/agents/security/`（securityのreviewのagentの定義） |
| P3 queue brokerの許可の範囲 | `src/domain/queue_broker.rs`（宛先の分類の`Destination`・`RegistryHost`・`judge_tunnel`）、`src/application/queue_broker.rs`（tunnelを開く許可の読み） |
| P4 updaterの検証 | `src/application/update.rs`、`src/application/install.rs`、`src/application/release_update.rs`、`src/domain/release_update.rs` |
| P5 守るpathの定義 | `dagq.toml`の守るpathの定義のkeyと、組み込みの規則を持つmodule |

組み込みの規則（ADR-t4840-3決定2）が、設定の一覧に依らず守るpathの変更に数えるもの: `dagq.toml`の守るpathの定義のkeyと、brokerの許可の範囲を広げるkey（registryの宛先）の変更。
名前の変更と削除は元のpathで照らす。

## 弱める操作の最初の一覧

- guardrailの無効化
- plan reviewのbypass（review無しの`ready`）
- force（拒否や関門を越えるflagの付いた状態の変更）
- 監査の範囲や保持の変更
- 守るpathの着地の承認

どのコマンドとflagが当たるかは、実装のtaskがpolicyの表に写し、[Authorization](../design/authorization.md)とpluginの権限の記述を同じ変更で直す。

## 周期・閾値・二次の置き場（段0）

| 項目 | 候補の値 | 理由 |
| --- | --- | --- |
| 定期の検証（chainのverifyと突き合わせのjob） | 1時間ごと | 着地は1時間に数件で、見張りの毎時の見直しと同じ間隔で異常を出せる |
| checkpoint | 1,000件ごとか1時間ごとの早い方 | verifyを最後のcheckpointから始められる大きさ |
| 二次の複製の遅れの閾値 | 10分か1,000件の早い方 | 複製は秒で終わるので、10分の遅れは止まっていると見てよい |
| 拒否の書けなかった件数のattention | 1件でも | 段0の拒否は少なく、書けないこと自体が異常 |

二次の置き場の候補: queueのdirの下の監査のdirのJSONLのファイル1つ（一次のDBと別のファイル）。
queueのDBと同じ場所なので守りは増えず、段0では一次が書き換えられたときの比べる相手と、段1で別のOSのユーザーへ移すときの形の確かめに使う。
外のanchorを有効にしたときの書き先（利用者が選ぶgitのremoteのrefか、利用者が選ぶファイル）も同じ節で実装のtaskが決める。

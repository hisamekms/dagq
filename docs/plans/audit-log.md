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

欄と意味は`src/domain/audit/record.rs`の`AuditRecord`・`SenderInput`・`Admission`のdoc commentが持つ。
照合を実装するtaskは、同じfileの中の同じ文脈の箇所を入れ替える手直し（diff hashが同じになるもの）が再承認になるtestを足す（ADR-t4840-2決定7）。

## action enum v1

actionの意味・分類・OCSFのclassは`src/domain/audit/mod.rs`の`AuditAction`のdoc commentが持つ。

## 送り手

送り手と書くactionは`src/domain/audit/mod.rs`の`Sender`と`AuditAction::senders`が持つ。

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

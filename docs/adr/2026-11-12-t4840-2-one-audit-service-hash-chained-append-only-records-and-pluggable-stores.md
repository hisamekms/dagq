---
id: adr-t4840-2
type: adr
title: 監査サービスを1つにし送り手は制御側の部品だけ、段0はqueue serviceの中のtraitとして状態の変更と同じtransactionで書き、連番・時刻・JCSとSHA-256のhash chain・checkpointは監査サービスが付け、一次はSQLiteとPostgresの2実装・二次は追記だけのJSONLに非同期に複製し、着地の照合は差分が触る各fileの変更後のblob hashの組で比べ、閲覧はaudit.readのuserとinboxだけ
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
  - queue-service
related:
  - adr-t4840-1
  - adr-t4840-3
  - adr-t3985-1
  - adr-t3981-1
  - adr-t728-2
  - adr-t728-3
  - adr-t3102-1
  - design-audit
  - design-queue-service
  - plan-audit-log
---

# ADR-t4840-2: 監査サービスと記録の形（送り手・置き場・hash chain・照合のhash・書けないとき・閲覧）

## Context

[ADR-t4840-1](2026-11-12-t4840-1-audit-purpose-threat-model-guarantee-levels-and-record-categories.md)は監査の目的と範囲を決めた。
人は2026-11-12に、監査サービスを1つ作って全ての送り手がそこへ送り、置き場はその中でプラガブルにすると決め、段0は監査サービスをtraitとしてqueue serviceに同居させる（人の決定1、段階案A）と決めた。
素案は正規化の形式・hashの算法・着地の照合で比べるhash・段0のprincipal・Postgresのstoreの配置の段0の扱いを選択に開いていた。
この決定はそれらを閉じ、着地の照合の形は人が2026-11-12に決めた（差分が触るfileの変更後のblob hashの組に結び、正規化したdiff hashは照合に使わない）。
欄の一覧・actionの列挙・OCSFの対応・数値は[監査ログ 段0の設計資料](../plans/audit-log.md)が持つ。

## Decision

1. **監査サービスは1つで、送り手は制御側の部品だけ。**
   送り手はqueue service・supervisor・integrate・updater・制御側の突き合わせのjob（段1からbrokerとexecutorのtelemetry）で、AI actorは監査サービスに直接送らない。
   送り手はactorを申告せず、記録の主体は送り手が受けた認証の結果（principal）から監査サービスが付ける。
   段0では、監査サービスはqueue serviceの中のtraitとして動き、状態を変えるuse caseが状態の変更と同じtransactionで記録を書く。
   段1で別のプロセス・別のOSのユーザー・別のDBに分け、outboxとプロセスをまたぐ冪等の送信をそこで入れる。
   チームの版は最初から別のserviceにする。
2. **連番・時刻・chain・checkpointは監査サービスが付け、置き場に依らない。**
   監査サービスは取り込みの順の連番（seq）・受けた時刻・前の記録のhash・checkpointを付ける。
   chainの順は取り込みの順で、因果は記録のcausationの欄で表す。
   冪等の鍵は送り手の識別と送り手ごとの連番の組 (sender_id, sender_seq) で、送り手ごとの連番の欠けと切り詰めを検出する。
3. **正規化はJCS、hashはSHA-256で、どちらも版を持つ。**
   hashの入力は、hashの欄を除いた記録をJSON Canonicalization Scheme（RFC 8785）で正規化したbyte列で、前の記録のhashを欄として含む。
   算法はSHA-256で、記録は正規化と算法の組の版（最初はv1）を持ち、版を上げるときは新しい版の最初の記録が古い版の最後のhashを前のhashとして持つ。
   recordのschemaの版は、actionの版と別に持つ。
4. **一次の置き場と二次の置き場。**
   一次（検索できる正本）はSQLiteとPostgresの2実装に固定し、段0はSQLiteだけを作る。
   一次の記録表は、UPDATEとDELETEをDBが拒む（SQLiteはtrigger）。
   訂正は元の記録を変えず、打ち消しの記録を足す。
   二次は追記だけの置き場で、最初はJSONLのファイル1つにし、一次のseqを読むカーソルで非同期に複製する。
   複製の遅れはfail-openで、閾値を超えたらinboxのattentionに出す。
5. **Postgresのstoreの配置の段0。**
   段0の監査はSQLiteの一次だけを持つので、Postgresのstoreの配置では監査を書かない。
   その配置では、pushの前のackを待たず（待つ相手が無い）、`doctor`と`status`は監査を記録していないことを保証の段階として出す。
   Postgresの一次は段2で作る。
6. **書けないときの扱い。**
   状態の変更の監査は同じtransactionで書くので、書けなければ状態の変更もcommitせず、監査は欠けない。
   取り消せない外への副作用（push・外部APIへの書き込み）の前だけ、attemptedの記録のackを待ってから実行する（fail-closed）。
   拒否の記録はfail-openにし、書けなければ件数をIとして後で記録する（拒否を大量に起こすDoSで状態の変更を止めない）。
7. **着地の照合は、差分が触る各fileの変更後のblob hashの組で比べる。**
   着地を許す判断は、その時のrunの差分が触る各file（pathごと）の変更後のblob hash（消したfileは削除の印）の組に結ぶ。
   integrateはmergeした後の結果について、最終的に着地する差分が触るfileの組とその各fileのblob hash（または削除の印）を承認した組と比べ、一つでも違えば着地せず再承認にする。
   差分の外のfileだけが変わるrebaseでは組は変わらず、承認を持ち越す。
   差分が触るfileをmainも変えていれば、衝突が無くてもmerge後のblob hashが変わるので再承認になる。
   衝突の自動解消と手直しは、触るfileのblob hashを変えるので別の変更として記録され、承認のやり直しになる。
   同じ文脈の箇所を入れ替える手直し（同じfileの中の同じ形の処理の、消す側を取り替えるなど）も、正規化したdiff hashは同じでも着地するfileの中身が違えばblob hashが変わるので再承認になり（中身が同じなら同じコードが着地する）、照合を実装するtaskはこれを確かめるtestを足す。
   行番号とindexの行を除いて正規化したdiff hash（`git patch-id --verbatim`と同じ正規化をSHA-256で）は、記録と表示にだけ使い、照合に使わない。
   tree hashは承認の時とmergeの時の両方を記録し、比べない。
8. **照合の相手は着地を許す全ての経路の判断。**
   照合の相手は、reviewのpass・AIのconcernのhighのland・上限の後の収束・人のapprove_landingのland・人やinboxが直接打つ`dagq integrate`の承認の、どれか1つの判断の記録である。
   supervisorが判断の後に起こすintegrateは新しい判断ではなく、その判断の記録を照合する。
   判断の後に内容が変わったときの行き先は経路で決まる: reviewの判断はreviewのやり直し、approve_landingはapprove_landingのaskの開き直し、直接のintegrateは打ち直し。
9. **外のanchorとcheckpointの署名。**
   外のanchor（chainの先頭のhashをagentの届かない外へ書く）は既定でoffで、利用者のremoteに勝手にpushしない。
   offのとき、`doctor`は「同じUIDの攻撃者がchain全体を書き直すと事後の検知もできない」と出す。
   段0のcheckpointは署名しない。
   checkpointの署名と鍵の置き場は段1が決める。
10. **principalとPII。**
    段0のprincipalは今のactorの役割とid（eventのactorと同じもの）で、[ADR-t3985-1](2026-11-03-t3985-1-team-version-identity-by-oidc-human-roles-on-staticpolicy-runner-registration-per-person-inbox-and-no-delegated-landing-approval.md)決定8の認証した呼び出しの主体を、チームの版のprincipalの元にする。
    PIIのdigestは鍵付きHMACにし、素のhashを使わない。
    仮名のprincipalとHMACの対応表は、個人の識別を持つチームの版（段2）から持つ。
11. **保持。**
    段0は監査の記録を消さない。
    分類ごとの年限・期間ごとのsegmentとcheckpoint・legal holdは段2が持つ。
12. **閲覧と読む権限。**
    閲覧は`dagq audit`のlist・trace・verify・exportで、trace（commit・task・run・requestから両方向に辿る）を中心にする。
    読む権限`audit.read`はuserとinboxだけに与え、worker・job・plannerには与えない。
    inboxに見せるときは、AIが書いた文字列（taskの説明・reviewのcomment・askの本文）をデータとして囲み、指示として読ませない。
13. **漏れの防止。**
    段0は、use caseとactionの対応表を全数で検査するtestで、監査を書かない特権のuse caseを見つける。
    特権のuse caseが監査のハンドルなしに書けない型（Audited\<T\>）と、clippyの`disallowed_methods`（gitの直接の呼び出しなど）は段1で入れる。

## Alternatives

- **送り手ごとに置き場へ直接書く**: chainと連番が置き場ごとに分かれ、置き場を替えると検証の形が変わる。監査サービスが付ける（決定2）。
- **段0から別プロセスにする**: outboxと冪等の送信が要り、host advisoryでは同じUIDが届くので守りは増えない。人の決定1のとおり同居させる（決定1）。
- **決定的CBOR**: byteは小さいが、二次のJSONLとCLIの出力がJSONで、正規化を2つ持つことになる。JCSにする（決定3）。
- **tree hashで照合する**: mainが動くたびに全ての承認がやり直しになる。差分が触るfileのblob hashの組で比べ、tree hashは記録だけにする（決定7）。
- **行番号を除いて正規化したdiff hashで照合する**: 同じfileの中に同じ文脈の処理が複数あると、変える箇所を別の箇所へ移した手直しも同じhashになり、未承認の変更へ承認を持ち越せる。記録と表示にだけ使う（決定7、人の判断）。
- **拒否の記録もfail-closed**: 拒否を大量に起こすだけで状態の変更を止められる（決定6）。
- **外のanchorを既定でon**: 利用者のremoteに無断で書く。既定でoffにし、offの限界を出す（決定9）。

## Consequences

- 段0のqueue serviceの書き込みは、監査の行の分だけtransactionが大きくなる。
- Postgresのstoreの配置は段2まで監査を持たず、それを`doctor`と`status`が明示する。
- 着地の照合が経路ごとでなく直前の1か所になるので、新しい着地の経路も照合の相手として判断を記録しないと着地できない。
- [ADR-t3981-1](2026-11-02-t3981-1-team-version-control-side-on-containers-postgres-and-object-store-with-app-side-write-serialization.md)決定7の「承認を持ち越すのは差分の中身が同じときだけ」は、触るfileの変更後のblob hashの組が同じときだけ持ち越す、より狭い形になる。

---
id: adr-t4840-3
type: adr
title: 監査・認可のpolicy・queue brokerの許可の範囲・updaterの検証のコードと守るpathの定義を守るpathにし、それを変えるrunは着地の直前の照合で変更後のblob hashの組に結んだ人の直接の承認（human_attested）なしに着地せず、弱める操作はinboxの中継を拒み、段0の人の直接の経路はDAGQ_ROLEの無いterminalのTTYでの確認にする（ADR-t728-3決定1・3とADR-t3985-1決定7のローカルの文をamends）
status: accepted
created: 2026-11-12
updated: 2026-11-12
accepted_on: 2026-11-12
amends:
  - adr-t728-3 decision 1
  - adr-t728-3 decision 3
  - adr-t3985-1 decision 7
owners:
  - hisamekms
tags:
  - architecture
  - security
  - audit
  - authorization
  - inbox
related:
  - adr-t4840-1
  - adr-t4840-2
  - adr-t728-1
  - adr-t728-3
  - adr-t3985-1
  - adr-t3981-1
  - adr-t2114-1
  - adr-t2257-1
  - design-audit
  - design-security
  - design-authorization
  - plan-audit-log
---

# ADR-t4840-3: 守るpathと人が直接答える経路

## Context

[ADR-t728-3](2026-09-27-t728-3-answer-and-delegated-authority-of-the-inbox.md)は、inboxが全てのaskにanswerでき`dagq-recover`の手作業も代行できる（決定1）とし、人しか出せない承認（I6）の強制を承認の経路が決まるまで先に送った（決定3）。
[ADR-t3985-1](2026-11-03-t3985-1-team-version-identity-by-oidc-human-roles-on-staticpolicy-runner-registration-per-person-inbox-and-no-delegated-landing-approval.md)決定7は、チームの版でapprove_landingの承認と認証の操作のinboxの代行を拒み、ローカルの既定の配置ではADR-t728-3決定1・3がそのまま効くとした。
監査（[ADR-t4840-1](2026-11-12-t4840-1-audit-purpose-threat-model-guarantee-levels-and-record-categories.md)）は、守る相手に人の承認を誤って中継する・捏造するinbox（c）を挙げ、監査と認可を弱めるコードの変更がAIの判断だけで着地する経路を残すと、記録そのものを信じられない。

人は2026-11-12に、守るpathの変更を着地の前に人の承認に回す（人の決定2、守るpath A）こと、弱める操作と守るpathの着地の承認はinboxの中継を拒み人が直接答える（人の決定3、人が直接答える経路A）ことを決めた。
この決定はそれを決定にし、素案が開いた守るpathの分類・設定が許可の範囲を広げる経路の扱い・段0の人の直接の経路の形を閉じる。
具体のpathの一覧は[監査ログ 段0の設計資料](../plans/audit-log.md)が持つ。

## Decision

1. **守るpathの分類。**
   守るpathは次の5つの分類のコードと設定である。
   (P1) 監査のコード（監査サービス・記録の形・chainとverify・照合）。
   (P2) 認可のpolicy（capabilityとactorの表・resourceの規則・着地の拒否の判定・answerの権限）。
   (P3) 今動いているqueueのbroker（[ADR-t2114-1](2026-10-08-t2114-1-queue-broker-is-a-host-process-with-three-destinations.md)）の許可の範囲を定義するコード（宛先の分類・registryの宛先・tunnelの判定）。
   (P4) updaterの検証のコード（自動更新と`install`の検査・入れ替えの関門）。
   (P5) 守るpathの定義そのもの。
   定義を変える変更自体もP5として守るpathの変更に数え、名前の変更と削除は元のpathで照らす。
2. **定義の置き場と設定の経路。**
   守るpathの一覧はpjの設定（`dagq.toml`）が持ち、dagqのruntimeにpjのpathを固定しない（[ADR-t2257-1](2026-10-12-t2257-1-container-runtime-takes-project-and-host-values-from-config.md)と同じ汎用性）。
   設定の中の守るpathの定義と、brokerの許可の範囲を広げる設定（registryの宛先など、brokerが読む`dagq.toml`のkey）の変更は、設定の一覧に依らず組み込みの規則で常に守るpathの変更に数える。
   設定の一覧から外すことで組み込みの規則は切れない。
3. **守るpathを変えるrunは人の直接の承認なしに着地しない。**
   関門は経路ごとではなく、着地の直前の照合（[ADR-t4840-2](2026-11-12-t4840-2-one-audit-service-hash-chained-append-only-records-and-pluggable-stores.md)決定7・8）に置く。
   最終的に着地する差分が守るpathに当たるrunは、どの経路（reviewのpass・AIのland・上限の後の収束・人のapprove_landingのland・直接の`dagq integrate`）からも、その差分が触るfileの変更後のblob hashの組に結んだhuman_attestedの承認が無ければ着地しない。
   pass・AIのland・収束の経路では、supervisorがapprove_landingのaskを開いて人に回す。
   inboxが打つ直接の`dagq integrate`は守るpathのrunでは拒む。
   自動更新と`install`で入れ替える版の間に守るpathの変更があれば、updaterはその分類を監査に記録する。
4. **弱める操作はinboxの中継を拒む。**
   弱める操作は、guardrailの無効化・plan reviewのbypass・force・監査の範囲や保持の変更・守るpathの着地の承認である。
   これらは人が直接答えた（human_attested）ものだけを受け、inboxが中継した答えと代行の操作はpolicyが拒む。
   普通の着地の承認とそれ以外のaskのanswer・手作業の代行は、今どおりinboxが中継でき、記録の経路はrelayed_by_inboxになる。
5. **段0の人の直接の経路はTTYでの確認。**
   hostの配置の段0では、`DAGQ_ROLE`の無いterminalから打つdagqのコマンドが、制御端末（TTY）で承認する対象の組のhashの短い接頭辞かaskの識別を人に打ち返させたものを、human_attestedとして受ける。
   制御端末の無い呼び出し、`DAGQ_ROLE`のある呼び出し（inboxのterminalの`!`を含む）は受けない。
   macOSのTouch IDは段0では使わない（Linuxのhostで同じ経路にならず、署名した配布物が要る）。
   チームの版はOIDCのstep-up（ADR-t3985-1と同じ発行元の、最近の再認証）にする。
6. **hostのadvisoryの限界と事後の材料。**
   hostの配置では、同じUIDのagentは擬似端末を開いて確認を打ち、`DAGQ_ROLE`を外せるので、human_attestedを偽れる。
   これを事後に見つける材料として、監査はhuman_attestedの記録に経路の証拠（制御端末の名前・呼び出しのプロセスの親の連鎖・socketのpeer・答えるまでの時間）を残し、突き合わせのjobはruntimeが起動したagentのsessionの下からの答えを異常としてattentionに出す。
7. **amendsの範囲と保つもの。**
   ローカルの既定の配置でも、inboxは決定4の弱める操作と守るpathの着地の承認を代行できない。
   これはADR-t728-3決定1（inboxの権限は今のまま）と決定3（I6を強制しない）を、この範囲で狭め、I6をこの範囲で強制する。
   ADR-t3985-1決定7の「ローカルの既定の配置ではADR-t728-3決定1・3はそのまま効き、inboxは今どおり全てのaskにanswerできる」の文も、同じ範囲で狭める。
   ADR-t728-3決定2（記録で人自身と代行を区別する。区別はactorの型から決める）と決定4（answerはinboxとuserのもの）は保ち、決定2の`AnswerAuthority`の区別をhuman_attestedとrelayed_by_inboxの元にする。
   ADR-t3985-1決定7のチームの版のapprove_landingの承認と認証の操作のinboxの代行の拒否は保つ。

## Alternatives

- **守るpathをruntimeに固定する**: dagqを使う外のpjには監査のコードが無く、pjごとの守るpathを持てない。設定に置き、定義の変更を組み込みで守る（決定2）。
- **`dagq.toml`全体を守るpathにする**: 変わる回数が多く、承認が形だけになる。brokerの許可の範囲と守るpathの定義のkeyに絞る（決定2）。
- **経路ごとに関門を置く**: 新しい経路を足すたびに関門を足し忘れる。着地の直前の照合の1か所にする（決定3）。
- **全ての着地の承認を人の直接の答えにする**: 人は普通の着地を中継のままにすると決めた（決定4）。
- **段0でTouch IDを使う**: macOSだけで、Linuxのhostで同じ経路にならず、配布物の署名が要り、指の確認は何を承認するか（組のhash）を人に打ち返させない。TTYにし、限界を書いて事後の材料を残す（決定5・6）。

## Consequences

- 人はinboxのterminalではなく`DAGQ_ROLE`の無い別のterminalで、守るpathの承認と弱める操作に答える。ADR-t728-3決定3が避けた運用の重さを、この範囲だけ受け入れる。
- 今の姿を書く[Authorization](../design/authorization.md)・[Security](../design/security.md)とpluginの権限の記述は、実装のtaskが同じ変更で直す（決定を先にacceptedにする）。
- hostの配置のhuman_attestedは偽れるので、防止ではなく事後の検出の材料で、防止は段1の別のOSのユーザーとチームの版のOIDCから得る。

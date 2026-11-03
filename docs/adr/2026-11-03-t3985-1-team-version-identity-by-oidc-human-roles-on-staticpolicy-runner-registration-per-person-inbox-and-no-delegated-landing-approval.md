---
id: adr-t3985-1
type: adr
title: チームの版の識別と役割は、pjが選ぶOIDCの発行元のJWTをdagqが検証してdagqの短いtokenに替え、CLIはdevice codeでloginし、人の役割をuserのactorの上の狭めとしてStaticPolicyに写し、runnerは人が発行した1回きりの登録でdagqの資格を得て能力の申告を割り当ての材料にだけ使い、inboxは人ごとにその人のマシンで動いてapprove_landingの承認を代行できない（ADR-t728-3決定1・3をamends、チームの版の配置だけ）
status: accepted
created: 2026-11-03
updated: 2026-11-03
accepted_on: 2026-11-03
amends:
  - adr-t728-3 decision 1
  - adr-t728-3 decision 3
owners:
  - hisamekms
tags:
  - architecture
  - security
  - authorization
  - queue-service
related:
  - adr-t3981-1
  - adr-t3102-1
  - adr-t1233-4
  - adr-t728-1
  - adr-t728-3
  - adr-t1975-1
  - design-authorization
  - design-security
  - plan-team-version-identity
---

# ADR-t3985-1: チームの版の認証と役割（OIDCとlogin・人の役割の写し・runnerの登録・チームのinboxと代行・脅威の模型）

## Context

[ADR-t3981-1](2026-11-02-t3981-1-team-version-control-side-on-containers-postgres-and-object-store-with-app-side-write-serialization.md)は、チームの版の制御側をcontainerとPostgreSQLとS3互換の置き場に置き、人の識別をOIDCのJWTでdagqが検証し（決定2）、runnerが外向きにpullし（決定5）、approve_landingの承認をOIDCで識別した人だけに許してinboxの代行を弾き（決定7）、high以上の優先度を管理者の役に限る（決定8）と決め、OIDCの設定の形・人のprincipalとtoken・役の表し方・runnerの登録と認証・チームのinboxを後のADRに残した。この決定はそれを決める。ADR-t3981-1の決定は前提で、変えない。

今の識別は1台のhostの中で閉じている。人（user）はhostのsocketのpeerの資格で表し（[ADR-t3102-1](2026-10-22-t3102-1-stage-5-principals-user-by-socket-peer-and-control-side-tokens.md)決定1）、AI actorと制御側は親のprincipalの依頼でserviceが発行するtokenで表す（同決定2、[ADR-t1233-4](2026-10-02-t1233-4-queue-service-lifecycle-outage-notice-and-principal-tokens.md)決定4）。hostでは同じOSのユーザーが全てを読めるので、どれも助言的である（ADR-t3102-1決定5）。inboxは全てのaskにanswerでき、人しか出せない承認（I6）は承認の経路を決めるまで強制しない（[ADR-t728-3](2026-09-27-t728-3-answer-and-delegated-authority-of-the-inbox.md)決定1・3）。チームの版では人とrunnerがネットワークの向こうにいて、socketのpeerでは識別できない。

具体の候補（検証する欄・tokenの置き場と期限の値・CLIのコマンド・設定の形・記録の形）は設計資料の[チームの版の識別の具体案](../plans/team-version-identity.md)が持ち、実装のtaskが定義のそばのdoc commentへ移す。

## Decision

ADR-t3102-1（userはhostのsocketのpeer、制御側・inbox・plannerのtokenは親の依頼でserviceが発行、hostは助言的）、ADR-t1233-4（実行側のprincipalは制御側が発行するtoken、値はenvに置かない）、ADR-t3981-1（チームの版の構成）の決定は保つ。この決定はその上に、遠くの人とrunnerの識別を足す。ADR-t3102-1決定1・3の「userにtokenを発行せず、serviceはuserのtokenを受けない」はhostのsocketの入口の規則で、チームの版でもhostのsocketの入口はuserのtokenを受けない。決定1の人のdagqのtokenは、ADR-t3102-1決定2の親の依頼による発行ではなく、チームの版の外向きの入口だけが受けるOIDCの検証の結果で、hostのsocketの入口では受けない。ローカルの既定の配置（OIDCの発行元を設定しない配置）の識別と権限は何も変えない。

1. **OIDCの発行元はpjが選び、dagqはJWTを検証してdagqの短いtokenに替える。** 発行元は利用するpjが選ぶOIDCに従うIdP（組織のIdPのほか、GitHub ActionsのOIDCはGitHub Actionsで動くrunnerの識別に使える）で、配置ごとの設定に置く。dagqは受けたJWTを、発行元の公開の鍵（discoveryから取り、未知の鍵の識別子で取り直し、取れなければ拒む）の署名、許した非対称の署名の方式だけ、issuerの完全一致、dagqのaudienceを含むこと、有効期間で検証する。人の同一性はissuerとsubjectの組で決め、mailや表示名では決めない。検証に通ったらserviceが、登録された人であることと役割を同じtransactionで確かめて、その人のprincipalに結びついたdagqの短いtokenを発行し、以降の呼び出しはそのtokenで受ける。発行元のJWTを毎回受ける形にしないのは、dagqの側で人の失効と役割の変更をすぐ効かせるためである。
2. **CLIのloginはdevice codeを既定にし、tokenはその人の資格の置き場に置く。** CLIはdevice authorization（別のブラウザで承認する）を既定にし、IdPが持たなければloopbackのauthorization code＋PKCEを使う。tokenはOSの資格の置き場（無ければその人だけが読めるファイル）に置き、env・argv・worktree・run dirに置かない（ADR-t1233-4決定4と同じ規則）。dagqのtokenは短く、切れたらIdPの更新で取り直す。承認（決定7）は、最近のloginで発行されたtokenに限る。
3. **人の役割は管理者・member・当番で、StaticPolicyのactorは全てuserのまま、役割をuserの許可を狭めるものとして写す。** 人の役割はdagqが持つ登録（issuerとsubjectの組から役割へ）が決め、IdPのgroupのclaimは登録の材料にしてよいが、権限はdagqの登録が決める。当番はmemberか管理者に重ねる持ち回りの印で、別の役割ではない。どの役割もStaticPolicyのactorは`user`（信頼の区分はHuman）のままにし、役割はprincipalの属性としてStaticPolicyの許可とresourceの規則が狭める。ローカルの既定には役割が無く、userは今の許可のままである。写しは次の表で、capabilityの群の名前は[Authorization](../design/authorization.md)のもの。

   | 人の役割 | StaticPolicyのactor | 許すcapability（userの許可からの差） |
   | --- | --- | --- |
   | 管理者 | user | userの許可の全て。加えて、人の役割の付け替え・共有のrunnerの承認・全てのrunnerの失効と、high以上の優先度を付けること（`task.write`・`goal.write`・計画の依頼の優先度の値へのresourceの規則） |
   | member | user | userの許可から`service.install`・`queue.admin`・`review.setup`・`review.setup_approve`・`task.ready_bypass_review`・`workspace.cleanup`・人の役割の付け替え・共有のrunnerの承認を除く。`ask.answer`・`ask.close`は宛先が自分か代理のaskだけ（宛先の規則はownerとaskのADR）。優先度はnormalまで（high以上はresourceの規則で拒む）。`service.lifecycle`は自分を親とするinboxを開くことだけ。runnerの登録と失効は自分が持ち主のものだけ |
   | 当番 | user（memberか管理者に重ねる） | 重ねた役割の許可に、運用のaskの`ask.answer`・`run.recover`を足す |

   優先度の権限そのもの（誰がどの優先度を付けられるか、AIのactorとの関係）と[ADR-t1975-1](2026-10-08-t1975-1-runtime-records-origin-and-person-set-priority-and-ai-actors-cannot-change-it.md)のamendsはADR-t3989-1が持ち、この決定は管理者の役への写し方だけを決める。チームの版の制御側の起動・停止・migrateは配置する側が行い（ADR-t3981-1決定2）、人のCLIの`service.lifecycle`は制御側のcontainerに及ばない。
4. **runnerは人が発行した1回きりの登録でdagqの資格を得る。** runnerの登録は、OIDCで識別した人（memberも管理者も、自分を持ち主とするrunnerを登録できる）がserviceに依頼して得る1回きりで期限の短い登録の値から始め、runnerはそれをdagqのrunnerの資格（長く持つ更新の資格と短いtoken）に替える。GitHub Actionsで動く控えのrunnerは、登録の値の代わりにGitHub ActionsのOIDCのJWT（repositoryとworkflowの欄を配置の設定で縛る）で識別してよい。runnerのprincipalは登録した人（持ち主）に結びつき、失効は持ち主か管理者が行い、次の呼び出しから効いて、そのrunnerが持つleaseを失わせる。runnerのprincipalは割り当てられたrunのclaim・生存の報告・結果の報告だけを持ち、着地・mainの更新・answerは持たない。runの中のworkerとwrapperのtokenは、今と同じく制御側がrunごとに発行し、runnerに渡す（ADR-t1233-4決定4・ADR-t3102-1決定2）。
5. **能力の申告は割り当ての材料で、権限の根拠にしない。** runnerはOS・arch・containerの有無・使えるClaudeの資格情報の種類（チームのAPI keyをbroker経由で使えるか、持ち主の契約か）を申告する。制御側は申告を仕事の割り当てにだけ使い、偽りの申告で得るのは割り当てた仕事の失敗だけになるようにする。安全に関わる区分は申告でなく制御側の登録が決める: 他の人のtaskを受けてよい共有のrunnerかどうかは管理者が承認した登録の印で決め、共有のrunnerはcontainerを必須にし（ADR-t3981-1決定5）、持ち主の契約の資格情報は持ち主のtaskにだけ割り当てる（ADR-t3981-1決定9）。検証の結果は申告を信じず、制御側がマシンの報告を照合する（ADR-t3981-1決定6）。
6. **チームのinboxは人ごとに、その人のマシンで動く。** inboxはそれを開いた人のprincipalを親とするtokenで動き（ADR-t3102-1決定2の親の依頼の形）、その人宛てのaskと、当番のときは運用のaskを読む。inboxの権限は親の人の役割を越えない（[ADR-t728-1](2026-09-27-t728-1-trust-domains-actors-and-default-deny-capability-authorization.md)決定5）。制御側に共有のinboxは置かない。
7. **チームの版では、approve_landingの承認と認証に当たる操作をinboxが代行できない（ADR-t728-3決定1・3をamends）。** OIDCの発行元を設定したチームの版の配置では、関門のapprove_landingの着地を承認するanswerは、OIDCで識別した人のprincipal（決定1・2のtoken）が出したものだけを受け、inboxのprincipalのものはStaticPolicyが拒む（I6の強制。承認の経路は人のCLIのlogin）。同じ配置で、人の役割の付け替えとrunnerの登録・失効もinboxの代行を拒む（I6の「認証」）。inboxはそれ以外のaskのanswerと手作業の代行を、親の人の役割の範囲で今どおり行え、記録で人自身と代行を区別する（ADR-t728-3決定2のまま）。ローカルの既定の配置ではADR-t728-3決定1・3はそのまま効き、inboxは今どおり全てのaskにanswerできる（ADR-t3981-1決定3）。ADR-t728-3決定2・4は保つ。
8. **脅威の模型の区分。** チームの版の信頼の区分を次にする。(i) 制御側（containerのtask・DB・S3互換の置き場）は信頼する制御側で、mainを進めるGitHub Appの鍵・DBの接続・チームのClaudeのAPI key・OIDCの設定を持つ唯一の場所にする。(ii) 登録したrunnerと検証役のマシンは、持ち主の人に結びついた信頼しきらない実行側で、報告はデータとして制御側が照合し、mainへの書き込みの資格を持たない。(iii) runの中のAI actorは今どおり信頼しないAI actorで、資格情報を持たない（brokerが差し替える）。secretは配置する側のsecretの置き場から制御側へ起動時に渡し、DB・S3互換の置き場・`dagq.toml`・runnerに置かない。監査の不変条件として、認証した全ての呼び出しについて、呼び出した人（issuerとsubjectの組）かrunner（その登録と持ち主）を後から追跡でき、tokenの発行と失効・役割の付け替え・runnerの登録と失効・承認も、誰がいつ行ったかを書き換えられない記録で追跡できるようにする（記録の形は実装が決める）。この区分は[Security](../design/security.md)の「将来の道筋」に実装の着地と同じ変更で足す。

## Alternatives

- **(a) dagqがIdPを持つ（自前のパスワードやGitHubのOAuthだけ）**: 特定のIdPに縛られ、組織の離職や多要素認証の決まりをdagqが持つことになる。OIDCの検証だけを持つ（決定1）。
- **(a) 発行元のJWTを呼び出しごとに受ける**: 実装は小さいが、dagqの側の失効と役割の変更がJWTの期限まで効かない。dagqの短いtokenに替える（決定1）。
- **(a) loopbackのredirectだけでloginする**: 遠くのマシンやcontainerの中のCLIでブラウザを開けない。device codeを既定にする（決定2）。
- **(b) 人の役割ごとにStaticPolicyのactorを足す（admin・memberのrole）**: 信頼の区分は同じHumanなのにactorが増え、userを前提にした規則（peerの資格、answerの出どころの記録）を役割ごとに書き分けることになる。actorはuserのまま、役割を狭めとして写す（決定3）。
- **(b) IdPのgroupのclaimをそのまま権限にする**: IdPの設定の誤りがdagqの権限に直に効き、dagqの記録に残らない。登録が決める（決定3）。
- **(c) runnerの申告を信じて割り当てと権限の両方に使う**: 遠くのマシンの申告は確かめられない。割り当てにだけ使い、権限は登録が決める（決定5）。
- **(c) runnerに人のtokenを持たせる**: runnerの失効と人の失効が分けられず、人が離れるとrunnerを止められない。runnerに別のprincipalを持たせる（決定4）。
- **(d) 制御側に共有のinboxを1つ置く**: 誰の言葉を代行したかが分からず、全員の権限の和を持つAI actorになる。人ごとにする（決定6）。
- **(d) チームの版でもinboxに承認を代行させる**: 人はADR-t3981-1決定7でinboxの代行を弾くと決めた。遠くの人はOIDCで識別できるので、ADR-t728-3決定3の残した「承認の経路」はloginで足りる（決定7）。
- **(e) secretをDBに暗号化して置く**: DBのbackupと読み取りの経路が全てsecretの経路になる。配置のsecretの置き場から渡す（決定8）。

## Consequences

- チームの版では、人は承認のたびに自分のlogin済みのCLIで答える。inboxで人に見せて答えを書かせる今の運用は、関門のapprove_landingには使えない。ローカルの既定は変わらない。
- StaticPolicyは役割の狭めとresourceの規則を足すだけで、actorと信頼の区分（ADR-t728-1）は作り直さない。今の姿を書く[Authorization](../design/authorization.md)と[Security](../design/security.md)は、実装のtaskが着地と同じ変更で直す（この決定はまだ実装されていない）。
- 発行元の鍵が取れない間は、新しいloginと鍵の識別子の変わったJWTを拒む（fail closed）。発行済みのdagqのtokenは期限まで使える。
- runnerの持ち主が失効すると、そのrunnerの資格も失効させる必要がある。登録が持ち主に結びつくので、制御側が辿れる。
- この決定は、ownerとaskの宛先・繰り上げ・当番の決め方（段6のADR）、優先度の権限（ADR-t3989-1）、runnerのpullとleaseの引き継ぎ、gitの資格情報とruleset、Claudeの資格情報の予算、HTTPのAPIの形を決めていない。

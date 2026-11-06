---
id: adr-t4400-1
type: adr
title: 古い行の写しはDBを読み終えた後に消えない移した履歴で読み、移す回・最初のVACUUM・読み戻しを別々に認可し、値はserviceが読み、queueに書くreportの送り手をsupervisorとuserに限る
status: accepted
created: 2026-11-06
updated: 2026-11-06
accepted_on: 2026-11-06
amends:
  - adr-t4124-1 decision 1
  - adr-t4124-1 decision 5
  - adr-t4124-1 decision 6
  - adr-t4124-1 decision 7
owners:
  - hisamekms
tags:
  - architecture
  - persistence
  - authorization
related:
  - adr-t4124-1
  - adr-t3986-1
  - adr-t3102-2
  - adr-t1233-5
  - adr-t728-1
  - adr-t3981-1
  - plan-team-version-write-leader
---

# ADR-t4400-1: 写しを読む順・移す操作の権限・設定を読む責務・reportの送り手

## Context

[ADR-t4124-1](2026-11-05-t4124-1-local-opt-in-archive-of-old-events-and-ledger-rows-port-parquet-boundary-mover-shrink-and-read-paths.md)のreview（run 3a72e48c-58f1-4690-8b96-38778957e803、ask 1510）は、読みの途中で移す係が行を消すと答えから行が落ちる例と、操作の権限・値の出どころ・reportの送り手の問題を指摘した。
人は元のADRをそのまま着地させ、読みの順と権限・値の責務を実装の前に直すこと、queueに書くreportはsupervisorとuserに限る案(b)を採ることを決めた（request 246）。
人は同時に、元の決定7・8のチームの版の資格情報の方針（書ける資格情報は制御側のtaskだけ、AIのactorには読むだけも渡さず、読むだけは配られた人だけ）を承認した（ask 1510の指摘4）。
写しの鍵の上書き（指摘1）はtask 4315が持ち、このADRでは決めない。

読みの順と権限・値の責務の具体案はplannerの判断で、人が方式まで承認したものではない。
plannerが足した読み戻しとの競合に対し、plan reviewは前後の印の和でも往復で行が落ちると指摘した（proposal 1984）。
移した記録のeventは元の決定5にあり、queueのeventはDBに残るので、消えない履歴を持つ追加の費用は小さい。
管理の既存の権限はplannerとsupervisorも持つため、最初のVACUUMには広すぎる。
serviceにはreportの設定を束縛したmain checkoutから安全に読む守りが既にあり、受け取った値で書き先を決めずに済む。

## Decision

元の決定1・5・6・7の以下の部分だけをamendsする。
ADR-t3986-1決定8の不変条件（置いて確かめてから消す・写しをdagqが消さない・eventのIDを保つ・状態を作る読み取りが読むkindは移さない・集計は読む側）、ADR-t3102-2決定1（queue DBを開くのはqueue serviceだけ）、ADR-t1233-5決定1・2・3（出力の形、serviceの経路とpolicy、roleの読める範囲）、ADR-t728-1のdefault denyを保つ。
ローカルの既定は移さず、opt-inの有効・境目と置き場の責務の分けも保つ。

1. **読む順の不変条件（元の決定1・7）。** 写しとDBを合わせる読みは、問いに要るDBの行を読み終え、その後に消えない移した履歴を見て読む区切りを決め、写しを読む。
   履歴は移す係がDBの行を消す同じtransactionで書き、読み戻しでも消さず、置き場へ移さない。
   移す回の外で写しを置く経路も同じ条件を満たし、ADR-t3986-1決定9のimportは、直接置く区切りとローカルから引き継ぐ区切りのどちらも選べる履歴を、importの同じtransactionで記録する。
   履歴には問いに掛かる写しの区切りを選べる情報を残し、履歴の読みにはDBの行を読むために使った古いsnapshotを使わない。
   読み戻しで消す印は、合わせる読みの区切りの元にしない。
   台帳を作る係と移す係の判断に加え、開き直しの読み戻しは印の範囲から対象を選び、書き込みのtransactionでその範囲を読んだ時と比べて違えば読み直す元の規則を保つ。
   findingの根拠の存在を判断するときに移した印の範囲も在ると数える用途も保つ。
   「移した記録が1つも無いqueueは置き場を読まない」の判定もDBを読み終えた後にこの履歴で行う。
   DBで既に得た行の鍵だけを保持して写しとの重なりを除き、台帳を畳む読みではDBの行を畳み終えた後の鍵を使う。
   eventはID、台帳は種類と鍵で除き、同じ台帳の行はDBを採り、写しどうしは入力の進んだ方を採る元の規則を保つ。
   DBから消えたため読めなかった行は、消したtransactionの履歴がDBを読み終えた後の読みで必ず見え、写しは消されないので必ず読める。
   importでDBに入れず写しへ直接置いた行も、同じimportのtransactionで記録した履歴から選べるので、移す回の履歴がまだ無くても答えに入る。
   読み戻された行がDBと写しの両方にあっても、実際に得たDBの行の鍵で除くため1度だけ出る。
   このため行が答えから落ちず、合わせる読みの再読も世代の比較も要らない。
   この順はshow・events・timelineのservice内の読みと、長い窓の行をページで受けて畳むCLI・supervisorの両方に当てる。
   ページをまたいでもDBの全ページを読み終えてから履歴を読み、写しのページへ進む。
   - ask 1510の例1（写しを先に読み、その後DBを読む間に移る）: 写しを後にするため、DBで得られなかった行も履歴から読める。
   - 例2（読み始めに印なしと判定し、その後初めて移る）: 読み終えた後の履歴で判定するため、最初の移す回も見落とさない。
   - 例3（DBの鍵だけ先に受け、後から行を畳む間に移る）: 鍵を先取りせず、実際に畳んだ行の鍵だけで写しを除くので、DBで畳めなかった行を飛ばさない。
   - 往復の例（対象の印なし → 移す係が行を消して印を作る → 読み手はその範囲のDBから行を得ない → 人の読み戻しが行を戻し印を消す → 読み終えた後も印なし）: 消したtransactionの履歴は残るため写しを読める。
     この読み手はDBからその行を得ていないので、写しから除かず答えに入れる。
2. **操作ごとの権限（元の決定5・6・7）。** 移す回・最初のVACUUM・人が打つ読み戻しは、それぞれ新しい別々のcapabilityで認可し、既存のcapabilityには載せない。
   移す回を持つのはsupervisorだけで、user・inbox・plannerは持たず、人が手で移す入口を作らない。
   最初のVACUUMを持つのはuserと、人の言葉で代行するinboxだけで、planner・supervisorは持たない。
   人が打つ読み戻しを持つのはuserだけで、inbox・planner・supervisorは持たない。
   それ以外のroleもこれらの操作を持たず、default denyで断る。
   開き直しの遷移の中の読み戻しはその遷移の認可で動き、人の読み戻しの権限を別に要求しない。
   写しの行のページの読みは元の決定7の認可のままで、capabilityの綴りは実装のAuthorizationが持つ。
3. **値はserviceが読む（元の決定5）。** クライアントはopt-inの値・境目・置き場のディレクトリを送らず、supervisorは周期で引数なしで移す回を呼ぶ。
   serviceは有効かどうかと境目を、stats・kpiと同じ守りでdagq.tomlから読む。
   読むのは束縛からGitなしで決めたmain checkoutだけで、クライアントが名指すcheckoutを受けず、ファイルの大きさの上限と通常のファイルであることを検査する。
   ディレクトリはservice自身のhost.tomlから読み、元の決定4の時刻の下限をserviceで検査し、下回れば移さず理由つきで断る。
   これらの値を持つparamsは何も書かずに断り、人が打つ読み戻しのparamsはtaskのIDだけにする。
   チームの版の読み元は配置の設定としてgoal 250が決める（ADR-t3981-1決定3のまま）。
4. **queueに書くreportの送り手（元の決定7）。** queueのreportのディレクトリに書くreportの送り手はsupervisorとuserだけに限り、inbox・plannerとそれ以外のroleはserviceが認可で断る。
   inboxは「userと同じ」の例外になる。
   inboxとplannerのexportの権限は保ち、クライアントの権限で自分の場所へreportを書くexportの動きを変えない。
   serviceがページを組み、値をエスケープし、SVGを検査し、queueのreportのディレクトリの下だけへ書く検査と、送ったprincipalを記録しsupervisor以外ならページにも出す規則を保つ。

## Alternatives

- **写しから読み始める、または読み始めに印なしを判定する**（決定1）: ask 1510の例1・2のとおり、後から移す回が確定すると行が両方から落ちる。
- **DBの鍵を先に受けて写しを除く**（決定1）: 例3で、鍵の後に消された行をDBで畳めず、写しからも除いてしまう。
- **読み始めの前と読み終えた後の印の和を使う**（決定1）: 往復の例では両端とも印なしになり、行が落ちる。
- **世代を比べて読み直す**（決定1）: 移す回と重なる長い読みを繰り返し、断る場合が増える。
- **ページ全体を覆う1つのsnapshotを持つ**（決定1）: ページの要求をまたいで読み取りのtransactionを持ち、WALのcheckpointを止める。
- **既存のqueue管理のcapabilityに載せる**（決定2）: plannerとsupervisorも持つため最初のVACUUMを人とinboxだけにできず、移す回と読み戻しの役割の違いも表せない。
- **クライアントが設定値を送る**（決定3）: クライアント由来の境目や書き先がserviceの操作を決め、下限やhostの境界を迂回する。
- **reportの案(a): inboxとplannerもqueueへ値を送れ、principalで見分ける**（決定4）: AIのactorが任意の偽の値をqueueのreportへ送れる境界を残すため、人が選んだ案(b)に従い退ける。

## Consequences

- 読み戻しで消す印とは別に、読む区切りを決める小さな履歴をDBに残し続ける。
- 元のConsequencesの「AIのactorは偽の値を送れる」という制約は、任意のAIクライアントがqueueにreportを書ける経路を閉じることでなくなる。
  serviceが値の真偽を確かめられないことは残り、supervisorとuserの値も送ったprincipalの主張として記録する。
  ローカルのhostの境界が助言的であることも残る。
- 設定の読み元と操作の権限はserviceが守り、読む側の集計と、クライアントの権限で自分の場所へ書くexportの責務は変わらない。

---
id: adr-t3989-1
type: adr
title: チームの版では、goalにownerを必須にしてtaskは所属のgoalのownerを継ぎ、askをowner・代わりに答える人へ届けて答えが無ければ当番・管理者へ繰り上げ、運用のaskは当番へ届け、high以上の優先度を管理者の役だけが付け・変え、ownerをまたぐ依存を依存される側のownerに知らせ、roadmapの共有はqueueを共有のstoreに置くことで行い書き出しと外部trackerを見送る（ADR-0047決定17・ADR-t1975-1決定1・3をamends、チームの版の配置だけ）
status: accepted
created: 2026-11-03
updated: 2026-11-03
accepted_on: 2026-11-03
amends:
  - adr-0047 decision 17
  - adr-t1975-1 decision 1
  - adr-t1975-1 decision 3
owners:
  - hisamekms
tags:
  - architecture
  - authorization
  - priority
  - goal
  - inbox
related:
  - adr-t3981-1
  - adr-t3985-1
  - adr-0047
  - adr-t1975-1
  - adr-t451-1
  - adr-t996-1
  - adr-t3737-1
  - adr-t2159-1
  - adr-t1639-1
  - adr-t2373-1
  - plan-team-version-owner-and-ask-routing
---

# ADR-t3989-1: チームの版のgoalのownerとaskの届け先（ownerの必須と継承・代わりに答える人と繰り上げ・当番・優先度の権限・ownerをまたぐ依存・goal 14の結論）

## Context

[ADR-t3981-1](2026-11-02-t3981-1-team-version-control-side-on-containers-postgres-and-object-store-with-app-side-write-serialization.md)決定8は、チームの版でgoalのownerを必須にしtaskが継ぐこと、ownerの代わりに答える人と繰り上げ、運用のaskを当番へ向けること、high以上の優先度を管理者の役だけが付けること、ownerをまたぐ依存をplannerが推奨を付けて知らせることを決め、その細目を後のADRに残した。[ADR-t3985-1](2026-11-03-t3985-1-team-version-identity-by-oidc-human-roles-on-staticpolicy-runner-registration-per-person-inbox-and-no-delegated-landing-approval.md)は、人をOIDCのissuerとsubjectの組で識別し、役割を管理者・memberと持ち回りの当番の印に写し、inboxを人ごとにその人のマシンで動かし、memberがanswerできるaskを宛先が自分か代理のものに限ると決め（決定3・6）、宛先の規則をこのADRに残した。この決定はそれを決める。ADR-t3981-1とADR-t3985-1の決定は前提で、変えない。

今のローカルの版では、人に届くものは全て1つのinboxに届き（[ADR-0047](0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)決定17。[ADR-t451-1](2026-10-02-t451-1-ai-decides-recommendable-asks-and-escalates-only-the-undecidable.md)・[ADR-t996-1](2026-09-29-t996-1-supervisor-runs-throughput-review-jobs-and-reports-to-inbox.md)・[ADR-t3737-1](2026-11-01-t3737-1-a-headless-job-whose-start-the-queue-service-did-not-do-starts-again-and-is-no-failure.md)が改めた形）、人の優先度の指定はrequestの欄で受け、人の出どころの優先度は人とinboxが全ての段で変えられる（[ADR-t1975-1](2026-10-08-t1975-1-runtime-records-origin-and-person-set-priority-and-ai-actors-cannot-change-it.md)決定1・3）。どちらも人が1人であることを前提にしている。goal 14（roadmapの共有・同期の検討。候補は(a)書き出し・(b)外部tracker・(c)共有のstore）は、採る方向をADRに書くことを受け入れ条件に持つ。

具体の候補（記録の形・繰り上げの待ちの値・設定の形・CLI・eventの種類）は設計資料の[チームの版のownerとaskの届け先の具体案](../plans/team-version-owner-and-ask-routing.md)が持ち、実装のtaskが定義のそばのdoc commentへ移す。

## Decision

ADR-t3981-1の決定3・8、ADR-t451-1決定1（AIが決めきれないものだけを人に上げる）、[ADR-t2159-1](2026-10-09-t2159-1-dagq-does-not-use-cmux-and-the-person-opens-the-inbox.md)決定4（askの人への通知の送り方）、[ADR-t1639-1](2026-10-04-t1639-1-goal-priority-is-the-source-tasks-inherit-and-goals-carry-tags.md)（taskはgoalの優先度を継ぐ）、ADR-t1975-1のその他の決定（[ADR-t2373-1](2026-10-12-t2373-1-out-of-scope-follow-up-move-takes-the-destination-goals-priority.md)が改めた決定4を含む）は保つ。ADR-0047決定17とADR-t1975-1決定1・3のamendsは、OIDCの発行元を設定したチームの版の配置にだけ効く。

1. **goalのownerは1人の人で必須、taskは所属のgoalのownerを継ぐ。** goalのownerは登録された人（ADR-t3985-1決定3）1人で、goalを作るときに決まる: 人の言葉で作ったgoalはその人（requestならrequestを出した人）、AI由来のgoalは元の対象のgoalのowner（follow_upはそのrunのgoalのowner、findingは対象のgoalのowner）、元のgoalが無ければ作った時点の当番の人、当番がいなければ管理者の1人（どの管理者かは実装が決め、記録に残す）。ownerはその時点の人に固定し、当番の持ち回りで動かない。goalに属するtaskはownerを持たず、所属のgoalのownerを継ぐ（優先度の継ぎ方と同じで、goalを移せばownerも移る）。goalの無いtaskだけは自分のownerを持ち、goalと同じ規則で決まる。taskをgoalから外したときは、外す前のgoalのownerをそのtaskのownerとして付ける。ownerを変えられるのは今のownerと管理者で、新しいownerは登録された有効な人に限る。登録を外された人がownerのgoalは、管理者が付け替えるまで決定3の繰り上げの先へ届く。
2. **ローカルの既定ではownerは1人で、何も変えない。** OIDCの発行元を設定しない配置では、ownerはhostのuser 1人で、記録はせず全てのgoalとtaskのownerとみなす。代わりに答える人・当番・管理者の役は無く、askは今どおり1つのinboxに届き、繰り上げは起きず、人とinboxが全ての段の優先度を付け・変える（ADR-0047決定17・ADR-t1975-1決定1・3は今のまま効く）。
3. **askの届け先は、ownerと代わりに答える人、答えが無ければ当番、最後に管理者（ADR-0047決定17をamends）。** チームの版では、ADR-0047決定17の「inbox」をaskの宛先の人のinbox（ADR-t3985-1決定6）と読む。run・task・goalに結びつくaskは、その対象のowner（goalに属するtaskとrunはgoalのowner、goalの無いtaskとそのrunはtask自身のowner、決定1）と、ownerが決めた代わりに答える人に最初から同時に届き、どちらが答えてもよい。worker_questionとapprove_landingもこの規則に従い、goalの無いtaskのものはtask自身のownerに届く。代わりに答える人はownerごとに0人以上を持ち、人が不在のときだけ答える順番待ちにはしない。決まった時間内に誰も答えなければ当番へ、さらに答えが無ければ管理者へ繰り上げ、繰り上げた後も元の宛先は答えられる。待つ時間は、askの人が要る理由と対象の効いている優先度（taskとそのrunのaskはそのtaskに効いている優先度で、個別の指定があればそれ、無ければ所属のgoalから継いだ値（ADR-t1639-1）。taskに結びつかないgoalのaskはgoalの優先度）で決め、優先度が高いほど短くする（急ぎのものほど止まっている時間の損が大きいため。値は配置ごとの設定に置き、ADRでは決めない）。approve_landingの承認は繰り上げの先でもOIDCで識別した人だけが出せ、繰り上げの先に管理者の誰も答えなければ着地は止まったままにする（承認を省いて進めない）。planner自身のreviseと`planner_question`のanswerがplannerに返ること（決定17の後半）は変えない。
4. **運用のaskとattentionは当番へ届ける。** 運用のaskは、ownerを持つ対象（goal・task・run）に結びつかないもので、goalの無いtaskやそのrunのaskは運用のaskでなく決定3でtaskのownerへ届く。運用のaskとattention（supervisorの停止・pushの失敗・復旧jobの失敗・plannerの無応答・ディスクなど制御側の資源・チームのAPI keyの認証とコスト）は当番に届け、答えが無ければ管理者へ繰り上げる。人の各自の契約の認証とコストのaskは、その契約の持ち主に届ける（ADR-t3981-1決定9）。当番がいない時間は管理者へ届ける。知らせるだけで操作を求めないattention（スループットの見直しの結論など）は当番のinboxに届け、繰り上げない。
5. **答えられる人とaskの通知。** askにanswerできるのは、その時点の宛先（owner・代わりに答える人・繰り上げで宛先になった当番と管理者）と管理者である（ADR-t3985-1決定3のmemberの`ask.answer`の宛先の規則はこれを指す）。answerとcloseの記録は、答えた人と、宛先に入った理由（owner・代理・繰り上げ）を残す。通知の送り方はADR-t2159-1決定4のまま変えず、この決定は各人のinboxのwatchが「自分のask」と見るものを、自分が宛先のask（繰り上げで宛先に入ったものを含む）に絞るだけである。繰り上げで宛先に入ったaskは、その人のwatchには新しく開いたaskとして見える。
6. **high以上の優先度を付け・変えられるのは管理者の役の人だけ（ADR-t1975-1決定1・3をamends）。** チームの版では、ADR-t1975-1決定1のrequestの優先度の欄に入れられる段は、requestを出した人の役割の範囲（管理者は全ての段、memberはnormalまで）に限る。memberのrequestにhigh以上があれば、runtimeは値を下げずにrequestを拒み、上限と管理者に頼めることを名指す（黙って下げると人の指定が記録と食い違うため）。ADR-t1975-1決定3の「人とinboxは`human`の値も変えられる」を、管理者（その人のinboxを含む）は全ての段を、memberは自分がownerのgoalと自分がownerのtaskの値をnormal以下の範囲でだけ付け・変えられる、と改める。high以上の値は、上げるのも下げるのも管理者だけが行う。inboxの権限は親の人の役割を越えない（ADR-t3985-1決定6）。AIのactorがチームの版で自分の判断で付ける値もnormalまでで、plannerがhigh以上を要ると判断したときは推奨と理由を残して管理者に`scope`のaskで上げる。goalから継ぐ値（ADR-t1639-1）、follow_upが移動先のgoalの値を取ること（ADR-t2373-1）、runtimeがrequestの人の出どころの値を付けること（ADR-t1975-1決定2。値は出した人の役割の範囲で受けたもの）、AIの移動で人の出どころの値をtaskに残すこと（同決定4）は、AIの判断で付けることに当たらず、この制限を受けない。人の出どころの値をAIのactorが変えられない保護（ADR-t1975-1決定3・4）はそのまま効く。
7. **ownerをまたぐ依存は、plannerが推奨を付けて足し、依存される側のownerに知らせる。** 人（memberも）が別のownerのtaskかgoalへの依存を足したときも、同じく依存される側のownerに知らせる。plannerは別のownerのtaskかgoalへの依存を足すとき、なぜ要るかと、待つことになる優先度の逆転の有無を推奨として依存に添える。依存は知らせを待たずに効き、runtimeは依存される側のownerのinboxに知らせのattentionを届ける（answerを求めないので繰り上げない）。依存される側のownerが異を唱えるときは、自分のgoalの範囲（memberなら優先度はnormalまで、決定6）で手当てし、memberの範囲を越える逆転の解消（high以上への引き上げ）は管理者に頼む。依存する側のownerは自分のgoalの依存なので知らせない。
8. **roadmapの共有はqueueを共有のstoreに置くことで行い、書き出しと外部trackerは見送る（goal 14の結論）。** チームの版はqueueを制御側のPostgreSQLに置き（ADR-t3981-1決定1）、全員が同じgoalの一覧・優先度・依存・ownerをHTTPのAPIとCLIで読むので、goal 14の候補(c)共有のstoreを採る。roadmapという別の記録は作らない（ADR-t1639-1決定8のまま。順はgoalの優先度、前後はgoalの依存が表す）。(a)書き出しは、共有のstoreを読めば足り、書き出した写しは古くなって正本と食い違うので見送る。人に見せる表が要れば、queueを読んで作る読み取りだけの表示にし、写しから戻す同期は持たない。(b)外部tracker（GitHub Issues・Projectsなど）との同期は、正本が2つになり衝突の解き方を持つことになるので見送り、要るときはqueueを正本とする片方向のadapter（PR連携と同じ後の選択肢、ADR-t3981-1決定7）として人が決める。goal 14を閉じるかは人の言葉で決める。

## Alternatives

- **owner: taskごとにownerを持たせる**: goalの中でaskの宛先が分かれ、goalの達成を誰が見るかがぼやける。継ぐ形なら移動で宛先が自然に移る。goalの無いtaskだけが持つ（決定1）。
- **owner: ownerを当番のような持ち回りにする**: 文脈を持たない人に判断が回る。ownerは固定し、持ち回りは繰り上げの先の当番が担う（決定1・3）。
- **owner: ownerを任意にし、無ければ共有のinboxへ**: ADR-t3985-1決定6が共有のinboxを退けた。必須にする（決定1）。
- **askの届け先: 代わりに答える人を不在のときだけの順番待ちにする**: 不在の判定（予定表・状態）をdagqが持つことになる。最初から同時に届け、答えが無いことだけを時間で見る（決定3）。
- **askの届け先: 繰り上げを全員への同報にする**: 誰も自分のこととして答えない。当番、管理者の順に狭く繰り上げる（決定3・4）。
- **askの届け先: approve_landingの承認を繰り上げの先で省く・自動で通す**: 人の関門の意味を失う（ADR-t3981-1決定7）。止めたままにする（決定3）。
- **askの届け先: 繰り上げの待ちを1つの固定の時間にする**: 急ぎのgoalのaskも急がないgoalのaskも同じだけ待ち、urgentのrunが長く止まる。人が要る理由と優先度で分ける（決定3）。
- **運用のask: goalに結びつかないaskも誰かのownerに寄せる（例えば最後に触った人）**: 制御側の不具合はその人の文脈でなく、持ち回りで受ける当番の方が早く手当てできる。当番がいない時間を空白にせず管理者へ届ける（決定4）。知らせるだけのattentionを繰り上げると、答えの要らない知らせが管理者に溜まるので繰り上げない。
- **答えられる人: 管理者を宛先に入るまで答えさせない**: 繰り上げを待つ間、知っている管理者が手を出せない。管理者はいつでも答えられ、記録で宛先に入った理由を区別する（決定5）。
- **ローカルの既定: ローカルでもownerの記録を必須にする**: 1人の使い方に記録の手間とmigrationの埋めが増え、宛先は1つのinboxのままで得るものが無い。hostのuserをみなしのownerにする（決定2）。
- **優先度: memberのhigh以上の指定を黙ってnormalに下げて受ける**: 人の言葉と記録が食い違い、気づかれない。拒んで名指す（決定6）。
- **優先度: memberのhigh以上の指定を承認待ちとして受け、管理者のaskにする**: 指定のたびに管理者のaskが増え、request自体が承認を待つ。拒んで管理者に頼む形が単純で、管理者の言葉はそのまま人の出どころになる（決定6）。
- **優先度: AIのactorにhigh以上を許す**: 優先度の裁定は管理者の役（ADR-t3981-1決定8）。推奨にとどめる（決定6）。
- **ownerをまたぐ依存: 依存される側のownerの承認を待って依存を足す**: 計画が他人の応答で止まり、承認のaskが増える。知らせにとどめる（決定7）。
- **ownerをまたぐ依存: 知らせない**: 依存される側が自分のgoalを後回しにしたとき、誰が待つかが見えない（決定7）。
- **goal 14: (a)書き出しを共有の手段にする**: 写しが正本より古くなり、写しを見た人の判断が食い違う。読み取りだけの表示にとどめる（決定8）。
- **goal 14: 外部trackerを正本にする**: dagqのclaim・依存・優先度の検査が外の記録に依る。queueを正本にする（決定8）。

## Consequences

- チームの版では、ownerを持たないgoalは作れない。登録を外す人のgoalは管理者が付け替える手間が要る。
- askは宛先が人ごとに分かれ、ローカルの1つのinboxの見張りの運用（ADR-0047決定17の今の形）はチームの版では人ごとのinboxと当番の運用になる。当番の決め方（持ち回りの組み方）は配置する側の運用で、dagqは印（ADR-t3985-1決定3）を持つだけである。
- memberはhigh以上を付けられないので、急ぎの件は管理者を経る。ローカルの既定は変わらない。
- 今の姿を書く`docs/design/`（askの宛先・優先度の出どころ・inbox）は、この決定がまだ実装されていないので変えず、実装のtaskが着地と同じ変更で直す。
- この決定は、繰り上げの待ちの値、記録の形、HTTPのAPIの形を決めていない。待ちの値と記録の形は実装のtaskが設計資料から選び、APIの形はHTTPのAPIのADRが決める。

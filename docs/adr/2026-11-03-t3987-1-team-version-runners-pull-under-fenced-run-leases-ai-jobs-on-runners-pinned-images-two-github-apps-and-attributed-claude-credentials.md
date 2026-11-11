---
id: adr-t3987-1
type: adr
title: チームの版のrunnerは外向きのHTTPSでpullし、制御側が割り当てて世代つきのrunのleaseで守り、失ったleaseは生き返らせず制御側が受け入れたrun branchのheadから別のrunnerが続け、AIのjobは全てrunnerで動いて制御側はagentもrepositoryのコードも走らせず、imageはCIがbuildしてdigestで固定し、gitはrunner用とmain用の2つのGitHub Appに分けてmainを制御側だけが進め、hostに依る機能は制御側でやめるかrunnerへ移し、Claudeの資格情報は割り当てで選んで契約ごとに記録し、チームのAPI keyだけに予算の上限を掛け、各自の契約は持ち主の共有でないrunnerだけで使う
status: accepted
created: 2026-11-03
updated: 2026-11-04
accepted_on: 2026-11-03
amended_by:
  - adr-t4332-1
owners:
  - hisamekms
tags:
  - architecture
  - runtime
  - security
  - queue-service
related:
  - adr-t3981-1
  - adr-t3985-1
  - adr-t1233-1
  - adr-t1233-2
  - adr-t2113-3
  - adr-t2177-1
  - adr-t2179-1
  - adr-t2257-1
  - adr-t1486-1
  - plan-team-version-runner
---

# ADR-t3987-1: チームの版のrunnerとjobの置き場（pullとleaseの引き継ぎ・jobの置き場・imageの配り方・gitの資格情報とruleset・hostに依る機能・Claudeの資格情報と記録と予算）

## Context

[ADR-t3981-1](2026-11-02-t3981-1-team-version-control-side-on-containers-postgres-and-object-store-with-app-side-write-serialization.md)は、チームの版の制御側をcontainerのtask 1つとPostgreSQLとS3互換の置き場に置き（決定1）、runnerが外向きのHTTPSでpullし、他の人のtaskを受けるrunnerをcontainer必須にし、runごとにGitHubのremoteからcloneし（決定5）、mainを制御側がGitHub Appのfast-forwardでだけ進め（決定6）、Claudeの資格情報をチームのAPI keyと各自の契約から選べるようにする（決定9）と決め、runnerの割り当て・imageの配り方・資格情報の予算を後のADRに残した。[ADR-t3985-1](2026-11-03-t3985-1-team-version-identity-by-oidc-human-roles-on-staticpolicy-runner-registration-per-person-inbox-and-no-delegated-landing-approval.md)はrunnerの登録・token・能力の申告と脅威の模型を決めた。この決定はその上で、runnerがどう仕事を取り、どこで何が動くかを決める。

今のローカルの版では、supervisorがhostで常駐し（launchd）、worker・resume・integrateのverificationはhostのdagq専用のPodman machineのcontainerで動き（[ADR-t2113-3](2026-10-08-t2113-3-only-workers-resume-and-integrate-verification-run-in-containers.md)）、headlessのjobはhostで動く。imageは制御側がmainのcheckoutからbuildし（[ADR-t2257-1](2026-10-12-t2257-1-container-runtime-takes-project-and-host-values-from-config.md)決定2）、containerのrunの事実はbundleと状態のファイルで渡り（[ADR-t2179-1](2026-10-12-t2179-1-control-side-runs-no-git-in-a-container-runs-clone-and-lands-in-its-own-worktree.md)）、Claudeの資格情報はhostのqueueのbrokerが`host.toml`で選んだものに差し替える（[ADR-t2177-1](2026-10-12-t2177-1-broker-swaps-the-run-token-for-a-claude-credential-chosen-in-host-toml.md)）。チームの版の制御側はクラウドのcontainerで、podmanもrepositoryのcheckoutも持たず、runnerは人ごとの別のマシンにある。

具体の候補（APIの呼び出しの並び・leaseと生存の報告の時間・registryとtagの形・GitHub Appの権限とrulesetの形・配置と`host.toml`の設定の形・記録の形・予算の単位）は設計資料の[チームの版のrunnerの具体案](../plans/team-version-runner.md)が持ち、実装のtaskが定義のそばのdoc commentへ移す。HTTPのAPIの形と版の互換、書き込みの係のleaseの期限、着地の照合の細目は、それぞれのADRが決める。

## Decision

ADR-t1233-1（制御側と実行側を分ける・queue service・client mode）、ADR-t3981-1（チームの版の構成。とくに決定5・9）、ADR-t3985-1（runnerの登録・token・能力の申告・脅威の模型）の決定は保つ。この決定はどれもamendsも置き換えもしない。ローカルの既定の配置（runnerがsupervisorと同じプロセスにいる配置）の動きは何も変えない。ADR-t1233-1決定1・ADR-t2113-3決定2・3・ADR-t2179-1決定5・ADR-t1233-2決定2・4が制御側やhostに置くもの（plannerとheadlessのjob・run branchを扱うgit・着地用のworktree・e2e）は1台のhostの配置の置き場で、チームの版の置き場はADR-t3981-1決定1・5・6が決めた制御側・runner・検証役のマシンの分け方の上でこの決定が割り振る。制御側と実行側を分けること、実行側がmainへの書き込みの資格を持たないこと、AI actorが資格情報を持たないことは、どの割り振りでも保つ。

1. **runnerは制御側が割り当てた仕事を取り、世代つきのleaseで持つ。** runnerは自分のrunnerのtokenで制御側のHTTPのAPIへ外向きのHTTPSで仕事を問い、制御側は書き込みの係の1つのtransactionで、runnerの登録（持ち主・共有の印）と申告（ADR-t3985-1決定5）と資格情報の線引き（決定8）と予算（決定10）に合うrunかjobを選び、claimとleaseを渡す。runnerは仕事を選ばない。leaseは1つのrunかjobの割り当て（返すか失うまでの全てのturn）を覆い、渡すたびに上がる世代の番号を持ち、生存の報告・結果の報告・run branchの更新の報告は全てその世代を添え、今の世代でない報告を制御側は拒む。leaseの期限は制御側の時計だけで決め、runnerの時計を信じない。leaseは書き込みの係のlease（ADR-t3981-1決定4）とは別の物で、同じ「世代で古い持ち手を拒む」原則に従う。runnerのprincipalが持つのは割り当てたrunとjobのclaim・生存の報告・結果の報告だけで（ADR-t3985-1決定4）、割り当てで渡すjobのtoken（plannerのものを含む）とgitのtoken（決定6）は、制御側がその割り当てに限って発行し、runnerのprincipalの権限を広げない。
2. **run branchの正は制御側が受け入れたheadで、失ったleaseは生き返らせず、そのheadから別のrunnerが続ける。** 制御側は、今の世代のleaseの報告で受け入れたrun branchのheadを記録し、resume・review・検証・着地は記録したheadのcommitを使い、GitHubのbranchの先端を信じない。run branchのrefはleaseの世代ごとに分け、1つの世代のrefはその世代のleaseを持つrunnerだけがfast-forwardでpushし、どの世代のrefも強制で更新しない。生存の報告が途絶えて期限を過ぎたlease、持ち主か管理者が失効させたrunnerのlease（ADR-t3985-1決定4）は失われ、同じ世代では戻らず、制御側はそのleaseに渡したgitのtokenを失効させる。runは今の復旧の扱い（leaseの無い未完のrunを復旧に回す）に乗り、引き継ぎは次の順に行う。まず制御側がGitHubのAPIで（gitを走らせずに）新しい世代のrefを記録したheadに作り、そのrefが記録したheadを指していることをGitHubから読み直して確かめて記録する。その後に、新しい世代のleaseとして、そのrunを受けられるrunner（元のrunnerでもよい）に割り当て、runnerはそのrefからcloneして新しいsessionのresumeで続ける。前の世代のrefは、新しい世代のrefを確かめて記録した後にだけ制御側が消す。pushが成功して報告の受け入れの前にrunnerが止まったときも同じで、前の世代のrefに残った受け入れていない先端は信じず、上書きも取り込みもせず、後継はその先端に触れずに新しい世代のrefで進める。不変条件として、受け入れたheadは、mainに入るかrunが取り消されるまで、いつも少なくとも1つの確かめたrefから届き、引き継ぎのどの段で制御側かrunnerが止まっても失われない（queueにhashだけを残す形には頼らない）。止まった後の引き継ぎは、確かめた最も新しい世代のrefから同じ順でやり直す。引き継ぐのは記録したheadとqueueの記録だけで、元のマシンのsession・受け入れていない変更・buildの出力は引き継がない（失ってよい物とする）。戻ってきた元のrunnerは報告を拒まれた時点でcontainerを止めてcloneを捨てる。runnerの止まり（SIGTERM）は、走っているturnを終えてheadを報告し、leaseを自分から返して、期限を待たずに次の割り当てに回す。
3. **AIのjobは全てrunnerで動き、制御側はagentもrepositoryのコードも走らせない。** planner・plan review・goal review・runのreviewのagent・triage・復旧のjob・見直しのjobは、runと同じpullとleaseでrunnerに割り当てる。各jobは、そのjobが扱う物のownerに帰属させ（runとそのreview・triage・復旧はrunのowner、plan review・goal reviewはgoalのowner、plannerは依頼した人、ownerの無い運用のjobはチーム）、帰属で割り当て先と資格情報を決める（決定8）。他の人とチームに帰属するjobは共有のrunnerでだけ動く。チームの版のrunnerは、共有かどうかに依らず、runもjobもcontainerで動かし、runnerの資格（runnerのtoken・gitのtoken・各自の契約とその置き場）はcontainerの外のrunnerのプロセスだけが持つ。共有でないrunnerのhostで着地前のrunのコードを動かす例外（決定7のhostのe2eと準備のコマンド）は、runnerの資格を読めない別のOSのuserかsandboxで動かし、どの経路（env・ファイル・OSの資格の置き場）でも資格へ届かせない（2026-11-04の人の決定）。その分離を用意して確かめられないrunnerには、hostで動かすコマンドを割り当てない。ローカルの既定のOSのsandboxの扱い（ADR-t2113-3決定2）は変えない。reviewのプログラムは着地の検証の一部として検証役のマシンで動く（ADR-t3981-1決定6）。runのreviewとhostのe2eがrunのownerのマシンで動きうることは、ownerが自分の着地の関門を承認してよいこと（ADR-t3981-1決定7）と同じ信頼の範囲として受け入れ、着地する中身は検証役のマシンの報告の照合と制御側のmainの更新が守る。受け入れるのは関門の結果を持ち主のマシンが出すことまでで、着地前のrunのコードがrunnerの資格に届くことは含めない。制御側に残るのは、supervisorのループの判断・書き込み・GitHubのAPIの読み取りとrefの更新とtokenの発行・Claudeの差し替え（決定8）だけで、agentのCLIとrepositoryのコードとgitのworktreeを持たない。
4. **imageはmainのCIがbuildしてregistryに置き、制御側がdigestで固定する。** dagqが配るbase（runtimeの要るものだけ。ADR-t2257-1決定2）は、dagqのreleaseがその版のdagqのclientを入れてarm64とamd64で公開のOCIのregistryに出す。projectの道具を足したimageは、利用するpjのCIがmainのContainerfileと入力（ADR-t2257-1決定2の、tagを決める物）からbuildしてpjの選ぶOCIのregistryに置き、制御側にdigestを届ける。制御側はdigestを、配置の設定で縛ったrepository・workflow・mainのrefの3つが揃ったCIの識別（ADR-t3985-1決定4のGitHub ActionsのOIDC）からだけ受け、baseのdagqの版が制御側のAPIの版と合わないものを拒む。制御側はmainのその時点のdigestをclaimのときにrunへ固定して割り当てに含め、runnerはdigestでだけpullし、tagでは取らない。verificationは着地の検証を始めるときのmainのdigestを使い、マシンの報告のimageのdigest（ADR-t3981-1決定6）を制御側が固定した物と照合する。runnerがimageを自分でbuildする経路はチームの版に置かない（ローカルの既定は今どおり制御側がbuildする）。
5. **共有のrunnerがcontainerでだけ動くことは、登録・runnerのつくり・報告の照合の3つで確かめる。** 遠くのマシンの中を制御側は証明できないので、申告は信じない（ADR-t3985-1決定5）。(i) 共有の印は管理者が承認した登録だけが持ち、承認の前提はrunnerをcontainerだけの形で立てたことである。(ii) runnerのつくりとして、共有の印を持つrunnerにはhostで直に走らせる実行の経路を持たせず、containerを使えなければ仕事を取らずに理由を返す。(iii) 共有のrunnerの結果の報告は、固定したdigestのcontainerで走ったことを添えたものだけを受け、添えないか食い違う報告は拒み、そのrunnerの共有の印を外して管理者に知らせる。(ii)と(iii)が捕まえるのは設定の誤りと改変していないrunnerの食い違いで、持ち主の意図的な改変は防げない。持ち主は登録した信頼しきらない実行側（ADR-t3985-1決定8(ii)）で、その改変に対しては登録の承認と失効、着地する中身の検証役のマシンの報告の照合と制御側のmainの更新が守る。
6. **gitの資格情報は、runner用とmain用の2つのGitHub Appに分け、どちらの鍵も制御側だけが持つ。** runner用のAppは、runnerのclone・fetchとrun branch（と検証役が着地の候補を置くbranch）のpushに使う。制御側がleaseを持つ間だけ、そのrepository 1つに絞った短いinstallation tokenを発行してrunnerに渡し、leaseを失えば失効させる（決定2）。tokenはrunnerのプロセスが持ち、containerの中の実行側には渡さない（実行側はpushの経路を持たない。ADR-t2257-1決定3）。runnerは各turnの終わりに、containerが書いたbundleを取り込んでからrun branchをpushしてheadを報告し（ADR-t2179-1決定2・3）、hookとdriverを走らせない（同決定8）。installation tokenはbranchの単位に絞れないので、runnerが他のrunや他の世代のrefへpushしても、制御側は記録したhead（決定2）しか使わず、受け入れない。main用のAppはmainのrefのfast-forwardの更新と、引き継ぎでの新しい世代のrefの作成と、終わったrunと前の世代のrefの削除だけに使い（ADR-t3981-1決定6）、制御側の外に出さない。rulesetは次の禁止を持つ。(i) mainの更新はmain用のAppにだけ許す。(ii) mainの強制の更新と削除は、main用のAppを含めて誰にも迂回させない（更新を許す規則と同じrulesetに置かず、bypassの無い別のrulesetにする）。(iii) runner用のAppには、run branchと着地の候補の名前空間の外のref（tagを含む）の作成・更新・削除を許さない。(iv) 名前空間の中でも強制の更新は誰にも許さず、削除はmain用のAppにだけ許す。制御側は、この禁止をrulesetが持っていることをGitHubのAPIで確かめられた間だけrunnerに仕事を割り当ててrunner用のAppのtokenを出し、確かめられない配置（rulesetを使えないplanや、禁止が欠けたruleset）では割り当てず書き込みのtokenも出さない。禁止の代わりに事後に見つけて知らせる形は採らない。runner用のAppにはCIのworkflowの定義を変える権限を与えず、runnerのpushでmain以外のrefから変えたworkflowが資格つきで走ることを防ぐ。そのためworkflowの定義を変えるrunはrunnerがpushできず、人を待つ（配置する側がその権限を与えるかを決める）。人のbypassを既定で置かない。mainの前にstatus checkもPRも求めない（CIはmainの後の最終関門のまま。ADR-t3981-1決定7）。
7. **hostに依る機能は、制御側でやめるか、runnerへ移すか、ローカルだけに残す。** 制御側はクラウドのcontainerでhostを持たないので、次の表のとおり扱う。

   | 機能 | 扱い | 理由 |
   | --- | --- | --- |
   | launchdによる常駐（supervisorの常駐と、その起動・停止のコマンド） | 制御側ではやめ、配置の側のcontainerの管理（health check・SIGTERMでの停止。ADR-t3981-1決定2）に任せる。runnerの常駐は持ち主のOSの仕組みに任せ、dagqはrunnerを前で動かすコマンドだけを配る。launchdへの登録はローカルの既定にだけ残す | 制御側のcontainerにはlaunchdが無く、runnerのOSはmacOSに限らない |
   | dagq専用のPodman machine（作成・起動・メモリの予約） | runnerへ移す。macOSのrunnerは今の仕組みのまま自分のmachineを持ち、Linuxのrunnerはmachineを使わない。制御側は使わない | containerを動かすのはrunnerだけで（決定3）、machineの資源はrunnerのマシンの事情である |
   | runsの置き場の掃除（clone・run dir・着地用のworktree・buildのvolume） | runnerへ移す。各runnerがrunの終わり・leaseを失ったとき・止まるときに自分の物を消す。制御側の置き場の物の保存期間は書き込みの係とstoreのADRが決める | 制御側はrunsの置き場を持たず、cloneとbuildの出力はrunnerのマシンにだけ在る |
   | hostの負荷の記録 | runnerへ移す。各runnerが自分のマシンの負荷を生存の報告に添え、制御側がrunnerごとに記録して割り当ての材料にする。人が読むhostの写しのファイルはローカルだけに残す。制御側のcontainerの指標は観測のADRが決める | 割り当てに要るのは仕事を走らせるマシンの負荷で、制御側のhostは在り続ける物ではない |
   | hostのe2eの関門（reviewのpassの後のhostのコマンド・準備のコマンド。ADR-t1233-2・ADR-t2257-1決定4） | runnerへ移し、runのownerの共有でないrunnerのhostで、記録したhead（決定2）について流す。同時に流すのはそのrunnerで1本で（ADR-t1233-2決定4の考え方）、runnerの資格（runnerのtoken・gitのtoken・各自の契約の置き場）を読めない別のOSのuserかsandboxで流す（決定3）。そのrunnerか分離が無ければ待つ | runのコードをcontainerの外で走らせるので、そのコードの持ち主が自分のマシンで引き受けるときだけ許し、資格には届かせない |
   | 固定バイナリの入れ替えと自動更新、その関門のe2e | ローカルだけに残す。制御側はimageの入れ替え（配置の側）、runnerは持ち主の入れ替えで、版の互換はHTTPのAPIの版で判定する | 入れ替えるのは配置する側と持ち主で、hostの固定バイナリの運用はローカルの本番queueのものである |
   | hostのqueueのbroker（containerの宛先） | runnerへ移す。runnerが自分のcontainerの宛先を持ち、queueの宛先は制御側のHTTPのAPIへ、チームのAPI keyのClaudeの宛先は制御側の差し替えへ、各自の契約の宛先はrunnerの中の差し替えへ送る（決定8） | containerの外への唯一の出口（ADR-t1233-1決定6）をrunnerのマシンで保ちつつ、チームのAPI keyを制御側の外に出さない |

8. **Claudeの資格情報は割り当てで選んでleaseに固定し、各自の契約は持ち主の共有でないrunnerだけで使う。** 資格情報は、チームのAPI key（制御側だけが持ち、制御側の差し替えが付ける。ADR-t3985-1決定8(i)）と、各自の契約（持ち主のrunnerのマシンにだけ置き、そのrunnerの中の差し替えが付け、制御側へ送らない。どの種類の契約を出すかと置き方は今どおりそのrunnerのhostの設定で人が選ぶ。ADR-t2177-1決定2・4）の2種類である。配置がチームのAPI keyを許すか、人が自分の契約を出すか、両方があるときどちらを先に使うかは、配置ごとの設定とその人の設定で選び、`dagq.toml`には置かない。どちらを使うかは制御側が割り当てのときにjobの帰属（決定3）から決めてleaseに固定し、leaseの間は変えない。認証の失敗と上限で差し替えが別の資格情報に切り替えないこと（ADR-t2177-1決定4）を保ち、別の資格情報になりうるのは控えの後の新しいleaseの選び直しだけである。runnerの中の差し替えは、制御側がそのleaseに固定した資格情報が各自の契約のときだけ契約を付ける。チームのAPI keyの要求は、runnerの差し替えがrunのtokenを検証したうえで、runnerのtokenとleaseの世代を添えて制御側の差し替えへ送り、制御側は今の世代のleaseのものだけをAnthropicへ送る。runのtokenはdagqの外へ送らない（ADR-t2177-1決定1）。各自の契約は、帰属がその人のjobで、その人が持ち主の共有でないrunnerにだけ割り当て、共有のrunnerは持ち主の分でも各自の契約を持たない。これはADR-t3981-1決定9とADR-t3985-1決定5の線引きを、共有のrunnerに契約を置かない形で強めたものである。
9. **どのjobをどの契約で動かしたかを記録し、各自の契約を他人のjobに使っていないことを記録で確かめる。** 実行ごとのトークンの記録（[ADR-t1486-1](2026-10-04-t1486-1-supervisor-records-token-usage-per-execution.md)）に、資格情報の種類・契約の持ち主・jobの帰属・走ったrunnerを足す。チームのAPI keyの使用量は制御側の差し替えが応答から数えた値を正とし、各自の契約の使用量はrunnerの報告をデータとして記録する。制御側は、各自の契約の記録で契約の持ち主とjobの帰属が違うものと、共有のrunnerで各自の契約を使った記録を見つけたら、そのrunnerへの割り当てを止めて管理者に知らせる。この照合が捕まえるのは割り当てと設定の誤りで、改変したrunnerの偽りの報告は捕まえない。他人のjobに各自の契約を使わせない本体は、制御側がそう割り当てないこと（決定8）である。
10. **予算の上限はチームのAPI keyに掛け、超えたら新しいturnを始めない。** 上限は配置全体とownerごとに置け、goalごとにも足せる。上限に達したら、その上限に当たるチームのAPI keyのleaseを新しく渡さず、持っているleaseは走っているturnを終えたところで返させ（次のturnを始めない）、超えた量を記録し、当番に運用のaskで知らせる。上限を変えるのは管理者の役だけである。人が各自の契約を先に使うと選んでいれば、その人のjobは次の割り当てでその人のrunnerへ回る。各自の契約にはdagqの上限を掛けず、契約の利用上限は今のClaudeの上限の控えに乗せる（そのrunnerのその契約だけを控える）。

## Alternatives

- **(a) 制御側からrunnerへ仕事を押す・常時の双方向の接続を張る**: 押すのはADR-t3981-1決定5が退けた。常時の接続は間のproxyとload balancerの切断に弱い。外向きの問いの繰り返しで足りる（決定1）。
- **(a) runnerが仕事を選んでclaimする**: 共有の印・資格情報の線引き・予算の判断がrunnerの側に散り、偽りの申告で他人の仕事を取れる。制御側が選ぶ（決定1）。
- **(a) 期限切れのleaseを元のrunnerが取り戻せる（ローカルの採り直しと同じ）**: 遠くでは生きているかを制御側が確かめられず、2つのrunnerが同じrun branchへpushしうる。失ったleaseは戻さず新しい世代で渡す（決定2）。
- **(a) GitHubのrun branchの先端を正にする**: installation tokenはbranchの単位に絞れず、leaseを失ったrunnerや他のrunのrunnerのpushが混ざりうる。制御側が受け入れたheadを正にする（決定2・6）。
- **(a) run branchを1本にして、後継が受け入れたheadへ強制で戻す**: 強制の更新をrunner用のAppに許すことになり、他のrunのbranchも書き戻せる。世代ごとのrefにして強制の更新を誰にも許さない（決定2・6）。
- **(a) 引き継ぎで前の世代のrefを先に消し、新しいrefは後継のrunnerが作る**: 消した後に後継も止まると、受け入れたheadを指すrefが無くなり、届かないcommitはGitHubのgcで消えうる。制御側が新しいrefを作って確かめてから前のrefを消す（決定2）。
- **(a) 元のマシンの作業の状態（session・buildの出力）を引き継ぐ**: 置き場とruntimeの互換が要り、他人のマシンの中身を運ぶことになる。GitHubのrun branchだけを引き継ぐ（決定2）。
- **(b) planner・headlessのjobを制御側のcontainerで動かす**: 信頼しないAI actorと信頼しないdiffが、mainを進める鍵・DBの接続・チームのAPI keyと同じ場所に入り（ADR-t3985-1決定8(i)を崩す）、制御側にrepositoryのcheckoutとagentのCLIが要る。runnerで動かす（決定3）。
- **(b) jobだけを動かす専用の種類のrunnerを作る**: 割り当てと資格情報の規則を2つ持つことになる。runと同じpullで、共有のrunnerがjobも受ける（決定3）。
- **(b) 共有でないrunnerではjobをhostのOSのsandboxで動かす（ローカルの既定と同じ）**: hostで動くagentがrunnerのprocessの資格（runnerのtoken・gitのtoken・各自の契約）を読める。チームの版のrunnerは全てcontainerで動かす（決定3）。
- **(c) 各runnerがimageをbuildする**: buildの結果がマシンごとに変わり、検証の報告のdigestを照合できない。CIがbuildしdigestで固定する（決定4）。
- **(c) 制御側がimageをbuildする**: 制御側がrepositoryのコードと道具を持つことになり、クラウドのcontainerの中でcontainerのbuildが要る（決定3を崩す）。
- **(c) 共有のrunnerのcontainerを申告で信じる**: 遠くの申告は確かめられない（ADR-t3985-1決定5）。登録・つくり・照合で確かめる（決定5）。
- **(d) 各自のGitHubのtokenや鍵をrunnerに置く**: その人が届く全てのrepositoryに効き、人が離れると止めにくく、mainを守るrulesetを人ごとに書くことになる。Appのinstallation tokenにする（決定6）。
- **(d) GitHub Appを1つにしてrunnerにも同じAppのtokenを渡す**: rulesetのbypassはAppの単位なので、mainを進められるAppのtokenがrunnerに渡る。Appを2つに分ける（決定6）。
- **(d) runnerがpushせず、bundleを制御側に送って制御側がpushする**: 制御側がgitと信頼しないobjectを扱うことになり、決定3を崩す。runnerがrun branchをpushする（決定6）。
- **(d) runner用のAppにworkflowの権限を与える**: run branchで変えたworkflowがrepositoryのsecretとimageを届ける識別つきで走りうる。与えず、workflowを変えるrunは人を待つ（決定4・6）。
- **(d) rulesetで禁止できない配置では、名前空間の外の変化を事後に見つけて知らせる**: 見つけたときには未承認のbranchやreleaseのtagが書き換わり、副作用が起きている。禁止を確かめられない配置にはrunnerを割り当てない（決定6）。
- **(e) hostのe2eを、runnerと同じOSのuserで、資格をenvから外すだけで流す**: 同じuserのプロセスは資格の置き場のファイルとOSの資格の置き場を読める。人は別のOSのuserかsandboxで動かし資格に届かせないと決めた（決定3・7）。
- **(e) hostに依る機能を制御側のcontainerで真似る**: launchd・podman・hostのe2eはcontainerの中に無く、無理に置くと制御側でrepositoryのコードを走らせる。やめるか移す（決定7）。
- **(f) チームのAPI keyをrunnerに配る**: keyがマシンの数だけ散り、失効と予算の数えが持ち主の手に移る。制御側だけが持つ（決定8）。
- **(f) 各自の契約を共有のrunnerでも使い、記録だけで線を引く**: 他人のjobに使ってから見つけることになる。共有でないrunnerにだけ置く形で先に防ぎ、記録で確かめる（決定8・9）。
- **(f) 上限や認証の失敗でturnの途中に別の資格情報へ切り替える**: ADR-t2177-1決定4が退けた。leaseに固定し、選び直すのは新しいleaseだけにする（決定8）。
- **(f) 上限に達したら走っているturnを止める**: 途中のturnの作業が無駄になり、run branchも中途で残る。新しい割り当てを控える（決定10）。


## Consequences

- 制御側はagentもgitのworktreeも持たないので、containerのimageは小さく、AIの振る舞いの事故が制御側の鍵に及ばない。代わりに、AIのjobを受けるrunnerが1台も無いとplanner・review・復旧も止まる。他の人とチームに帰属するjobは共有のrunnerでチームのAPI keyでだけ動くので、チームの版の配置には、jobを受ける共有のrunnerとチームのAPI keyが要る。チームのAPI keyを許さない配置では、そのjobは割り当て先が無く待つ。
- チームのAPI keyの予算が尽きると、各自の契約と自分のrunnerを持たない人のrunとjob（plan review・goal review・復旧を含む）が止まる。超える量は、上限に達したときに走っていたturnの分までである。
- leaseを失ったrunは、受け入れていない作業をやり直す。runnerはturnの終わりごとにheadを報告するので、失うのは多くとも1つのturnの作業である。
- rulesetを使えないGitHubのplanのrepositoryでは、チームの版のrunnerが動かない。
- pushが成功して報告が受け入れられる前に止まったturnの作業は失われ、前の世代のrefは消される。
- hostのe2eが要るrunのownerは、共有でないrunnerのhostに、runnerの資格を読めない別のOSのuserかsandboxを用意する。
- 共有の印を持つrunnerしか持たない人は、hostのe2eが要るrunと各自の契約を使えない。共有でないrunnerを別に登録する。
- workflowの定義を変えるrunは人を待つ。
- チームのAPI keyのClaudeの通信は全て制御側を通るので、制御側の帯域とstreamの扱いが要る。その引き換えに使用量の数えが制御側で正確になる。
- imageはpjのCIとregistryを前提にする。CIが動かない間は新しいdigestが来ず、前のdigestで動き続ける。
- GitHub Appを2つ作ることは配置する側の手間で、手順は配る文書（段5）が書く。
- 今の姿を書く`docs/design/`は、実装のtaskが着地と同じ変更で直す（この決定はまだ実装されていない）。
- この決定は、HTTPのAPIの形と版の互換、書き込みの係のleaseの期限と保存期間、検証役のマシンの選び方と着地の照合、ownerとaskの届け先と当番の決め方、予算の単位と既定の値を決めていない。

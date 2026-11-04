---
id: adr-t3990-2
type: adr
title: チームの版の配置の設定は環境変数とファイルにだけ置いてdagq.tomlに置かず、healthは書き込みの係かどうかとDBの届きに依らず、SIGTERMでは新しい仕事を止めて書き込みの列を空にしてから係を手放し、migrateはHTTPの外の一度きりのコマンドにする
status: accepted
created: 2026-11-04
updated: 2026-11-04
accepted_on: 2026-11-04
owners:
  - hisamekms
tags:
  - architecture
  - runtime
  - queue-service
  - migrations
related:
  - adr-t3981-1
  - adr-t3985-1
  - adr-t3986-1
  - adr-t3987-1
  - adr-t1233-1
  - adr-t3102-2
  - adr-t3102-3
  - adr-t3990-1
  - adr-t3990-3
  - adr-t3990-4
  - adr-t3990-5
  - design-queue-service
  - plan-team-version-control-side
---

# ADR-t3990-2: チームの版の制御側のcontainerの起動と停止（設定の入口・health・SIGTERMの順・migrate）

## Context

[ADR-t3981-1](2026-11-02-t3981-1-team-version-control-side-on-containers-postgres-and-object-store-with-app-side-write-serialization.md)は、制御側をcontainerのtask 1つとPostgreSQLとS3互換の置き場にし（決定1）、配置は利用するpjが持ってdagqは部品を配る（決定2）と決め、部品の形とHTTPのAPIを後に残した。[ADR-t3986-1](2026-11-03-t3986-1-write-leader-fencing-trigger-placement-postgres-migration-series-archive-local-import-and-store-tests.md)は係の交代の間の振る舞いと、migrateを別に流すコマンドでの互換のmigrationの順を決めた。[ADR-t3985-1](2026-11-03-t3985-1-team-version-identity-by-oidc-human-roles-on-staticpolicy-runner-registration-per-person-inbox-and-no-delegated-landing-approval.md)は、制御側の起動・停止・migrateを配置する側が行い、secretを配置のsecretの置き場から渡すと決めた。この決定は、配るcontainerの起動から停止までの約束を決める。

今の設定は`dagq.toml`（repositoryの規則）と`host.toml`（hostの事情）に分かれ、serviceの起動・停止・migrateはhostの固定バイナリのコマンドが行う。

具体の候補は設計資料の[チームの版の制御側の具体案](../plans/team-version-control-side.md)が持ち、実装のtaskが定義のそばのdoc commentへ移す。

## Decision

[ADR-t1233-1](2026-10-02-t1233-1-control-and-execution-sides-queue-service-broker-and-client-mode.md)（とくに決定2の互換をAPIの版で判定すること、決定3のユースケース単位のAPIとtransactionをクライアントとの往復で分けないこと）、[ADR-t3102-3](2026-10-22-t3102-3-stage-5-outage-notice-and-use-cases-for-multi-step-control-operations.md)（決定3〜5のユースケースの単位とleaseと前提の確かめ、副作用の順）、[ADR-t3981-1](2026-11-02-t3981-1-team-version-control-side-on-containers-postgres-and-object-store-with-app-side-write-serialization.md)（とくに決定1・2）、ADR-t3985-1・ADR-t3986-1・ADR-t3987-1の決定は保つ。この決定はどれもamendsも置き換えもしない。ローカルの既定の配置（`host.toml`、固定バイナリ、`up`・`down`・`install`）は何も変えない。

1. **配置の設定は環境変数とファイルにだけ置き、`dagq.toml`にも`host.toml`にもDBにも置かない。** storeの種類とDBの接続の文字列、S3互換の置き場（宛て・bucket・資格）、OIDCの発行元とaudience、2つのGitHub App（ADR-t3987-1決定6）、チームのClaudeのAPI key、imageのdigestを受けるCIの識別の縛り（ADR-t3987-1決定4）、見直しの時間帯は配置ごとの設定である。値は環境変数で受け、secretは環境変数か、配置の側のsecretの置き場がmountしたファイルで受ける（ADR-t3985-1決定8）。S3互換の資格は、明示しなければ実行の環境の標準の資格の探し方に任せる。制御側は起動のときに要る設定を全て確かめ、欠けや矛盾があれば値を出さずに名前だけを出して起動しない（fail closed）。HTTPの入口は、OIDCの発行元が設定されていなければ開かない（認証の無いチームの入口を作らない）。repositoryの規則（関門の宣言・reviewなど）は今どおり`dagq.toml`が持ち、制御側はcheckoutを持たないので、mainの`dagq.toml`を、runner用のGitHub Appからそのrepository 1つの中身の読み取りだけに絞って自分のために発行したtokenで読む。main用のAppはこの読み取りに使わない（main用のAppの用途を限るADR-t3987-1決定6を保つ）。配置の設定と`dagq.toml`はどちらも相手の値を上書きしない。
2. **healthは「このプロセスが答えられるか」だけで、係かどうかとDBの届きに依らない。** health checkは認証の要らない入口で、プロセスが要求を受けて答えられ、内側のループが止まっていない（DBに届かず断りながら回っているのは止まっていない内に数える）ときに良いと答え、版の範囲のほかに何も出さない。係を引き継ぐのを待つプロセスも良いと答え（ADR-t3986-1決定3）、DBに届かない間も良いと答える（起動し直してもDBは戻らず、係のleaseを手放さずに落ちれば交代が延びる）。DBに届かない間の要求は、届かないと分かる答えで断る（ADR-t3102-3決定2の「届かない」を呼び出し元が見分けられる形）。停止を始めたプロセスは悪いと答え、TLSの終端に新しい要求を回させない。
3. **SIGTERMでは、新しい仕事を止め、走っている要求と始めた副作用の記録を終え、書き込みの列を空にしてから係を手放す。** 順は次のとおり。(1) healthを悪くし、supervisorのループは新しいclaim・jobの割り当て・着地を始めない（ADR-t3102-3決定5）。(2) 受けている要求に答え、始めた副作用（mainの更新・refの作成と削除・tokenの失効）の結果の記録を終える。(3) 書き込みの列を空にし、最後の書き込みで係を手放す（ADR-t3986-1決定3）。(4) 終わる。配置の側の猶予が先に尽きれば手放さずに終わり、次の係は期限を待って引き継ぐ（安全さは世代が守る。ADR-t3986-1決定2）。列を空にする前に手放さない。runnerのleaseは制御側の停止で手放さず失わせもしない: runnerのleaseはDBに残る。ADR-t3987-1決定2の「生存の報告が途絶えて期限を過ぎた」の期限は、制御側が報告を受けて書ける間（係が居てDBに届く間）だけで数え、係の交代とDBに届かない間の後は、受けられるようになった時を起点に数え直す。制御側の側の不在でrunを失わせないためで、喪失の条件そのものは変えない。
4. **migrateはHTTPの外の一度きりのコマンドにし、serviceは起動で自分ではmigrateしない。** 配置の側は、制御側と同じimageでmigrateのコマンドを一度だけ流す。そのコマンドは同じbuildのserviceのプロセスを、入口を開かずmigrateだけを行って終わる形で起動し、配置の設定のDBの資格で届く（DBを開いてmigrationを適用するのはserviceのプロセスだけ、という[ADR-t3102-2](2026-10-22-t3102-2-only-the-queue-service-opens-the-db-schema-by-the-service-build-and-migration-order.md)決定1・2を保つ）。HTTPのユースケースにも人のCLIの遠い呼び出しにもしない（遠くのtokenでschemaを変えさせない。ADR-t3985-1決定3の、制御側の起動・停止・migrateは配置する側が行うこと）。これはADR-t3102-2決定2の明示した時機に、配置する側が流すmigrateを1つ足すもので、ADR-t3986-1決定3が名指した形である。互換のmigrationは動いている係を止めずに適用する（ADR-t3986-1決定3の確かめ）。非互換のものは今と同じくdrainの後にだけ適用する: コマンドは係が手放されていることに加え、runnerのものを含めてleaseを持つrunとjobが無いことを、適用と同じtransactionで確かめ、どれかが残れば何も適用せずに断る（ADR-t3102-2決定2）。チームの版のserviceは、DBのschemaが自分のbuildの要る版より古ければmigrateせずに起動を断り、新しすぎれば今のschemaの下限の規則（ADR-t3102-2）で開けるかを判定する。順は、互換のときはmigrateを流してから制御側のimageを替え、その後にrunnerを替える。非互換のときは、新しい割り当てを止めて走っているrunとjobを終わらせ、制御側を止めてからmigrateを流し、新しいimageの制御側を起動する。

## Alternatives

- **`dagq.toml`に配置の設定を置く（環境変数の差し込みを含む）**: repositoryの全員とrunのagentが読む文書に接続先とsecretの在り方が入り、配置ごとに違う値がcommitに混ざる（ADR-t3981-1決定3）。
- **配置の設定を1つの設定のファイル（書式を持つもの）にする**: 書式の版の互換をdagqが持ち、containerの基盤が値を差し込む普通の形（環境変数とsecretのmount）からmountの手間が1つ増える。値は環境変数にし、ファイルはsecretとその置き場がmountする物だけにする（決定1）。
- **設定をDBに置く**: DBの接続先をDBから読めず、secretがDBのbackupの経路に乗る（ADR-t3985-1決定8）。
- **特定のクラウドのsecretの置き場のSDKから直に読む**: クラウドに縛られる。配置の側が環境変数かファイルに出す（決定1）。
- **healthに係であることやDBの届きを含める**: 係を待つプロセスが入れ替えで起動できず（ADR-t3986-1決定3）、DBの障害で制御側が起動し直し続けて係を手放さずに落ちる。プロセスが答えられるかだけにする（決定2）。
- **SIGTERMで直ちに係を手放す**: 列に残った書き込みが新しい係の書き込みと並ばずに失われうる。列を空にしてから手放す（決定3）。
- **制御側の停止の間にrunnerのleaseの期限を数え続ける**: 制御側の入れ替えのたびに走っているrunを失わせる。居なかった間を数えない（決定3）。
- **serviceが起動のたびに自分でmigrateする**: 入れ替えで並んだ複数のtaskが同時にschemaを変えに行き、非互換のmigrationを係が動いている間に流しうる。別のコマンドにする（決定4）。
- **非互換のmigrationを係の手放しだけを条件に適用する**: runnerのrunが走ったまま非互換のschemaになる。drainをtransactionの中で確かめる（決定4）。
- **migrateをHTTPのユースケースにする**: 遠くの人のtokenでschemaを変えられる。配置の側のDBの資格に限る（決定4）。

## Consequences

- 制御側の起動は配置の設定が全て揃うまで失敗するので、配置の誤りは起動の時に名前で分かる。OIDCの発行元の無い配置ではHTTPの入口が開かない。
- DBの障害の間もhealthは良いので、配置の側はhealthとは別にDBの監視を持つ（ADR-t3990-5決定1の文書に書く）。
- 制御側の入れ替えの間、runnerは報告を拒まれるか届かず、走っているturnは続けて次の報告で送り直す。runは失われない。
- 今の姿を書く`docs/design/`は、実装のtaskが着地と同じ変更で直す（この決定はまだ実装されていない）。

---
id: adr-t3986-1
type: adr
title: queue serviceの書き込みを1つの係の列に並べ、書き込みのtransactionの最初のリーダーの行の条件付きの更新で係と世代を確かめ、交代の間の書き込みは拒み、triggerはSQLiteに残してPostgresでは索引かアプリへ移し、Postgresのmigrationは基点から名前でSQLiteの系列に対応させ、古いeventと台帳は閉じた物だけを置き場へ移し、ローカルからは止めて一度に移し、storeのcaseは1つの組を両方のadapterで流す
status: accepted
created: 2026-11-03
updated: 2026-11-03
accepted_on: 2026-11-03
owners:
  - hisamekms
tags:
  - architecture
  - persistence
  - queue-service
  - migrations
related:
  - adr-t3981-1
  - adr-t3982-1
  - adr-t3102-2
  - adr-t3102-3
  - adr-0067
  - adr-0007
  - adr-t2895-1
  - adr-t1925-1
  - adr-t3802-1
  - adr-0078
  - adr-t1410-1
  - adr-t1662-2
  - adr-t614-2
  - adr-t876-1
  - adr-t3985-1
  - design-authorization
  - design-persistence
  - design-measurement
  - plan-team-version-sqlite-dialect
  - plan-team-version-write-leader
---

# ADR-t3986-1: 書き込みの係とPostgreSQLのstore（係とfencing・triggerの移し先・migrationの系列・データの伸び・ローカルからの移し方・testの分担）

## Context

[ADR-t3981-1](2026-11-02-t3981-1-team-version-control-side-on-containers-postgres-and-object-store-with-app-side-write-serialization.md)は、書き込みの直列化をqueue serviceの係とleaseとfencingで守りSQLiteとPostgresで同じ仕組みにすること（決定4）、ローカルのSQLiteを既定に残しPostgresをadapterとして足すこと（決定3）を決め、leaseの期限と引き継ぎ・Postgresのschemaとmigration・古いeventと台帳の移し方・ローカルからの移し方を後のADRに残した。[ADR-t3982-1](2026-11-02-t3982-1-store-adapter-shares-ports-row-mapping-and-standard-sql-and-splits-dialect-by-functions-and-per-adapter-statements.md)はstoreのadapterの形を決め、triggerの移し先とPostgresのmigrationの系列を後のADRに残した（決定7）。

今はqueue serviceが要求ごとに接続を開き、SQLiteの`BEGIN IMMEDIATE`のlockが書き込みを並べ、制御側のプロセスが1台のhostに1つであることはserviceのlockのファイルが守る（[Queue service](../design/queue-service.md)の「置き場所と形」）。チームの版では制御側のcontainerのtaskが入れ替わるとき（配置の更新・落ちたときの起動し直し）に2つのプロセスが同じDBに届きうるので、ファイルのlockもSQLiteのlockも係を1つにできない。runのleaseは自分のtokenの行を書き込みの最初に更新し0行なら何も書かない形で、heartbeatの古さを拒む理由にしない（[ADR-0007](0007-run-level-leases-parallel-execution.md)、[SQLite persistence](../design/persistence.md)の「Runtime ownership」）。triggerの数と分類は[棚卸し](../plans/team-version-sqlite-dialect.md)の「CREATE TRIGGERの内訳」が持つ。本番のqueue.dbの大きさの内訳はADR-t3981-1のContextにあり、大半は台帳の行とrunのeventで、台帳はSSOTから作り直せるビューである（[計測](../design/measurement.md)の「SSOTとビュー」）。表の列・確かめのSQLの形・値の案は[計画](../plans/team-version-write-leader.md)が持つ。

## Decision

ADR-t3981-1の決定3・4、ADR-t3982-1のadapterの形（共有のSQL・方言の関数・adapterごとの文、書き込みの名前付きの入口でleaseを確かめること）、[ADR-t3102-2](2026-10-22-t3102-2-only-the-queue-service-opens-the-db-schema-by-the-service-build-and-migration-order.md)の決定1・2（DBを開くのはqueue serviceだけ、schemaはserviceのbuildが上げる）を保つ。このADRはどのADRもamendsも置き換えもしない。

1. **書き込みの係の列。** queue serviceのプロセスは書き込みのtransactionを1つの列に並べ、1つずつ確定させる。読み取りは列に並ばず、読み取りの接続で並行に答える。列に並べるのはプロセスの中の順で、プロセスをまたぐ排他は決定2が守る。
2. **係が1つであることの不変条件。** DBにリーダーの行を1つ置き、係の識別子・世代の番号・leaseの期限を持たせる。全部の書き込みのtransactionは最初の文で、係の識別子と世代の番号を条件にリーダーの行を更新し、更新が0行ならそのtransactionで何も書かずに拒む。係の書き込みの条件は自分の識別子と世代で、同じ更新でleaseを延ばす。係でないプロセスが書く唯一の場合である互換のmigrationの条件は、読んだ今の係の識別子と世代である（決定3）。係になる（引き継ぐ）のはleaseが切れたか手放された行を条件付きの更新で取るときだけで、そのとき世代の番号を1つ進める。安全さは世代の番号だけで守り、時計に頼らない: 古い世代の係の書き込みは、時計がどうずれても最初の更新で拒まれる。leaseの期限は引き継いでよい時を決めるだけで、自分のleaseが過ぎても誰も引き継いでいなければ係の書き込みは通る（runのleaseと同じ）。全部の書き込みが同じ行を最初に更新するので、Postgresでもプロセスをまたぐ書き込みはその行で並ぶ。advisory lockもDBの他のlockの機能も使わず、SQLiteとPostgresで同じ確かめを共有のSQLで行う。DBのファイルがserviceのlockのファイルと同じqueueのディレクトリにあるSQLiteのstoreに限り、そのlockを持つことが前の係のプロセスが居ないことの証になるので、期限を待たず引き継いでよい（確かめと世代は同じ）。
3. **交代の間の書き込みは拒む。** 係でないプロセス（引き継ぐ前、手放した後、拒まれて係でなくなった後）に来た書き込みは、待たせずに「交代中」と分かる答えで拒み、何も書かない。送り直すかは呼び出し元が決め、書き込みのユースケースは前提を同じtransactionで確かめ直すので（[ADR-t3102-3](2026-10-22-t3102-3-stage-5-outage-notice-and-use-cases-for-multi-step-control-operations.md)決定4）送り直しても二重にならない。読み取りは係を要さず、交代の間も答える。引き継ぎを待つプロセスはhealthを良いと答え（入れ替えで起動できるように）、拒まれて係でなくなったプロセスは終わって入れ替えに任せる。SIGTERMなど予定した停止では、係は列を空にしてから行を手放し、次の係は期限を待たずに引き継ぐ。参考の構成の入れ替えは、古いtaskを止めてから新しいtaskを起動する形にし、2つが重なる間の書き込みの拒みを無くす（重なっても安全さは決定2が守る）。
   migrationとの順: 互換のmigrationは、新しいbuildのserviceのプロセスが明示した時機に適用し（ADR-t3102-2決定2。チームの版ではADR-t3981-1決定2のmigrateを別に流すコマンド）、互換の宣言のとおり動いている前のbuildの係を止めずに済むので、係を引き継がずに適用する（ローカルでは適用した新しいbuildのserviceがそのまま係を引き継ぐ）。それでも決定2の「全部の書き込み」の例外にはしない: migrationのtransactionも最初の文でリーダーの行を条件付きで更新し、条件は適用の前に読んだ今の係の識別子と世代の番号にする（係の識別子もleaseも変えない）。この更新がリーダーの行を鎖すので、係の書き込みとmigrationは同じ行で並び、読んだ後に引き継ぎがあって世代が変わっていれば0行になって何も適用しない（古い世代を前提にしたmigrationは拒まれる）。行を鎖した後に同じtransactionでschemaの版を読み直し、もう適用されていれば何もしないので、重なったmigrateも1つだけが通る。確かめの文は係の書き込みと同じ共有のSQLで、SQLiteとPostgresで同じである。非互換のmigrationは今と同じくdrainの後にだけ適用し、適用するプロセスは係を引き継いで世代を進めてから適用し、終われば手放す。これで非互換の適用の間に前の係が書くことも、新しい係が立つことも無い。
4. **leaseの期限と交代の時間の決め方。** 期限は、落ちたときに書き込みが止まる時間の上限（期限と、引き継ぐ側が試す間隔の和）と、延ばしの書き込みの頻度・一時の遅れ（DBのfailover・containerの一時停止）で係を失う回数のつり合いで決める。期限は延ばす間隔の数倍に置き、延ばしが数回続けて遅れても係を失わないようにする。予定した交代は手放しで済むので期限に依らない。期限の判定は引き継ぐ文がDBの時計で行い、制御側のtaskの時計のずれが引き継ぎの早さを変えないようにする。値はコードの定義のそばに置く。
5. **条件付きの一意の索引で二重に守る不変条件。** 係の列があっても、次はDBの一意の索引でも守り、SQLiteとPostgresの両方の系列に同じ意味で置く: queue全体で`integrating`のrunは高々1つ、taskごとに未完了のrunは1つ・`integrated`のrunは1つ、走っているplan reviewは高々1つ、同じ対象と種類の開いたaskは1つ、同じ対象の片付いていないfindingは1つ、リーダーの行は1つ。書き込みのtransactionが守る他の規則（開いた再計画の操作は高々1つなど）は索引にしない。
6. **triggerの移し先。** SQLiteのtriggerはローカルの動きを変えないよう今のまま残し、Postgresの系列にはtriggerを1つも置かない。検索の索引を保つtrigger（task・goal・note・着地のcommitを1つの索引に写し、taskの状態とgoalも写して持つ）は、Postgresでは検索のportの書き込みに移し、元の行を書くtransactionの中で索引の行を書く。索引の行にどの索引の型を張るかは日本語の検索のSpikeの結論に従い、元の表の列に張る索引だけで足りると分かった種類は索引の行を書くことをやめてよい。着地したrunのeventから着地のcommitの行を作るtriggerは、索引でなく記録を作るので、Postgresではrunの着地を書くadapterの書き込みが同じtransactionで作る。記録の変更を禁じるtriggerは、Postgresではportが追記の操作しか持たないことと、それを確かめるtestに移す（DBの権限で更に絞るのは配置する側の選択として文書に書く）。受け入れ条件の版を進めるtriggerは、Postgresのadapterの書き込みが同じtransactionで版を進める文に移す。testが失敗を注入するために作る一時のtriggerはSQLiteのadapterのtestに残し、Postgresでは作らない。移した振る舞いは決定10の共通のtestが両方のadapterで確かめる。
7. **Postgresのmigrationの系列。** Postgresの系列は、ある版のSQLiteのschemaと同じ意味のschemaを一度に作る基点から始め、基点の後はSQLiteのmigrationの1本ごとに同じ名前の部分を持つPostgresのmigrationを1本置く（Postgresで要らない変更も、要らないと書いた1本を置く）。schemaの版・互換の宣言・下限はSQLiteの側の1本が持ち、両方のstoreで同じ版の数が同じschemaの意味になる。対応は番号でなく名前で取るので、着地での番号の振り直し（[ADR-0067](0067-migrations-are-listed-by-build-and-renumbered-on-landing.md)決定3）はSQLiteのファイルだけを動かし、Postgresの側を書き換えない。Postgresの系列はSQLiteの`migrations/`と別の置き場に置き、ADR-0067の名前と番号の規則の外にする。対応の欠けと名前の重なりはbuildで止める。基点より前のSQLiteのmigrationをPostgresに書き直さない。
8. **データの伸びと保存期間。** チームの版では、閉じたtaskのrunのeventと、確定した台帳の行のうち、境目より古いものをS3互換の置き場へ移し、queue DBから消す。eventはSSOTのままで（[ADR-t1662-2](2026-10-04-t1662-2-measurement-stores-ssot-and-views.md)決定1）、境目より古いeventのSSOTの置き場が置き場の写しになるだけなので、dagqは写しを消さず、参考の構成も写しに期限を付けない（期限を付けるかは配置する側が正本を失うと知って選ぶ）。台帳はビューのままで、境目より古い行を作り直すときは置き場のeventから作る（ADR-t1662-2決定7・8を保ち、旧方式の行も写しに残る）。移さないもの: 開いたtask・goal・run・askに関わる行、確定していない台帳の行、状態を作る読み取り（属性の畳み込み・所属の判断・着地のcommitの記録など、eventを最初から読むもの）が読むkindのevent（どのkindが当たるかはコードの定義が持つ）。境目は、制御側の判断と表示のSQLが読む一番長い窓より後に置く。閉じたtaskが開き直ったときは、そのtaskの移したeventをDBに読み戻し、写しは消さない。eventのIDは保つので、置き場の写しとDBの答えを合わせる集計はIDで重なりを除く。移すのは係の書き込みで、置き場に置いて確かめてからDBの行を消す（逆の順にしない。置いて消せなかった写しは残ってよく、移し直しは同じ区切りの写しを置き直す）。写しは列を持つ読み取り向けの形で区切りごとに置く。集計（DuckDBなど）は置き場の写しを読む側で直接読み、queue serviceは集計の処理を持たない。ローカルの既定は移さない。
9. **ローカルのqueueから移す手順。** 移すのは止めて一度に移す形にする: ローカルの`down`でsupervisorを止めてrunを終わらせ（drain。`down`はsupervisor・broker・serviceの順に止める）、supervisorとbrokerを止めたままローカルのserviceだけを起動し直し（既存のDBを開くのはserviceだけ、ADR-t3102-2決定1）、そのserviceがexportのユースケースで行を書き出してからserviceを止め、チームの版では空のPostgresのdatabase（表を1つも持たないものをADR-t3102-2決定1の「DBの無い所」に数える）に`init`の例外で最新のschemaを作り、exportしたローカルのqueueの版がそれと同じであることをimportの前提にして（違えばローカルを先にmigrateする）、そのserviceがimportのユースケースで係として行を入れ（認可は決定11）、表ごとの数と中身の照合が合ってからチームの版を始める。IDは保つ。移すものと移さないものの境は次のとおりで、照合も移す表だけで行う。(i) 移すのはqueueの記録（状態の表・event・計測のSSOTの表とビュー）である。(ii) 移し先が自分で持ち、移し元の行で書き換えないもの: リーダーの行、schemaの版と下限、DBの種類の印、importの記録、IDの次の値（移した行から合わせる）、検索の索引（移し先の検索のportが移した行から書く）。importはリーダーの行を確かめに使うだけで書き換えないので、移し先の係と世代（決定2）は移行の間も保たれる。(iii) 移さないhostの実行の状態とhostへの束縛: supervisorの登録・runのlease・runのプロセス・hostのworkspaceと、queueとローカルのrepositoryの束縛・ローカルのbinaryの入れ替えの記録。exportは未完了のrun・持ち主のいるlease・生きている登録が残っていれば拒む。決定8の境目より古い行はimportでDBに入れず置き場へ直接置く。ローカルのqueueは消さず、照合が合わなければローカルに戻る。runのディレクトリのファイルを移すかは、runnerの仕組みを決める後のADRが決める（ADR-t3982-1決定6）。止める時間を測るSpikeは要るとし、importの実装の後に本番の形の行数で作ったデータで測り、一度に移す形で止まる時間が受け入れられないと分かったときだけ別の形（差分の追いかけ）を決め直す。
10. **testの分担。** storeの約束を確かめるcaseは、adapterを受ける1つの組としてlibrary（既定で入らないcargoのfeatureの後ろ）に書き、同じ組をSQLiteとPostgresのadapterで流す。SQLiteでは今のSQLiteのadapterのtest（[ADR-t2895-1](2026-11-01-t2895-1-sqlite-adapter-tests-in-infrastructure-open-sqlite-in-place.md)）から組を呼び、着地の検証とCIで流れる。Postgresではtestのbinaryにせず、組を流して結果で終わるCIだけのプログラムにし、CIのPostgresのjobがPostgresの接続先を渡して流す。接続先が無ければ流さずに落ち（黙って飛ばさない）、着地の検証にも、全部のtestのbinaryを流すCIのcoverageの関門にも入らない（[ADR-t1925-1](2026-10-07-t1925-1-landing-verifies-unit-tests-and-selected-integration-tests-and-ci-is-the-final-gate.md)のCIを最終関門にする形）。組に置けるのはstoreの約束のcaseと、Postgresに固有の境界（migrationの系列・型の写し）のcaseだけで、判断のcaseはdomainとapplicationのunit testに置く。方言の関数が返すSQLは両方の方言の分をDBなしのunit testで確かめ、着地の検証で流れる。古い世代の係の書き込みが拒まれること（決定2）は組のcaseにする。test binaryを`it`・`e2e`・`plugin`に限ること（[ADR-0078](0078-one-integration-test-binary.md)決定1・2）、unit testが外部プロセスを使わないことと境界の種類（[ADR-t1410-1](2026-10-03-t1410-1-decisions-in-unit-tests-boundaries-in-integration-tests.md)）、ITの登録簿と本数のratchet（[ADR-t3802-1](2026-10-31-t3802-1-integration-test-boundaries-in-a-named-registry-with-a-module-ratchet.md)）は変えず、組は`tests/it`の本数を増やさない。
11. **exportとimportの認可。** 全部の表の行を書き出すexportと、全部の表の行を入れるimportは、完了したtask・run・所属の判断のように普段のユースケースでは変えられない行まで書けるので、`queue.admin`にも他の既存のcapabilityにも含めず、この2つだけの専用のcapabilityにする。持てるのはactorがuser（人）のprincipalだけで、planner・inbox・supervisor・worker・job・runnerには与えない。チームの版ではその中でも管理者の役の人に絞る（[ADR-t3985-1](2026-11-03-t3985-1-team-version-identity-by-oidc-human-roles-on-staticpolicy-runner-registration-per-person-inbox-and-no-delegated-landing-approval.md)決定3の、役割でuserの許可を狭める形）。importが通るのは初回の移行だけで、importのtransactionは最初の書き込みの前に、移し先のDBのqueueの表がどれも空で、まだimportの記録が無いことを同じtransactionで確かめ、違えば何も書かずに拒む。importを終えた記録を残し、2回目のimportを拒む。

日本語の検索の方式はSpikeのtaskの結論に任せ、このADRでは決めない。

## Alternatives

- **advisory lockかDBのlockの機能で係を1つにする**（決定2）: SQLiteとPostgresで守り方が分かれ、ADR-t3981-1決定4が退けた。
- **書き込みごとにleaseの期限も確かめ、過ぎた係の書き込みを拒む**（決定2）: 時計と一時の遅れに安全さを結び、誰も引き継いでいないのに書き込みが止まる。世代の番号で安全さは足りる。
- **交代の間の書き込みを新しい係が立つまで待たせる**（決定3）: 落ちたときは期限まで接続を握り、呼び出し元の時間切れと見分けにくい。拒めば何も書かれず、送り直すかを呼び出し元が選べる。
- **制御側のtaskの時計で期限を判定する**（決定4）: containerの間の時計のずれで、生きている係を早く引き継いで世代を進める回数が増える。
- **PostgresにもPL/pgSQLのtriggerを書く**（決定6）: 規則がSQLiteとPostgresの2つのDBの言葉に分かれ、アプリのtestから見えにくく、DBに原子性と耐久性だけを求める方針（ADR-t3981-1決定4）から外れる。
- **Postgresでは元の表の列に索引を張るだけで検索の索引の行を持たない**（決定6）: 今の索引はtaskの状態やgoalを別の表から写して持ち、1つの索引で種類をまたいで探すので、元の表の索引だけでは同じ検索にならない。Spikeが種類ごとに足りると示した所だけそうする。
- **互換のmigrationだけは係の行を確かめない例外にし、schemaの版の行の条件付きの更新で並べる**（決定3）: migrateどうしは並ぶが、係の書き込みとは別の行なので並ばず、古い世代の拒みも保てず、ADR-t3981-1決定4とADR-t3982-1決定2の「全部の書き込みのtransactionの最初で確かめる」を崩す。
- **migrationを適用するプロセスがいつも係を引き継ぐ**（決定3）: 生きている係からは引き継げず、互換のmigrationの後に新しいbuildのserviceが起動する入れ替えの順と行き詰まる。互換のmigrationは前のbuildの係が動き続けられる変更に限られている。
- **SQLiteのtriggerもアプリへ移す**（決定6）: ローカルの動きとschemaを変え、互換のmigrationの後の前のbuildへのrollbackでtriggerを頼る書き込みが壊れる。
- **Postgresの系列を番号で対応させるか、1本のmigrationに両方の方言を入れる**（決定7）: 着地の番号の振り直しが2つのファイルを動かすか、ADR-0067の名前の規則を変えることになり、ADR-0067のamendsが要る。
- **Postgresの系列を独立の版で持つ**（決定7）: 互換の宣言と下限・API versionの判定がstoreごとに分かれる。
- **SQLiteの全部のmigrationをPostgresに書き直す**（決定7）: 表の作り直しなどSQLiteの事情の文を移すだけになる。
- **eventと台帳をDBに置いたまま、partitionで古い区切りを外す**（決定8）: Postgresに固有で、SQLiteと読み方が分かれ、DBの大きさも減らない。
- **queue serviceに集計の処理を入れる**（決定8）: 書き込みの係と同じプロセスに重い読み取りを載せる。
- **止めずに差分を追いかけて移す**（決定9）: 両方のstoreの変更を追う仕組みが要る。まず一度に移し、止まる時間を測ってから要否を決める。
- **importを`queue.admin`で認める**（決定11）: `queue.admin`はplannerとsupervisorも持つので、plannerがimportを通して完了したtaskや所属の判断を書き換えられる。既存の役割の権限を広げないよう専用のcapabilityにし、人と初回と空のDBに限る。
- **Postgresの組を`tests/it`に置く**（決定10）: hostにPostgresが無いので着地とcoverageでは飛ばすことになり、Postgresは今の境界の種類に無く、ITの本数と登録簿を増やす。
- **Postgresの組を4本目のtest binaryか、libのtestにする**（決定10）: test binaryを3本に限るADR-0078か、unit testに外部プロセスを使わせないADR-t1410-1・t2895-1の例外を変えることになり、全部のtest binaryを流すcoverageの関門が接続先の無い所で落ちる。

## Consequences

- 書き込みのtransactionごとにリーダーの行の更新が1つ増える（ADR-t3981-1のConsequences）。全部の書き込みが同じ行で並ぶので、Postgresでも書き込みの並行は増えない（チームの版の書き込みの量で足りるかは測る）。
- 制御側のtaskが落ちたときは期限と試す間隔の分だけ書き込みが止まり、その間の書き込みは拒まれる。予定した入れ替えでは手放しの分だけ短い。
- SQLiteとPostgresで同じ振る舞いをtriggerとアプリの別々の書き方で持つ所があり、ずれは決定10の共通のtestがCIのPostgresのjobで見つける。着地の検証では見つからず、CIの後に直す。
- migrationを足すtaskは、基点の後はPostgresの側の1本も足す。
- 境目より古いrunの`show`と、開き直したtaskは置き場の写しを読む経路が要る。読み戻しの経路の形は実装の段が決める。
- Postgresの系列にもリリースしたmigrationを変えない規則（[ADR-t614-2](2026-09-27-t614-2-released-migrations-are-immutable.md)）とCHECK制約を使わない規則（[ADR-t876-1](2026-09-28-t876-1-no-sqlite-check-constraints-until-schema-is-stable.md)）を当て、今の検査の範囲をPostgresの置き場へ広げる。名前の重なりをbuildで止めるのはADR-0067の規則に足すもので、ADR-0067の決定は変えない。
- 決めていないもの: 日本語の検索の方式（Spike）、runのディレクトリのファイルの移し方（runnerのADR）、backupと復元。

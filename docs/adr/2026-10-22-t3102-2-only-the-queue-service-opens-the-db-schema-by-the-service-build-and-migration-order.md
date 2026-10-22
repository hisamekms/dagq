---
id: adr-t3102-2
type: adr
title: 段(5)の後にqueue DBを開くのはqueue serviceのプロセスと、DBの無い所に作る`init`だけにし、migrateとその前の複製をserviceのユースケースにしてschemaをserviceのbuildが決め、本番のserviceを動かすbinaryをqueueごとに決めて別のbinaryの起動と入れ替えを拒み、互換は全てのクライアント（claimのときに写したrunnerと開発中のbinaryの読み取りを含む）でAPI versionで判定し、移行は読み取り・wrapper・人とinboxとplannerの書き込み・supervisorとintegrate・例外の閉じ込めの順に1段ずつ着地させる（ADR-t1233-1決定2・5・8、ADR-0073決定5・14・15・17・18をamends）
status: accepted
created: 2026-10-22
updated: 2026-10-22
accepted_on: 2026-10-22
amends:
  - adr-t1233-1 decision 2
  - adr-t1233-1 decision 5
  - adr-t1233-1 decision 8
  - adr-0073 decision 5
  - adr-0073 decision 14
  - adr-0073 decision 15
  - adr-0073 decision 17
  - adr-0073 decision 18
owners:
  - hisamekms
tags:
  - runtime
  - security
  - architecture
  - migrations
related:
  - adr-t1233-1
  - adr-t1233-4
  - adr-t1233-5
  - adr-t3102-1
  - adr-t3102-3
  - adr-0045
  - adr-0073
  - adr-t728-1
  - adr-t2391-1
  - design-queue-service
  - design-persistence
---

# ADR-t3102-2: DBを開くプロセスの例外、schemaを上げる経路、API versionの範囲、移行の順

## Context

[ADR-t1233-1](2026-10-02-t1233-1-control-and-execution-sides-queue-service-broker-and-client-mode.md)決定2は、最終の姿でqueue serviceをqueue DBを開く唯一のプロセスにし、互換をschemaではなくAPI versionで判定して、開発中のbinaryがqueueを開いてschemaを上げる問題を構造的に無くすとした。今はsupervisor・session wrapperとhook・人とinboxとplannerのCLIもDBを開き、`init`と`dagq migrate`がmigrationを適用し、状態を変えないコマンドは古いqueueをメモリの複製でmigrateして読む（[SQLite persistence](../design/persistence.md)の「Database setup and migrations」）。claimのときに写したrunnerのbinaryは、互換のmigrationの後もDBを開いて動き続ける。

goal 182の決定の分け方は[ADR-t3102-1](2026-10-22-t3102-1-stage-5-principals-user-by-socket-peer-and-control-side-tokens.md)のContext。goal 182の他のtaskが「ADR-t3102-2」と名指すものの、DBを開く例外・schemaの経路・API versionの範囲・移行の順と並存の規則はこのADRに、serviceが落ちたときの知らせとユースケースの単位とtransactionの規則は[ADR-t3102-3](2026-10-22-t3102-3-stage-5-outage-notice-and-use-cases-for-multi-step-control-operations.md)に置く。

## Decision

1. **段(5)の後にDBを開くプロセスはserviceと`init`だけ。** queue serviceのプロセス（`service serve`）が既存のqueueのDBを開く唯一のプロセスになる。例外は、DBの無い所に最新のschemaのDBを作る`init`だけで、既存のqueueへの`init`はDBを開かず、要る処理をserviceのユースケースに送る。migrate・非互換のmigrateの前の複製・runの復旧（`recover`）・`doctor`のDBの診断はserviceのユースケースにし、CLIのプロセスはDBを開かない。testがライブラリとしてDBを開くのはプロセスの経路ではないので、この例外の数に入れない。serviceに届かないときに、手でDBを開いて直す経路は作らない（直すのはserviceを起動し直すこと）。
2. **schemaはserviceのbuildが決める。** migrationを適用するのはserviceのプロセスだけで、自分のbuildの知るmigrationを明示した時機にだけ適用する: queueの管理の権限を持つprincipalのmigrateの依頼と、`install`・自動更新・`up`の入れ替えが新しいbuildのserviceを起動するとき（そのserviceは受け付けを始める前に互換のmigrationを適用する）。開いただけと、supervisorの見張りの起動し直しでは適用しない（[ADR-0073](0073-kind-additions-are-compatible.md)決定5の「開いただけではmigrateしない」を保つ）。入れ替えの手順の順（確かめる・migrate・差し替え・引き継ぎ）のうち、migrateは新しいbuildのserviceの起動の中に移る。非互換のmigrationを使用中のsupervisor・run・wrapperがあるあいだ適用しない規則は、serviceがその判定と適用を1つのtransactionの中で行う形で保つ。
3. **本番のserviceを動かすbinaryはqueueごとに決め、別のbinaryの起動と入れ替えを拒む。** queueは自分のserviceを動かしてよいbinaryを1つ持ち、それを変えるのは`install`（と自動更新の引き継ぎ）だけにする。`up`・`service start`・`service serve`・supervisorの見張りは、そのbinaryでなければserviceを起動も入れ替えもしない（[ADR-t1233-4](2026-10-02-t1233-4-queue-service-lifecycle-outage-notice-and-principal-tokens.md)決定1の「開発中のbinaryは本番のserviceを起動しない」をこの検査で守る）。使い捨てのqueueは、それを作ったbinaryがそのqueueのbinaryになる。開発中のbinaryにはDBを開く経路も（決定1）、本番のserviceになる経路も無いので、本番のschemaを上げられない。
4. **互換は全てのクライアントでAPI versionで判定する。** serviceの呼び出し元の全て、つまりworker・job・observer、claimのときに写したrunnerのbinaryで動くwrapperとhook、supervisorとintegrator、人とinboxとplannerのCLI、本番を読む開発中のbinaryは、要求のAPI versionがserviceの幅に入るときだけ答えを受け、外れれば断られ、DBを開くことに戻らない。状態を変えないコマンドのメモリの複製でのmigrate（ADR-0073決定18）は、DBを開かなくなるので無くなる。serviceの受ける下限を上げるのは非互換のmigrationと同じ扱いにし、下限より古いversionのrunnerやsupervisorが動いているあいだは上げない（drainの後の入れ替えで上げる）。
5. **人のCLIは環境が宛てを名指さなくてもserviceへ送る。** 段(5)の後、宛ての環境もtokenも無いCLIは、queueの解決（cwdや`--db`）からそのqueueのserviceのsocketを求めてuserとして送り（ADR-t3102-1決定1）、DBを開かない。宛ての環境があればその宛てに送ることと、両方があれば断ることは変えない。
6. **移行の順。** 段(5)は次の順で、どこで止めても動く1段ずつを着地させる。
   1. principalと発行のユースケース（ADR-t3102-1決定1〜3）と、ADR-t3102-3決定1の落ちたときの知らせの新しい形。まだどの経路も外さない
   2. 人・inbox・plannerと本番を読む開発中のbinaryの読み取り（`status`と`watch`を含む）のservice経由化。宛ての無いCLIの読み取りの送り先（決定5）と、届かないことを返す形（ADR-t3102-3決定2）を同じ段で入れる
   3. runとplannerのsession wrapperとhookのservice経由化と、workerとplannerのpolicyからの`session.run`と`session.record`の除去（ADR-t3102-1決定4）
   4. 人・inbox・plannerの書き込みのservice経由化（宛ての無いCLIの書き込みも決定5の送り先へ）
   5. supervisorとintegrateのservice経由化（ADR-t3102-3決定3・4）
   6. migrate・複製・復旧・診断のserviceへの移し替え（決定1・2）、serviceを動かすbinaryの検査（決定3）、DBを直接開く経路の除去
7. **並存の期間の規則。** 移行の途中は、DBを直接開くプロセスとserviceが今のSQLiteのtransactionとleaseの規則で並ぶ（ADR-t1233-1のConsequences）。各段で次を守る。
   - service経由にしたプロセスは、どの段でも、届かないときにDBを直接開くことへ戻らない（fail closed）。1つのコマンドの中で経路を混ぜない
   - 固定binaryの入れ替え（`install`・自動更新・rollback）を壊さない。入れ替えの前のbinaryのsupervisorとrunnerはDBを直接開いたまま動き終えられ、そのためにその段の着地に非互換のmigrationを含めず、serviceは前のbinaryのクライアントのAPI versionを受け続ける
   - hostのrunの今の振る舞い（claim・resume・review・着地）を段の前後で変えない
   - 段6の後の`install --rollback`で戻した前のbuildのserviceは、互換のmigrationの後のschemaを下限の規則で開き、非互換のmigrationの後のrollbackは今と同じく拒む（ADR-0073決定14）
8. **DBを直接開く経路が残らないことの検査はRustのbuildに置く。** DBを開く型を作れるのはserviceの中と`init`の入口だけになるよう、crateかmoduleの境界と型（作れる場所を限ったconstructor）とclippyの禁止で強制し、shellのscriptで探さない。段6の着地はこの検査を通る形で行い、それまでの段では残りの経路を検査が例外として挙げる。
9. **ADR-t1233-1決定2・5・8を変える範囲。** 決定2の「唯一のプロセス」に決定1の`init`の例外を足し、互換をAPI versionで判定する範囲を決定4の全てのクライアントと決める。決定5の「段(5)までCLIはDBを直接開く」の後の姿を決定5（宛ての無いCLIもserviceへ）と決める。決定8の段(5)の中身を決定6の順に分ける。ADR-0073決定5・14・15・17のmigrateする主体をserviceのプロセスに（決定2）、決定18の開発中のbinaryの読み取りをAPI versionの判定に（決定4）改め、明示した時機にだけmigrateすること・互換の宣言と下限・非互換のdrainと退避は変えない。serviceが唯一の窓口であること、ユースケース単位のAPI、service側の認可は変えない。

ユースケースの名前、queueがserviceのbinaryを持つ欄と置き場、APIの版の値、検査に使うlintとmoduleの名前は実装のtaskが[docs/design/](../design/)（[Queue service](../design/queue-service.md)・[SQLite persistence](../design/persistence.md)）に書く。

## Alternatives

- **`migrate`をCLIのコマンドのままにし、DBを直接開く例外にする**: 開発中のbinaryが`migrate`を打てば本番のschemaが上がり、決定2が無くそうとした問題が残る。
- **serviceが起動のときに自分でmigrateする**: 開いただけでmigrateしない規則（ADR-0045）が崩れ、非互換のmigrationの使用中の検査と`install`の確認の手順を通らない。
- **開発中のbinaryの検出をbuildの識別子だけで行う**: 開発中のbinaryも正しい識別子を持ちうる。どのbinaryが動かしてよいかをqueueが持つ方が、使い捨てのqueueの扱いと揃う。
- **運用の規則（AGENTS.mdの固定binaryの規則）だけに任せる**: 人とAIの誤りを止められず、「構造的に無くす」（決定2）にならない。
- **読み取りのコマンドはDBを読み取り専用で開き続ける**: schemaの互換の問題が読み取りに残り、古いqueueのメモリの複製でのmigrateも残る。
- **一度に全部を切り替える**: 入れ替えの途中で前のbinaryのrunnerが動き続けるので、段ごとに並存させないとhostのrunと固定binaryの入れ替えが壊れる。
- **検査をshellのscriptで文字列から探す**: 境界の規則はclippy・型・crateの分割に置く方針（[ADR-t2391-1](2026-10-12-t2391-1-boundary-rules-to-compiler-clippy-and-rust-tests.md)）で、呼び出しの形を変えれば抜けられる。

## Consequences

- 開発中のbinaryで本番を読むのは、そのbinaryのAPI versionがserviceの幅に入るときだけになる。外れれば固定binaryで読む。
- serviceが居ないとmigrateも診断もできず、直す手は`up`か`service start`になる。
- `install`の手順は、serviceの入れ替えとmigrateの依頼の順を持つ。

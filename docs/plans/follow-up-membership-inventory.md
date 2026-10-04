---
id: plan-follow-up-membership-inventory
type: plan
title: 既存の open の goal に残る follow_up 由来の task と draft の所属の分類案
status: active
created: 2026-10-05
updated: 2026-10-05
owners:
  - hisamekms
tags:
  - measurement
  - follow-up
  - planner
related:
  - adr-t1504-1
  - adr-t1504-2
  - adr-t808-1
  - plan-follow-up-kinds
---

# 既存の open の goal に残る follow_up 由来の task と draft の所属の分類案

goal 97（task 1512）の棚卸し。[ADR-t1504-1](../adr/2026-10-04-t1504-1-follow-ups-belong-to-the-goal-whose-acceptance-needs-them.md)の基準と dagq skill の `reference/register.md`「A follow_up's membership」の手順で、open の goal に残る follow_up 由来の未完了の task と draft を 1 件ずつ分類した**案**である。所属の判断の記録（`judge-follow-up`）・`set-goal`・`cancel`・goal の close・acceptance の変更は行っていない（goal 97 の constraints。worker には権限も無い）。適用は権限のある runtime の planner か人が行う（「適用の依頼」）。案は記録した時点の queue に対するもので、適用する前に `show ID --full` の `membership_judgements` と goal の `acceptance_version` が変わっていないかを確かめる。

## 数えた時刻とコマンド

- 時刻: 2026-10-04T15:22:35Z〜15:26Z（UTC）。`dagq goal list` を 15:22:35Z と 15:24:07Z の 2 回取り、86 件の open の goal の `goal show --full` の task の状態ごとの件数が 2 回目の `goal list` と全て一致することを確かめた（1 回目と 2 回目の間に goal 89 の task 1734 が draft から canceled に変わったため、2 回目を正とする）。続けて取った各 task の `show --full` の状態も `goal show` と全て一致した。
- コマンド（固定バイナリ `~/.local/bin/dagq` 0.4.0-dev+e70840a9、読み取りだけ）:
  - `dagq goal list`（`closed` が false の 86 件を open とする。`status` の欄は閉じた goal でも `open` と出るので使わない）
  - open の goal ごとに `dagq goal show ID --full`（`tasks` と `follow_up_memberships`）
  - 未完了（draft・submitted・ready・in_progress）の task ごとに `dagq show ID --full`（`origin`・`membership_judgements`・events の `task_edited` の元の context）
  - 出どころの run の receipt（`runs/<run>/receipt.json` の `follow_ups[index]`）、必要に応じて `dagq search`・`dagq related`・`dagq goal show <source goal> --full`、repository の `git log`・`grep`（実装済みかの確認）
- follow_up 由来の判定: `show` の `origin.origin` が `follow_up`（draft_origins の follow_up）のもの 128 件と、context が receipt の follow_up の draft の置き換えだと名指すもの 3 件（task 244・249・256。2026-09-25 の draft の棚卸しで人が follow_up の draft 126・217・231 を置き換えて登録した）。`origin` が `goal_gap` の 4 件（1285・1311・1312・1481）は ADR-t1504-1 により判定しないので数えない。context が follow-up に触れるだけの task（1642・1644・1678）は follow_up 由来ではない。

### 件数

| 数えたもの | 件数 |
|---|---|
| goal 全体（`goal list`） | 125（closed 39、open 86） |
| open の goal の task の合計 | 1,287（completed 636、canceled 383、draft 16、submitted 0、ready 249、in_progress 3） |
| そのうち未完了 | 268（draft 16 + ready 249 + in_progress 3） |
| そのうち follow_up 由来 | 131（53 goal にまたがる。draft 5、ready 126） |
| 既に planner の所属の判断の記録があるもの | 27（全て今も成り立つと見た） |
| 判定を記録せずに扱うもの | 6（244・249・256・1344・705・755。下の「判定を記録しないもの」） |
| 残る 98 件の分類案 | required 13、out_of_scope 80、undecided 5 |

判定を記録する 125 件（既存の判定 27 件を含む）の分類案: required 16、out_of_scope 104（今の goal のまま 35、既存の別 goal へ 31、新しい goal の案へ 38）、undecided 5。cancel の候補（重複・実装済み・不要）は 6 件を含めて 13 件。

goal ごとの件数は末尾の「goal ごとの件数」の表にあり、その合計は上の `goal list` の合計と一致する。

## 分類の読み方

- 判定は出どころの goal（source goal、`origin.material.source_goal_id`）の acceptance に対して行った。set-goal で移された 38 件も source goal で読んだ。
- 問いは「この follow-up を実施しなくても source goal の acceptance を満たしたと言えるか」だけ。影響の大小・priority・関連性・worker の category では決めていない。所属と採用（実施の価値）と priority は別で、別 goal に移す案は採用も優先も意味しない。
- **分類**: required（source goal に残す・戻す）、out_of_scope（別 goal へ。所属先の案は既存の open の goal を優先し、無いものは新しい goal の題の案を書いた）、undecided（根拠を調べても goal の意図が要るもの。人の確認に回す）。
- **項目**: source goal の acceptance の該当の項目。「—」は acceptance のどの項目もこれを求めていないこと。
- **証拠**: 出どころの receipt（run ID と `follow_ups` の位置）、登録の event ID（`registration_event_id`）、commit、文書の節、関連の task。
- **処置の案**: 重複・実装済み・不要と見たもので、cancel の候補（根拠つき）。canceled の follow-up は goal を持たないので、cancel するなら所属の判断は要らない（register.md の 5）。
- **採用の注意**: ADR-t808-1 の上限（depth 3 以上、登録時に source goal が無い・閉じていた・不明）により採用に人の adopt が要るもの。所属の判断とは別。

## 判定を記録しないもの

`judge-follow-up` が受け付けないか、記録が要らないもの。分類の行は扱いを決める材料として残す。

- task 244・249・256: `show` の `origin` が null（follow_up の draft 126・217・231 を人が置き換えて登録した task で、draft_origins を持たない）。runtime は `no follow_up origin` で記録を拒む。所属を変えるなら `set-goal`、要らなければ `cancel`。分類の行は今の goal の acceptance に照らした案。
- task 1344: 登録時の source goal が none なので、判定の対象が無く runtime も拒む（register.md）。不要と見たので `cancel` の候補。
- task 705・755: source goal 34 のまま out_of_scope にする置き先が無く（source goal と同じ goal への out_of_scope は runtime が拒む）、実装済み・不要と見た cancel の候補。canceled の follow-up は goal を持たないので判定を記録せずに cancel できる（register.md の 5）。

## 新しい goal の案についての注意

out_of_scope のうち 38 件は既存の open の goal に合うものが見つからず、新しい goal の題の案（21 通り、うち似たものがある）を書いた。goal 38 は draft の大きな親の goal なので置き先にしていない。適用する planner は、案の goal を作る前に `search` と `goal list` で既存の goal を探し直し、似た案をまとめる（例: test の fixture と後片付けの案、goal 74 の後続の案）。goal 106 の goal のラベルが入れば、ADR-t1639-2 のとおり source goal のラベルの low の受け皿の goal を既定の置き先にする。無関係な巨大な保守 goal には詰めない。

## 人の確認が要るもの

worker は acceptance を達成扱いにせず、goal の意図が要るものをここに挙げる。planner は人に `planner_question --because scope` で確かめてから記録する（acceptance を弱める変更は planner が自動で行わない。ADR-t1504-1）。

| task | goal | 分類の案 | 人に確かめること |
|---|---|---|---|
| 787 | 21 | out_of_scope | 787 の所属は答えに依らない。goal 21 の acceptance (2)「in-cmux mode でも画面を閉じても失われない」は in-cmux mode の廃止（goal 92・111）で、(4)「task が kind を持つ」は kind の廃止（goal 69 の change＋area）で前提が古い。goal 21 を達成扱いで閉じてよいか、acceptance を改めるか。 |
| 1342 | 57 | out_of_scope | goal 57 (2)「対話の経路は Claude の既定として残る」は既定を非対話にした決定（task 1340・note 60084）と対話の経路の廃止（goal 92）で古い。goal 57 を達成扱いにするか acceptance を改めるか（1342 の所属は答えに依らない）。 |
| 1205 | 57 | undecided | goal 57 (6) は manual-smoke の結果が「ある」ことで足りるか、確認点（config.toml の検査）の通過まで要るか。後者なら required。上の (2) の古さも同じ問いで確かめる。 |
| 244 | 12 | undecided（不要の候補。判定の記録の対象外） | goal 12 の observer の acceptance（note と draft goal を残す・閾値超えを ask で上げる）は ADR-0044・ADR-t451-1 で古い。244 の前提も無くなった見込み。goal 12 を達成扱いにするか acceptance を改めるか、244 を cancel するか。 |
| 688 | 52 | undecided | goal 52 (5) の「実装が task になっている」は登録で足りるか、着地まで要るか。登録で足りるなら out_of_scope。 |
| 747 | 52 | undecided | 688 と同じ問い。747 の (1) の README の部分は commit 8945049a で済んでいるので、残すなら (1) を外す edit が要る。 |
| 773 | 37 | undecided | 着地の先回りの検査（landing recheck）が環境の原因で落ちて resume に回る件を、goal 37 の「integration verification」に含めるか。含めるなら required、含めないなら out_of_scope。 |
| 1630 | 101 | undecided | goal 101 の acceptance の「test が起動したシェルのループ」に、本物の detached な dagq の wrapper が入るか。入らないなら out_of_scope（所属先の案は本文）。 |

## 気づいたこと（適用の前に planner が確かめる）

- 例として名指された goal 20 の task 792 と goal 33 の task 436 は、acceptance を読み直しても古くなっていなかった（792: goal 20 の (2) は今の `dependency_graph`・READY_QUERY のとおり、(5) の llvm-cov 80% は ci.yml に残る。436: goal 33 の (2) は task 338 で入り、(4) は tests/it/related.rs と本番の複製 9 組中 8 組で確かめ済み）。どちらも out_of_scope の案で、人の確認には上げない。goal 20 の goal review 6（2026-09-27 開始）は終わりの event が無いので、閉じる前に goal review が回るかを確かめる（所属とは別の運用の件）。
- task 1653（goal 107）と 1660（goal 108）は set-goal で移されているが所属の判断の記録が無い。1660 は goal 97 の閉じる条件で未判定として数えられる。判断を記録する。
- task 1611 は `follow_up_memberships` の depth が 0 だが、context では 1334→1500→1611 の深さ 3 で、ask 383 で人が adopt と答えている。
- task 1359 は既存の判定 18（out_of_scope、goal 68 へ）がそのまま成り立つ。問題（build の時間の測定）は goal 68（test の時間）より goal 36（run の build の共有）に合うので、置き先を変えるかは planner が判断する（変えるなら `--corrects` で記録し直す）。この文書の案は判定 18 のまま。
- goal 51 の acceptance は event 14557 の goal_updated で「対象（mechanical かつ下位…」の途中で切れている（前の文は event 14480）。601・631 の判定には影響しないが、goal 51 の goal review の前に人か planner が確かめる。
- task 1026（goal 66）は task 1239 の着地（2026-10-01T21:03Z）で worker が e2e を流さなくなったので、比較の「後」の区間をその着地で区切るよう task の文を直す必要がある。
- task 1648（goal 97）と 1576（goal 87）は required だが、中身は commit 4855bc84（task 1505）と 8279f86b（task 1400）で入ったと見られる（実装済みの cancel の候補）。1648 は acceptance (2) の競合を名指す test があるかを確かめてから cancel する。

## 適用の依頼（goal ごと）

記録は `judge-follow-up ID --classification ... --acceptance-item ... --reason ... --evidence ...`（out_of_scope は `--destination-goal`）。項目・理由・証拠は下の「goal ごとの分類案」の該当の行を使う。既存の判定のままのものは記録し直さない。

| goal | 依頼 |
|---|---|
| 3 | out_of_scope を記録: 681→新しい goal。判定を記録せずに: 249（set-goal で案の goal へ）・256（set-goal で案の goal へ） |
| 8 | out_of_scope を記録: 708→goal 111 |
| 11 | out_of_scope を記録: 712→goal 106・751→goal 30・847→goal 71・922→goal 30・940→goal 109 |
| 12 | 判定を記録せずに: 244（人の確認の後に cancel か据え置き） |
| 17 | out_of_scope を記録: 711→goal 32 |
| 20 | out_of_scope を記録: 792→新しい goal |
| 21 | out_of_scope を記録: 787→新しい goal |
| 29 | out_of_scope を記録: 597→新しい goal・636→新しい goal・810→新しい goal。既存の判定のまま: 1585 |
| 30 | out_of_scope を記録: 800→新しい goal・905→新しい goal |
| 31 | required を記録: 654。out_of_scope を記録: 707→新しい goal・796→goal 34。cancel の候補: 707（不要）・1565（実装済み）。既存の判定のまま: 1565 |
| 33 | out_of_scope を記録: 436→新しい goal |
| 34 | out_of_scope を記録: 643→goal 57・706→goal 30・758→goal 74・780→goal 30・903→新しい goal・1015→goal 30。判定を記録せずに: 705（cancel（実装済み））・755（cancel（不要））。既存の判定のまま: 1321・1386 |
| 36 | required を記録: 1625。out_of_scope を記録: 482→新しい goal。既存の判定のまま: 1382 |
| 37 | out_of_scope を記録: 1283→新しい goal。人に意図を確かめてから記録: 773。判定を記録せずに: 1344（cancel（不要）） |
| 39 | out_of_scope を記録: 1514→goal 72・1515→goal 37 |
| 40 | required を記録: 661・663・700。out_of_scope を記録: 714→新しい goal・720→新しい goal・765→新しい goal・1001→新しい goal |
| 45 | out_of_scope を記録: 1628→goal 92。cancel の候補: 1628（不要） |
| 48 | 既存の判定のまま: 1563・1646 |
| 51 | required を記録: 631。out_of_scope を記録: 601→新しい goal |
| 52 | required を記録: 668。out_of_scope を記録: 1156→goal 32・1483→新しい goal。人に意図を確かめてから記録: 688・747 |
| 55 | out_of_scope を記録: 784→新しい goal・1152→新しい goal。既存の判定のまま: 1564・1647 |
| 56 | out_of_scope を記録: 900→新しい goal。既存の判定のまま: 1384・1411 |
| 57 | out_of_scope を記録: 1342→goal 86。人に意図を確かめてから記録: 1205。既存の判定のまま: 1385・1594 |
| 61 | out_of_scope を記録: 1332→goal 40 |
| 62 | out_of_scope を記録: 934→goal 72・937→新しい goal。cancel の候補: 934（不要） |
| 66 | required を記録: 1026。out_of_scope を記録: 1025→新しい goal・1278→新しい goal |
| 68 | out_of_scope を記録: 1121→新しい goal・1261→新しい goal・1262→新しい goal・1293→goal 118・1295→新しい goal。cancel の候補: 1293（不要）。既存の判定のまま: 1359 |
| 72 | out_of_scope を記録: 1004→goal 40・1073→goal 76・1074→goal 76。cancel の候補: 1073（不要） |
| 73 | out_of_scope を記録: 1109→goal 89。既存の判定のまま: 1476・1703 |
| 74 | required を記録: 1190。out_of_scope を記録: 1183→goal 75・1191→新しい goal・1336→新しい goal・1501→新しい goal・1611→新しい goal・1613→新しい goal |
| 79 | out_of_scope を記録: 1350→新しい goal |
| 82 | out_of_scope を記録: 1260→新しい goal・1324→新しい goal・1326→新しい goal・1351→goal 37・1352→goal 101・1464→goal 82・1629→goal 37。cancel の候補: 1260（実装済み） |
| 87 | cancel の候補: 1576（実装済み）。既存の判定のまま: 1576 |
| 89 | 既存の判定のまま: 1657 |
| 90 | required を記録: 1537 |
| 92 | 既存の判定のまま: 1577 |
| 93 | out_of_scope を記録: 1600→goal 75・1601→goal 66 |
| 97 | required を記録: 1648。cancel の候補: 1648（実装済み） |
| 98 | required を記録: 1651 |
| 100 | out_of_scope を記録: 1650→goal 100 |
| 101 | 人に意図を確かめてから記録: 1630 |
| 102 | required を記録: 1605 |
| 107 | out_of_scope を記録: 1653→goal 107 |
| 108 | out_of_scope を記録: 1660→goal 108 |
| 109 | 既存の判定のまま: 1661 |
| 111 | 既存の判定のまま: 1544 |
| 112 | 既存の判定のまま: 1727 |
| 115 | out_of_scope を記録: 1616→goal 115・1617→goal 115・1618→goal 115・1619→goal 115・1620→goal 115・1621→goal 115・1637→goal 115 |
| 116 | 既存の判定のまま: 1704 |
| 117 | 既存の判定のまま: 1705 |
| 120 | 既存の判定のまま: 1729 |
| 122 | 既存の判定のまま: 1724 |
| 123 | 既存の判定のまま: 1725 |

## goal ごとの分類案

### goal 3: Rust runtime を domain / application / infrastructure のレイヤーと型＋関数の方針でリファクタリングする

acceptance の版 1。follow_up 由来の未完了 3 件、follow_up でない未完了 2 件。

- **task 681**（ready・source goal: goal 3・depth 1）domain の kind 比較も event_kind 定数に寄せる
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (1) src/domain 配下に rusqlite・std::fs・…・anyhow への参照がない / (2) …コマンド関数を持つ
  - 理由: goal 3 の (1)〜(7) は domain の外部 I/O・非公開のフィールド・newtype・注入・ユースケースの置き場を求めていて、domain の中の kind の文字列リテラルを定数にすることは求めていない。task 221 の receipt も domain の中は受け入れ条件の対象外と書いている。
  - 証拠: receipt of run 6fa147a0-ad7e-4050-94d3-f4cce8b9de04 (task 221) の follow_ups「domain の kind 比較も event_kind 定数に寄せる」; event 19038; src/domain/plan_quality.rs:195・203・247-271、areas.rs:98、resume.rs:92-93 に kind のリテラルが残っている（未実装）
  - 所属先の案: new: domain・compose の残りの小さな整理（goal 3 の受け入れの外の refactor の受け皿）（goal 3 から移す）
- **task 249**（ready・source goal: none（出どころの goal 無し）・depth —）domain: integrate で task を completed にするのを Task のコマンド関数に通す
  - 分類: 範囲外で別 goal（out_of_scope）
  - 記録: しない。judge-follow-up の対象外（origin が null で follow_up の draft ではなく、runtime は no follow_up origin で拒む）。所属を変えるなら set-goal、要らなければ cancel。下の分類は所属を変えるかの判断の材料として、今の goal の acceptance に照らした案
  - 項目: (2) Task・Goal・TaskRun のフィールドが非公開で、new/restore の入口と Result<_, DomainError> を返すコマンド関数を持つ
  - 理由: source_goal は null なので、今の goal 3 で判定した。(2) が求めるのは Task がコマンド関数を持つことで、src/domain/task.rs には claim・edit・transition などがあり、文言の上では満たしている。integrate での completed への遷移を domain に通すことまでは求めていないので、これが無くても (2) は満たせる。
  - 証拠: registration は draft の棚卸し（2026-09-25）の人の決定による task 217 の置き換えで、receipt の follow_up は無い; src/infrastructure/runtime_store/transitions.rs:725 が UPDATE tasks SET status='completed' を直接打ち、TaskAction に complete が無い（未実装。前の run の branch dagq/6e865c70… の head f0c86ac5 は main に無い）
  - 所属先の案: new: domain・compose の残りの小さな整理（goal 3 の受け入れの外の refactor の受け皿）（goal 3 から移す）
  - 採用の注意: source goal が不明（source_goal_id・depth が null で、registration_event も無い）。ADR-t808-1 では採用に人の adopt が要るが、context には 2026-09-25 の棚卸しで人が登録を決めたとある
- **task 256**（ready・source goal: none（出どころの goal 無し）・depth —）compose: ask・review・session にも Generators を注入する
  - 分類: 範囲外で別 goal（out_of_scope）
  - 記録: しない。judge-follow-up の対象外（origin が null で follow_up の draft ではなく、runtime は no follow_up origin で拒む）。所属を変えるなら set-goal、要らなければ cancel。下の分類は所属を変えるかの判断の材料として、今の goal の acceptance に照らした案
  - 項目: (4) 現在時刻と ID 生成が application に注入され、テストで固定できる
  - 理由: source_goal は null なので、今の goal 3 で判定した。(4) は application への注入を求めているが、compose::ask・review・session は起動部分（composition root）で、application は SqliteQueue と Supervisor の Generators を通して注入を受ける。これが無くても (4) の範囲は変わらない。application に残る SystemTime::now は goal 115 の task 1618 が受け持つ。
  - 証拠: registration は draft の棚卸し（2026-09-25）の人の決定による task 231 の置き換え; src/compose.rs:2396 の ask、2805 の session、3102 の review_in が Generators を受け取らず SqliteQueue::open を使う（未実装）; goal 115 の task 1618（application の SystemTime::now の件）
  - 所属先の案: new: domain・compose の残りの小さな整理（goal 3 の受け入れの外の refactor の受け皿）（goal 3 から移す）
  - 採用の注意: source goal が不明（source_goal_id・depth が null）。ADR-t808-1 では採用に人の adopt が要るが、context には 2026-09-25 の棚卸しで人が登録を決めたとある

### goal 8: maintainer の機械的な作業を runtime に寄せる: needs_session の自動 resume、exit timeout で放棄しない、integrate の push、follow_ups の draft 登録、evidence の検証、prompt 待ちの検知

acceptance の版 1。follow_up 由来の未完了 1 件、follow_up でない未完了 0 件。

- **task 708**（ready・source goal: goal 8・depth 1）dagq-inbox skill: reference/asks.md の stalled の項を、非対話の run の stalled の ask の reason ごとの question と選択肢に合わせる
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: dagq-maintain / dagq-land / dagq-session skill と docs/design/supervisor-lifecycle.md がこの挙動に更新されている
  - 理由: goal 8 の acceptance が求める skill の更新は dagq-maintain・dagq-land・dagq-session と supervisor-lifecycle.md で、dagq-inbox の asks.md の非対話の stalled の記述は含まれない。対話の worker の廃止を plugin の案内に反映する goal 111（依存先 1438 も goal 111）に合う。
  - 証拠: receipt of run c2569438-1371-42ca-9183-ad3377ec093d (task 318) follow_ups[0]; event 20452; plugins/claude-dagq/skills/dagq-inbox/reference/asks.md 14 行が今も send_unconfirmed・intervene・画面を案内: 未実装; task 1438 は goal 111 で ready
  - 所属先の案: goal 111（goal 8 から移す）

### goal 11: 実行効率: 検証を integrate の 1 回にし、review を supervisor の工程にして通れば着地し、build cache を共有し、worker の起動を軽くし、graph と stats で詰まりを数字で見る

acceptance の版 1。follow_up 由来の未完了 5 件、follow_up でない未完了 0 件。

- **task 712**（ready・source goal: goal 11・depth 0）docs: inspect.md の set-goal と set-priority の対象に submitted を足す
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: —（source goal の acceptance のどの項目もこれを求めていない）
  - 理由: goal 11 の acceptance（検証を integrate の 1 回・review→着地・run の env・ダイアログ・graph・stats）は inspect.md の set-goal / set-priority の対象の記述と関係がなく、これなしで満たせる。
  - 証拠: receipt of run 43dc4533-f97d-4959-97ee-c3574eff89d9 (task 665) follow_ups[0]; event 20699; plugins/claude-dagq/skills/dagq/reference/inspect.md 128・133・181・182・188 行が今も「draft or ready」（main 60972db3）: 未実装; src/domain/task.rs の set_goal・set_priority は require_editable（Draft \| Submitted \| Ready）
  - 所属先の案: goal 106（goal 11 から移す）
- **task 751**（ready・source goal: goal 11・depth 1）runtime: needs_session / failed の run を /exit を送れずに abandon したとき attention に出す
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: —（source goal の acceptance のどの項目もこれを求めていない）
  - 理由: goal 11 の acceptance に、/exit を送れずに abandon した run の attention は含まれない。開いた session を人に知らせる話で、止まった session を inbox に知らせる goal 30 の問題に合う。
  - 証拠: receipt of run 6ac89554-e104-48ab-a522-cbd7b6b7fdf5 (task 237) follow_ups[0]; event 22406; src/application/supervise/mod.rs の exit_abandoned_session と src/domain/run/history.rs の session_left_open が残る（ExitSession は ReviewAndIntegrate・RecoverRun だけ）: 未実装; commit d751a0c9 runtime: 対話の worker の run を起こさないようにし、対話の run だけが使う画面・ダイアログ・/exit・打鍵の処理…を消す; task 1440（goal 92, ready）: 非対話の session wrapper の workspace の経路を消して background だけにする
  - 所属先の案: goal 30（goal 11 から移す）
- **task 847**（ready・source goal: goal 11・depth 0）runtime: REVISE の session span の attempt を revise_requested の attempt に合わせる
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: stats が run の作業・検証・着地待ち・resume 回数と閾値超えを返す
  - 理由: goal 11 の stats の条件は run の時間と resume 回数・閾値超えで、REVISE の session span の attempt の番号を revise_requested に揃えることは含まない（番号で突き合わせる処理も今は無い）。無くても acceptance は満たせる。
  - 証拠: receipt of run 9a6fba4f-bf2d-4a5c-b8c0-91acef8adfcc (task 781) follow_ups[0]; event 28267; src/domain/sessions.rs 275・288〜289 行と src/infrastructure/sessions.rs 2174 行が今も revise_requested − revise_unsent で attempt を作る: 未実装; src/application/supervise/landing.rs 881 行で revise_unsent は今も記録される
  - 所属先の案: goal 71（goal 11 から移す）
- **task 922**（ready・source goal: goal 11・depth 1）引き継いだ段の RecoveryWatch の組み立て直しを段の錨に限る（非対話の run）
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: —（source goal の acceptance のどの項目もこれを求めていない）
  - 理由: goal 11 の acceptance は検証・review・着地・env・ダイアログ・graph・stats で、引き継いだ段の RecoveryWatch の組み立て直しを錨に限ることは含まない。復旧の監視の状態の復元の話で、止まった session の検知の goal 30 に合う。
  - 証拠: receipt of run 836281e1-4e1c-44fa-a228-a1bd8137b0af (task 743) follow_ups[0]; event 33148; src/application/supervise/session.rs 380〜388 行の SessionWatch::adopt は _anchor を受けて使わず、RecoveryWatch::adopt（recovery.rs 312 行）は全イベントから Stalled の held を作る: 未実装; task 1437 は completed（commit d751a0c9）
  - 所属先の案: goal 30（goal 11 から移す）
- **task 940**（ready・source goal: goal 11・depth 0）Running の run の worker_question の answer で runtime_delivers と status の判定をそろえる
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: —（source goal の acceptance のどの項目もこれを求めていない）
  - 理由: goal 11 の acceptance に worker_question の answer の status の表示は含まれない。status の表示を answer 時の runtime_delivers に揃える話で、同じ型（runtime_delivers と status の判定のずれ）を扱う goal 109 に合う。
  - 証拠: receipt of run 9c3206f3-50ae-4fcb-8306-dbea21a7b801 (task 584) follow_ups[0]; event 34121; src/application/health.rs 1408〜1426 行が Running でも stale な lease で DeliverAnswer を出す: 未実装; open goal 109（runtime が適用する人の答えの status の表示を runtime_delivers と揃える）
  - 所属先の案: goal 109（goal 11 から移す）

### goal 12: maintainer を退役させ、run 単位の job（review / triage）と定期起動の observer に置き換える

acceptance の版 1。follow_up 由来の未完了 1 件、follow_up でない未完了 0 件。

- **task 244**（draft・source goal: none（出どころの goal 無し）・depth —）runtime: task の無い blocked の ask が開いているとき、observer の別の警告が消えないようにする
  - 分類: 判断できない（undecided）
  - 記録: しない。judge-follow-up の対象外（origin が null で follow_up の draft ではなく、runtime は no follow_up origin で拒む）。所属を変えるなら set-goal、要らなければ cancel。下の分類は所属を変えるかの判断の材料として、今の goal の acceptance に照らした案
  - 項目: observer が定期に起動して note と draft goal を残し、閾値超えを ask で inbox に上げる
  - 理由: source_goal は null なので今の goal 12 で読んだ。goal 12 の『observer が note と draft goal を残し、閾値超えを ask で inbox に上げる』は ADR-0044（task 292）で observer が note と draft goal を書かず finding を記録し、ADR-t451-1（task 1319）で blocked の ask は人が要る見立ての finding ごとに 1 件に限る形に変わっており、この draft の前提（task の無い blocked の ask が queue に 1 件しか開けず別の警告が捨てられる）が今も起きるかも、acceptance を達成とみなすかも決めきれない。
  - 証拠: registration の event・receipt が無い（source_task・run・event が null）; docs/design/supervisor-lifecycle/observer.md の 27 行（observer は note と draft goal を書かず finding を記録）と 99〜106 行（findingごとにopenなblockedのaskは1件）; commit 523ae7ff observer の prompt と CLI を変え、leave it / wait の見立ては finding だけにし、blocked の ask は人が要る見立てに限る（task 1319）
  - 処置の案: 不要として cancel の候補。根拠: 確度は中: 警告は種類・subject ごとの finding として別々に記録され、blocked の ask は finding に紐づき finding ごとに 1 件なので、『別の警告が捨てられる』問題は今の仕組みでは起きない見込み（asks_open の制限が残るかは未確認）。cancel の候補
  - 人の確認: 要る。goal 12 の acceptance の observer の部分（note と draft goal を残す・閾値超えを ask で上げる）が ADR-0044・ADR-t451-1 で古くなっている。達成扱いにするか acceptance を改めるかは人の判断が要る
  - 採用の注意: source goal が unknown（source_goal_id が null、出どころの記録が無い）ので、ADR-t808-1 により採用には人の adopt が要る

### goal 17: Keep cmux backend calls reliable under high host load

acceptance の版 1。follow_up 由来の未完了 1 件、follow_up でない未完了 0 件。

- **task 711**（ready・source goal: goal 17・depth 0）install --allow-breaking: drain した supervisor の --max-load を起動し直す supervisor に引き継ぐ
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: the supervisor stops claiming new runs while the load average is above a configurable threshold
  - 理由: goal 17 の acceptance は閾値を設定できること（up --max-load）と cmux の再試行・test・backend_failures を求め、install --allow-breaking の drain の後に値を引き継ぐことは求めない。実施しなくても acceptance を満たせる。
  - 証拠: receipt of run 416aeaac-03d3-4583-9c7c-b96f4f753870 (task 577) follow_ups[0]; event 20552; task 577・623 completed
  - 所属先の案: goal 32（goal 17 から移す）

### goal 20: タスクがゴールに依存できるようにし、ゴールが閉じるまで claim されないようにする

acceptance の版 1。follow_up 由来の未完了 1 件、follow_up でない未完了 0 件。

- **task 792**（ready・source goal: goal 20・depth 1）効く優先度: 永久に claim されない task を通して推移的に継承しない
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (2) 依存先ゴールが achieved で閉じるまで candidates / claim に出ない…abandoned で閉じたゴールは解かない
  - 理由: goal 20 の (1)〜(5) は goal 依存の登録・claim の待ち・循環の拒否・文書を求めていて、効く優先度の推移的な継承は求めていない。acceptance の古さも確かめた: (2) の「achieved まで待つ・abandoned は解かない」は src/application/mod.rs の dependency_graph と READY_QUERY の今の振る舞いのとおりで、(5) の llvm-cov 80% も .github/workflows/ci.yml:95 の --fail-under-lines 80 に残っている。前提は変わっておらず、この follow-up が無くても達成と言える。
  - 証拠: receipt of run 53964893-5dfe-4332-b22a-95beb48bc468 (task 304) の follow_ups「効く優先度: 永久に claim されない task を通して推移的に継承しない」; event 25252; src/application/mod.rs:308 の effective_priority は今も waiter を 1 件ずつ見る（未実装）。commit 1993efe7 と bc5990e5 は直接の waiter だけを除く; goal 20 の残りの task は 792 だけ（178・179・304・791 は completed）。goal review 6 は 2026-09-27 に始まったが、終わりの event が無い
  - 所属先の案: new: abandoned の goal への依存で永久に claim されない task を inbox に知らせ、効く優先度の継承から推移的に外す（goal 20 の後続）（goal 20 から移す）

### goal 21: 監査と障害調査のために、ローカルの計測を tracing の 1 系統にし、run の記録に分類コードと不足していた値を足す

acceptance の版 1。follow_up 由来の未完了 1 件、follow_up でない未完了 0 件。

- **task 787**（ready・source goal: goal 21・depth 1）Telemetry::capture を使う test の callsite interest の取りこぼしを防ぐ
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (6) cargo fmt / test / clippy / llvm-cov 80% が通る
  - 理由: 取りこぼしは同じ process で並行する cargo test でだけ起き、関門の nextest では起きず、lifecycle_up の個別の回避で今の test は通るので、(6) はこれなしで満たせる。test の補助の頑健性の改善。
  - 証拠: receipt of run 04d5a9c6-cd17-4b3c-bdf4-56d232037804 (task 255) follow_ups[0]; event 25005; tests/it/lifecycle_up.rs 1263〜1264 行の _second と rebuild_interest_cache が残り、src/infrastructure/telemetry.rs の capture に共通の対策が無い: 未実装
  - 所属先の案: new: test の fixture と補助の process の隔離と後片付けを固める（template の上書き・EXDEV・残る template・host の lock・tracing の capture・時間切れで残る wrapper）（goal 21 から移す）
  - 人の確認: 要る。goal 21 の acceptance は古くなっている。(2)「in-cmux mode でも画面を閉じても失われない」は in-cmux mode の廃止（goal 92・111）、(4)「task が kind を持つ」は kind の廃止（goal 69 で change＋area）が前提を変えた。787 の所属（out_of_scope）は変わらないが、goal 21 を達成扱いで閉じるか・acceptance を改めるかは人が決める。

### goal 29: 計画を立てる planner と、計画を検査して ready にする plan review job を分け、複数の場所で同時に計画できるようにする

acceptance の版 1。follow_up 由来の未完了 4 件、follow_up でない未完了 0 件。

- **task 597**（ready・source goal: goal 29・depth 0）concern で保留中の proposal の submitted の task を編集したら plan review を受け直させる（ADR-0041 決定 9）
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (3) plan review が pass なら ready、revise なら差し戻し…人の判断が要るものは inbox の ask になる / (6) 決定が ADR にあり
  - 理由: goal 29 の acceptance が求めるのは planner と plan review の分離と pass・revise・ask の流れで、concern で保留中の proposal を編集したときの再検査（ADR-0041 決定 9 の端）は項目に無い。(6) も決定が ADR にあることまでで、その全部の実装は求めていない。
  - 証拠: receipt of run 15de5697-4abc-4ad1-9339-b36abca407eb (task 400) の follow_ups「concern で保留中の proposal の submitted の task を編集したら plan review を受け直させる」; event 15125; commit a8394fb8 plan review 中に submitted の task が編集されたら verdict を適用せずにかけ直す（ADR-0041 決定 9。task 400 の範囲で、concern の保留中は含まない）; src/infrastructure/proposals.rs で review_hold を外すのは resubmit などだけで、edit の経路には無い（未実装。前の run の branch dagq/3d5b6166… の head b9dc4df3 は main に無い）
  - 所属先の案: new: plan review の保留中の編集の再検査と、予想ファイルの読み取りの計測・cache（goal 29 の後続）（goal 29 から移す）
- **task 636**（ready・source goal: goal 29・depth 0）plan review: 予想ファイルの読み取り時間を記録し、着地 commit の差分のファイルを恒久的に cache する
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (2) ready にするのは plan review job だけ / (3) plan review が pass なら ready…
  - 理由: 予想ファイルの読み取り時間の記録と、着地 commit の差分の cache は性能の改善で、goal 29 の (1)〜(6) のどれもこれを求めていない。
  - 証拠: receipt of run a6cbbc75-b514-42ac-8187-e4734e8105d0 (task 591) の follow_ups「plan review: measure how long reading expected files takes on a large queue」; event 16854; src/application/supervise/claim_defer.rs:366-378 で HOT_REFRESH_SECS ごとに defer.expected.clear() したまま、着地 commit の恒久 cache は無い（未実装）
  - 所属先の案: new: plan review の保留中の編集の再検査と、予想ファイルの読み取りの計測・cache（goal 29 の後続）（goal 29 から移す）
- **task 810**（ready・source goal: goal 29・depth 1）runtime: goal への依存（task_goal_dependencies）で abandoned の goal を待つ task を inbox に知らせる
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (1)〜(6) のどれにも当たらない
  - 理由: abandoned の goal に goal 依存で待つ task を inbox に知らせることは、planner と plan review の分離という goal 29 の acceptance に無い。goal 20 の (2) も「abandoned で閉じたゴールは解かない（graph で見える）」までで、知らせは求めていない。
  - 証拠: receipt of run a945a4f0-b30d-4269-ab07-55e5d29ea387 (task 421) の follow_ups「Tell the inbox of tasks waiting on a goal closed as abandoned via a goal dependency」; event 26386; src/infrastructure/stranded.rs に task_goal_dependencies への参照が無い（未実装）。commit 1993efe7 は abandoned の goal への新しい依存を拒むだけで、閉じる前に張られた依存は扱わない
  - 所属先の案: new: abandoned の goal への依存で永久に claim されない task を inbox に知らせ、効く優先度の継承から推移的に外す（goal 20 の後続）（goal 29 から移す）
- **task 1585**（ready・source goal: goal 87・depth 1）非対話の runtime の planner の起動の失敗（launch）で控えを待つことの integration test を足す
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: goal 87 の (3) runtime の planner を非対話で動かせ、revise・answer が turn として届き…
  - 理由: goal 87 の acceptance は非対話の planner の経路と切り替えと評価を求めていて、起動の失敗（launch）の控えの integration test は求めていない。login と利用上限の test は既にあり、この test が無くても goal 87 は満たせる。
  - 証拠: receipt of run f4b8ea86-dff0-4086-a2c8-5eb760569be9 (task 1397) の follow_ups（category test_gap）; event 75948; dagq goal show 87 の acceptance (1)〜(5); judgement 17
  - 所属先の案: 今の goal 29 のまま（移動不要）
  - 既存の判定 17: 今も成り立つ。out_of_scope は今も成り立つ（goal 87 の acceptance は acceptance_version 1 のまま変わっていない）。行き先の goal 29 は planner の goal で問題に合うので、移す必要は無い

### goal 30: supervisor が、止まった worker の session を検知して促し、解消しなければ inbox に知らせる

acceptance の版 1。follow_up 由来の未完了 2 件、follow_up でない未完了 0 件。

- **task 800**（ready・source goal: goal 30・depth 0）stale lease の recover の後、一時停止から戻った supervisor が stall_resolved を重ねうる
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (3) stats が走っている run の alert…を返す / (5) task 182 と 205 の型を再現する runtime test がある
  - 理由: goal 30 の acceptance は止まった session の促し・ask・stats の alert・閾値・再現 test で、stale lease の recover の後に一時停止から戻った supervisor が stall_resolved を重ねる稀な二重記録は含まない。stats の閾値の検知の正しさの改善で、無くても acceptance を満たしたと言える。
  - 証拠: receipt of run 7ed5d464-7816-44af-9d2b-93a5ba24cd16 (task 379) follow_ups[1]; event 25665; src/application/supervise/stall.rs 793 行目 record_resolved（event を確かめずに記録。未実装を確認）
  - 所属先の案: new: stall の検知の結末の記録と stats の閾値の検知の数え方を正す（goal 30 の後続。800・905・937 と goal 34 の 780 を集める）（goal 30 から移す）
- **task 905**（ready・source goal: goal 30・depth 2）runtime: stalled の ask の ask_opened に alert と reason を載せ、recovery_finished の無い ask も判別する
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (1) …なお解消しなければ inbox に stalled の ask が開く / (3) stats が走っている run の alert…を返す
  - 理由: goal 30 の (1)(3) は stalled の ask が開くことと alert が stats に出ることで、満たされている。recovery_finished の前に supervisor が死んだ ask の alert の判別は、閾値の内訳の推定の精度の改善で、無くても acceptance を満たしたと言える。
  - 証拠: receipt of run 44cb2cfa-8089-41f5-849a-03fd1b29632d (task 799) follow_ups[0]; event 32190; src/domain/stats/thresholds.rs の ask_threshold（recovery_finished から推定。未実装を確認）
  - 所属先の案: new: stall の検知の結末の記録と stats の閾値の検知の数え方を正す（goal 30 の後続。800・905・937 と goal 34 の 780 を集める）（goal 30 から移す）

### goal 31: observer と supervisor の知見を finding として記録し、proposal を作る経路と、記録を見やすくする CLI を用意する

acceptance の版 1。follow_up 由来の未完了 4 件、follow_up でない未完了 0 件。

- **task 654**（ready・source goal: goal 31・depth 0）runtime: supervise の --observe-interval の既定を 3 時間（10800 秒）にする
  - 分類: 元 goal に必須（required）
  - 項目: (5) 決定が ADR にあり、skill と AGENTS.md が新しい流れを説明している
  - 理由: ADR-0044 決定 21（goal 31 の constraints の人の決定 2026-09-25）は observer の起動間隔を 3 時間と決めているが、src/main.rs の既定は 3600 のままで、skill の reference/observer.md も default 3600 と書く。skill が ADR の決めた流れを説明していないので、これ無しでは (5) を満たしたと言えない。
  - 証拠: receipt of run 4ad28554-6c27-444b-8ffa-311951c733c1 (task 296) follow_ups[0]; event 18002; dagq goal show 31 --full の constraints『起動間隔は 3 時間』; docs/adr/0044 決定 21; src/main.rs 616・3553 行目（既定 3600）; docs/design/supervisor-lifecycle/observer.md 27・42 行目
  - 所属先の案: source goal に残す
- **task 707**（ready・source goal: goal 31・depth 1）plugin: dagq skill の reference/observer.md の events の例を events --full と絞り込みの形にそろえる
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (3) dagq findings、events --full と絞り込み、dagq timeline RUN、observe --history がある / (5) skill と AGENTS.md が新しい流れを説明している
  - 理由: (3) はコマンドがあることを求め、events --full と絞り込みは実装済み。skill の例 `events --all --full --run RUN_ID` は正しい構文で、runtime の prompt と並びが違うだけなので、無くても (3)(5) を満たしたと言える。
  - 証拠: receipt of run 32e5b271-1a90-4fa3-87a2-9695e929dc5d (task 420) follow_ups[0]; event 20435; plugins/claude-dagq/skills/dagq/reference/observer.md 12 行目（まだ --all --full の形）
  - 所属先の案: new: plugin の skill の例と案内を runtime の prompt と揃える（低優先の受け皿）（goal 31 から移す）
  - 処置の案: 不要として cancel の候補。根拠: 今の例 `events --all --full --run RUN_ID` は正しく動く構文で（receipt 自身が valid syntax と書く）、変えても読み手の得る情報は増えない。採らないなら cancel の候補
- **task 796**（ready・source goal: goal 31・depth 0）runtime: 着地した run の閉じていない ask を kind を問わず着地で閉じる（worker_question を含む）
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (1)〜(5) observer・finding・proposal・CLI・ADR
  - 理由: goal 31 の acceptance は observer と finding・proposal・読み取りの CLI を対象にし、着地した run の worker_question などの ask を着地で閉じることは含まない。着地後に残る ask が inbox と asks に出続ける問題は、inbox を人の判断が要るものだけにする goal 34 に合う。
  - 証拠: receipt of run 9422a2ef-aa41-4486-8f93-78d5c1c7265f (task 329) follow_ups[1]; event 25426; src/application/integrate.rs 2090 行目 close_landing_asks（approve_landing と blocked だけを閉じる。未実装を確認）
  - 所属先の案: goal 34（goal 31 から移す）
- **task 1565**（ready・source goal: goal 87・depth 1）finding_planner_exhausted を ATTENTION_KINDS に入れる
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (1)〜(5) planner の経路と非対話化
  - 理由: source goal 87 の acceptance に finding の planner の尽きた知らせは入らない。内容は commit ac1e5eb3（task 1450）が ATTENTION_KINDS への追加と event_attention の unit test の行まで済ませている。
  - 証拠: receipt of run 20a43f22-ab42-4d93-8af6-b6a758a089fa (task 1395) follow_ups[3]; event 72517; commit ac1e5eb3 runtime: 人の planner を前提にした attention（decide the draft / finding / waiting tasks in a planner）と keep_draft の案内を…（本文に finding_planner_exhausted was missing from ATTENTION_KINDS … I added it）; src/domain/mod.rs の ATTENTION_KINDS に finding_planner_exhausted（2001 行目）と attention の test の行（3389 行目）
  - 所属先の案: 今の goal 31 のまま（移動不要）
  - 処置の案: 実装済みとして cancel の候補。根拠: commit ac1e5eb3（task 1450、completed）が ATTENTION_KINDS に finding_planner_exhausted を足し、src/domain/mod.rs の event_attention の test に (finding_planner_exhausted, Some(DecideFinding)) の行を足した。events の attention は ATTENTION_KINDS から決まるので残りは無い見込み。cancel の候補
  - 既存の判定 6: 今も成り立つ。goal 87 の acceptance に要らないことと goal 31 への所属は今も成り立つ。ただし判定の後に ac1e5eb3 で実装済みとなったので、所属より cancel（実装済み）を検討すべき

### goal 33: 重複や実装済みの task を、全文を読まずに見つけられるようにする（search・related・duplicate-of）

acceptance の版 1。follow_up 由来の未完了 1 件、follow_up でない未完了 0 件。

- **task 436**（ready・source goal: goal 33・depth 0）runtime: related の paths の重なりを対称にし、glob 同士の交わりを正しく判定する
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (2) dagq related TASK が、paths の重なり・…から、関連の強い task を理由つきで並べる / (4) 2026-09-25 の重複の組のうち少なくとも半分が related の上位 5 件に出る
  - 理由: goal 33 (2) の paths の重なりを手がかりに理由つきで並べる機能は task 338 で入り、(4) は tests/it/related.rs と本番の複製（9 組中 8 組）で確かめ済みなので、点数の対称性と glob 同士の交わりの改善なしで acceptance を満たせる（受け入れ条件は精度の対称性を求めていない）。acceptance は ADR-0063 による置き換えの後も (1)〜(5) とも今の仕組みに当てはまり、古くなってはいない。goal 33 の未完了はこの task だけ。未実装（globs_overlap は今も片方を literal として glob_matches にかける）。
  - 証拠: receipt of run 1dc0d896-be9c-41c8-87db-afa57dc6ba70 (task 338) follow_ups[1]; event 8877; src/domain/related.rs:372 globs_overlap（a == b \|\| glob_matches(a, b) \|\| glob_matches(b, a)）; tests/it/related.rs の at_least_half_of_the_known_duplicates_are_in_each_others_top_five; docs/design/persistence.md 390 行（本番の複製で 9 組中 8 組が上位 5 件）
  - 所属先の案: new: related の手がかりの精度を上げる（paths の点数の対称性・glob 同士の交わり）（goal 33 から移す）

### goal 34: inbox を人の判断が要るものだけにし、それ以外のイレギュラーは runtime と復旧 job が片付ける

acceptance の版 1。follow_up 由来の未完了 10 件、follow_up でない未完了 0 件。

- **task 643**（ready・source goal: goal 34・depth 0）runtime: worker と planner の claude の argv に prompt 全文を載せず、prompt.txt を指す短い指示だけを渡す
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (1) 案の表の runtime の行が自動で直り、event に残る / (2) 復旧 job が許された操作で直すか、直せないときだけ inbox に上げる
  - 理由: goal 34 の acceptance は inbox に来るイレギュラーの自動の片付けと ask の分類・集計・文書で、worker・planner の argv に prompt を載せるか（pgrep -f が他の run の session に当たる問題）はどの項目にも当たらないので、この task なしで満たせる。未実装: provider-lifecycle.md の「promptの渡し方」は今も『workerとplannerのsessionとturn（command・planner_command・turn_command）は今もpromptを位置引数で渡す（task 643の範囲）』と書く。description は廃止された対話の ClaudeCode::command を前提に書かれており、非対話の turn_command に合わせて書き直しが要る。
  - 証拠: receipt of run d6dcc90b-64cb-48fb-a47d-2470540a5f64 (task 359) follow_ups[0]; event 17380; docs/design/provider-lifecycle.md の headless jobのinterface「promptの渡し方」（task 1560 で job だけ stdin にした）; commit 0c5f042e fix: headless の job の prompt を argv ではなくファイルか stdin で渡し…（job だけで worker/planner は対象外）; commit d751a0c9 runtime: 対話の worker の run を起こさないようにし…（ClaudeCode::command の対話の経路は消えた）
  - 所属先の案: goal 57（goal 34 から移す）
- **task 705**（ready・source goal: goal 34・depth 1）runtime: 全ての生きている run の alert の復旧 job の失敗が ask を開くようになった後、Live::ask_on_failure と failed_live の stalled の解除の規則を消す
  - 分類: 範囲外で別 goal（out_of_scope）
  - 記録: しない。cancel（判定を記録せずに。source goal 34 と同じ goal への out_of_scope は runtime が拒み、canceled の follow-up は goal を持たないので所属の判断は要らない。register.md の 5）
  - 項目: (2) 復旧 job が、許された操作で直すか、直せないときだけ inbox に上げる
  - 理由: flag の掃除で、goal 34 の acceptance (2) は復旧 job の失敗を ask にする task 562 の着地で満たされ、この task なしで言える。しかもこの task の受け入れ条件は task 562 の commit で既に満たされている（Live::ask_on_failure・fail_live・LiveStep::Failed は消え、domain::recovery の clears / failed_live の stalled の規則は『older runtime の recovery_failed を adopt で読むため』とコメントに残す理由がある。docs と plugin に ask_on_failure の言及は無い）。
  - 証拠: receipt of run 09f14674-453b-47b6-8ae4-2bbd7e6c40c2 (task 442) follow_ups[1]; event 20310; commit 98e986fd runtime: 生きている run の alert で復旧 job が失敗したら recover by hand の attention ではなくその alert の ask を開く（本文: Removed fail_live, LiveStep::Failed and Live::ask_on_failure; the failed_live reader … remain only for recovery_failed events an older runtime recorded）; src/domain/recovery.rs の failed_live の doc comment（it is still read for a run adopted from one）; grep で src・docs・plugins に ask_on_failure が無い
  - 所属先の案: 今の goal 34 のまま（移動不要）
  - 処置の案: 実装済みとして cancel の候補。根拠: task 562 の commit 98e986fd が Live::ask_on_failure・fail_live・LiveStep::Failed を消し、failed_live / clears の stalled の規則を adopt で古い event を読むために残す理由を src/domain/recovery.rs のコメントに書いた。受け入れ条件 (1)〜(5) が満たされているので cancel の候補（移す先は不要）
- **task 706**（ready・source goal: goal 34・depth 1）test: 非対話の run の stalled の復旧 job で、job が開いている run の adopt と、続く alert の job の直列化を確かめる
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (2) 復旧 job が、許された操作で直すか、直せないときだけ inbox に上げる
  - 理由: 非対話の stalled の復旧 job の adopt と直列化を確かめる test を足す task で、goal 34 の acceptance (2) の振る舞い（直すか、直せないときだけ inbox）は既存の実装と test（tests/it/runtime_headless_stall.rs の an_adopted_headless_stall_whose_job_waits_gets_no_nudge_job_or_ask など）で成り立っており、この追加の境界の test なしで満たせる。求める 2 つの case（job が開いているあいだの adopt、turn_without_receipt の後の permission_denied の直列化）は未実装。
  - 証拠: receipt of run 09f14674-453b-47b6-8ae4-2bbd7e6c40c2 (task 442) follow_ups[2]; event 20312; tests/it/runtime_headless_stall.rs の test 一覧（adopt中の job の重なりと permission_denied の続きの case は無い）
  - 所属先の案: goal 30（goal 34 から移す）
- **task 755**（ready・source goal: goal 34・depth 1）runtime: queue_hold の done で、affected に載った失敗した run の review と生きている run の復旧 job も起動し直す
  - 分類: 範囲外で別 goal（out_of_scope）
  - 記録: しない。cancel（判定を記録せずに。source goal 34 と同じ goal への out_of_scope は runtime が拒み、canceled の follow-up は goal を持たないので所属の判断は要らない。register.md の 5）
  - 項目: (2) 復旧 job が、許された操作で直すか、直せないときだけ inbox に上げる
  - 理由: description の前提（task 438 の後、壁で失敗した review と生きている run の復旧 job が ask も recover by hand も出ずに止まる）は task 438 の実装で起きない形になった: 壁の review は Phase::ReviewHeld で待ち控えが解けた pass でやり直し、生きている run の復旧 job は何も記録せず控えの後の pass で次の job が始まる。goal 34 の acceptance はこの task なしで満たせ、問題自体が無くなっている。
  - 証拠: receipt of run bb7c25d9-c7a0-4450-b19b-6b63555a8843 (task 437) follow_ups[1]; event 22563; commit 6fcdcd42 runtime: detect usage limits (cost) and auth errors from headless job output into the queue_hold asks（A review at a wall goes to Phase::ReviewHeld … reviewed again when the hold ends. A live-run recovery records nothing and the next job starts after the hold. test: two reviews at a login in one ask, landing after done）; docs/design/supervisor-lifecycle/queue-hold.md の「起動し直さないもの」
  - 所属先の案: 今の goal 34 のまま（移動不要）
  - 処置の案: 不要として cancel の候補。根拠: task 438 の commit 6fcdcd42 が、壁で止まった review を ReviewHeld で控えの後に自動でやり直し、生きている run の復旧 job を控えの後の pass で次の job にする形にしたので、done で起動し直す必要が無い（tests/it/runtime_queue_hold_detect.rs の login の 2 つの review が done の後に着地する test）。cancel の候補
- **task 758**（ready・source goal: goal 34・depth 1）runtime: begin_plan_review / begin_goal_review should not interrupt the job of a supervisor whose heartbeat is stale while its pid lives
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (1) 案の表の runtime の行が自動で直り、event に残る / (2) 復旧 job が…
  - 理由: plan review / goal review の行の supervisor の生存を heartbeat だけで判定して host の sleep 明けに二重起動する問題で、goal 34 の acceptance（inbox に来るイレギュラーの自動の片付け・ask の分類・集計・文書）のどの項目にも当たらず、この task なしで満たせる。未実装（plan_reviews.rs:516・goal_reviews.rs:361 は今も heartbeat の新しさだけで判定）。
  - 証拠: receipt of run 09e109f7-ef47-41f3-9764-5c4d0acdb815 (task 443) follow_ups[0]; event 22858; src/infrastructure/plan_reviews.rs:516 と src/infrastructure/goal_reviews.rs:361（heartbeat.is_some_and(\|at\| now - at <= HEARTBEAT_TIMEOUT_SECS) だけ）
  - 所属先の案: goal 74（goal 34 から移す）
- **task 780**（ready・source goal: goal 34・depth 0）stats: put a pending long_background ask in background_alert_secs
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (4) 自動で直した件数が stats に出て…
  - 理由: stats の stall_thresholds で pending の long_background の ask が idle_without_receipt_secs に数えられる分類の誤りで、goal 34 の (4)（自動で直した件数と ask の集計）とは別の指標なので、この task なしで満たせる。未実装（thresholds.rs の detections の ask_opened の分岐は idle_process だけを recovery_finished で振る）。stall の検知と閾値の stats は goal 30 の (3)(4) に合う。
  - 証拠: receipt of run 86dd72e0-e30f-464f-8ab7-c5a5e5beb263 (task 645) follow_ups[0]; event 24293; src/domain/stats/thresholds.rs の detections の "ask_opened" の分岐（idle_job は alert == idle_process だけ）
  - 所属先の案: goal 30（goal 34 から移す）
- **task 903**（ready・source goal: goal 34・depth 0）runtime: concurrent git worktree add by two supervisors in one repository collides on .git/worktrees/worktree
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (1) 案の表の runtime の行が自動で直り、event に残る
  - 理由: 同じ repository の 2 つの supervisor が同時に git worktree add して admin dir（.git/worktrees/worktree）で衝突する provisioning の不具合で、goal 34 の acceptance の案の表の行にも ask の分類にも当たらず、この task なしで満たせる。未実装（adapters.rs の worktree add に repository をまたぐ排他も一意の basename も無い）。合う既存の open goal が見当たらない（goal 74 は SQLite busy・integrating の lease・e2e の関門、goal 102 は片付けの CPU）。
  - 証拠: receipt of run 93e08770-a7ef-4717-988b-e068659ad8e0 (task 754) follow_ups[0]; event 32096; src/infrastructure/adapters.rs:1372・1473 の git worktree add（flock などの排他なし）
  - 所属先の案: new: 同じ repository で複数の supervisor が動くときの run の worktree の作成（provisioning）の排他（goal 34 から移す）
- **task 1015**（ready・source goal: goal 34・depth 2）runtime: 非対話の worker の session で、次の turn として送った入力（答え・hold の continue・stall の促しほか）の後、その turn が終わるまで前の turn の idle marker を idle と数えないことを、supervisor の引き継ぎ（adopt）の後も含めて確かめ、外れるところを直す
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (1) 案の表の runtime の行（送信の確認の漏れ、/exit の時間切れ、既知のダイアログ、needs_session の resume の引き継ぎ、古い receipt、衝突だけの resume）
  - 理由: 非対話の session で入力の後に前の turn の idle marker を idle と数えて促し・stalled の ask・復旧 job を重ねないことを確かめる task で、goal 34 (1) の案の表の行（送信の確認の漏れ・ダイアログなど）は対話の経路の項目で、対話の経路は task 1437 で消えた。この判断は stall の促しと stalled の ask の正しさ（goal 30 (1)）に属し、goal 34 の acceptance はこの task なしで満たせる。
  - 証拠: receipt of run 66983421-7494-4777-9260-03a8c30cf51e (task 870) follow_ups[0]; event 38492; commit d751a0c9 runtime: 対話の worker の run を起こさないようにし…（task 1437）
  - 所属先の案: goal 30（goal 34 から移す）
- **task 1321**（ready・source goal: goal 42・depth 1）ADR-t451-1 の実装の着地後に、ask の kind ごとの件数と人の答え待ち、AI が決めた land の後の差し戻し・revert を数えて前後を比べる
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: —（source goal の acceptance のどの項目もこれを求めていない）
  - 理由: goal 42 の acceptance (1)〜(3) は skill・AGENTS.md の方針、ADR-t451-1、実装 task の登録で、前後比較の測定は含まないので、この task なしで満たせる。既存の判定 11（out_of_scope、goal 34 へ）のとおりで、今も成り立つ。
  - 証拠: receipt of run 2893dc54-2218-40ee-93a7-8d8835ae7c94 (task 451) follow_ups[5]; event 59098; dagq goal show 42 --full の acceptance; dependencies 1319・1320・1389・1392 はすべて completed
  - 所属先の案: 今の goal 34 のまま（移動不要）
  - 既存の判定 11: 今も成り立つ。goal 42 の acceptance に測定は無く、ask の件数と答え待ちの前後比較は goal 34 (4)（inbox に来る件数の変化）に合う。現 goal は既に 34 で移動不要。needs_recheck は false
- **task 1386**（ready・source goal: goal 42・depth 2）show に task とその run の ask を並べる asks 欄を足し、recommendation と confidence を載せる（ADR-t451-1 決定 1 の表示）
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: —（source goal の acceptance のどの項目もこれを求めていない）
  - 理由: goal 42 の acceptance (1)〜(3) は方針・ADR・実装 task の登録で、show に asks 欄を足す表示は含まないので、この task なしで満たせる。既存の判定 12（out_of_scope、goal 34 へ）は今も成り立つ。未実装（src に asks_total が無い）。
  - 証拠: receipt of run 6f7ea894-b95f-4f36-93ba-97d6d1ce3dc9 (task 1318) follow_ups[0]; event 62742; dagq goal show 42 --full の acceptance; grep で src に asks_total が無い
  - 所属先の案: 今の goal 34 のまま（移動不要）
  - 既存の判定 12: 今も成り立つ。show の asks 欄は inbox が task 単位で ask と推奨を見るためで goal 34 に合う。現 goal は既に 34。needs_recheck は false

### goal 36: 並列数を上げて得をできるようにする: run の build を安全に共有し、着地待ちを内訳で見て長い待ちを削る

acceptance の版 1。follow_up 由来の未完了 3 件、follow_up でない未完了 1 件。

- **task 482**（ready・source goal: goal 36・depth 0）runtime: refresh run_env_program_missing when the set of missing programs changes
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (1) build の共有方式が ADR-0049 で決まり…有効になり / (4) 並列数を上げるかどうかを人が判断できる材料が揃っている
  - 理由: goal 36 の acceptance は build の共有・wait_to_land の内訳・待ちを削る判断・並列数の材料で、ツールの無い host での振る舞い（constraints）は task 395 が満たした。見つからない変数の集合が変わったときに attention の文が古いままになるのは表示の改善で、無くても acceptance を満たしたと言える。
  - 証拠: receipt of run d429aad0-476d-4cdd-af0c-450aa86d7f4b (task 395) follow_ups[1]; event 10311; src/domain/run_env.rs 79 行目 transition（found→missing でしか記録しない。未実装を確認）
  - 所属先の案: new: [run.env] のツールの検査と attention・doctor の表示を今の状態に合わせる（goal 36 から移す）
- **task 1382**（ready・source goal: goal 86・depth 1）runtime: 待ちと着地待ちの run の worktree の target の大きさを記録して、待つ間に抱える disk を読めるようにする
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (2) ゼロベースへ移る時期を判断する計測の項目・基準値・条件の案が文書にある / (4) disk の空きと非対話の健全性を日々読める
  - 理由: source goal 86 の (2) は計測の項目の案が文書にあることを求め、E4 は『取れない項目』として文書にある。(4) は disk の空き（task 1371）で、待つ run の target の大きさの記録は無くても満たせる。
  - 証拠: receipt of run 57151b60-d708-4c4b-9edc-a37573e7909c (task 1369) follow_ups[0]; event 62732; docs/plans/zero-based-headless-readiness.md の項目 E4; dagq goal show 86 --full の acceptance
  - 所属先の案: 今の goal 36 のまま（移動不要）
  - 既存の判定 7: 今も成り立つ。goal 86 の acceptance に要らないことと、待つ run が抱える資源として goal 36（並列数と着地待ち）に置くことは今も成り立つ。needs_recheck は false
- **task 1625**（ready・source goal: goal 36・depth 1）measure: claim の間隔（task 1479、ADR-t1479-1）の着地の前後で、load_average の保留と claim 後 6 分の load1 の最大を比べ、finding 36 を resolve できるかを docs/plans/ に書く
  - 分類: 元 goal に必須（required）
  - 項目: (4) 並列数を上げるかどうかを人が判断できる材料（load・build 時間・着地待ち）が揃っている
  - 理由: source task 1479 は goal 36 の task として claim の間隔で load の振る舞いを変えたので、(4) の load の材料は変えた後の値で揃える必要がある。この測定は 1479 の前後の load_average の保留と claim 後の load1 を比べるもので、無いと変えた後の load の材料が無く (4) を満たしたと言えない。
  - 証拠: receipt of run 2aa0ae42-617e-498a-b9b4-7f0ed7e98bca (task 1479) follow_ups[1]; event 78811; commit eeb575c6 runtime: load の保留が有効なとき新しい claim の間を空け…（ADR-t1479-1）; docs/plans/ に claim-spacing.md はまだ無い
  - 所属先の案: source goal に残す

### goal 37: Keep integration verification from failing on host load and disk, not on the code

acceptance の版 1。follow_up 由来の未完了 3 件、follow_up でない未完了 0 件。

- **task 773**（ready・source goal: goal 37・depth 0）landing recheck: don't send a run held for a host failure (integration_held) to a resume when the recheck command fails on the host
  - 分類: 判断できない（undecided）
  - 項目: An integration verification that fails because of disk space or a timing-sensitive test under load does not resume the worker session
  - 理由: acceptance は「integration verification」の環境の失敗が resume しないことを求める。着地の先回り検査（[recheck]、ADR-0068）は本番の dagq.toml で有効で、その環境の失敗は今も park_rechecked で resume に回るが、recheck を acceptance の integration verification に含めるかは goal の意図による（description の動機の resume のコストには当たる）。
  - 証拠: receipt of run d6018610-c66b-45f4-a571-c9f8b678cf84 (task 639) follow_ups[0]; event 23766; src/application/supervise/recheck.rs:680 CheckFailed は分類を持たず、633 で Error になる（未実施）; dagq.toml:30 の [recheck] が有効
  - 人の確認: 要る。landing recheck を goal 37 の「integration verification」に含めるかの goal の意図の判断が要る（含めるなら required、含めないなら out_of_scope で別 goal）。
- **task 1283**（ready・source goal: goal 37・depth 1）test: 着地の検証が host の分類（killed か timeout）で落ち、その host のやり直しが FLAKY だけで落ちたとき、NEXTEST_FLAKY_RESULT=pass のやり直しで着地することを確かめる
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: it is retried or reported with its cause, and stats or the run record show that cause
  - 理由: host の失敗のやり直し（task 639）と flaky のやり直し（task 1039）は実装と test 済みで acceptance を満たす。この task は 2 つが続く経路の test を 1 本足すだけで、acceptance はこの組み合わせの test を求めない。
  - 証拠: receipt of run e87ccfef-cf34-48a3-a407-8c3cd40ff7ca (task 1039) follow_ups[0]; event 57687; tests/it/runtime_verify_retry.rs（host のやり直しの test 群）・runtime_verify_flaky.rs:237 a_flaky_retry_keeps_coverage_and_host_failures（逆向きの経路のみ。未実施）
  - 所属先の案: new: 着地の検証のやり直し（host・flaky）の経路の test の補強（goal 37 から移す）
- **task 1344**（ready・source goal: none（出どころの goal 無し）・depth 0）wait_for_background in runtime_review_background has a 30s cap no runtime limit backs
  - 分類: 範囲外で別 goal（out_of_scope）
  - 記録: しない。cancel（判定を記録せずに。登録時の source goal が none なので judge-follow-up の対象外で、runtime も拒む）
  - 項目: tests/runtime.rs has no fixed wall-clock wait that fails at the load avg seen in this queue
  - 理由: source goal は null のため今の goal 37 で読んだ。対象の tests/it/runtime_review_background.rs（wait_for_background の 30 秒の上限）は対話の run の test とともに削除されており、この task なしで acceptance に反する待ちは残っていない。
  - 証拠: receipt of run 5328601b-11ed-451e-b732-f4b38036d4d4 (task 1023) follow_ups[0]; event 60571; commit d751a0c9 runtime: 対話の worker の run を起こさないようにし、…それだけを確かめる integration test を消す（tests/it/runtime_review_background.rs を削除）; grep: wait_for_background・background_work_holds_the_revise_until_it_ends は tests/・src/ に 0 件
  - 所属先の案: 今の goal 37 のまま（移動不要）
  - 処置の案: 不要として cancel の候補。根拠: 対象のファイル tests/it/runtime_review_background.rs と 3 つの呼び出し元の test が commit d751a0c9 で削除され、直す対象が無い（cancel の候補）。
  - 採用の注意: source_goal_id が null（出どころの goal が不明）なので、ADR-t808-1 により採用には人の adopt が要る（所属の判定とは別）。

### goal 39: Keep runs that wait for a person or a recover from going stale against main

acceptance の版 1。follow_up 由来の未完了 2 件、follow_up でない未完了 2 件。

- **task 1514**（ready・source goal: goal 39・depth 1）本番の stats で landing recheck の件数と負荷を前後で確かめる
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: stats shows how many landings conflicted after a wait, compared with landings that did not wait
  - 理由: goal 39 の acceptance は待った run の rebase の確かめと記録と、待った/待たなかった着地の衝突数の比較（task 1312）を求める。ADR-t1310-1 の recheck のきっかけ拡大による host の負荷の前後測定は含まず、実施しなくても満たせる。
  - 証拠: receipt of run f0e8b6ab-6aa1-4080-9784-2e3d59648b28 (task 1310) follow_ups[0]; event 69463; task 1514 の context（task 1312 とは測るものが違う）
  - 所属先の案: goal 72（goal 39 から移す）
- **task 1515**（ready・source goal: goal 39・depth 1）runtime_waiting::a_returning_run_counts_toward_the_limit が負荷の下で run_dir の unwrap で落ちる
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (acceptance 全体) 待った run の rebase の確かめ・承認後の着地・stats の比較
  - 理由: 負荷の下での test の run_dir の unwrap の待ち不足は goal 39 の機能の acceptance に関係せず、実施しなくても満たせる。負荷で落ちる test を直す問題は goal 37 に合う。
  - 証拠: receipt of run f0e8b6ab-6aa1-4080-9784-2e3d59648b28 (task 1310) follow_ups[1]; event 69465; tests/it/runtime_waiting.rs:280 に second.run_dir().unwrap() が残る（未実施。依存 1435 は completed、1439 は ready）
  - 所属先の案: goal 37（goal 39 から移す）

### goal 40: dagq の流れと worker の時間を KPI として時系列で測り、人が HTML と push で確かめられ、KPI の悪化から改善の proposal が上限付きで自動で回る

acceptance の版 1。follow_up 由来の未完了 7 件、follow_up でない未完了 0 件。

- **task 661**（ready・source goal: goal 40・depth 0）kpi: candidates の標本の有効区間を supervisor の停止で切る
  - 分類: 元 goal に必須（required）
  - 項目: (2) dagq kpi が ADR どおりの KPI を期間・種類・変更の印の前後で出し
  - 理由: ADR-0051 決定 3 の candidates の starved は「空き slot があるのに candidates が 0 で ready が残っていた時間」で、決定 10 は stale の supervisor の区間を最後の heartbeat で閉じると決めている。今の window.rs の candidates() は最後の標本を now まで有効にし、supervisor の居ない停止中の時間も starved_secs と平均に入るので、この follow-up なしでは (2) の「ADR どおりの KPI」を満たしたと言えない。
  - 証拠: receipt of run 2ea99280-ed1b-4d84-b1e1-a2e31deed83b (task 570) follow_ups[0]; event 18404; src/domain/kpi/window.rs の fn candidates（1405 行付近、until = 次の標本か self.now、supervisor の区間で切らない）; docs/adr/0051-kpi-time-series-report-and-push.md 決定 3（113 行）・決定 10（159 行）
  - 所属先の案: source goal に残す
- **task 663**（ready・source goal: goal 40・depth 0）marks: a pruned supervisor_stopped mark at last_heartbeat_at
  - 分類: 元 goal に必須（required）
  - 項目: (2) dagq kpi が ... 変更の印の前後で出し
  - 理由: ADR-0051 決定 10 は supervisor の停止を記録する印とし、stale の supervisor は最後の heartbeat で区間を閉じると決めている。pruned の supervisor_stopped の印の時刻が今は prune の時刻（created_at）のままで、印の前後比較の区切りが数時間ずれうるため、この follow-up なしでは (2) の印の前後の比較が ADR どおりと言えない。
  - 証拠: receipt of run 565675bb-486b-41a7-b39e-5bbf5780ff1e (task 571) follow_ups[0]; event 18504; src/domain/marks.rs の marks()（220〜235 行、at は payload の at か created_at で last_heartbeat_at を見ない）; docs/adr/0051-kpi-time-series-report-and-push.md 決定 10（152・159 行）
  - 所属先の案: source goal に残す
- **task 700**（ready・source goal: goal 40・depth 1）kpi: improvement_proposals を event から数えて出す（ADR-0051 決定 25 の数え方）
  - 分類: 元 goal に必須（required）
  - 項目: (2) dagq kpi が ADR どおりの KPI を ... 出し
  - 理由: ADR-0051 の KPI の表は improvement_proposals（改善の群、決定 25 の数え方、参考）を KPI として定めているが、window.rs は値を固定で null・unavailable not_recorded にしている。ADR の表にある KPI が常に出ないので、この follow-up なしでは (2) の「ADR どおりの KPI を出し」を満たしたと言えない。
  - 証拠: receipt of run aefa7b15-3f68-4e65-b218-92e8b7fccb1b (task 433) follow_ups[0]; event 20190; src/domain/kpi/window.rs 774〜775 行（put("improvement_proposals", ALL, Measure::total(None, 0)) と NOT_RECORDED）; docs/adr/0051-kpi-time-series-report-and-push.md の KPI の表（99 行）と決定 25; docs/design/supervisor-lifecycle/kpi.md 128 行
  - 所属先の案: source goal に残す
- **task 714**（ready・source goal: goal 40・depth 1）runtime: forecast の snapshot のきっかけから、parallel の変わらない引き継ぎの supervisor_started を外す
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (1)〜(6) のいずれも forecast の snapshot のきっかけに触れない
  - 理由: forecast の snapshot は ADR-0070 の仕組みで、goal 40 の acceptance（ADR-0051 の KPI・レポート・push・observer・文書）はそのきっかけの細部を求めない。今の実装は ADR-0070 決定 3 (b) のとおりすべての supervisor_started を数えており、handoff を外すのは標本のノイズを減らす改善なので、実施しなくても goal 40 の acceptance を満たせる。
  - 証拠: receipt of run 44aba3e8-b7e7-43de-afc5-f2c3b29bb7db (task 475) follow_ups[0]; event 20864; src/domain/forecast/snapshot.rs の trigger()（QUEUE_MARK_KINDS に SUPERVISOR_STARTED、handoff の区別なし。未実装）; goal 40 の acceptance (1)〜(6)
  - 所属先の案: new: KPI・日次レポート・完了見込み（forecast）・依存図の後回しの改善（goal 40 の範囲外。goal 106 のラベルができたらそのラベルの受け皿の goal）（goal 40 から移す）
- **task 720**（ready・source goal: goal 40・depth 1）dagq.toml の [kpi.targets] に完了見込みの答え合わせの初めの目標（forecast.p50_error_ratio・forecast.p90_hit_rate）を足す
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (2) ... 目標と比べる
  - 理由: (2) の目標との比較は ADR-0051 の KPI について task 572 が dagq.toml の [kpi.targets] に 7 つの目標を入れて満たしている。forecast.* の目標は ADR-0070 の案で goal 40 の acceptance の KPI ではないので、足さなくても goal 40 の acceptance を満たせる。
  - 証拠: receipt of run 963383cf-b0c1-48a7-bed6-e37643c4b0e4 (task 476) follow_ups[1]; event 21065; dagq.toml の [kpi.targets]（first_pass_rate〜asks_per_landing の 7 つ、forecast.* は無い。未実装）; task 572 completed
  - 所属先の案: new: KPI・日次レポート・完了見込み（forecast）・依存図の後回しの改善（goal 40 の範囲外。goal 106 のラベルができたらそのラベルの受け皿の goal）（goal 40 から移す）
- **task 765**（ready・source goal: goal 40・depth 1）runtime: 依存図で、描く member の無い goal を待つ task に「goal N 待ち」を示す
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (3) supervisor が日次で ... HTML と JSON のレポートを書き
  - 理由: 依存図（ADR-0077、task 532〜534）は goal 40 の acceptance の項目に無く、(3) は HTML と JSON のレポートを書くことだけを求める。member の無い goal を待つ task の表示は図の見やすさの改善なので、実施しなくても goal 40 の acceptance を満たせる。
  - 証拠: receipt of run 387e5e57-43a9-4301-babf-f8c0c699ef30 (task 533) follow_ups[1]; event 23204; src/application/diagram.rs 292〜295 行（members.get(&goal) が None なら continue。未実装）; task 764 completed
  - 所属先の案: new: KPI・日次レポート・完了見込み（forecast）・依存図の後回しの改善（goal 40 の範囲外。goal 106 のラベルができたらそのラベルの受け皿の goal）（goal 40 から移す）
- **task 1001**（ready・source goal: goal 40・depth 2）runtime: 日次・週次の KPI レポートの HTML に、期間ごとの host の負荷（load1 と cpu_total の平均・最大・p90）の表の節を足す
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (3) supervisor が日次で queue のディレクトリに HTML と JSON のレポートを書き
  - 理由: (3) は日次の HTML と JSON のレポートとその履歴を求めるだけで、host の負荷（KPI ではない参考の値、task 872 で JSON には載る）を HTML に出すことは求めない。実施しなくても goal 40 の acceptance を満たせる。
  - 証拠: receipt of run bf67ed21-1dd1-4d2f-8dd5-a33752bee41f (task 872) follow_ups[0]; event 37327; src/domain/kpi/report/html.rs の host_cpu（task 992 の Host CPU 節は cpu_per_landing と load_per_core だけで、load1・cpu_total の平均・最大・p90 の表は無い。未実装）
  - 所属先の案: new: KPI・日次レポート・完了見込み（forecast）・依存図の後回しの改善（goal 40 の範囲外。goal 106 のラベルができたらそのラベルの受け皿の goal）。代わりに goal 72（スループットの制約を常時見る数値、992 の Host CPU 節を持つ）も候補（goal 40 から移す）

### goal 45: よく衝突するファイルでの rebase 衝突を減らし、着地待ちを短くする

acceptance の版 1。follow_up 由来の未完了 1 件、follow_up でない未完了 0 件。

- **task 1628**（ready・source goal: goal 45・depth 1）planner_screen_idle の person の planner の test も、heartbeat が本当の時刻のまま時計を 23 秒先に進める
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (1)〜(3) は migration・hotspot・conflict_hotspots だけ
  - 理由: goal 45 の acceptance は migration の衝突と hotspot の控えだけで、planner_screen_idle の test の時計は関係しないので、実施しなくても満たせる。さらに人の planner は廃止され、task 1577（ADR-t1433-2 決定 5）と 1441 が人の planner と画面の idle の経路を消すので、この test 自体が消えるか意味を失う見込みが高い。
  - 証拠: receipt of run 029cc29d-5584-4cc1-94ac-069822f33b4e (task 775) follow_ups[0]; event 79464; tests/it/planner_screen_idle.rs:273 a_persons_planner_shows_idle_by_its_screen_and_is_not_asked_to_exit（まだある）; task 1577 の acceptance（人の planner の exited_notice と猶予が src に無い）; goal 92 の acceptance (2)（対話に固有の画面の処理とそれだけを確かめる test が消えている）
  - 所属先の案: goal 92（goal 45 から移す）
  - 処置の案: 不要として cancel の候補。根拠: 人の planner は ADR-t1394-1 で廃止され、task 1577・1441（goal 92）が人の planner の行を cmux なしで閉じた扱いにし、画面の idle の経路を消す。1577 の着地の後にこの test が残っていなければ cancel してよい（1628 は 1577 に依存しているので、その時に確かめる）。

### goal 48: actor の名前を 1 つに揃え、inbox を唯一の入口にし、用件ごとの desk を新設して、用件と plan の時間を測れるようにする

acceptance の版 1。follow_up 由来の未完了 2 件、follow_up でない未完了 4 件。

- **task 1563**（ready・source goal: goal 87・depth 1）status の inbox 向けの欄に、planner を待っている open の依頼を出す
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (2) inbox が人の言葉で計画を依頼すると runtime の planner が立って…進み
  - 理由: goal 87 の (2) は依頼から submit か却下まで進むことで、待っている依頼を status に出す欄は ADR-t1394-1 の Consequences の残りの便宜。依頼は dagq requests で読め、無くても acceptance は満たせる。
  - 証拠: receipt of run 20a43f22-ab42-4d93-8af6-b6a758a089fa (task 1395) follow_ups[1]; event 72513; docs/design/supervisor-lifecycle/plan-planners.md「予定: inboxからの依頼と非対話のplanner」の 3 が未実装のまま
  - 所属先の案: 今の goal 48 のまま（移動不要）
  - 既存の判定 15: 今も成り立つ。判定は今も成り立つ。
- **task 1646**（ready・source goal: goal 87・depth 1）planner request（task 1533 の続きの依頼）にも --text-file と --text - の入力を足す
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (2) / (5)
  - 理由: goal 87 の acceptance は依頼の経路と planner の非対話化・評価・skill で、planner request の --text-file / --text - の入力は要らない。
  - 証拠: receipt of run 350d0486-d941-44c7-aaa8-6b30da3c51c7 (task 1539) follow_ups[0]; event 80651; src/main.rs 1699〜1711 行の PlannerCommand::Request が今も --text / --file だけ: 未実装
  - 所属先の案: 今の goal 48 のまま（移動不要）
  - 既存の判定 16: 今も成り立つ。判定は今も成り立つ。

### goal 51: C: task の重さの予測を記録し、model / effort の選択を試せるようにする

acceptance の版 1。follow_up 由来の未完了 2 件、follow_up でない未完了 0 件。

- **task 601**（ready・source goal: goal 51・depth 0）task に由来する手戻りの率の基準値を母集団で出し直す
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: run の実績（... task に由来する手戻り）と並べて stats か kpi で読める / 役割ごとの model / effort を dagq.toml で設定でき ... effort が 1 段上がる
  - 理由: goal 51 の acceptance は手戻りを stats か kpi で読めることと設定・段上げの仕組みを求め、変更の前の手戻りの率の基準値を文書と note に残すことは求めない（効果の比較と既定の変更は description (d) で別の task）。実施しなくても acceptance を満たせる。
  - 証拠: receipt of run e2986107-c215-4d0c-af4c-448d9059a047 (task 574) follow_ups[0]; event 15303; docs/plans/spike-predictor-replay.md に基準値の節が無い（未実装）; task 578・580 completed; goal 51 の acceptance（event 14557 の版。保存された本文が「下位…」で切れている）
  - 所属先の案: new: goal 51 の model / effort の変更（段上げ・役割ごとの設定・試し）の効果の評価（基準値と前後比較）（goal 51 から移す）
- **task 631**（ready・source goal: goal 51・depth 0）runtime: planner の session 区間を proposal に結び付け、その model / effort を計画の品質の層に足す（ADR-0079）
  - 分類: 元 goal に必須（required）
  - 項目: worker 以外のアクターの session の model / effort が記録され、計画の品質の指標を model / effort と proposal の特徴で層別して読める
  - 理由: ADR-0079 決定 7 は計画の品質を「proposal を判断した session（plan review の job と、proposal を作り直した planner）」の model / effort で層別すると決め、task 579 は plan review の job の分だけを入れた。planner の分が無いと acceptance の層別が ADR の定めた形に足りず、差し戻しの段上げ（580）の効果も読めないので、この follow-up なしでは満たしたと言えない。
  - 証拠: receipt of run bfd79180-d24c-482e-b96b-e279742a689a (task 579) follow_ups[0]; event 16490; task 387・580 completed; src/domain/plan_quality.rs に planner の model / effort の層が無い（task の description による。未実装）
  - 所属先の案: source goal に残す

### goal 52: dagq を他の repository で使えるようにする（dogfooding だから回っている前提を取り除く）

acceptance の版 1。follow_up 由来の未完了 5 件、follow_up でない未完了 0 件。

- **task 668**（ready・source goal: goal 52・depth 1）plugin: dagq-recover の push の手順を git push origin main の固定から doctor の repository の branch と remote に変える
  - 分類: 元 goal に必須（required）
  - 項目: (3) 配布物（plugins/claude-dagq）... に、dagq の repository に固有の規則 ... が残っていない
  - 理由: plugins/claude-dagq の dagq-recover は今も push_failed の手当てに `git push origin main` を打たせ、integrate が main を origin へ push すると書いており、goal 52 の description (1) が取り除く対象に挙げた main・origin の固定が配布物に残っている。master や origin の無い repository で誤った手順になるので、この follow-up なしでは (3) を満たしたと言えない。
  - 証拠: receipt of run a71e73a8-4297-4511-b0e0-8d66aff08dad (task 619) follow_ups[1]; event 18593; plugins/claude-dagq/skills/dagq-recover/SKILL.md:54 と reference/review-by-hand.md:23,44 の git push origin main（未実装）; goal 52 の description (1)・解決後
  - 所属先の案: source goal に残す
- **task 688**（ready・source goal: goal 52・depth 1）release skill と README の Release: リリースの変更で marketplace の ref を上げ、着地後すぐに tag を push する（ADR-t617-1）
  - 分類: 判断できない（undecided）
  - 項目: (5) plugin の公式の配布の手順と plugin とバイナリの version の合わせ方 ... が ADR で決まり、実装が task になっている
  - 理由: (5) の「実装が task になっている」が、task として登録されていれば足りるのか、実装の着地まで要るのかで結論が分かれる。前者なら ADR-t617-1 は accepted・検査（687）は着地済みで、release skill の手順は人が出すリリースの時の作業なので範囲外、後者なら marketplace を release tag に合わせる手順はその決定の実装の一部で必須になる。根拠（acceptance・constraints の「cargo の新しい version は準備ができたら人が出す」）だけでは goal の意図が決まらない。
  - 証拠: receipt of run 47397081-4331-497d-a39e-744da643b460 (task 617) follow_ups[1]; event 19542; .claude/skills/release/SKILL.md に marketplace の ref と使い捨ての HOME での plugin install の確認が無い（未実装）; .claude-plugin/marketplace.json は source ./plugins/claude-dagq のまま; README.md に Release の見出しが無い（release の記述は ## Developing dagq）。acceptance の「README の Release」は今の README と合わないので task の書き直しも要る
  - 人の確認: 要る。goal 52 の acceptance (5) の「実装が task になっている」の意図（登録で足りるか、着地まで要るか）を人に確かめる必要がある。747 も同じ問いに依る。
- **task 747**（ready・source goal: goal 52・depth 0）plugin: README・dagq-recover の update.md・dagq-inbox に外部の project のリリースの更新（approve_release・install --release・手での更新）を書く
  - 分類: 判断できない（undecided）
  - 項目: (5) ... 外部の project の更新の仕組みが ADR で決まり、実装が task になっている / (6) README が ... 今の導入手順を書いている
  - 理由: (1) の README の部分は commit 8945049a で Update 節（host.toml の [update]、approve_release、install --release、手での更新）として既に書かれている。残る dagq-recover の update.md と dagq-inbox の approve_release の案内が (5) に要るかは、688 と同じく「実装が task になっている」が登録で足りるか着地まで要るかに依り、goal の意図が要る。
  - 証拠: receipt of run 30d07ecb-7b6b-40b3-8eeb-51dcfca19018 (task 618) follow_ups[3]; event 21822; commit 8945049a docs: README を cargo install と公式の手順の plugin から始まる利用者向けの導入手順に書き直す（README の ## Update 節に (1) の内容がある）; plugins/claude-dagq に approve_release・install --release の記述が無い（grep で 0 件）
  - 人の確認: 要る。688 と同じく goal 52 の acceptance (5) の意図を人に確かめる必要がある。範囲を残すなら、README の部分は実装済みなので task の (1) を外す edit が要る。
- **task 1156**（ready・source goal: goal 52・depth 0）runtime: 今の build ですでにその版のバイナリの install・retry の答えを、黙って捨てずに理由つきの update_dropped にする
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (5) ... 外部の project の更新の仕組みが ADR で決まり、実装が task になっている
  - 理由: 外部の project の更新の仕組みは ADR-t618-1・t618-2 で決まり、task 744〜746・868 で実装が着地している。この follow-up は、すでにその版の supervisor に届いたバイナリの頼みが何も残さない縁の場合を理由つきの update_dropped にする改善（receipt の category improvement）なので、(5) のどちらの読み方でも実施しなくても満たせる。
  - 証拠: receipt of run bcd24f98-843c-4d81-8c2e-7163d9334832 (task 868) follow_ups[0]; event 46226; task 744・745・746・868 completed; src/application/release_update.rs の dropped_requests は plugin だけの頼みを扱う（task の description の記述。未実装）
  - 所属先の案: goal 32（goal 52 から移す）
- **task 1483**（ready・source goal: goal 52・depth 1）docs: README の plugin の節から v0.4.0 の前提の「Until then ... main」の文を除き、リリースの前後どちらでも正しい形にする
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (6) README が cargo install と公式の plugin の手順から始まる今の導入手順を書いている
  - 理由: README.md:33 の「Until then the marketplace serves the plugin on main」は v0.4.0 のリリースの前の今は正しく、(6) は今の導入手順を求めるので、この文を直さなくても今の README は (6) を満たす。古くなるのは人が出すリリースの時（constraints: cargo の新しい version は人が出す）なので、リリースの準備の範囲。
  - 証拠: receipt of run 371c15a0-5d96-48f8-9b84-091a815d7f2b (task 630) follow_ups[0]; event 68554; README.md:33（Until then の文が残る。未実装）; Cargo.toml version = "0.4.0-dev"
  - 所属先の案: new: dagq v0.4.0 のリリースの準備（README の marketplace の文・release skill の marketplace の ref の手順）。688 を範囲外とした場合も同じ置き先（goal 52 から移す）

### goal 55: actor を明示し、default deny の capability 認可を application の境界に入れて、信頼する制御側と信頼しない AI actor を分ける（host 実行のまま）

acceptance の版 1。follow_up 由来の未完了 4 件、follow_up でない未完了 0 件。

- **task 784**（ready・source goal: goal 55・depth 1）runtime: observe と auto-update の子プロセスが書く event の actor が supervisor であることを test で固定する
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: 状態を変える event が actor（role と id）を記録する
  - 理由: task 730 が observe と自動更新の子プロセスに supervisor の actor の env を渡したので、event の actor の記録は実装済みで acceptance を満たす。この task はそれを固定する回帰 test を足すだけで、acceptance は test を求めていない。
  - 証拠: receipt of run f29341a3-4655-43f4-89f5-5b0fdd42b4fc (task 730) follow_ups[2]; event 24515; task 730 の context と description（src/application/supervise/mod.rs・update.rs の supervisor_actor().env()）; tests/it/runtime_actor_env.rs・runtime_observer.rs・cli_version.rs に observe/update の event の actor を assert する test は無い（grep、未実施）
  - 所属先の案: new: actor と認可（goal 55）の後続の回帰 test と権限の文書の整合（goal 55 から移す）
- **task 1152**（ready・source goal: goal 55・depth 2）docs: authorization.md・security.md・roles.md・AGENTS.md の「4つのjob」を 5 つの job に直す
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: docs/design にセキュリティの文書があり、host 実行が sandbox ではないことと Podman への移行の道筋を書いている
  - 理由: acceptance はセキュリティの文書があり sandbox でないことと移行の道筋を書くことまでで、job の数の言い回し（4つ→5つ）の整合は求めていない。文書は既にあるので、実施しなくても満たせる。
  - 証拠: receipt of run ab21fc74-a8cc-410d-9bda-4c7e5f8d8370 (task 859) follow_ups[2]; event 46127; grep: docs/design/security.md:67,94・authorization.md:127,131,154,200,230・supervisor-lifecycle/roles.md:100 に「4つのjob」が残る（未実施）; src/domain/actor.rs:117 is_headless_job は 5 つの job
  - 所属先の案: new: actor と認可（goal 55）の後続の回帰 test と権限の文書の整合（goal 55 から移す）
- **task 1564**（ready・source goal: goal 87・depth 1）--request の planner_question を、その依頼の planner 自身だけが開けるようにする
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (1)〜(5) のどれも依頼の planner_question を開ける actor の制限を求めない
  - 理由: goal 87 の acceptance は廃止と移譲の経路・非対話の planner・評価・文書を求め、--request の planner_question を開ける planner の resource の制限は含まないので、実施しなくても満たせる。
  - 証拠: receipt of run 20a43f22-ab42-4d93-8af6-b6a758a089fa (task 1395) follow_ups[2]; event 72515; goal 87 の acceptance (1)〜(5); src/domain/authorization.rs:180,222 の Resource::NewAsk は kind・run・task だけで request を持たない（未実施）
  - 所属先の案: 今の goal 55 のまま（移動不要）
  - 既存の判定 13: 今も成り立つ。goal 87 の acceptance に要らないことは今も成り立つ（未実施を確かめた）。移し先の goal 55 の acceptance でも必須とは言いにくく（Authorizer の default deny と列挙の deny test が対象）、goal 55 を閉じるときは範囲外として残る点に注意。
- **task 1647**（ready・source goal: goal 87・depth 1）runtime: request add で、権限を持たない actor を --text-file や --text - の入力を読む前に authorization_denied で拒み、planner request が読む前に拒む順を回帰テストで守る
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (1)〜(5) のどれも request add の認可と入力読み込みの順を求めない
  - 理由: goal 87 の acceptance (2) は inbox からの依頼で planner が立つ流れを求めるが、権限の無い actor を入力の前に拒む順は含まない。実施しなくても満たせる。
  - 証拠: receipt of run 350d0486-d941-44c7-aaa8-6b30da3c51c7 (task 1539) follow_ups[1]; event 80653; goal 87 の acceptance (2); src/main.rs:1822 request_words が stdin・file を読み、2241 で Command::Request は authorized_in_application（未実施）
  - 所属先の案: 今の goal 55 のまま（移動不要）
  - 既存の判定 14: 今も成り立つ。goal 87 の acceptance に要らないことは今も成り立つ。goal 55 の「状態を変える全ての CLI コマンドが Authorizer を通ってから mutation する」も mutation 前の検査は満たしており、必須ではない。

### goal 56: session の終わりを idle の印だけに頼らず検知し、印が書けなくても planner・worker が放置されないようにする

acceptance の版 1。follow_up 由来の未完了 3 件、follow_up でない未完了 0 件。

- **task 900**（ready・source goal: goal 56・depth 2）runtime: 非対話の runtime の planner の時間切れ（planner_unresponsive）の reason と payload を、画面ではなく turn の記録から原因（止められた turn か、idle のまま終わらない仕事か）を区別して伝える
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (2) …画面も読めないまま動きが無ければ inbox に知らせる
  - 理由: (2) の知らせは対話の planner では tell_of_silent_planners、非対話の planner では tell_of_stopped_planner_turns（turn・outcome つき）で既に出ており、この task は知らせの原因の区別と文面・payload の改善なので、無くても (2) は満たせる。
  - 証拠: receipt of run ec247db3-67a2-49fb-99e6-5546a60be05e (task 823) follow_ups[0]; event 31680; src/application/supervise/planner_turns.rs 195〜232 行（turn・outcome は持つが cause・turns/ の path は無い）: 一部だけ; src/application/supervise/plan_review.rs 1096 行の『no idle marker, no idle screen』の文面は対話の planner 用に残る; docs/design/supervisor-lifecycle/plan-planners.md 57 行（idle のまま期限切れは revise の時間切れだけが見る）
  - 所属先の案: new: 非対話の runtime の planner・worker の止まり方を turn と wrapper の記録から原因つきで残し知らせる（1384・1411 もこの題に合う）（goal 56 から移す）
- **task 1384**（ready・source goal: goal 86・depth 1）runtime: wrapper の process が死んでいたときも wrapper_heartbeat_expired を残し、run_waiting_ended に待っていた ask の ID を残す
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (5) 待ちの最中に wrapper を失った非対話の run が開き直され…
  - 理由: goal 86 の (5) は開き直しそのもの（task 1372 で着地）で、wrapper_heartbeat_expired の記録の欠けと run_waiting_ended の ask の ID は計測の記録の改善なので、無くても acceptance は満たせる。
  - 証拠: receipt of run 57151b60-d708-4c4b-9edc-a37573e7909c (task 1369) follow_ups[2]; event 62736; src/application/supervise/exit.rs 81〜89 行は Silent（process が生きている）ときだけ記録: 未実装; task 1372 completed
  - 所属先の案: 今の goal 56 のまま（移動不要）
  - 既存の判定 8: 今も成り立つ。goal 86 の acceptance に要らない判定は今も成り立つ。移し先 56 は題に合うが 56 の acceptance にも要らないので、56 を閉じるときにもう一度所属の判断が要る点に注意。
- **task 1411**（ready・source goal: goal 86・depth 1）measure: 待ちの最中に wrapper を失った非対話の run の開き直し（headless_session_reopened・session_reopen_failed）を本番で数え、3 回・60 秒の上限が cmux の停止に足りるかを見積もり、zero-based-headless-readiness.md の項目 A と毎週の手順に足す
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (2) ゼロベースへ移る時期を判断する計測の項目・基準値・条件の案が文書にある
  - 理由: (2) は計測の項目・基準値・条件の案が文書にあることで、開き直しの件数と上限の見積もりを足すのはその改善。開き直しの仕組み (5) は 1372 で着地済みで、無くても acceptance は満たせる。
  - 証拠: receipt of run eebaca9b-ebfd-4104-b22e-8091905c2951 (task 1372) follow_ups[0]; event 64026; docs/plans/zero-based-headless-readiness.md に headless_session_reopened・session_reopen_failed が無い（grep 0 件）: 未実装
  - 所属先の案: 今の goal 56 のまま（移動不要）
  - 既存の判定 9: 今も成り立つ。判定は今も成り立つ。56 の acceptance にも要らない点は 1384 と同じ。

### goal 57: worker を非対話（headless）の経路で動かせるようにし、Claude Code と Codex CLI をその経路に乗せ、task ごとに provider を選び、使えない provider からもう一方へ相互にフォールバックする

acceptance の版 1。follow_up 由来の未完了 4 件、follow_up でない未完了 0 件。

- **task 1205**（ready・source goal: goal 57・depth 2）measure: task 1174 の着地後の本番の Codex の非対話の run の記録と ~/.codex/config.toml の更新時刻・[projects] を突き合わせ、Codex が run の前後で config.toml を書かなくなったかを manual-smoke.md の結果に書く
  - 分類: 判断できない（undecided）
  - 項目: (6) 実 Codex と実 Claude の非対話の worker の手動スモークの手順と結果が docs/design/manual-smoke.md にある / (4) Codex の worker の権限…
  - 理由: goal 57 (6) は手順と結果が manual-smoke.md に『ある』ことを求め、task 1102 の結果は既にあるが、その結果は手順 6 の確認点『~/.codex/config.toml の更新時刻が変わっていない』を満たさないと記録している（manual-smoke.md 222 行）。task 1174 で直した後の確認が無いまま (6) を達成とみなせるか（結果が『ある』だけでよいか、確認点の通過まで要るか）は goal の意図が要るので決めきれない。
  - 証拠: receipt of run 2e3e25f6-f7cf-48c3-9dec-b264daaa705d (task 1174) follow_ups[0]; event 49359; docs/design/manual-smoke.md 222 行（config.toml の更新時刻が turn_started と同じ秒、手順 6 を満たさない）と 191 行（task 1174 から -c で worktree の trust を渡して止めている）; dependencies 1114・1175 は completed
  - 人の確認: 要る。goal 57 (6) を『結果がある』で達成とするか、確認点の通過まで要るかの意図。required なら goal 57 に残し、out_of_scope なら Codex の人の設定を変えないことを扱う新しい goal が要る。加えて goal 57 (2)『対話の経路は Claude の既定として残る』は ADR-t1340-1（既定を非対話に）と goal 92（対話の経路の廃止）で古くなっており、acceptance の改訂の要否も人に確かめたい
- **task 1342**（ready・source goal: goal 57・depth 1）measure: task 1340 の着地の後、非対話の Claude の worker で最初の worker_question の answer の turn と needs_session の resume の turn が通ったかを docs/plans/headless-worker-measurement.md の条件の表に足す
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (1) 非対話の worker の経路…worker の質問の answer・review の revise・needs_session の resume を同じ session の id の resume の呼び出しで続け…着地する / (2) …対話の経路は Claude の既定として残る（既定を切り替えるかは測定の task の結果で別に決める）
  - 理由: goal 57 (1) の answer の turn と needs_session の resume は、同じ非対話の経路を通る Codex にフォールバックした run で本番を通って着地している（headless-worker-measurement.md 303〜306 行）ので、非対話の Claude での初回の確認なしで (1) は満たせる。この測定は既定を非対話にした後の評価で、goal 86 (1) の『切り替えの基準値・評価』に合う。
  - 証拠: receipt of run 732f927d-1fd4-488e-9d19-52e4e23b79d0 (task 1200) follow_ups[0]; event 60235; docs/plans/headless-worker-measurement.md の 2a・2b の行（294・295 行）と 303〜313 行; task 1340（ADR-t1340-1）は completed
  - 所属先の案: goal 86（goal 57 から移す）
  - 人の確認: 要る。goal 57 (2) の『対話の経路は Claude の既定として残る（既定を切り替えるかは測定の task の結果で別に決める）』は、人の決定（note 60084）と task 1340 で既定が非対話になり、goal 92 で対話の経路自体が廃止されたため古い。goal 57 を達成扱いにしてよいか、acceptance を改めるかは人の判断が要る（この task の所属の判定はその答えに依らない）
- **task 1385**（ready・source goal: goal 86・depth 1）runtime: task_created に provider と worker_mode（明示した値か既定か）を残す
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (2) ゼロベースへ移る時期を判断する計測の項目・基準値・条件の案が文書にある
  - 理由: goal 86 (2) は計測の項目・基準値・条件の『案が文書にある』ことで、項目 C を task_created から数えられるようにする記録の追加は要らない。既存の判定 10（out_of_scope、goal 57 へ）は今も成り立つ。未実装（sqlite.rs の task_created の payload は goal_id だけ）。
  - 証拠: receipt of run 57151b60-d708-4c4b-9edc-a37573e7909c (task 1369) follow_ups[3]; event 62738; src/infrastructure/sqlite.rs の task_created の記録（json!({"goal_id": task.goal_id()})）; dagq goal show 86 --full の acceptance
  - 所属先の案: 今の goal 57 のまま（移動不要）
  - 既存の判定 10: 今も成り立つ。所属の判定は今も成り立つ。ただし goal 92 で対話の経路が廃止され --interactive の拒否が別 goal にあるので、description の『--interactive の task を数える』目的は薄れており、provider の記録だけに絞るかは採用の段で確かめたい（所属とは別）
- **task 1594**（ready・source goal: goal 92・depth 1）runtime: 非対話の worker の turn が receipt を書いた後に非 0 で終わったときの扱いを決め（receipt を validation にかける）、receipt と session の終了のどちらを先に見たかで結果が変わらないようにする
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: —（source goal の acceptance のどの項目もこれを求めていない）
  - 理由: goal 92 の acceptance (1)〜(6) は cmux を inbox だけにすること・対話の経路の廃止・tests/it と e2e の cmux 依存と本数・検証時間の記録で、非対話の turn が receipt を書いて非 0 で終わったときの扱いは含まないので、この task なしで満たせる。既存の判定 4（goal 57 へ）は今も成り立つ。未実装（該当の ADR は docs/adr に無い）。
  - 証拠: receipt of run ae4bbfa9-0041-4400-86ba-faa65ba8867e (task 1435) follow_ups[0]; event 76466; dagq goal show 92 --full の acceptance; docs/adr に t1594 の ADR が無い
  - 所属先の案: 今の goal 57 のまま（移動不要）
  - 既存の判定 4: 今も成り立つ。非対話の turn の終わり方の決定性は goal 57 (1)『process の終了と出力から turn の結果を読み』に合う。現 goal は既に 57。needs_recheck は false

### goal 61: inbox の watch が /clear・compaction・再起動の後も張り直されることを仕組みで保証し、張られていないあいだに開いた ask を inbox に届ける

acceptance の版 1。follow_up 由来の未完了 1 件、follow_up でない未完了 0 件。

- **task 1332**（ready・source goal: goal 61・depth 2）runtime: ask_seen_wait で watcher が戻る前に答えた・閉じた ask を、その終わりの時刻で打ち切って数える
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: Stop hook が止めて watch を張らせる。status --role inbox と doctor が watcher の有無と居ない時間を出す。…知らせを 1 回打ち込み、event に残る
  - 理由: goal 61 の acceptance は watch を張り直させる仕組み・表示・知らせを求めていて、KPI ask_seen_wait の数え方は求めていない。これが無くても達成と言える。
  - 証拠: receipt of run 62f3a648-30aa-46e6-a5c1-681667398d6e (task 1021) の follow_ups「ask_seen_wait: decide how to count asks closed before the watcher returns」（category decision）; event 59603; src/domain/stats/ask_seen.rs の seen_waits が ask の答え・閉じを見ない（未実装）
  - 所属先の案: goal 40（goal 61 から移す）

### goal 62: 着地の速度: receipt の後に居座る run を止め、worker の stress の重い部分を CI の定時実行に移す

acceptance の版 1。follow_up 由来の未完了 2 件、follow_up でない未完了 0 件。

- **task 934**（draft・source goal: goal 62・depth 1）measure: after task 918 lands, check that slot-holding human waits at night are near zero
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: 夜の人の答え待ちが着地を遅らせた量が測られ、次の打ち手の材料が docs/plans に残る
  - 理由: goal 62 の該当項目は task 919 の測定（docs/plans/night-human-wait-measurement.md）で満たされている。task 918 の着地後の効果を確かめる再測定で、無くても acceptance を満たしたと言える。
  - 証拠: receipt of run 5f4e0a1e-ea20-4cfe-bd2c-4699a6f392be (task 919) follow_ups[0]; event 33943; docs/plans/night-human-wait-measurement.md; commit d751a0c9 runtime: 対話の worker の run を起こさないようにし、対話の run だけが使う画面・ダイアログ・/exit・打鍵の処理と…を消す
  - 所属先の案: goal 72（goal 62 から移す）
  - 処置の案: 不要として cancel の候補。根拠: 比べる対象の stuck_exit の待ちは対話の worker の /exit の待ちで、commit d751a0c9（task 1437）で対話の worker の run が起きなくなったので生じない。比べる基準（09-27→28）の前提が変わっており、夜の着地の速さは inbox の毎時・日次の見直しで既に読める。acceptance も空の draft で、cancel の候補
- **task 937**（ready・source goal: goal 62・depth 1）runtime: stats の threshold の検知に long_background の left_to_phase（receipt の後の escalate）を数える
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: receipt の後に background の処理が残って idle にならない run が…止まって着地へ進む（438 の型の再現 test がある）
  - 理由: goal 62 の該当項目は task 918 が満たした（receipt の後の escalate）。stats の background_alert_secs の detections がその left_to_phase を数えないのは stats の数え方の問題で、無くても acceptance を満たしたと言える。
  - 証拠: receipt of run c43d0982-896c-4afe-8e53-a475b4d8784a (task 918) follow_ups[0]; event 34055; src/domain/stats/thresholds.rs 294〜299 行目（left_to_phase を IDLE_PROCESS だけで数える。未実装を確認）; task 780（goal 34、ready）が同じ関数を触る依存先
  - 所属先の案: new: stall の検知の結末の記録と stats の閾値の検知の数え方を正す（goal 30 の後続。800・905・937 と goal 34 の 780 を集める）（goal 62 から移す）

### goal 66: e2e の置き場所を分ける: 差分が狭い範囲に触れる run だけ worker で必須にし、全部の e2e は自動更新が固定バイナリを入れ替える前の関門で流す

acceptance の版 1。follow_up 由来の未完了 3 件、follow_up でない未完了 0 件。

- **task 1025**（ready・source goal: goal 66・depth 1）runtime: 着地の resume（integration_approved）を経た run も、rebase 後の差分と dagq.toml の [e2e] paths から e2e の要否を決め直し、要るなら着地の前に host の e2e の工程（task 1239）を流す
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: validating が dagq.toml の path と run の差分から e2e の evidence の要否を決め、範囲の外の run は e2e なしで着地できる / 自動更新が入れ替えの前に新しいバイナリで e2e を流し
  - 理由: goal 66 の acceptance は validating が差分と [e2e] paths から要否を決めること（task 965）と、本番を守る自動更新の関門（task 964）を求め、どちらも着地している。着地の resume で差分が新しく [e2e] paths に触れる穴は、関門が本番の入れ替えを守る前提（constraints）の上の追加の安全策で、実施しなくても acceptance を満たせる。
  - 証拠: receipt of run b72a6029-72e9-4929-960b-f3587970e473 (task 965) follow_ups[1]; event 39202; task 964・965・966 completed、task 1239（goal 84、host の e2e の工程）は 2026-10-01 に着地; goal 66 の constraints（関門 F4 を先に着地させてから worker の e2e を狭める）
  - 所属先の案: new: 着地の前に runtime が host で流す e2e の工程の後回しの改善（要否の判定の穴・後始末）（goal 66 から移す）
- **task 1026**（ready・source goal: goal 66・depth 1）measure: dagq.toml の [e2e] paths で worker の e2e を狭めた前後で、runtime の run の e2e の時間と work の時間を比べ docs/plans/e2e-narrowing-measurement.md に残す
  - 分類: 元 goal に必須（required）
  - 項目: 導入の前後で runtime の run の e2e の時間と work の時間が比べられている
  - 理由: goal 66 の acceptance の最後の項目そのものの測定で、docs/plans に前後の比較の文書がまだ無いので、実施しなければ acceptance を満たせない。なお task 1239（2026-10-01T21:03Z 着地）で worker が e2e を流さなくなったので、「後」の群は 966 の着地（2026-09-28T23:59Z）から 1239 の着地までに限るか、1239 を重なりとして扱うよう task を直す必要がある。
  - 証拠: receipt of run b72a6029-72e9-4929-960b-f3587970e473 (task 965) follow_ups[2]; event 39204; docs/plans に e2e-narrowing-measurement.md が無い（未実装）; run_integrated: task 966 2026-09-28T23:59:58Z、task 1239 2026-10-01T21:03:54Z
  - 所属先の案: source goal に残す
- **task 1278**（ready・source goal: goal 66・depth 1）supervisor の中の着地前の e2e で、後始末に失敗した root を supervisor が生きているあいだも拾い直す
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: 自動更新が入れ替えの前に新しいバイナリで e2e を流し、落ちたら入れ替えずに ask か attention で知らせる
  - 理由: goal 66 の acceptance は関門で e2e を流して落ちたら知らせることを求め、関門の後始末に失敗した root の拾い直しは求めない。対象の着地前の e2e は goal 84 の工程で、supervisor の pid の root が次の起動まで残る後始末の不具合なので、実施しなくても goal 66 の acceptance を満たせる。
  - 証拠: receipt of run 0fdebb98-1375-4f94-8d7a-4634aa83f28f (task 1011) follow_ups[0]; event 57342; src/infrastructure/e2e_gate.rs 175 行（!owner(&earlier).is_some_and(alive) のときだけ clean_up。未実装）
  - 所属先の案: new: 着地の前に runtime が host で流す e2e の工程の後回しの改善（要否の判定の穴・後始末）（goal 66 から移す）

### goal 68: test の実時間の待ちを減らし、着地の検証の test 段と worker の手元の test を縮める

acceptance の版 1。follow_up 由来の未完了 6 件、follow_up でない未完了 0 件。

- **task 1121**（ready・source goal: goal 68・depth 0）template::queue が既存の file を黙って上書きする
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (4) 重い群で判断が unit test に移り…外部プロセスの起動・固定の待ち・fixture の実時間が減ったことが数で示され
  - 理由: goal 68 の acceptance は test の時間の内訳・修正の前後の数字・script・重い群の移し替えを求め、fixture の template の前提を assert で確かめる安全の補強は含まない。今の呼び出しはどれも新しい path を渡し害が無い（receipt の記述）ので、無くても acceptance は満たせる。
  - 証拠: receipt of run e3683bec-bbad-42b5-9fe0-c7866cc8fe19 (task 1060) follow_ups[0]; event 44539; tests/common/template.rs の queue()（fs::copy で上書き、既存 file の assert 無し）と repository() を main 60972db3 で確認: 未実装; goal 68 の other_open_tasks は 0（1059・1060・1079・1080 は completed）
  - 所属先の案: new: test の fixture と補助の process の隔離と後片付けを固める（template の上書き・EXDEV・残る template・host の lock・tracing の capture・時間切れで残る wrapper）（goal 68 から移す）
- **task 1261**（ready・source goal: goal 68・depth 1）template::script の hardlink が別 filesystem で EXDEV になるときに copy へ戻す
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (4) …fixture の実時間が減ったことが数で示され
  - 理由: EXDEV は今の host と CI（同じ APFS）では起きず、goal 68 の acceptance（時間の内訳・修正の前後の数字・script・移し替え）はこの fallback なしで満たせる。別 volume の環境への頑健性の改善で、goal 68 の完了条件ではない。
  - 証拠: receipt of run a0ccef76-40ec-47b4-bb72-89dad4a9df51 (task 1079) follow_ups[0]; event 56227; tests/common/template.rs:109 の fs::hard_link(...).unwrap() に EXDEV の fallback が無いことを main 60972db3 で確認: 未実装
  - 所属先の案: new: test の fixture と補助の process の隔離と後片付けを固める（template の上書き・EXDEV・残る template・host の lock・tracing の capture・時間切れで残る wrapper）（goal 68 から移す）
- **task 1262**（ready・source goal: goal 68・depth 1）stub_templates の test が毎回 target/tmp/fixture-templates に新しい template を残す
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (4) …fixture の実時間が減ったことが数で示され
  - 理由: stub_templates の 1 本の test が target/tmp に template を残す後片付けの問題で、goal 68 の acceptance の時間の短縮・計測・移し替えのどれにも要らない。
  - 証拠: receipt of run a0ccef76-40ec-47b4-bb72-89dad4a9df51 (task 1079) follow_ups[1]; event 56229; tests/it/stub_templates.rs に template の dir・.lock を消す処理（remove_dir・drop guard）が無いことを main 60972db3 で確認: 未実装
  - 所属先の案: new: test の fixture と補助の process の隔離と後片付けを固める（template の上書き・EXDEV・残る template・host の lock・tracing の capture・時間切れで残る wrapper）（goal 68 から移す）
- **task 1293**（ready・source goal: goal 68・depth 1）docs: slow-test-waits.md の 7 章の手順の 30 本のうち main から消えた 1 本を注記する
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (4) …同じ方法の本番の coverage の関門の log の前後で…比べた結果が docs/plans にある
  - 理由: goal 68 の前後比較は済んでいて（task 1059・1080 は completed、goal 68 の other_open_tasks は 0）、acceptance (4) の比較は関門の log の方法で 7 章の 30 本の手順に依らない。今後この手順で測る人への注記なので、無くても acceptance は満たせる。
  - 証拠: receipt of run 8b047e26-ba81-482b-9392-89d07543fa29 (task 1080) follow_ups[0]; event 58130; docs/plans/slow-test-waits.md に 1239・404f127a の注記が無い（grep で 0 件）: 未実装; tests/it/runtime_evidence.rs に a_diff_outside_the_e2e_paths_… が無いことを grep で確認（消えたのは事実）; task 1288 completed
  - 所属先の案: goal 118（goal 68 から移す）
  - 処置の案: 不要として cancel の候補。根拠: goal 68 の測定（1059・1080）は済み、7 章の手順で測る未完了の task が goal 68 に無い。goal 118 などで 7 章の手順を再び使う予定が無ければ不要（cancel の候補）。使うなら goal 118（dagq::it の時間の追跡）へ。判断は planner。
- **task 1295**（ready・source goal: goal 68・depth 1）test: cli_broker の test の dagq の起動で XDG_CONFIG_HOME を test の tempdir に向け、host の podman machine の lock を取り合わない
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (2) 内訳から選んだ修正が着地し…test の時間の合計と integrate の test 段の中央値が縮んだ / (4)
  - 理由: cli_broker の test の XDG_CONFIG_HOME の隔離は、host の podman machine の lock の取り合いで 1 本が遅くなる件で、goal 68 が内訳から選んだ修正（F1〜F5・G1〜G4）とその前後比較はこれなしで済んでいる。test の隔離の欠けの修正で、acceptance の達成には要らない。
  - 証拠: receipt of run da7a64f3-83c0-4f88-9855-fe39523c2712 (task 1077) follow_ups[1]; event 58206; tests/it/cli_broker.rs の dagq() helper（19〜27 行）が XDG_DATA_HOME だけを差し替え XDG_CONFIG_HOME を差し替えないことを main 60972db3 で確認: 未実装; src/infrastructure/broker_podman.rs の machine_lock_home は XDG_CONFIG_HOME を読む
  - 所属先の案: new: test の fixture と補助の process の隔離と後片付けを固める（template の上書き・EXDEV・残る template・host の lock・tracing の capture・時間切れで残る wrapper）（goal 68 から移す）
- **task 1359**（ready・source goal: goal 65・depth 1）measure: build script の rerun-if-changed を worktree に依らない形にしたときの dagq の crate の build の短縮を測る
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: —（source goal の acceptance のどの項目もこれを求めていない）
  - 理由: goal 65 の acceptance は着地中の worker の優先度を下げたときの spike と、進めるなら CPU の取り分の port と adapter の ADR・実装・比較で、build script の rerun-if-changed の測定は要らない。planner の判定（out_of_scope）は今も成り立つ。
  - 証拠: receipt of run 9c4e96bf-ddf5-406d-8b43-f6fd7c9cde9e (task 968) follow_ups[0]; event 61194; dagq goal show 65 --full の acceptance; docs/plans に worktree-seed.md の続きの文書が無い（ls docs/plans）: 未実装; dagq goal show 36 --full の acceptance (1)（build の共有方式と run の build 時間の短縮）
  - 所属先の案: 今の goal 68 のまま（移動不要）
  - 既存の判定 18: 今も成り立つ。goal 65 に要らないという分類は今も成り立つ。移し先の goal 68 は test の時間の goal で、この task は build の時間（dagq の crate の build し直し・worktree の seed）の測定なので、問題は run の build の共有を扱う goal 36 の方が合う。移し先を goal 36 に変えるなら --corrects の訂正が要る（分類は同じでも行き先の変更は記録し直す）。変えるかは planner が判断する。

### goal 72: スループットの制約（着地の直列処理と CPU）を常時見る数値と、毎週の見直しの手順を持つ

acceptance の版 1。follow_up 由来の未完了 3 件、follow_up でない未完了 0 件。

- **task 1004**（ready・source goal: goal 72・depth 1）runtime: 目標割れの検査（push）と observer の KPI の入力にも host の読み手を渡し、cpu_per_landing・load_per_core の目標を判定する
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: dagq kpi が landing_utilization と cpu_per_landing を期間ごとに出し、日次レポートに載る。reference/kpi.md に毎週の見直しの手順がある
  - 理由: goal 72 の acceptance は kpi とレポートに 2 つの数値が出ることと見直しの手順で、目標割れの検査（push）と observer の入力にまで host の読み手を渡すことは求めていないので、この task なしで満たせる。KPI の目標割れの push と observer の finding は goal 40 (4)(5) に合う。
  - 証拠: receipt of run e613355c-9490-49e7-9f30-f1d73f7bfae4 (task 992) follow_ups[0]; event 37739; dagq goal show 40 --full の acceptance (4)(5)
  - 所属先の案: goal 40（goal 72 から移す）
- **task 1073**（draft・source goal: goal 72・depth 1）measure: 毎時のスループットの見直しの起動回数・割合・所要時間を本番の 1 週間で数え、docs/plans/throughput-review-hourly-measurement.md に残す
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: dagq kpi が landing_utilization と cpu_per_landing を…、reference/kpi.md に毎週の見直しの手順
  - 理由: 毎時のスループットの見直しの起動回数と費用の測定で、goal 72 の acceptance（2 つの KPI と毎週の手順）には含まれず、この task なしで満たせる。また測る対象の規則（ADR-t996-1 決定 2: 規則に当たった時だけ起動）は goal 76 が『毎時は毎回 agent を起動する』に改めるところ（task 1172 ready）で、job の件数・所要時間は task 1173 で stats の jobs と kpi の job.* から読める。
  - 証拠: receipt of run 9626ce5a-fb55-423b-ab72-ac8285f56360 (task 997) follow_ups[3]; event 41002; dagq goal show 76 --full の acceptance (1)(4)、task 1172（ready）・1173（completed）
  - 所属先の案: goal 76（goal 72 から移す）
  - 処置の案: 不要として cancel の候補。根拠: 『規則を変えるかを人が決める材料』の問いは goal 76 の決定（毎時は毎回起動、task 1172）で答えが出ており、起動回数・失敗率・所要時間は task 1173 の stats jobs / kpi job.* で読める。採用するなら goal 76 の規則の下で測り直す形に書き換えが要る。cancel の候補
- **task 1074**（ready・source goal: goal 72・depth 1）test: exec で引き継いだ supervisor が前のプロセスの throughput-review の子を 1 度だけ reap する経路を supervisor の test で確かめる
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: dagq kpi が landing_utilization と cpu_per_landing を…、reference/kpi.md に毎週の見直しの手順
  - 理由: exec の引き継ぎ後に throughput-review の子を reap する経路の test で、goal 72 の acceptance（KPI と毎週の手順）に当たらず、この task なしで満たせる。スループットの見直しの job の運用は goal 76 に合う。未実装（tests に reap_handed_over_reviews を通す test が無い）。
  - 証拠: receipt of run 9626ce5a-fb55-423b-ab72-ac8285f56360 (task 997) follow_ups[4]; event 41004; grep で tests に reap_handed_over_reviews が無い
  - 所属先の案: goal 76（goal 72 から移す）

### goal 73: worker 以外の headless の job を provider に透過な形で Codex でも動かせるようにし、まず goal review job を Codex で動かす。どの provider・model で動いたかを全ての job で記録し、Claude と比べられるようにする

acceptance の版 1。follow_up 由来の未完了 3 件、follow_up でない未完了 0 件。

- **task 1109**（ready・source goal: goal 73・depth 0）test: 非対話の run で turn の process group の外で走る子孫を、復旧 job の stop_processes が pid で止めることを integration test で確かめる
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (1)〜(6) は worker 以外の job の Codex 化と provider の記録
  - 理由: 1109 は、非対話の run の process group の外に残る子を、復旧 job の stop_processes が止めることを確かめる test で、job の provider 化を求める goal 73 の acceptance に無い。goal 89 の (2)「止める経路（…復旧 job の stop_processes…）で process が残らない」の方が合う。
  - 証拠: receipt of run 94bd8a16-be84-4a68-a558-4b18d5fa0e86 (task 1085) の follow_ups（category test_gap）; event 43213; tests/it/runtime_background_process.rs の stop_processes_stops_a_background_sessions_orphan（commit 036871ca）は background の wrapper の孤児の場合を一部確かめるが、別の process group の子・wrapper の子孫のままの場合と design の照合はしない（重複ではない）
  - 所属先の案: goal 89（goal 73 から移す）
- **task 1476**（ready・source goal: goal 94・depth 1）runtime: Codex の review job で必須の subagent が動くこと・sandbox を継ぐこと・worktree の .codex が効かないことを実 Codex で確かめ、確かめられたら Codex の review に subagent の定義を渡して Claude への切り替えをやめる
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: goal 94 の (2) provider が使えない・失敗したときに黙って省かない
  - 理由: goal 94 の (2) は、Codex の review を必須の subagent のときに Claude へ切り替えること（task 1455）で満たされている。実 Codex で subagent を確かめて有効にするのは goal 94 の acceptance に無い。
  - 証拠: receipt of run c77e93d4-1d71-4920-aa5b-098b9f4deaa0 (task 1455) の follow_ups（category remaining_scope）; event 67288; dagq goal show 94: verdict achieved、closed_at 2026-10-04T11:00Z（ask 419）; src/infrastructure/codex.rs:400 の runs_review_subagents が今も false; judgement 5
  - 所属先の案: 今の goal 73 のまま（移動不要）
  - 既存の判定 5: 今も成り立つ。今も成り立つ。goal 94 は achieved で閉じ、acceptance_version 1 のまま。行き先の goal 73 は Codex の headless job の goal で合う
  - 採用の注意: source goal 94 は閉じている（achieved）。ADR-t808-1 では、所属の判定とは別に採用に人の adopt が要る。judgement 5 は request 15 の人の言葉（「A,B は提案通りでOK」）を根拠にしている
- **task 1703**（ready・source goal: goal 94・depth 0）manual-smoke.md の Codex の run review のスモーク手順 3 の起動引数の記述を今の argv に直す
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: goal 94 の (1)〜(7)（AGENTS.md・plugin・review の subagent・ADR）
  - 理由: goal 94 の acceptance は manual-smoke.md の Codex の run review のスモークの起動引数の記述を含まない。goal 94 はこれ無しで achieved で閉じた。
  - 証拠: receipt of run a1b7032b-e911-42b9-950e-e5d6aa749a93 (task 1570) の follow_ups（category docs_drift、membership_proposal は out_of_scope）; docs/design/manual-smoke.md:454 に今も「起動引数は `--sandbox read-only` を含む」とある（未実装）; judgement 27
  - 所属先の案: 今の goal 73 のまま（移動不要）
  - 既存の判定 27: 今も成り立つ。今も成り立つ。goal 94 は achieved で閉じた。行き先の goal 73 には Codex のスモーク（task 1114）がある
  - 採用の注意: source goal 94 は閉じている（achieved）。registration_event_id は null（source_goal_provenance は recorded）。ADR-t808-1 では、所属の判定とは別に採用に人の adopt が要る

### goal 74: supervisor の一時的な失敗と起動し直しで着地と自動更新が止まらないようにする（2026-09-29 の 8 時間の着地の停止から）

acceptance の版 1。follow_up 由来の未完了 7 件、follow_up でない未完了 0 件。

- **task 1183**（ready・source goal: goal 74・depth 0）test: e2e の期限切れで supervisor の stderr の reader の join が詰まっても、それまでに読んだ stderr を出して失敗する
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (3) 自動更新の e2e の関門が…高負荷の下の期限切れ…で不安定に止まらない
  - 理由: 1183 は期限切れのときに stderr を出す診断の改善で、期限切れそのものを減らさない。(3) の高負荷の下の期限切れは task 1008（commit 59f59753）の側の話で、診断が無くても (3) が満たせるかどうかは変わらない。
  - 証拠: receipt of run 87010ac2-d847-4133-84fa-d8309d834148 (task 1008) の follow_ups「e2e の期限切れで supervisor の stderr の join が詰まると診断が出ない」（category improvement）; event 48020; commit 59f59753 test: 自動更新の e2e の関門で、高負荷のときに e2e が期限切れ…で落ちる原因を test ごとに調べ、待ち方か原因を直す; tests/e2e.rs:306 fn joined と supervise_once が今も joined(stderr) で待ってから panic する（未実装。前の run の branch dagq/2515ab51… の head 6035e800 は main に入っていない）
  - 所属先の案: goal 75（goal 74 から移す）
- **task 1190**（draft・source goal: goal 74・depth 1）measure: task 1162（machine の lock の場所と podman の再試行、関門の broker:: の skip）の後、自動更新の e2e の関門で broker の e2e が podman で落ちなくなったかを数える
  - 分類: 元 goal に必須（required）
  - 項目: (3) …podman machine の ssh の handshake の失敗で不安定に止まらない。podman に繋がらないときの関門の扱いが ADR に決まり、流さなかった e2e は記録に残る
  - 理由: (3) は「podman の失敗で関門が不安定に止まらない」という結果を求める。task 1162（a7a4255e）の後も a7a4255e を含むビルド b62a1322 の関門で broker:: の e2e が 2 回落ちていて（update_failed 52857・53634、原因は payload から分からない）、数えて分けないと (3) を満たしたと言えない。
  - 証拠: receipt of run 314a9d7b-dae7-4d17-8afe-bfeebbb436a8 (task 1162) の follow_ups「lock の場所を変えた後、並んで走る broker の e2e が落ちなくなったかを数える」（category measurement）; event 48240; commit a7a4255e runtime: 自動更新の e2e の関門で、podman machine の ssh の handshake の失敗で broker の e2e が落ちないようにし…; event 52857（2026-09-30T13:29Z、commit b62a1322、broker::a_preferred_worker_does_its_task_through_the_broker_and_lands が流し直しでも落ちた）と event 53634（2026-10-01T13:55Z、同じ test）。git merge-base で b62a1322 は a7a4255e を含む
  - 所属先の案: source goal に残す
  - 採用の注意: status が draft なので submit が要る（所属の判断とは別）。境の後の関門の回数が足りるかは (4) が受ける
- **task 1191**（ready・source goal: goal 74・depth 1）runtime: 読むだけの dagq broker status と、lock を持ったまま podman を呼ぶ broker stop が、podman の Reconnecting の待ちで長く止まらないようにする
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (3) podman machine の ssh の handshake の失敗で不安定に止まらない
  - 理由: 1191 は人が打つ dagq broker status と broker stop の待ちの長さの話で、自動更新の e2e の関門の判定には関わらない。context も本番の run はまだ broker を使わず今の運用への効きは小さいと書いており、これが無くても (3) は満たせる。
  - 証拠: receipt of run 314a9d7b-dae7-4d17-8afe-bfeebbb436a8 (task 1162) の follow_ups「Reconnecting の待ちで dagq broker status と stop が遅くなる」（category improvement）; event 48242; src/application/broker.rs:352 の RECONNECT が今も 1 種類だけ（未実装）
  - 所属先の案: new: podman machine を使う broker・e2e の関門の待ちの上限と gvproxy の後片付けを全経路で揃える（goal 74 の後続）（goal 74 から移す）
- **task 1336**（ready・source goal: goal 74・depth 1）measure: 本番の supervisor_heartbeat_retried を task 1119 の後と task 1333・1334 の後で数え、heartbeat の busy の頻度と長さと stale の余裕を見る
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (1) 一時的な SQLite の busy では supervisor の heartbeat が再試行し、lease を本当に失ったときだけ supervisor が止まる
  - 理由: (1) は再試行の仕組みを求めていて、task 1119 がそれを入れた。busy の頻度と長さを数える測定は lock を縮める次の手を決めるためのもので、これが無くても (1) は満たせる。
  - 証拠: receipt of run 6000103e-2245-4dcf-aa11-756ffd8b7536 (task 1119) の follow_ups「本番で supervisor_heartbeat_retried の件数と secs を数え…」（category measurement）; event 59707; task 1119 completed（再試行の実装）。本番の supervisor_heartbeat_retried は event 60114（2026-10-02T03:06Z）の 1 件だけ; 依存先の 1333・1334 は completed
  - 所属先の案: new: queue DB の write lock を握る時間を縮め、heartbeat の busy と閉じの記録の取りこぼし・二重を減らす（goal 74 の後続）（goal 74 から移す）
- **task 1501**（ready・source goal: goal 74・depth 2）runtime: record_open_turns の transcript の turns の解析を write transaction の前に出す
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (1) 一時的な SQLite の busy では supervisor の heartbeat が再試行し、lease を本当に失ったときだけ supervisor が止まる
  - 理由: (1) は busy を再試行で受けることで満たされる（task 1119）。record_open_turns の解析を transaction の外に出すのは lock を握る時間を縮める改善で、(1)〜(3) のどれもこれを求めていない。
  - 証拠: receipt of run 3be848c0-6af6-4d0b-94d9-65f37e594fdf (task 1334) の follow_ups「record_open_turns parses the turns of transcripts inside its write transaction」（category improvement）; event 69275; src/infrastructure/sessions.rs の record_open_turns が今も transaction の中で turns(&transcript.records) を呼ぶ（未実装）
  - 所属先の案: new: queue DB の write lock を握る時間を縮め、heartbeat の busy と閉じの記録の取りこぼし・二重を減らす（goal 74 の後続）（goal 74 から移す）
- **task 1611**（ready・source goal: goal 74・depth 0）sessions: commit hook の後に COMMIT が失敗して transaction が残ると、ROLLBACK の prepare が authorizer で Committed にし、rollback された閉じの worktime.jsonl の行が書かれる
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (1) 一時的な SQLite の busy では…再試行し / (2) …着地がやり直される
  - 理由: 1611 は、COMMIT が失敗して rollback された閉じの worktime.jsonl の行が書かれうるという計測の正しさの欠陥で、WAL と BEGIN IMMEDIATE の下ではほぼ起きない。supervisor が止まらないことにも着地にも自動更新にも関わらないので、これが無くても (1)〜(3) は満たせる。
  - 証拠: receipt of run 2f3b8539-be0c-413d-bc66-967100c48df3 (task 1500) の follow_ups（category defect）; event 77938; context: depth 3 の上限のため ask 383 で人が adopt と答えた
  - 所属先の案: new: queue DB の write lock を握る時間を縮め、heartbeat の busy と閉じの記録の取りこぼし・二重を減らす（goal 74 の後続）（goal 74 から移す）
  - 採用の注意: 入力の depth は 0 だが、context では 1334→1500→1611 の深さ 3 で planner 898 が ask 383 で人に確かめ、adopt と答えられている。人の adopt は済んでいる
- **task 1613**（ready・source goal: goal 74・depth 1）runtime: e2e の関門の broker::connect にも gvproxy の後片付けを通し、関門が止めた machine の孤児の gvproxy を片付ける
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (3) 自動更新の e2e の関門が…podman machine の ssh の handshake の失敗…で不安定に止まらない
  - 理由: 1613 は関門が起こした machine の孤児の gvproxy を片付けるもので、host の資源の後片付けにあたる。関門の判定（podman に繋がるか、broker:: を流すか）は変えないと task 自身が定めているので、これが無くても (3) は満たせる。
  - 証拠: receipt of run 42db7f74-ac0b-44be-8cc8-fe50b21c9331 (task 1579) の follow_ups「e2e の関門の broker::connect にも gvproxy の後片付けを通す」（category remaining_scope）; event 78295; src/infrastructure/e2e_gate.rs:520 が今も broker::connect（processes なし）を呼ぶ（未実装）; task 1579・1189 completed
  - 所属先の案: new: podman machine を使う broker・e2e の関門の待ちの上限と gvproxy の後片付けを全経路で揃える（goal 74 の後続）（goal 74 から移す）

### goal 79: sandbox の中のプロセスが sccache の server を起動してほかの build を巻き込まないようにし、誰がいつ起動したかを見えるようにする

acceptance の版 1。follow_up 由来の未完了 1 件、follow_up でない未完了 1 件。

- **task 1350**（ready・source goal: goal 79・depth 1）supervisor の周回が sccache の server の起動と外部コマンド（lsof など）の待ちで止まらないようにする
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (2) supervisor（sandbox の外）が server を起動・維持し…event に残す
  - 理由: goal 79 の acceptance は sandbox の中から server を起動しないこと・supervisor が起動して記録すること・検知・doctor と status の表示・ADR で、同期の起動でも満たせる。周回が sccache の起動や lsof の待ちで止まらないようにするのは supervisor の周回の頑健さの改善で、無くても acceptance を満たしたと言える。
  - 証拠: receipt of run 7a664310-8442-4bd9-b6e1-52319f2f2cab (task 1215) follow_ups[1]; event 60896; src/infrastructure/sccache.rs の SystemSccache::start（周回の中で同期。未実装を確認）
  - 所属先の案: new: supervisor の周回を外部コマンド（sccache の起動・lsof・ps など）の待ちで止めない（goal 79 から移す）

### goal 82: goal 38 の段 (1)〜(3): 制御側と実行側の分け方・queue service・broker を ADR で決め、queue service と少数の API を作り、worker と job の dagq をクライアントモードにして queue DB の path を渡さない

acceptance の版 1。follow_up 由来の未完了 7 件、follow_up でない未完了 0 件。

- **task 1260**（ready・source goal: goal 82・depth 1）runtime: クライアントモードの dagq の locate（と plugin の --resolve）は DB の path を出さずにクライアントモードであることを答え、doctor・service status・broker status・planners は制御側のコマンドであることの分かる error で拒む
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (3) worker・resume・headless の job の…dagq がクライアントモードで ask・show・note などを今までどおり使え
  - 理由: (3) は task 1236 の着地で満たされている: クライアントモードの locate は DB の path を出さずに client_mode: true・db: null・db_exists: null を答え、dagq skill も client_mode のとき init しないと書いている。doctor ほかは no_use_case で fail closed に断る。残るのは service への到達（hello）を locate に載せることと、断りの文を『制御側のコマンド』と分かる形にする改善だけで、(3) の達成には要らない。
  - 証拠: receipt of run 8375e094-f794-43ad-95a8-c67c8d3c9836 (task 1242) follow_ups[0]; event 56133; commit 36ad60b0 runtime: worker・resume・headless の job の dagq をクライアントモードにし、それらのプロセスに queue DB の path を渡さず、queue service 経由で今までの dagq のコマンドを使えるようにする; docs/design/queue-service.md の「クライアントモード」（locate は {client_mode: true, socket, db: null, db_exists: null} を答え、doctor は no_use_case）; plugins/claude-dagq/skills/dagq/SKILL.md 26 行目（client_mode: true … Never init）・reference/authority.md 22 行目
  - 所属先の案: new: queue service のクライアントモードの表示（locate の到達・断りの文）と token の後片付け（goal 82 の後続）（goal 82 から移す）
  - 処置の案: 実装済みとして cancel の候補。根拠: 主要部分（locate が path を出さずクライアントモードを答える・db_exists: false を出さない・skill が init しない・doctor ほかを DB を開かずに断る）は commit 36ad60b0（task 1236）で実装済み。未実装は locate に hello の結果と API の version を載せることと、no_use_case の文を制御側のコマンドと分かる文にすることだけ。残りを採るなら description と acceptance をその差分に絞る必要がある
- **task 1324**（ready・source goal: goal 82・depth 1）runtime: run が終わったら worker の queue service の token（credentials/ の値と tokens/ の記録）を失効させて消す
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (2) service の起動・停止の責任と、落ちたときの知らせ方が ADR のとおりに動く
  - 理由: ADR-t1233-4 決定 4 は token が run の終わりで失効することを求め、service は run_holds_token で終わった run の token を断るので失効は成り立っている。残る credentials/ のファイルと tokens/ の記録の片付けは決定にも (2)(3) にも無い後始末で、無くても acceptance を満たしたと言える。
  - 証拠: receipt of run 8542edbf-06e4-4fd1-8f52-14d2b86e7d8c (task 1236) follow_ups[2]; event 59226; docs/adr/2026-10-02-t1233-4-queue-service-lifecycle-outage-notice-and-principal-tokens.md 決定 4; docs/design/queue-service.md の「まだ無いもの」1 項目目（未実装を確認）
  - 所属先の案: new: queue service のクライアントモードの表示（locate の到達・断りの文）と token の後片付け（goal 82 の後続）（goal 82 から移す）
- **task 1326**（ready・source goal: goal 82・depth 1）runtime: queue service の authorization_denied の断りに role・capability・reason を載せ、クライアントモードの dagq がローカルの CLI と同じ denied の形で出す
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (3) role の制限が service 側で判定される
  - 理由: (3) は role の判定が service 側にあることを求め、クライアントモードの断りは queue_service:{code: authorization_denied} で出ており判定は service 側で行われている。断りに role・capability・reason を載せてローカルの CLI と同じ形にするのは読みやすさの改善で、無くても acceptance を満たしたと言える。
  - 証拠: receipt of run 8542edbf-06e4-4fd1-8f52-14d2b86e7d8c (task 1236) follow_ups[4]; event 59230; docs/design/queue-service.md の「クライアントモード」の断りの項（{error, queue_service:{code}} のまま。未実装を確認）
  - 所属先の案: new: queue service のクライアントモードの表示（locate の到達・断りの文）と token の後片付け（goal 82 の後続）（goal 82 から移す）
- **task 1351**（ready・source goal: goal 65・depth 1）test: queue_service_reads の 2 本（the_read_roles_read_what_their_prompts_name_as_the_command_line_prints_it・a_worker_reads_what_a_measure_task_reads_as_the_command_line_prints_it）が負荷の下で kept changing で落ちる比べ方を直す
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (spike) 着地中に worker の run の優先度を下げたときの…が測られ / (進める場合) host の adapter が実装され…比べられている
  - 理由: source goal 65 の acceptance は着地の枠と CPU の取り分の spike・ADR・adapter・前後比較を求め、queue_service_reads の時刻に依る比べ方の不安定さはそのどれにも入らない。goal 82 も実装の項目で、この test の安定化を求めていない。負荷の下で関門を落とす test の修正は goal 37 の問題に合う。
  - 証拠: receipt of run 357172fe-31b4-4ff0-9e0a-3cbb414db1d0 (task 961) follow_ups[0]; event 60914; tests/it/queue_service_reads.rs 87 行目の panic!("{use_case:?} {params} kept changing")（未修正を確認）; dagq search 'kept changing' で他の task なし
  - 所属先の案: goal 37（goal 82 から移す）
- **task 1352**（ready・source goal: goal 65・depth 1）fix: test の process が Drop を通らずに終わると、test が起動した dagq service serve と一時のディレクトリが host に残る
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (spike) 着地中に worker の run の優先度を下げたときの…が測られ
  - 理由: source goal 65 の acceptance（着地の枠と CPU の取り分の spike・ADR・adapter・前後比較）は、test が起動した service serve の孤児の片付けを含まない。test が host に残す process の問題は goal 101（test が取り残すものを host に残さない）に合う。
  - 証拠: receipt of run 357172fe-31b4-4ff0-9e0a-3cbb414db1d0 (task 961) follow_ups[1]; event 60916; src/infrastructure/queue_service.rs の queue_gone（DB が残れば止まらない。未修正）; note 65104（ppid 1 の service serve が 10 個残っていた調べ）
  - 所属先の案: goal 101（goal 82 から移す）
- **task 1464**（ready・source goal: goal 87・depth 1）fix: クライアントモードの dagq が queue service の 16 MiB を超える答えを途中で切って unreachable と報告するのを直し、絞り方を案内する断りにする
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (1)〜(5) いずれも planner の経路と非対話化・評価・文書
  - 理由: source goal 87 の acceptance は planner の廃止と移譲・非対話の planner・評価・skill を求め、queue service の答えの 16 MiB の上限での切れ方は含まない。queue service のクライアントモードの不具合で、今の goal 82（(3) クライアントモードで今までどおり使える）が問題に合うので所属先は今のままでよい。
  - 証拠: receipt of run 5f471151-8eb7-4bc2-ac0b-1e1294ae69d1 (task 1401) follow_ups[0]; event 65301; src/infrastructure/queue_service.rs 383 行目 .take(MAX_REQUEST_BYTES as u64 * 16)（未修正を確認）; dagq goal show 87 --full の acceptance
  - 所属先の案: 今の goal 82 のまま（移動不要）
- **task 1629**（ready・source goal: goal 82・depth 2）test: runtime_headless::a_silent_turn_is_stopped_and_its_recovery_job_resumes_the_session（1583 の後はその行き先）が負荷の下で resume の turn も silent で止まって落ちる原因を直す
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (3) worker・resume・headless の job の…dagq がクライアントモードで…使え
  - 理由: goal 82 の acceptance は queue service とクライアントモードを求め、runtime_headless の沈黙の判定が負荷の下で resume の turn を誤って止める不安定さはそのどれにも入らない（元の task 1328 自体が goal 65 から移された test の修正）。負荷で落ちる test と runtime の判定の問題は goal 37 に合う。元の test は task 1583（commit e20efd41）で消えており、この task は行き先の境界の test を対象にする書き方になっている。
  - 証拠: receipt of run b7a02003-fc30-4bac-83a5-1c5b0669dd4b (task 1328) follow_ups[0]; event 79524; commit e20efd41 refactor: runtime_cleanup と runtime_headless（1557・1558 の対象を除く）の状態の判断を副作用のない関数の unit test に移し…（task 1583、a_silent_turn_is_stopped… はもう tests に無い）
  - 所属先の案: goal 37（goal 82 から移す）

### goal 87: planner を runtime が立てるものだけにして対話と非対話を切り替えられるようにし、非対話で 1 週間測って問題なければ非対話を既定にする

acceptance の版 1。follow_up 由来の未完了 1 件、follow_up でない未完了 1 件。

- **task 1576**（ready・source goal: goal 87・depth 1）plugin の skill と hook のコメントを dagq plan の廃止に合わせる
  - 分類: 元 goal に必須（required）
  - 項目: (5) plugin の skill と AGENTS.md が新しい流れに合っている
  - 理由: (5) は plugin が dagq plan の廃止の流れに合うことを求めるので、取りこぼしがあれば満たせない。ただし名指した箇所は task 1400 の commit で直っており、残る記述は廃止・拒否の説明だけに見える。
  - 証拠: receipt of run 6cc1b23c-5b6f-4787-bac5-a16fe020ed01 (task 1399) follow_ups[0]; event 74087; commit 8279f86b plugin: dagq-inbox・dagq-planner・dagq・dagq-recover の skill と AGENTS.md を、人が開く planner の廃止と inbox からの移譲の流れに書き直す（task 1400）; grep -rn 'dagq plan' plugins: session-start.sh:6・requests.md・up-down.md:7・register.md:65・dagq-planner/SKILL.md:3 はいずれも廃止・拒否の説明。goal-review-by-hand.md・session-event.sh に該当なし
  - 所属先の案: source goal に残す
  - 処置の案: 実装済みとして cancel の候補。根拠: description の列挙箇所（dagq-recover の 5・goal-review-by-hand.md・plan-review-by-hand.md・register.md・hooks のコメント）が commit 8279f86b（task 1400）で廃止の記述に直っている。依存の 1544（ready）の着地後に grep で残りが無いことを確かめれば cancel の候補。
  - 既存の判定 24: 今も成り立つ。(5) に必要という判定は今も成り立つ。ただし中身は 8279f86b で実質実装済みに見える。

### goal 89: 非対話の worker と planner の session wrapper を cmux の workspace なしで、supervisor から切り離した background の process として動かす

acceptance の版 1。follow_up 由来の未完了 1 件、follow_up でない未完了 1 件。

- **task 1657**（ready・source goal: goal 89・depth 1）runtime: background の wrapper の停止に使った signal（SIGTERM で終わったか SIGKILL まで要ったか）を記録し、評価の文書の wrapper_stopped を実在の記録に合わせる
  - 分類: 元 goal に必須（required）
  - 項目: (2) …止める経路で process が残らない / (4) cmux の呼び出しの失敗・残った workspace・startup の基準値と切り替え後の比較が文書にあり
  - 理由: 評価の文書の SIGKILL の基準は wrapper_stopped を数えるが runtime がその event を記録しないので常に 0 件になり、(4) の比較と (2) の process の残りを確かめる材料が欠ける。task 1409（1 週間後の判定）の前提でもある。
  - 証拠: receipt of run 779a05bb-8929-4ca6-88d0-476412309ef3 (task 1408) follow_ups[1]; event 81990; src 全体に wrapper_stopped / WrapperStopped が無い（grep 0 件）: 未実装; task 1440 は goal 92 で ready（依存）; task 1409 は goal 89 で draft
  - 所属先の案: source goal に残す
  - 既存の判定 23: 今も成り立つ。判定は今も成り立つ。

### goal 90: worker が review に渡す前に受け入れ条件の各項目を根拠と照合し、初回の review の差し戻しと着地までの時間を減らす

acceptance の版 1。follow_up 由来の未完了 1 件、follow_up でない未完了 0 件。

- **task 1537**（draft・source goal: goal 90・depth 1）goal 90 (4): measure the after interval again with docs/plans/acceptance-check/after.sh once it has N runs
  - 分類: 元 goal に必須（required）
  - 項目: (4) 変更の後の同じ本数の reviewed run について同じ script の前後比較があり、層をそろえた比較と本数・限界が書かれている
  - 理由: task 1423 の後の区間は T が未確定で値が全て『未取得』の暫定で、docs/plans/acceptance-check.md 自身が『goal 90 の受け入れ条件 (4) を満たすとは判定しない』と書く。後の区間を N 本そろえて測り直すこの task なしでは (4) を満たせない。
  - 証拠: receipt of run f7d0d744-913d-44fa-9cad-32ea792d10a2 (task 1423) follow_ups[0]; event 71050; docs/plans/acceptance-check.md 24 行と 336〜382 行（後の区間 task 1423 は暫定、after は 0（未取得））; commit 2205c584 measure: 受け入れ条件の対応づけ（task 1420・1421）が効いた後の reviewed run を…
  - 所属先の案: source goal に残す

### goal 92: cmux を inbox だけが使う形の runtime の部分を進め、tests/it と e2e の cmux への依存と本数を減らして着地の検証の実行時間を縮める

acceptance の版 2。follow_up 由来の未完了 1 件、follow_up でない未完了 6 件。

- **task 1577**（ready・source goal: goal 87・depth 1）ADR-t1433-2 決定 5: 廃止前から開いている人の planner の行を閉じた扱いにして cmux を呼ばない
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: goal 87 (2) inbox からの移譲・dagq plan の拒否
  - 理由: goal 87 の acceptance は人の planner の廃止と inbox からの移譲を求め、廃止の時点で開いていた人の planner の扱いは task 1399 が ADR-t1394-1 決定 9 のとおり実装して満たしている。ADR-t1433-2 決定 5 の cmux を呼ばない扱いは後の goal 92 の決定で、goal 92 の acceptance (4) が明示しているので、goal 87 は実施しなくても満たせる。
  - 証拠: receipt of run 6cc1b23c-5b6f-4787-bac5-a16fe020ed01 (task 1399) follow_ups[1]; event 74089; goal 92 の acceptance (4)「廃止前から開いている人の planner の行も cmux を呼ばずに閉じた扱いにする」; src/application/planner.rs の close_exited_person_planners と PERSON_PLANNER_CLOSE_GRACE_SECS がまだある（未実装）; judgement 3
  - 所属先の案: 今の goal 92 のまま（移動不要）
  - 既存の判定 3: 今も成り立つ。今も成り立つ。goal 87・92 の acceptance の版は変わっておらず（87 は v1、92 は v2 だが judgement は 87 の v1 に対するもの）、goal 92 の acceptance (4) がこの作業を含む。needs_recheck は false。

### goal 93: broker の e2e の image の build を速くして、host の e2e の工程の待ちを縮める

acceptance の版 1。follow_up 由来の未完了 2 件、follow_up でない未完了 2 件。

- **task 1600**（ready・source goal: goal 93・depth 1）e2e の印の例に一時除外したケースの名前が使われている
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (1) build 識別子だけが変わった build で依存の crate を compile し直さない / (3) ADR-t827-1 の版の一致を弱めない
  - 理由: goal 93 の acceptance は broker の image の build のキャッシュ・前後の測定・版の一致で、e2e の印の書式の例の名前は含まない。実施しなくても満たせる。e2e の印（quarantine）は goal 75 の仕組み。
  - 証拠: receipt of run d1eae627-ff2b-44fc-ae3d-209811406d46 (task 1582) follow_ups[1]; event 76883; grep: auto-update.md:49・src/domain/e2e_quarantine.rs:7・.config/e2e-quarantine.toml:12 に up_in_cmux_… が残る（未実施）
  - 所属先の案: goal 75（goal 93 から移す）
- **task 1601**（ready・source goal: goal 93・depth 1）tests/e2e.rs の module doc が、存在しない CODEX_WORKER_E2E_EXCLUSIONS を名指している
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (1)〜(3) は broker の image の build のキャッシュと測定と版の一致
  - 理由: tests/e2e.rs の module doc の古い Codex の除外の記述は goal 93 の acceptance に関係せず、実施しなくても満たせる。e2e をどこで流すか（ADR-t1233-2 決定6）の記述の整合で、goal 66 の問題に合う。
  - 証拠: receipt of run d1eae627-ff2b-44fc-ae3d-209811406d46 (task 1582) follow_ups[2]; event 76885; grep: tests/e2e.rs:26 が CODEX_WORKER_E2E_EXCLUSIONS を名指し、src に定数は無い（未実施）
  - 所属先の案: goal 66（goal 93 から移す）

### goal 97: follow-up を元の goal の受け入れ条件の達成に必要かで所属させ、必須の作業と発見済みの follow-up の所属判断が済んだ goal を閉じられるようにする

acceptance の版 1。follow_up 由来の未完了 1 件、follow_up でない未完了 3 件。

- **task 1648**（ready・source goal: goal 97・depth 1）integrate の register_follow_ups が元の goal の閉鎖を transaction の外で読み、間に閉じると follow-up が失われる
  - 分類: 元 goal に必須（required）
  - 項目: (1) runtime の契約（…閉じる条件と並行・再起動での検査…） / (3) 判定と close の間の並行・再起動でも閉じる条件を検査し直す
  - 理由: 登録と close の競合で follow-up が失われると、所属の判断を受けないまま goal が閉じうる。(3) の並行での検査と ADR-t1504-2 決定 8 の契約に必須だが、中身は task 1505 の commit 4855bc84 で入ったとみられる。integrate は goal_closed を読まなくなり、SqliteQueue::register_follow_ups が BEGIN IMMEDIATE の中で source goal と開閉を読む。
  - 証拠: receipt of run 22254e6a-db9d-4a25-abcb-75d3c04e0feb (task 1504) の follow_ups（category defect）; event 81051; commit 4855bc84 runtime: follow_up の draft の所属の判断…を記録する CLI と行を足し…（Dagq-Task 1505。本文に『Registration snapshots source task goal/state in its transaction』『The obsolete caller-supplied goal_closed parameter is removed』）; src/application/integrate.rs:915-920 のコメントと src/infrastructure/draft_planners.rs:76-86（transaction の中で source_goal・goal_open を読む）; 1648 の created_at は 2026-10-04T01:57Z で、4855bc84 は 03:49Z。登録の後に実装された
  - 所属先の案: source goal に残す
  - 処置の案: 実装済みとして cancel の候補。根拠: commit 4855bc84（task 1505）で integrate の外での goal_closed の読み取りが無くなり、登録の transaction の中で source goal と開閉を読むようになった（acceptance (1)）。(2) の「integrate が開いていると見た後・登録の前に閉じる」場合を名指す test があるかは確かめきれていない。ただ、外で読む値が無くなったので競合の窓そのものが無い。cancel の前に、runtime_integrate::integrate_registers_the_landed_follow_ups_as_draft_tasks_of_the_goal_once が閉じた source の場合を確かめているかを見る

### goal 98: review・revise 中の run を表示と開発フローの判定で取りこぼさない

acceptance の版 1。follow_up 由来の未完了 1 件、follow_up でない未完了 0 件。

- **task 1651**（ready・source goal: goal 98・depth 1）plugin: dagq-recover の reference/doctor.md と dagq skill の reference/inspect.md の status・doctor の runs の記述を、task 1520 の一覧の範囲と progress・lease_pid・blocker に合わせる
  - 分類: 元 goal に必須（required）
  - 項目: (4) 各変更は自動テストと現在の設計・plugin の記述で裏づけられる
  - 理由: (4) は plugin の記述での裏づけを求め、task 1520 が変えた status・doctor の runs の範囲と progress・lease_pid・blocker を dagq-recover の reference/doctor.md と dagq の reference/inspect.md が古いまま書いている。直さないと (4) を満たしたと言えない。
  - 証拠: receipt of run e9536c58-fd75-45ad-a56f-142528c22ca6 (task 1520) follow_ups[0]; event 81618; plugins/claude-dagq/skills/dagq-recover/reference/doctor.md 10 行目（claimed..integrating だけ。未修正を確認）; plugins/claude-dagq/skills/dagq/reference/inspect.md 24 行目（runs: unfinished runs with their leases）
  - 所属先の案: source goal に残す

### goal 100: runtime の責務をレイヤーとコンテキスト（計画管理・実行と着地・観測と分析・host 運用）の 2 軸で分け、変更の集中する箇所と実時間に依る判断を狭め、境界を設計文書・review の subagent・機械的な検査で守る

acceptance の版 1。follow_up 由来の未完了 1 件、follow_up でない未完了 8 件。

- **task 1650**（ready・source goal: goal 3・depth 1）application::queue_reads::answer を context ごとの読み取りに分ける
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (1)〜(7): domain の純粋さ・非公開の欄・newtype・時刻/ID の注入・use case の application への移動・test・ADR
  - 理由: goal 3 の acceptance は queue_reads::answer の context ごとの分割を求めず、実施しなくても満たせる。context ごとの分割と要る port だけを取ることは goal 100 の (1)(4) の問題で、今の goal 100 に置かれている。
  - 証拠: receipt of run e080a3ed-ad7d-47c6-bc29-2c89b6b89cda (task 1549) follow_ups[0]; event 81275; task 1650 の context（ADR-t1504-1 決定 1・3 で goal 3 では範囲外、置き先は goal 100）; goal 100 の event 90261（1650 を受け入れ条件に要る task として high にした）
  - 所属先の案: 今の goal 100 のまま（移動不要）

### goal 101: test が取り残すシェルのループと、runtime の lsof による cwd 走査で host に掛かる負荷をなくす

acceptance の版 1。follow_up 由来の未完了 1 件、follow_up でない未完了 0 件。

- **task 1630**（ready・source goal: goal 101・depth 1）test: background wrapper processes the tests start (real `dagq session --background`) are not stopped on a test's timeout
  - 分類: 判断できない（undecided）
  - 項目: test が成功・失敗・panic・タイムアウトのどれで終わっても、test が起動したシェルのループ（待ちの while と無限ループ）が host に残らない
  - 理由: acceptance の字面は『シェルのループ』で、本物の detached な dagq の wrapper（session --background）は該当せず、task 1580 の後は turn も test の process とともに終わり、wrapper も自分の heartbeat・上限で終わる。一方 planner は『時間切れでも host に残さない』の残りの穴として採っており、goal が test の起動した process 全般を含む意図かで結論が変わる。
  - 証拠: receipt of run 65029e8f-df34-4b15-9e24-40346c53fae5 (task 1580) follow_ups[0]; event 79631; tests/it/runtime_background.rs・runtime_background_process.rs・planner_headless.rs・plan_review.rs に on_timeout の登録が無い（grep 0 件）: 未実装; task 1439（goal 92, ready）が runtime の it を background の wrapper で動かすので対象の test が増える見込み
  - out_of_scope と決めた場合の所属先の案: new: test の fixture と補助の process の隔離と後片付けを固める（template の上書き・EXDEV・残る template・host の lock・tracing の capture・時間切れで残る wrapper）（out_of_scope と決めた場合の案）
  - 人の確認: 要る。

### goal 102: supervisor の終わった run の worktree の片付けが CPU を使い続けないようにする（2026-10-03 の supervisor の 1 コア前後の常用から）

acceptance の版 1。follow_up 由来の未完了 1 件、follow_up でない未完了 1 件。

- **task 1605**（ready・source goal: goal 102・depth 1）supervisor が git を呼ぶときの message の言語を固定する
  - 分類: 元 goal に必須（required）
  - 項目: (3) task が completed / canceled の run の、run_dir の下の .git の壊れた worktree が、cleanup_failed を繰り返さずに片付くか、片付けの候補から外れる
  - 理由: task 1587 の片付け（may_remove_broken）は git の stderr を英語の部分一致で読むので、git の翻訳が入った locale の supervisor では一致せず cleanup_failed に戻り、(3) が host の locale 次第になる。今の host の message は英語（event 75379）だが、acceptance は runtime の性質として片付くことを求めるので、message を固定するこの task なしでは (3) を満たしたとは言い切れない。
  - 証拠: receipt of run 7aad617e-ed02-463a-b04d-5894d173eb24 (task 1587) follow_ups[0]; event 77308; src/infrastructure/adapters.rs の LC_ALL=C は ps の呼び出し（201 行）だけで、worktree remove / repair には無い
  - 所属先の案: source goal に残す

### goal 107: status の 1 回の呼び出しで同じ run の events を重ねて読まないようにし、頻繁に呼ばれる読み取りの問い合わせを減らす

acceptance の版 1。follow_up 由来の未完了 1 件、follow_up でない未完了 0 件。

- **task 1653**（ready・source goal: goal 98・depth 1）refactor: application::health::status で同じ run の run_events を一度だけ読み、runs・slots_and_waits で使い回す
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: goal 98 (1) status と doctor の一覧の取りこぼしと工程・枠の表示 / (4) 各変更の test と文書の裏づけ
  - 理由: goal 98 の acceptance は表示の取りこぼし・forecast・up --no-wait の拒否とその裏づけで、出力を変えない run_events の読み取り回数の削減は求めないので、実施しなくても満たせる。置き先は context のとおり goal 107 で、移動は済んでいる。
  - 証拠: receipt of run e9536c58-fd75-45ad-a56f-142528c22ca6 (task 1520) follow_ups[2]; event 81622; goal 98 の acceptance（event 69857 の版）; goal 107 の description（goal 98 の範囲外として作った）; task 1653 の context（planner 938 の訂正）
  - 所属先の案: 今の goal 107 のまま（移動不要）
  - 採用の注意: 入力の existing_judgements が空で、goal 107 の follow_up_memberships でも判定の記録が 0 件。context に書かれた planner 938 の判断が follow_up_judged として記録されていないので、ADR-t1504-2 の記録が要る（判断の内容はこの案と同じ）。

### goal 108: goal review が follow-up の所属の判断待ちで起動せず open に留まる goal を、止めている follow-up と理由とともに status・doctor の attention で見えるようにする

acceptance の版 1。follow_up 由来の未完了 1 件、follow_up でない未完了 0 件。

- **task 1660**（ready・source goal: goal 97・depth 1）所属の判断待ちで goal review が起動しない goal を status/doctor の attention で見えるようにする
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: goal 97 の (3) 未判定か要再確認の follow-up のある goal を goal review も goal close も achieved で閉じず…
  - 理由: goal 97 の (3) は判断の済んでいない follow-up のある goal を閉じないことまでを求め、その状態を attention で知らせることは求めていない（task 1507 が閉じない側を実装した）。この可視化が無くても goal 97 は満たせる。行き先は今いる goal 108 で、acceptance がこの task の中身そのもの。
  - 証拠: receipt of run cc801ef6-5ffe-4fc8-8644-f30ba3626b24 (task 1507) の follow_ups（category improvement）; event 82280; goal 108 の acceptance (1)〜(3); src/application/health.rs に該当の attention が無い（src に goal_follow_ups_unsettled が無い、未実装）
  - 所属先の案: 今の goal 108 のまま（移動不要）
  - 採用の注意: judge-follow-up の記録（existing_judgements）が無い。set-goal で goal 108 に移されているが、所属の判断の行は judge-follow-up で記録する必要がある（goal 97 の閉じる条件で未判定として数えられる）

### goal 109: runtime が適用する人の答え（approve_goal・correct_goal）の status の表示を、supervisor が適用する判定（answer 時に記録した runtime_delivers）と揃える

acceptance の版 2。follow_up 由来の未完了 1 件、follow_up でない未完了 0 件。

- **task 1661**（ready・source goal: goal 97・depth 1）runtime: 答えた approve_goal・correct_goal の ask の status の attention を、answer 時に記録した runtime_delivers で判定し、supervisor が適用する対象と揃える
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (4) achieved の後の誤分類を、達成の履歴を残した訂正の記録と人への判断にできる
  - 理由: source goal 97 の (4) は task 1509 が満たし、supervisor は記録した runtime_delivers で答えを適用する。status の attention が今の状態で判定し直して食い違うのは表示の整合で、無くても (4) を満たしたと言える。所属先はこのために作った goal 109 でよい。
  - 証拠: receipt of run e6cc9e1f-9055-4d2c-beb5-df48acb81614 (task 1509) follow_ups[0]; event 82631; src/application/health.rs 1503・1511 行目（applies_goal_answer・applies_correction_answer で今の状態を判定。未実装を確認）
  - 所属先の案: 今の goal 109 のまま（移動不要）
  - 既存の判定 1: 今も成り立つ。判定のとおり goal 97 の (4) はこれ無しで満たされ、health.rs はまだ今の状態で判定し直しているので task は今も要る。needs_recheck は false

### goal 111: 対話の worker の廃止を CLI・worker の prompt・plugin・AGENTS.md・docs/design に反映し、cmux を inbox だけにする形の案内を揃える

acceptance の版 1。follow_up 由来の未完了 1 件、follow_up でない未完了 1 件。

- **task 1544**（ready・source goal: goal 92・depth 1）plugin の skill と hook の記述を goal 92 の ADR-t1433-1〜5 に合わせる
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: goal 92 v2 の末尾: prompt・plugin・AGENTS.md・docs/design の対話の worker の案内の除去は…この goal の acceptance ではない
  - 理由: goal 92 の acceptance（版 2）は plugin の案内の除去を明示的に別の goal の acceptance とし、この task なしで満たせる。
  - 証拠: receipt of run aa91a7cb-0c71-4d5f-a8ce-85a4008fedcc (task 1433) follow_ups[0]; event 71587; goal 92 の acceptance（acceptance_version 2）の末尾の文; goal 111 の acceptance (2)
  - 所属先の案: 今の goal 111 のまま（移動不要）
  - 既存の判定 2: 今も成り立つ。acceptance_version 2 のまま変わっておらず、判定は今も成り立つ。

### goal 112: worker が変えた挙動を説明する文書の候補を、変えた名前で探して照合し、初回の run の review の docs_drift の差し戻しを減らす

acceptance の版 2。follow_up 由来の未完了 1 件、follow_up でない未完了 1 件。

- **task 1727**（ready・source goal: goal 112・depth 1）config: design-consistency の review の subagent の paths を plugin・tests・scripts・AGENTS.md・dagq.toml・.dagq の変更にも当て、ADR-t1688-1 決定 2 の探した名前の確かめを goal 112 の実例の型の差分で効かせる
  - 分類: 元 goal に必須（required）
  - 項目: (3) ….dagq/review-agents/design-consistency.md が summary の探した名前と差分の名前の対応を確かめる / (4)
  - 理由: design-consistency.md に ADR-t1688-1 の項目はあるが、dagq.toml の paths が plugins/・tests/・AGENTS.md に当たらず、goal 112 が見落としの型に挙げる 1462・1400 の差分で確かめが走らない。(3) の『確かめる』と (4) の効果の測定がこれなしでは実例の型で成り立たない。
  - 証拠: receipt of run 43ca40ac-f40a-4c91-8048-b4630765c2e2 (task 1688) follow_ups[0]; registration_event_id は null（source_goal_provenance recorded）; dagq.toml 194〜195 行の design-consistency の paths は src・crates・migrations・docs/design・docs/development のまま: 未実装; .dagq/review-agents/design-consistency.md 7 行に ADR-t1688-1 の項目がある
  - 所属先の案: source goal に残す
  - 既存の判定 32: 今も成り立つ。判定は今も成り立つ（worker の membership_proposal は undecided だったが、planner の required の根拠は dagq.toml で確かめられる）。

### goal 115: レイヤー境界の違反を解消し、scripts/check-layer-deps.sh の許可の一覧を減らす

acceptance の版 1。follow_up 由来の未完了 7 件、follow_up でない未完了 0 件。

- **task 1616**（ready・source goal: goal 100・depth 1）domain の test から crate::application::timestamp を無くす（stats.rs・stats/thresholds.rs・stats/conflicts.rs）
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (2) 今ある違反は理由と行き先の task を持つ許可の一覧にだけある
  - 理由: goal 100 の (2) は違反が行き先 task 付きで許可の一覧にあることまでを求め、違反の解消は求めない。architecture.md の「今の違反と行き先」に L1 の行（行き先 task 1616）があり許可の一覧も行き先を持つので、実施しなくても (2) を満たす。
  - 証拠: receipt of run 00364554-d18c-4735-8a91-565a7f6fd240 (task 1545) follow_ups[1]; event 78461; docs/design/architecture.md「今の違反と行き先」の L1 の行（task 1616）; goal 100 の event 90261（planner:976 の note、request 19 で人が承認）: 1615〜1620・1637・1621 は goal 100 の受け入れ条件に要らないとして goal 115 へ移した; grep: src/domain/stats.rs:2213・stats/thresholds.rs:830・stats/conflicts.rs:418 に crate::application::timestamp が残る（未実施）
  - 所属先の案: 今の goal 115 のまま（移動不要）
- **task 1617**（ready・source goal: goal 100・depth 1）domain::landing_branch が anyhow を返すのを DomainError に直す
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (2) 今ある違反は理由と行き先の task を持つ許可の一覧にだけある
  - 理由: 違反の解消は goal 100 の acceptance に無く、L2（landing_branch の anyhow）は行き先 task 1617 付きで「今の違反と行き先」と許可の一覧に載っているので (2) は満たせる。
  - 証拠: receipt of run 00364554-d18c-4735-8a91-565a7f6fd240 (task 1545) follow_ups[2]; event 78463; docs/design/architecture.md「今の違反と行き先」の L2 の行（task 1617）; goal 100 の event 90261（planner:976 の note、request 19 で人が承認）: 1615〜1620・1637・1621 は goal 100 の受け入れ条件に要らないとして goal 115 へ移した; grep: src/domain/landing_branch.rs:82 に anyhow::Result が残る（未実施）
  - 所属先の案: 今の goal 115 のまま（移動不要）
- **task 1618**（ready・source goal: goal 100・depth 1）application の SystemTime::now（supervise/jobs.rs:220・headless_session.rs:531）を注入した Clock に替える
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (2) 今ある違反は理由と行き先の task を持つ許可の一覧にだけある / (4) revise・reopen・resume・stall の時間と観測に依る判断が…関数と unit test に移り
  - 理由: L4 の SystemTime::now（jobs.rs・headless_session.rs）は行き先 task 1618 付きで一覧にあり (2) を満たす。(4) の時間の判断の移動は revise・reopen・resume・stall が対象で、jobs.rs と headless_session.rs の壁時計は含まない。
  - 証拠: receipt of run 00364554-d18c-4735-8a91-565a7f6fd240 (task 1545) follow_ups[3]; event 78465; docs/design/architecture.md「今の違反と行き先」の L4 の行（task 1618）; goal 100 の event 90261（planner:976 の note、request 19 で人が承認）: 1615〜1620・1637・1621 は goal 100 の受け入れ条件に要らないとして goal 115 へ移した; grep: src/application/supervise/jobs.rs:220・headless_session.rs:63,669 に SystemTime::now が残る（未実施）
  - 所属先の案: 今の goal 115 のまま（移動不要）
- **task 1619**（ready・source goal: goal 100・depth 1）application::planner_handoff の test が使う infrastructure::run_files::LocalRunFiles を application::memory_files に替える
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (2) 今ある違反は理由と行き先の task を持つ許可の一覧にだけある
  - 理由: L3（planner_handoff の test の LocalRunFiles）は行き先 task 1619 付きで一覧にあり、違反の解消は goal 100 の acceptance に無い。
  - 証拠: receipt of run 00364554-d18c-4735-8a91-565a7f6fd240 (task 1545) follow_ups[4]; event 78467; docs/design/architecture.md「今の違反と行き先」の L3 の行（task 1619）; goal 100 の event 90261（planner:976 の note、request 19 で人が承認）: 1615〜1620・1637・1621 は goal 100 の受け入れ条件に要らないとして goal 115 へ移した; grep: src/application/planner_handoff.rs:71 に LocalRunFiles が残る（未実施）
  - 所属先の案: 今の goal 115 のまま（移動不要）
- **task 1620**（ready・source goal: goal 100・depth 1）infrastructure::queue_service の crate::view::task_detail への参照を無くし、application::prompt と application::health を context ごとに分ける
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (2) 今ある違反は理由と行き先の task を持つ許可の一覧にだけある
  - 理由: L6（queue_service の crate::view::task_detail）は行き先 task 1620 付きで一覧にあり、違反の解消は goal 100 の acceptance に無い。
  - 証拠: receipt of run 00364554-d18c-4735-8a91-565a7f6fd240 (task 1545) follow_ups[5]; event 78469; docs/design/architecture.md「今の違反と行き先」の L6 の行（task 1620）; goal 100 の event 90261（planner:976 の note、request 19 で人が承認）: 1615〜1620・1637・1621 は goal 100 の受け入れ条件に要らないとして goal 115 へ移した; grep: src/infrastructure/queue_service.rs:850 に crate::view::task_detail が残る（未実施）
  - 所属先の案: 今の goal 115 のまま（移動不要）
- **task 1621**（ready・source goal: goal 100・depth 1）越境の transaction T1・T2・T6 の中で、他の context の表を直接の SQL ではなく所有する context の infrastructure の関数で書く
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (1) …境界をまたぐ transaction と、今の違反とその行き先の task を持ち / (4) submodule は自分の context の状態だけを変える
  - 理由: (1) は越境の transaction を設計文書に明記するまでで task 1545 が済ませた。(4) は impl Supervisor の submodule の共有状態の話で、infrastructure の SQL の書き込みの所有（T1・T2・T6）は含まないので、実施しなくても goal 100 の acceptance を満たす。
  - 証拠: receipt of run 00364554-d18c-4735-8a91-565a7f6fd240 (task 1545) follow_ups[6]; event 78471; goal 100 の event 90261（1621 の判断の理由: (1) は 1545 で済み、(4) は SQL の書き込みの所有を含まない）; docs/design/architecture.md の X3（249 行）と「今の違反と行き先」の X3・C1 の行（未登録のまま、未実施）
  - 所属先の案: 今の goal 115 のまま（移動不要）
- **task 1637**（ready・source goal: goal 100・depth 1）config: check-layer-deps.sh の字句の処理で、入れ子の /* */・複数行の #[cfg(test)]・test が必須の cfg（cfg(all(test, ...)) など）・inline mod の中の #[cfg(test)] mod を正しく扱い、本番のコードを数え落とさずに誤検出しないようにする
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (2) 禁止依存を検査する script が CI と task の verify で流れ、新しい違反で exit 1 になり…
  - 理由: (2) は script が流れ新しい違反と古い項目で exit 1 になることを求め、task 1546 で満たされている。入れ子の /* */・複数行の cfg などの字句の精度の改善は (2) に書かれておらず、実施しなくても満たせる。
  - 証拠: receipt of run f298ee8f-c527-48e7-ad5e-a35cde5cfb76 (task 1546) follow_ups[0]; event 80082; goal 100 の event 90261（planner:976 の note、request 19 で人が承認）: 1615〜1620・1637・1621 は goal 100 の受け入れ条件に要らないとして goal 115 へ移した; task 1546（completed）が scripts/check-layer-deps.sh を足した; scripts/check-layer-deps.sh:146 は /* を in_block=1 の 1 段で扱う（入れ子は未対応、未実施）
  - 所属先の案: 今の goal 115 のまま（移動不要）

### goal 116: runtime の planner が人の planner_question の答えを待つあいだ runtime_planners の枠を空け、答えで新しい planner が続ける

acceptance の版 1。follow_up 由来の未完了 1 件、follow_up でない未完了 0 件。

- **task 1704**（ready・source goal: goal 96・depth 0）docs: ADR で、人の planner_question の答えだけを待つ runtime の planner が runtime_planners の枠を持ち続けるか空けるか、空けるときの条件・文脈の引き継ぎ・答えの届け方・planner の種類ごとの扱いを決める（ADR-t1704-1）
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (3) Spike の着地から再計画の planner の起動までが session の保持なしでつながり、planner と worker が互いの枠を持って待たない
  - 理由: goal 96 (3) は Spike の着地から再計画の planner までの経路の話で、ADR-t1487-1 決定 5 で依頼が着地と同じトランザクションで永続し Spike の planner も worker も待たないので、この ADR なしで満たせる。無関係な planner が人の planner_question を待って runtime_planners の枠を持つのは runtime の planner 全般の問題で、そのための goal 116 がある。既存の判定 28 は今も成り立つ。
  - 証拠: receipt of run 56dfaeaa-897d-47c2-b6b8-6907f23c7f0d (task 1487) follow_ups[0]（membership_proposal は undecided、(3)）; registration_event_id は null; dagq goal show 96 --full の acceptance (3); docs/adr に t1704 の ADR はまだ無い
  - 所属先の案: 今の goal 116 のまま（移動不要）
  - 既存の判定 28: 今も成り立つ。goal 116 の acceptance がこの ADR の内容そのもので、現 goal も 116。needs_recheck は false

### goal 117: この repository の文書と skill の中の相対 link の切れ（file や見出しの anchor の不一致）を直す

acceptance の版 1。follow_up 由来の未完了 1 件、follow_up でない未完了 0 件。

- **task 1705**（ready・source goal: goal 94・depth 2）release skill の README の Release 節への link の anchor が切れている
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: goal 94 (6) 整理の前の AGENTS.md と plugin の固有の記述の全項目が新しい正本で見つかり ... 規則の本文の重複が無く
  - 理由: README.md#release の anchor の切れは task 1607 より前からあり、規則の移動や重複ではないので、goal 94 の acceptance は実施しなくても満たせる（goal 94 は achieved で閉じた）。置き先の goal 117 は link の切れを受ける goal で合っている。
  - 証拠: receipt of run 097be999-9251-4f76-82fe-78371d489a73 (task 1607) follow_ups[0]（membership_proposal も out_of_scope）; event 90447 task_goal_changed 94→117; event 90448 follow_up_judged; event 90676 goal_closed（goal 94、2026-10-04T11:00Z、achieved）; .claude/skills/release/SKILL.md:8 は今も README.md#release（未実装）、README.md に Release の見出しは無い
  - 所属先の案: 今の goal 117 のまま（移動不要）
  - 既存の判定 29: 今も成り立つ。今も成り立つ。link はまだ切れたままで、goal 94 の acceptance の版も 1 のまま。needs_recheck は false。
  - 採用の注意: source goal 94 は閉じている（2026-10-04T11:00Z）が、登録（2026-10-04T10:43Z）の時点では open で、planner の follow_up_adopted（event 90453）で採用済み。登録時に閉じていなかったので ADR-t808-1・ADR-t1504-2 決定 5 の人の adopt は要らない。

### goal 120: goal 100 の Supervisor の状態の分割の後に、supervise の中の判断を行き先の context の module で unit test に移し、tests/it の重い module を境界に絞る

acceptance の版 1。follow_up 由来の未完了 1 件、follow_up でない未完了 6 件。

- **task 1729**（ready・source goal: goal 119・depth 1）test: goal 100 の状態の分割の後に、runtime_heartbeat::the_supervisor_policy_retries_until_the_leases_would_go_stale を HeartbeatPolicy を持つ module の #[cfg(test)] の unit test に移す
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: goal 119 の (1) 判断が src の…unit test に移り…対応づけられ / (4) …supervise/mod.rs…を変えていない
  - 理由: goal 119 の (4) と constraints が supervise/mod.rs を変えることを禁じていて、task 1709 の receipt は HeartbeatPolicy の 1 本を延期として理由つきで対応づけている。これを移さなくても goal 119 の (1) は満たせる。
  - 証拠: receipt of run 278d3f15-ff01-425e-8af0-33b628edcaf9 (task 1709) の follow_ups（category remaining_scope、membership_proposal は out_of_scope）; judgement 33
  - 所属先の案: 今の goal 120 のまま（移動不要）
  - 既存の判定 33: 今も成り立つ。今も成り立つ。goal 119 の acceptance_version は 1 のまま。goal 120 は 1552・1553 の後に supervise の判断を unit test に移す goal で合う

### goal 122: goal review が作った goal_gap の draft を runtime の planner が set-goal で別の goal に移すのを runtime が拒む（ADR-t1504-2 決定10）

acceptance の版 1。follow_up 由来の未完了 1 件、follow_up でない未完了 0 件。

- **task 1724**（ready・source goal: goal 97・depth 1）runtime: runtime の planner が goal_gap の draft を set-goal で元の goal の外（別の goal か goal なし）へ移すのを理由つきで拒む（ADR-t1504-2 決定10）
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (1) ADR が…set-goal による迂回を塞ぐこと…を決めている / (2) …ADR-t808-1 の上限と権限を所属の変更で迂回できない
  - 理由: goal 97 の (1) は ADR が契約を決めることを求め、ADR-t1504-2 決定 10 で決まっている。(2) の runtime の強制は follow_up の draft が対象で、goal review の goal_gap の draft は follow_up でないので、拒否を実装しなくても (2) を満たしたと言える。
  - 証拠: receipt of run c9db74e7-7802-4f62-bd01-e9ff853dec21 (task 1510) follow_ups[0]; registration_event_id は null（source_goal_provenance: recorded）; src/infrastructure/follow_up_membership.rs に goal_gap の検査が無い（grep で確認）; docs/adr/2026-10-04-t1504-2-*.md 決定 10
  - 所属先の案: 今の goal 122 のまま（移動不要）
  - 既存の判定 30: 今も成り立つ。goal_gap は follow_up でなく goal 97 の (2)〜(4) の強制の対象外という判定は今も成り立つ。未実装で、goal 122 がこのための goal。needs_recheck は false

### goal 123: AGENTS.md の大きさの上限（9,216 byte）に余裕を戻し、短い参照の 1 行を足すたびに他の行を削る状態をなくす

acceptance の版 1。follow_up 由来の未完了 1 件、follow_up でない未完了 0 件。

- **task 1725**（ready・source goal: goal 97・depth 1）docs: AGENTS.md を読む案内の参照を失わずに詰め、上限（9,216 byte）から目安 800 byte 以上の余裕を戻す
  - 分類: 範囲外で別 goal（out_of_scope）
  - 項目: (5) …AGENTS.md は短い参照に留まる
  - 理由: goal 97 の (5) は AGENTS.md が短い参照に留まることを求め、AGENTS.md は 9,215 byte で上限 9,216 内に収まり満たされている。余裕の回復は一般の保守。
  - 証拠: receipt of run c9db74e7-7802-4f62-bd01-e9ff853dec21 (task 1510) follow_ups[1]; wc -c AGENTS.md = 9215、scripts/check-agents-md-size.sh:17 limit=9216; task 1510 の receipt の membership_proposal（out_of_scope）
  - 所属先の案: 今の goal 123 のまま（移動不要）
  - 既存の判定 31: 今も成り立つ。AGENTS.md は今も 9,215 byte で余裕 1 byte、判定は成り立つ。
## goal ごとの件数

2026-10-04T15:24:07Z の `dagq goal list` の open の goal 86 件。「未完了」は draft・submitted・ready・in_progress の和、「follow_up 由来」はこの文書で分類した件数。

| goal | 題（短縮） | total | completed | canceled | draft | submitted | ready | in_progress | 未完了 | うち follow_up 由来 |
|---|---|---|---|---|---|---|---|---|---|---|
| 3 | Rust runtime を domain / application / infrastructu… | 63 | 30 | 28 | 0 | 0 | 5 | 0 | 5 | 3 |
| 8 | maintainer の機械的な作業を runtime に寄せる: needs_session の自… | 22 | 16 | 5 | 0 | 0 | 1 | 0 | 1 | 1 |
| 11 | 実行効率: 検証を integrate の 1 回にし、review を supervisor の工… | 48 | 30 | 13 | 0 | 0 | 5 | 0 | 5 | 5 |
| 12 | maintainer を退役させ、run 単位の job（review / triage）と定期起動… | 25 | 12 | 12 | 1 | 0 | 0 | 0 | 1 | 1 |
| 13 | 計画を queue の goal で表す: goal の入れ子・依存・rank・key、roadma… | 8 | 0 | 7 | 1 | 0 | 0 | 0 | 1 | 0 |
| 14 | roadmap の共有・同期を検討する（書き出し、外部 tracker、共有ストア） | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| 17 | Keep cmux backend calls reliable under high host l… | 7 | 5 | 1 | 0 | 0 | 1 | 0 | 1 | 1 |
| 20 | タスクがゴールに依存できるようにし、ゴールが閉じるまで claim されないようにする | 9 | 4 | 4 | 0 | 0 | 1 | 0 | 1 | 1 |
| 21 | 監査と障害調査のために、ローカルの計測を tracing の 1 系統にし、run の記録に分類コー… | 17 | 9 | 7 | 0 | 0 | 1 | 0 | 1 | 1 |
| 29 | 計画を立てる planner と、計画を検査して ready にする plan review job… | 45 | 28 | 13 | 0 | 0 | 4 | 0 | 4 | 4 |
| 30 | supervisor が、止まった worker の session を検知して促し、解消しなければ… | 18 | 12 | 4 | 0 | 0 | 2 | 0 | 2 | 2 |
| 31 | observer と supervisor の知見を finding として記録し、proposal… | 23 | 11 | 8 | 0 | 0 | 4 | 0 | 4 | 4 |
| 32 | 固定バイナリを、待たずに・随時・自動で入れ替えられるようにし、リリースと開発のバイナリを versi… | 32 | 24 | 7 | 0 | 0 | 1 | 0 | 1 | 0 |
| 33 | 重複や実装済みの task を、全文を読まずに見つけられるようにする（search・related・… | 13 | 8 | 4 | 0 | 0 | 1 | 0 | 1 | 1 |
| 34 | inbox を人の判断が要るものだけにし、それ以外のイレギュラーは runtime と復旧 job … | 94 | 59 | 25 | 0 | 0 | 10 | 0 | 10 | 10 |
| 36 | 並列数を上げて得をできるようにする: run の build を安全に共有し、着地待ちを内訳で見て長… | 53 | 26 | 23 | 0 | 0 | 4 | 0 | 4 | 3 |
| 37 | Keep integration verification from failing on host… | 16 | 11 | 2 | 0 | 0 | 3 | 0 | 3 | 3 |
| 38 | worker と job を隔離環境で動かし、queue へのアクセスを queue service… | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| 39 | Keep runs that wait for a person or a recover from… | 10 | 5 | 1 | 0 | 0 | 4 | 0 | 4 | 2 |
| 40 | dagq の流れと worker の時間を KPI として時系列で測り、人が HTML と push… | 55 | 29 | 19 | 0 | 0 | 7 | 0 | 7 | 7 |
| 45 | よく衝突するファイルでの rebase 衝突を減らし、着地待ちを短くする | 10 | 8 | 1 | 0 | 0 | 1 | 0 | 1 | 1 |
| 46 | repository の Rust の版を Rust の stable の release ごと（6… | 5 | 1 | 1 | 0 | 0 | 3 | 0 | 3 | 0 |
| 48 | actor の名前を 1 つに揃え、inbox を唯一の入口にし、用件ごとの desk を新設して、… | 12 | 1 | 5 | 1 | 0 | 5 | 0 | 6 | 2 |
| 51 | C: task の重さの予測を記録し、model / effort の選択を試せるようにする | 12 | 7 | 3 | 0 | 0 | 2 | 0 | 2 | 2 |
| 52 | dagq を他の repository で使えるようにする（dogfooding だから回っている前… | 67 | 35 | 27 | 0 | 0 | 5 | 0 | 5 | 5 |
| 53 | project の構成に合わせた stats・KPI を測り、その project の開発の効率を上… | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| 54 | 終わった planner・run・job の資産を片付け、queue の dir が際限なく増えない… | 16 | 9 | 6 | 0 | 0 | 1 | 0 | 1 | 0 |
| 55 | actor を明示し、default deny の capability 認可を applicati… | 38 | 23 | 11 | 0 | 0 | 4 | 0 | 4 | 4 |
| 56 | session の終わりを idle の印だけに頼らず検知し、印が書けなくても planner・wo… | 15 | 7 | 5 | 0 | 0 | 3 | 0 | 3 | 3 |
| 57 | worker を非対話（headless）の経路で動かせるようにし、Claude Code と Co… | 64 | 32 | 28 | 0 | 0 | 4 | 0 | 4 | 4 |
| 59 | worker を broker 優先にし、required mode では host の直接の操作に… | 5 | 0 | 0 | 0 | 0 | 5 | 0 | 5 | 0 |
| 61 | inbox の watch が /clear・compaction・再起動の後も張り直されることを仕… | 9 | 6 | 2 | 0 | 0 | 1 | 0 | 1 | 1 |
| 62 | 着地の速度: receipt の後に居座る run を止め、worker の stress の重い部… | 9 | 4 | 3 | 1 | 0 | 1 | 0 | 2 | 2 |
| 64 | 差し戻し・worker の問い・follow_up・cancel の理由を、起きたときに分類コードで… | 22 | 17 | 4 | 0 | 0 | 1 | 0 | 1 | 0 |
| 65 | 着地の処理を worker の slot から分け、着地中は worker の run の CPU … | 7 | 3 | 3 | 1 | 0 | 0 | 0 | 1 | 0 |
| 66 | e2e の置き場所を分ける: 差分が狭い範囲に触れる run だけ worker で必須にし、全部の… | 13 | 5 | 5 | 0 | 0 | 3 | 0 | 3 | 3 |
| 68 | test の実時間の待ちを減らし、着地の検証の test 段と worker の手元の test を… | 50 | 27 | 17 | 0 | 0 | 6 | 0 | 6 | 6 |
| 70 | integrate が着地する差分の area から検証コマンドを選び、task の宣言（--ver… | 3 | 0 | 0 | 3 | 0 | 0 | 0 | 3 | 0 |
| 71 | 計測を作り直す: run と task の一生を重ならない区間の列（工程の境目は superviso… | 17 | 1 | 3 | 0 | 0 | 13 | 0 | 13 | 0 |
| 72 | スループットの制約（着地の直列処理と CPU）を常時見る数値と、毎週の見直しの手順を持つ | 20 | 8 | 9 | 1 | 0 | 2 | 0 | 3 | 3 |
| 73 | worker 以外の headless の job を provider に透過な形で Codex … | 24 | 15 | 6 | 0 | 0 | 3 | 0 | 3 | 3 |
| 74 | supervisor の一時的な失敗と起動し直しで着地と自動更新が止まらないようにする（2026-0… | 29 | 15 | 7 | 1 | 0 | 6 | 0 | 7 | 7 |
| 75 | 自動更新と install の e2e の関門で、落ちた e2e を 1 回流し直し、印（quara… | 9 | 5 | 3 | 0 | 0 | 1 | 0 | 1 | 0 |
| 76 | 毎時のスループットの見直しを毎時 agent で分析し、規則に当たらない時間もレポートを残す | 2 | 1 | 0 | 0 | 0 | 1 | 0 | 1 | 0 |
| 77 | inbox を Codex CLI の対話の session でも動かせるようにし、inbox がど… | 2 | 0 | 0 | 0 | 0 | 2 | 0 | 2 | 0 |
| 79 | sandbox の中のプロセスが sccache の server を起動してほかの build を… | 4 | 1 | 1 | 0 | 0 | 2 | 0 | 2 | 1 |
| 80 | plan review・スループットの見直し・observer・復旧の headless の job… | 17 | 8 | 5 | 0 | 0 | 4 | 0 | 4 | 0 |
| 82 | goal 38 の段 (1)〜(3): 制御側と実行側の分け方・queue service・brok… | 25 | 11 | 7 | 0 | 0 | 7 | 0 | 7 | 7 |
| 83 | この repository を Linux で build と test を通し、CI に Linu… | 6 | 3 | 2 | 0 | 0 | 1 | 0 | 1 | 0 |
| 86 | 非対話を Claude の worker の既定にした後の評価と暫定の対応、ゼロベースの形へ移る時期… | 12 | 8 | 3 | 1 | 0 | 0 | 0 | 1 | 0 |
| 87 | planner を runtime が立てるものだけにして対話と非対話を切り替えられるようにし、非対… | 21 | 11 | 8 | 1 | 0 | 1 | 0 | 2 | 1 |
| 88 | Codex の非対話の runtime の planner を作り、この repository の … | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| 89 | 非対話の worker と planner の session wrapper を cmux の w… | 19 | 7 | 10 | 1 | 0 | 1 | 0 | 2 | 1 |
| 90 | worker が review に渡す前に受け入れ条件の各項目を根拠と照合し、初回の review … | 6 | 4 | 1 | 1 | 0 | 0 | 0 | 1 | 1 |
| 92 | cmux を inbox だけが使う形の runtime の部分を進め、tests/it と e2e… | 18 | 5 | 6 | 0 | 0 | 7 | 0 | 7 | 1 |
| 93 | broker の e2e の image の build を速くして、host の e2e の工程の… | 6 | 1 | 1 | 1 | 0 | 3 | 0 | 4 | 2 |
| 95 | actor ごとのトークン消費を Execution 単位で記録して kpi で読む | 7 | 2 | 1 | 0 | 0 | 4 | 0 | 4 | 0 |
| 96 | Spike（実験・試作・測定で計画の前提を確かめる task）を通常の task として計画・実行・… | 9 | 1 | 0 | 0 | 0 | 8 | 0 | 8 | 0 |
| 97 | follow-up を元の goal の受け入れ条件の達成に必要かで所属させ、必須の作業と発見済みの… | 11 | 7 | 0 | 0 | 0 | 3 | 1 | 4 | 1 |
| 98 | review・revise 中の run を表示と開発フローの判定で取りこぼさない | 6 | 3 | 2 | 0 | 0 | 1 | 0 | 1 | 1 |
| 99 | 長期化した非対話 task を、成果と受け入れ条件を保って分割・スコープ再配分できる正式な再計画フロ… | 8 | 0 | 0 | 0 | 0 | 8 | 0 | 8 | 0 |
| 100 | runtime の責務をレイヤーとコンテキスト（計画管理・実行と着地・観測と分析・host 運用）の… | 13 | 4 | 0 | 0 | 0 | 9 | 0 | 9 | 1 |
| 101 | test が取り残すシェルのループと、runtime の lsof による cwd 走査で host… | 3 | 2 | 0 | 0 | 0 | 1 | 0 | 1 | 1 |
| 102 | supervisor の終わった run の worktree の片付けが CPU を使い続けないよ… | 5 | 2 | 1 | 0 | 0 | 2 | 0 | 2 | 1 |
| 104 | 着地の順番を待つだけの run を slot の外に出し、空いた枠で docs・config の軽い… | 4 | 1 | 1 | 0 | 0 | 2 | 0 | 2 | 0 |
| 105 | 固定バイナリの入れ替えを待つ task を、入れ替えの前に claim せず、復旧 job が入れ替… | 4 | 0 | 0 | 0 | 0 | 4 | 0 | 4 | 0 |
| 106 | goal に優先度とラベルを持たせて task の優先度を goal から継ぎ、goal list … | 7 | 1 | 1 | 0 | 0 | 5 | 0 | 5 | 0 |
| 107 | status の 1 回の呼び出しで同じ run の events を重ねて読まないようにし、頻繁に… | 1 | 0 | 0 | 0 | 0 | 1 | 0 | 1 | 1 |
| 108 | goal review が follow-up の所属の判断待ちで起動せず open に留まる go… | 1 | 0 | 0 | 0 | 0 | 1 | 0 | 1 | 1 |
| 109 | runtime が適用する人の答え（approve_goal・correct_goal）の stat… | 1 | 0 | 0 | 0 | 0 | 1 | 0 | 1 | 1 |
| 110 | worker の中身（session の step）と実行する側の環境（container・k8s）… | 7 | 0 | 1 | 0 | 0 | 6 | 0 | 6 | 0 |
| 111 | 対話の worker の廃止を CLI・worker の prompt・plugin・AGENTS.… | 2 | 0 | 0 | 0 | 0 | 2 | 0 | 2 | 1 |
| 112 | worker が変えた挙動を説明する文書の候補を、変えた名前で探して照合し、初回の run の re… | 3 | 1 | 0 | 0 | 0 | 2 | 0 | 2 | 1 |
| 113 | worker の run ごとに、読んだ指示（runtime の prompt の雛形・plugin… | 2 | 0 | 0 | 0 | 0 | 2 | 0 | 2 | 0 |
| 114 | 改善案の効果を A/B で測る仕組みを作る: 実験の宣言・task 単位の決定的な群の割り当て・群ご… | 9 | 0 | 0 | 0 | 0 | 9 | 0 | 9 | 0 |
| 115 | レイヤー境界の違反を解消し、scripts/check-layer-deps.sh の許可の一覧を減… | 8 | 1 | 0 | 0 | 0 | 7 | 0 | 7 | 7 |
| 116 | runtime の planner が人の planner_question の答えを待つあいだ r… | 1 | 0 | 0 | 0 | 0 | 1 | 0 | 1 | 1 |
| 117 | この repository の文書と skill の中の相対 link の切れ（file や見出しの… | 1 | 0 | 0 | 0 | 0 | 1 | 0 | 1 | 1 |
| 118 | tests/it の足した・変えた test による時間の増え直しを機械的に止め、dagq::it … | 3 | 1 | 0 | 0 | 0 | 1 | 1 | 2 | 0 |
| 119 | goal 100 の状態の分割に触れない tests/it の判断を unit test に移し、す… | 6 | 2 | 0 | 0 | 0 | 3 | 1 | 4 | 0 |
| 120 | goal 100 の Supervisor の状態の分割の後に、supervise の中の判断を行き… | 7 | 0 | 0 | 0 | 0 | 7 | 0 | 7 | 1 |
| 121 | domain を crate に切り出すかを、試作の枝での測定（ビルド時間・依存の制御・変更容易さ・… | 2 | 0 | 0 | 0 | 0 | 2 | 0 | 2 | 0 |
| 122 | goal review が作った goal_gap の draft を runtime の plan… | 1 | 0 | 0 | 0 | 0 | 1 | 0 | 1 | 1 |
| 123 | AGENTS.md の大きさの上限（9,216 byte）に余裕を戻し、短い参照の 1 行を足すたび… | 1 | 0 | 0 | 0 | 0 | 1 | 0 | 1 | 1 |
| 124 | plan review が、人の依頼（request）の proposal で人の言葉により付いた … | 1 | 0 | 0 | 0 | 0 | 1 | 0 | 1 | 0 |
| 125 | review agent を eval で測って改善する: 外の Spike の成果（eval の実… | 1 | 0 | 0 | 1 | 0 | 0 | 0 | 1 | 0 |
| 計（86 件） |  | 1287 | 636 | 383 | 16 | 0 | 249 | 3 | 268 | 131 |

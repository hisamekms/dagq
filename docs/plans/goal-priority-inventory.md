---
id: plan-goal-priority-inventory
type: plan
title: 既存の open・draft の goal のラベルと優先度の初期値の案と、high 以上の goal に残る task の後回しの判定の案
status: active
created: 2026-10-05
owners:
  - hisamekms
tags:
  - measurement
  - goal
  - priority
  - planner
related:
  - adr-t1639-1
  - adr-t1639-2
  - adr-t1504-1
  - adr-t1504-2
  - plan-follow-up-membership-inventory
---

# 既存の open・draft の goal のラベルと優先度の初期値の案と、high 以上の goal に残る task の後回しの判定の案

goal 106（task 1642）の棚卸し。[ADR-t1639-1](../adr/2026-10-04-t1639-1-goal-priority-is-the-source-tasks-inherit-and-goals-carry-tags.md)（goal の優先度とラベル）と [ADR-t1639-2](../adr/2026-10-04-t1639-2-defer-improvements-outside-acceptance-to-a-low-goal-per-tag.md)（後回しの判定と受け皿の goal）に合わせて、読んだ時点の open と draft の goal 全部のラベルと優先度の初期値と、high 以上の案の goal に残る未完了の task の後回しの判定を書いた**案**である。

- **この文書は案で、何も適用していない。** goal の優先度・ラベル、task の優先度・所属、所属の判断の記録（`judge-follow-up`）、受け皿の goal の追加は行っていない（goal 106 の constraints。worker には本番の queue を変える権限も無い）。適用は権限のある runtime の planner か人が、goal 106 の機能（task 1640・1641）が固定バイナリに入った後に行う（「5. 適用の手順」）。
- **goal の優先度の値の適用と、今の task の優先度の並び（2026-10-02 の段と 2026-10-03 の方針）との調整は、人がこの文書とは別に決める。** 下の優先度の案は判断の材料で、人の決定ではない。案の値と今の task の値がずれる点は表の「ずれ」の列に書いた。
- 案は読んだ時点の queue に対するもので、適用する前に `goal list` と `show ID --full` で状態・所属・`membership_judgements` が変わっていないかを確かめる。

## 読んだ時刻とコマンド

- 時刻（UTC、2026-10-04）: `dagq goal list` を 20:08:08Z に読み、open と draft の 93 goal の `dagq goal show ID --full` を 20:08:32Z〜20:08:37Z、その未完了（draft・submitted・ready・in_progress）の task 256 件の `dagq show ID --full` を 20:08:44Z〜20:08:59Z に読んだ。goal 87 の task のうち未完了でない 1539（follow_up でない）と follow-up の 1565 と、2026-10-03 の方針の先行の不具合 6 件（793・1247・1269・1500・1518・1345）の `dagq show` は続けて 20:10Z までに読んだ。同じ時点の 1 回の読み取りで、周回は無い。表の各行の「読んだ時刻」はこの範囲を書く。
- コマンド（固定バイナリ `~/.local/bin/dagq` 0.4.0-dev+0df576f2、読み取りだけ）:
  - `dagq goal list`（`closed` が false のものを対象にし、`goal.status` で open と draft を分ける。`status` が open でも `closed` が true の goal は閉じている）
  - `dagq goal show ID --full`（`goal` の title・description・acceptance・constraints、`tasks`、`follow_up_memberships`）
  - `dagq show ID --full`（`task` の priority・status・goal_id・context、`origin`、`membership_judgements`、`events`）
  - 起点の分類: goal 97 の棚卸し [follow-up-membership-inventory.md](follow-up-membership-inventory.md)（task 1512、2026-10-04T15:22Z〜15:26Z の時点）
- 2026-10-02 の段と 2026-10-03 の方針は goal 106 の description の「goal の優先度の初期値の案」と、goal と task の description・context に書かれた人の決定から読んだ。

### 件数の照合

20:08:08Z の `dagq goal list` は 136 goal で、`closed` が false のものが 93。内訳は `status` が open のもの 75 件と draft のもの 18 件で、下の「2. goal ごとの表」は 93 行（open 75・draft 18）で一致する。閉じた 43 goal（open で `closed` が true の 37 件と draft で abandoned の 6 件）は対象にしない。各 goal の task の状態ごとの件数は `goal show` と `goal list` で全て一致し、`show` の各 task の状態も `goal show` と一致した。

2026-10-03 の方針の先行の不具合 6 件（793・1247・1269・1500・1518・1345）は全て completed で、goal の案には効かない。

## 1. ラベルの語彙の案

後続の config の task（1643）が `dagq.toml` と開発文書に写す。名前は小文字の 1 語。goal には 0 個以上を付け、先頭を主なラベルにする（ADR-t1639-2 決定 3 の「主なラベル」で受け皿を選ぶとき、planner は先頭から読む）。

| ラベル | 意味 | 付ける基準 |
|---|---|---|
| `headless` | worker・planner・job を非対話（turn ごとの呼び出し）と background の wrapper で動かすこと | 非対話の経路・wrapper・その評価と測定を作るか変える goal。非対話の run の stall や待ちの改善が主題なら `reliability` を主にして `headless` を添える |
| `codex` | Codex の provider（worker・job・planner・inbox）を使えるようにし、Claude と比べること | provider を Codex に広げるか切り替える goal。2026-10-02 の段のレビュー系の Codex 化かどうかは優先度で表し、ラベルでは分けない |
| `cmux` | cmux への依存（backend の呼び出し・対話の経路・workspace・it と e2e の cmux の fake）を減らすか安定させること | cmux を呼ぶ箇所を消すか、その呼び出しの失敗を扱う goal と、その案内の整理 |
| `throughput` | 着地の速度・着地の検証と test の時間・slot と claim・CPU の取り合い | 着地までの時間か着地の件数を直接動かす goal。数えるだけのものは `observability` |
| `enterprise` | 他の repository・隔離環境・認可・Linux とコンテナ・配布とリリースで dagq を使えるようにすること | dogfooding の前提を取り除く goal（2026-10-02 の段のエンタープライズ） |
| `planning` | planner・plan review・goal・follow-up の所属・依頼（request）・Spike・再計画の流れ | 計画を立てる・検査する・goal を開閉する仕組みを変える goal |
| `observability` | 計測・stats・kpi・日次と毎時の見直し・finding・トークンの記録 | 数えて読めるようにすることが主題の goal |
| `architecture` | レイヤーとコンテキストの責務の分割・境界の検査・refactor | 振る舞いを変えずに構造を変える goal |
| `reliability` | supervisor・自動更新・復旧 job・inbox の届け・host の後片付けが止まらず残さないこと | 運用の停止・取りこぼし・資源の漏れを直す goal |
| `review` | run の review・plan review・goal review の判定とその差し戻しの減らし方 | review 系の actor の判定や入力を変える goal（provider の切り替えは `codex`） |
| `docs` | 文書と skill の整合（link・AGENTS.md の大きさ・リリースの文） | 文書だけを直す goal |

- 検討して足した理由: `codex`・`throughput`・`enterprise`・`headless`・`cmux` だけでは 93 goal のうち 52 goal（計画・計測・構造・運用の停止・review・文書）にラベルが付かず、ADR-t1639-2 決定 2 の受け皿（ラベルごと）を選べない。そこで今の goal の主題の塊から `planning`・`observability`・`architecture`・`reliability`・`review`・`docs` を足し、全ての goal に 1 つ以上付くようにした。
- 足さなかったもの: `test`（test の時間は `throughput`、test の隔離と後片付けは `reliability` で足りる）、`security`（認可と隔離は `enterprise` の中にある）、`plugin`（変更の範囲で、テーマではない。`[areas]` が持つ）。
- ラベルは優先度を表さない（優先度は goal の優先度が持つ。ADR-t1639-1 決定 1）。

## 2. goal ごとの表

優先度の案は次の順で当てた。(a) goal の description に人の決定の優先度があればそれ（goal 113・114・119・120、goal 89 の urgent）。(b) 2026-10-02 の段: worker の非対話化とレビュー系の Codex 化 = interrupt、効果の高いスループット = high、エンタープライズ = normal、その他 = low（例は goal 106 の description）。(c) 2026-10-03 の方針: 先に進める goal 87・89・92 と責務の分割（goal 100）= high、後続の整理 = normal、大きな拡張・条件待ちと goal 95・96・97・99 の大きな機能 = low。(d) task 1512 の範囲外の follow-up を集めた goal 126〜135 は受け皿と同じく low（ADR-t1639-2 決定 1）。案と goal 106 の description の例が違うのは goal 80 だけで、理由を根拠の列に書いた。

「今の task の優先度」は読んだ時点の未完了（draft・submitted・ready・in_progress）の task の priority の件数。「ずれ」は ADR-t1639-1 決定 5 の移行（normal の task は goal から継ぐ状態に、normal 以外は同じ値の個別の指定に読み替える）の後に案の優先度を goal に付けたとき、今の値から変わるものと残るもの。draft の goal の task は ADR-t1639-1 決定 4 で効く優先度の計算から除かれるので、goal が draft の間は「変わる」と書いた値も claim の順には効かない。

案の件数: interrupt 3（57・73・86）、urgent 1（89）、high 13（36・37・62・65・68・71・72・87・92・100・104・119・120）、normal 13（3・38・52・53・55・59・82・83・106・110・111・113・115）、low 63。

| goal | title（短く） | status | ラベル案 | 優先度案 | 根拠 | 今の task の優先度 | ずれ | 読んだ時刻 |
|---|---|---|---|---|---|---|---|---|
| 3 | Rust runtime を domain / application / infr… | open | architecture | normal | 2026-10-03 の方針の normal（後続の整理）。goal 100 の責務の分割の後のレイヤーの整理 | normal 2 | なし | 20:08:08〜20:08:59Z |
| 8 | maintainer の機械的な作業を runtime に寄せる: needs_se… | open | reliability | low | その他。未完了 0 件で、goal review で閉じられるかを見る候補 | 未完了 0 件 | なし（未完了 0 件） | 20:08:08〜20:08:59Z |
| 11 | 実行効率: 検証を integrate の 1 回にし、review を super… | open | throughput | low | スループットだが未完了 0 件で効く task が無い。閉じられるかを見る候補 | 未完了 0 件 | なし（未完了 0 件） | 20:08:08〜20:08:59Z |
| 12 | maintainer を退役させ、run 単位の job（review / tria… | open | reliability | low | その他。未完了 0 件 | 未完了 0 件 | なし（未完了 0 件） | 20:08:08〜20:08:59Z |
| 13 | 計画を queue の goal で表す: goal の入れ子・依存・rank・ke… | open | planning | low | その他。入れ子・rank・roadmap は作らない（人の決定、ADR-t1639-1 決定 8）。残る draft 117 の扱いは planner が決める | low 1 | なし | 20:08:08〜20:08:59Z |
| 14 | roadmap の共有・同期を検討する（書き出し、外部 tracker、共有ストア） | open | planning | low | その他。task 0 件。roadmap は作らない（人の決定） | 未完了 0 件 | なし（未完了 0 件） | 20:08:08〜20:08:59Z |
| 17 | Keep cmux backend calls reliable under hig… | open | cmux, reliability | low | その他（条件待ち）。残る 1781 は goal 92 の後の測定 | normal 1 | normal 1 件は移行で継ぐ状態になり low になる | 20:08:08〜20:08:59Z |
| 21 | 監査と障害調査のために、ローカルの計測を tracing の 1 系統にし、run … | open | observability | low | その他。未完了 0 件 | 未完了 0 件 | なし（未完了 0 件） | 20:08:08〜20:08:59Z |
| 29 | 計画を立てる planner と、計画を検査して ready にする plan re… | open | planning | low | その他 | low 1 | なし | 20:08:08〜20:08:59Z |
| 30 | supervisor が、止まった worker の session を検知して促し… | open | reliability, headless | low | その他。非対話の run の stall の改善で、worker の非対話化そのものではない | low 5 | なし | 20:08:08〜20:08:59Z |
| 31 | observer と supervisor の知見を finding として記録し、… | open | observability | low | その他 | low 1 | なし | 20:08:08〜20:08:59Z |
| 32 | 固定バイナリを、待たずに・随時・自動で入れ替えられるようにし、リリースと開発のバイナ… | open | reliability, enterprise | low | その他（自動更新）。688・747・1156 は goal 52 から来た follow-up（688・747 は task 1512 で人の確認待ち） | normal 4・low 1 | normal 4 件は移行で継ぐ状態になり low になる | 20:08:08〜20:08:59Z |
| 34 | inbox を人の判断が要るものだけにし、それ以外のイレギュラーは runtime … | open | reliability | low | その他 | normal 2・low 1 | normal 2 件は移行で継ぐ状態になり low になる | 20:08:08〜20:08:59Z |
| 36 | 並列数を上げて得をできるようにする: run の build を安全に共有し、着地待… | open | throughput | high | 2026-10-02 の段の効果の高いスループット（goal 106 の例） | normal 3・low 1 | normal 3 件は移行で継ぐ状態になり high になる。low 1 件は個別の指定のまま low（goal より低い） | 20:08:08〜20:08:59Z |
| 37 | Keep integration verification from failing… | open | throughput, reliability | high | 2026-10-02 の段の high に不安定な test と検証の時間切れが入っており、負荷で落ちる関門はその型（goal 106 の例には無い。この文書の判断） | normal 5・low 1 | normal 5 件は移行で継ぐ状態になり high になる。low 1 件は個別の指定のまま low（goal より低い） | 20:08:08〜20:08:59Z |
| 38 | worker と job を隔離環境で動かし、queue へのアクセスを queue… | draft | enterprise | normal | エンタープライズ（goal 106 の例）。task 0 件の draft | 未完了 0 件 | なし（未完了 0 件） | 20:08:08〜20:08:59Z |
| 39 | Keep runs that wait for a person or a reco… | open | throughput | low | その他。着地の衝突の改善だが効果の大きさの測定が無い | normal 2 | normal 2 件は移行で継ぐ状態になり low になる | 20:08:08〜20:08:59Z |
| 40 | dagq の流れと worker の時間を KPI として時系列で測り、人が HTM… | open | observability | low | その他 | low 5 | なし | 20:08:08〜20:08:59Z |
| 46 | repository の Rust の版を Rust の stable の rele… | open | reliability | low | その他（toolchain の定期の引き上げ） | low 3 | なし | 20:08:08〜20:08:59Z |
| 48 | actor の名前を 1 つに揃え、inbox を唯一の入口にし、用件ごとの des… | open | planning | low | その他 | low 6 | なし | 20:08:08〜20:08:59Z |
| 51 | C: task の重さの予測を記録し、model / effort の選択を試せるよ… | open | observability | low | その他（model / effort の選択の材料） | normal 1・low 1 | normal 1 件は移行で継ぐ状態になり low になる | 20:08:08〜20:08:59Z |
| 52 | dagq を他の repository で使えるようにする（dogfooding だ… | open | enterprise | normal | エンタープライズ（goal 106 の例） | normal 1 | なし | 20:08:08〜20:08:59Z |
| 53 | project の構成に合わせた stats・KPI を測り、その project … | draft | enterprise, observability | normal | エンタープライズ（他の project の stats）。task 0 件の draft | 未完了 0 件 | なし（未完了 0 件） | 20:08:08〜20:08:59Z |
| 54 | 終わった planner・run・job の資産を片付け、queue の dir が… | open | reliability | low | その他 | low 1 | なし | 20:08:08〜20:08:59Z |
| 55 | actor を明示し、default deny の capability 認可を a… | open | enterprise | normal | エンタープライズ（goal 106 の例） | normal 2 | なし | 20:08:08〜20:08:59Z |
| 56 | session の終わりを idle の印だけに頼らず検知し、印が書けなくても pl… | open | reliability, headless | low | その他 | low 2 | なし | 20:08:08〜20:08:59Z |
| 57 | worker を非対話（headless）の経路で動かせるようにし、Claude C… | open | headless, codex | interrupt | 2026-10-02 の段の worker の非対話化（Codex の worker を含む） | low 3 | low 3 件は個別の指定のまま low（goal より低い） | 20:08:08〜20:08:59Z |
| 59 | worker を broker 優先にし、required mode では host… | open | enterprise | normal | エンタープライズ（goal 106 の例） | normal 4 | in_progress の 838・840 には効かない | 20:08:08〜20:08:59Z |
| 61 | inbox の watch が /clear・compaction・再起動の後も張り… | open | reliability | low | その他（inbox の watch） | normal 1 | normal 1 件は移行で継ぐ状態になり low になる | 20:08:08〜20:08:59Z |
| 62 | 着地の速度: receipt の後に居座る run を止め、worker の str… | open | throughput | high | 効果の高いスループット（goal 106 の例） | normal 1 | normal 1 件は移行で継ぐ状態になり high になる | 20:08:08〜20:08:59Z |
| 64 | 差し戻し・worker の問い・follow_up・cancel の理由を、起きたと… | open | observability | low | その他 | low 1 | なし | 20:08:08〜20:08:59Z |
| 65 | 着地の処理を worker の slot から分け、着地中は worker の ru… | open | throughput | high | 効果の高いスループット（goal 106 の例、2026-10-02 の段でも high） | high 1 | なし | 20:08:08〜20:08:59Z |
| 66 | e2e の置き場所を分ける: 差分が狭い範囲に触れる run だけ worker で… | open | throughput | low | スループットだが残りは測定と文書の修正で、効果の高い修正が残らない | normal 1・low 1 | normal 1 件は移行で継ぐ状態になり low になる | 20:08:08〜20:08:59Z |
| 68 | test の実時間の待ちを減らし、着地の検証の test 段と worker の手元… | open | throughput | high | 効果の高いスループット（goal 106 の例） | normal 1 | normal 1 件は移行で継ぐ状態になり high になる | 20:08:08〜20:08:59Z |
| 70 | integrate が着地する差分の area から検証コマンドを選び、task の… | draft | throughput, planning | low | 2026-10-02 の段で goal 70 は low（人の決定） | low 3 | なし | 20:08:08〜20:08:59Z |
| 71 | 計測を作り直す: run と task の一生を重ならない区間の列（工程の境目は s… | open | observability, throughput | high | 計測は段では「その他」に当たるが、着地待ちの誤分類を直すスループットの判断の土台で、今の task 13 件が high（request 13 の計画）。人の確認点 | high 13・low 1 | low 1 件は個別の指定のまま low（goal より低い） | 20:08:08〜20:08:59Z |
| 72 | スループットの制約（着地の直列処理と CPU）を常時見る数値と、毎週の見直しの手順を… | open | throughput | high | 効果の高いスループット（goal 106 の例） | normal 1 | normal 1 件は移行で継ぐ状態になり high になる | 20:08:08〜20:08:59Z |
| 73 | worker 以外の headless の job を provider に透過な形… | open | codex | interrupt | 2026-10-02 の段のレビュー系の Codex 化（goal review、goal 106 の例） | normal 1・low 1 | normal 1 件は移行で継ぐ状態になり interrupt になる。low 1 件は個別の指定のまま low（goal より低い） | 20:08:08〜20:08:59Z |
| 74 | supervisor の一時的な失敗と起動し直しで着地と自動更新が止まらないようにす… | open | reliability | low | その他。2026-10-02 の段で high にした supervisor の停止の修正は済み、残りは low の 2 件 | low 2 | なし | 20:08:08〜20:08:59Z |
| 75 | 自動更新と install の e2e の関門で、落ちた e2e を 1 回流し直し… | open | reliability | low | その他 | normal 1・low 2 | normal 1 件は移行で継ぐ状態になり low になる | 20:08:08〜20:08:59Z |
| 76 | 毎時のスループットの見直しを毎時 agent で分析し、規則に当たらない時間もレポー… | open | observability | low | その他 | low 2 | なし | 20:08:08〜20:08:59Z |
| 77 | inbox を Codex CLI の対話の session でも動かせるようにし、… | open | codex | low | Codex 化だがレビュー系ではない（inbox）のでその他 | low 2 | なし | 20:08:08〜20:08:59Z |
| 79 | sandbox の中のプロセスが sccache の server を起動してほかの… | open | reliability | low | その他。2026-10-02 の段で high にした sccache の server の修正は済み、残る 1216 は low | low 1 | なし | 20:08:08〜20:08:59Z |
| 80 | plan review・スループットの見直し・observer・復旧の headle… | open | codex | low | goal 106 の例では interrupt だが、レビュー系（plan review・スループットの見直し）は済み、残る 1223〜1226（observer・復旧の Codex 化）は 2026-10-02 に人が「レビュー系ではない」として low にした。人の確認点 | low 4 | なし | 20:08:08〜20:08:59Z |
| 82 | goal 38 の段 (1)〜(3): 制御側と実行側の分け方・queue serv… | open | enterprise | normal | エンタープライズ（goal 106 の例） | normal 1 | なし | 20:08:08〜20:08:59Z |
| 83 | この repository を Linux で build と test を通し、C… | open | enterprise | normal | エンタープライズ（Linux とコンテナ） | normal 1 | なし | 20:08:08〜20:08:59Z |
| 86 | 非対話を Claude の worker の既定にした後の評価と暫定の対応、ゼロベー… | open | headless | interrupt | 2026-10-02 の段の worker の非対話化（goal 106 の例） | interrupt 1・low 1 | low 1 件は個別の指定のまま low（goal より低い） | 20:08:08〜20:08:59Z |
| 87 | planner を runtime が立てるものだけにして対話と非対話を切り替えられ… | open | headless, planning | high | goal 106 の記述（goal 87 = high）と 2026-10-03 の方針（先に進めるのは goal 87・89・92） | high 2 | なし | 20:08:08〜20:08:59Z |
| 88 | Codex の非対話の runtime の planner を作り、この repos… | draft | codex, headless | low | Codex の planner はレビュー系ではなく、goal 87 の評価を待つ（2026-10-03 の方針の low = 条件待ち）。task 0 件の draft | 未完了 0 件 | なし（未完了 0 件） | 20:08:08〜20:08:59Z |
| 89 | 非対話の worker と planner の session wrapper を … | open | headless, cmux | urgent | 2026-10-03 の人の順（94 → 89 → 87）を分けるために task を urgent にした今の値を保つ（2026-10-03 の方針の interrupt と同じく人の例外） | urgent 2 | なし | 20:08:08〜20:08:59Z |
| 90 | worker が review に渡す前に受け入れ条件の各項目を根拠と照合し、初回の… | open | review, throughput | low | その他。残りは時期を待つ測定の draft 1537 だけ | normal 1 | normal 1 件は移行で継ぐ状態になり low になる | 20:08:08〜20:08:59Z |
| 92 | cmux を inbox だけが使う形の runtime の部分を進め、tests/… | open | cmux, throughput | high | 2026-10-03 の方針（先に進めるのは goal 87・89・92、削除の経路は high） | high 7・normal 1 | normal 1 件は移行で継ぐ状態になり high になる。in_progress の 1439 には効かない | 20:08:08〜20:08:59Z |
| 93 | broker の e2e の image の build を速くして、host の … | open | throughput | low | スループットだが残りは測定の draft 1452 だけ（キャッシュの task 1451 は済み） | normal 1 | normal 1 件は移行で継ぐ状態になり low になる | 20:08:08〜20:08:59Z |
| 95 | actor ごとのトークン消費を Execution 単位で記録して kpi で読む | open | observability | low | 2026-10-03 の方針（goal 95・96・97・99 の大きな機能は構造の分割の後） | low 4 | なし | 20:08:08〜20:08:59Z |
| 96 | Spike（実験・試作・測定で計画の前提を確かめる task）を通常の task と… | open | planning | low | 2026-10-03 の方針（goal 95・96・97・99 の大きな機能は構造の分割の後） | low 8 | なし | 20:08:08〜20:08:59Z |
| 97 | follow-up を元の goal の受け入れ条件の達成に必要かで所属させ、必須の… | open | planning | low | 2026-10-03 の方針（goal 95・96・97・99 の大きな機能は構造の分割の後） | low 2 | なし | 20:08:08〜20:08:59Z |
| 98 | review・revise 中の run を表示と開発フローの判定で取りこぼさない | open | review | low | その他 | normal 1 | normal 1 件は移行で継ぐ状態になり low になる | 20:08:08〜20:08:59Z |
| 99 | 長期化した非対話 task を、成果と受け入れ条件を保って分割・スコープ再配分できる… | open | planning, headless | low | 2026-10-03 の方針（goal 95・96・97・99 の大きな機能は構造の分割の後） | low 8 | なし | 20:08:08〜20:08:59Z |
| 100 | runtime の責務をレイヤーとコンテキスト（計画管理・実行と着地・観測と分析・h… | open | architecture | high | 2026-10-03 の方針（責務の 2 軸の分割を goal 92 の後に進める）と goal の記述の人の承認 | high 9 | なし | 20:08:08〜20:08:59Z |
| 101 | test が取り残すシェルのループと、runtime の lsof による cwd … | open | reliability | low | その他（test が host に残す負荷）。効果の測定が無い | normal 1 | normal 1 件は移行で継ぐ状態になり low になる | 20:08:08〜20:08:59Z |
| 102 | supervisor の終わった run の worktree の片付けが CPU … | open | reliability | low | その他（supervisor の CPU） | normal 1 | normal 1 件は移行で継ぐ状態になり low になる | 20:08:08〜20:08:59Z |
| 104 | 着地の順番を待つだけの run を slot の外に出し、空いた枠で docs・co… | open | throughput | high | 効果の高いスループット（goal 106 の例） | normal 1 | normal 1 件は移行で継ぐ状態になり high になる | 20:08:08〜20:08:59Z |
| 105 | 固定バイナリの入れ替えを待つ task を、入れ替えの前に claim せず、復旧 … | open | reliability | low | その他。in_progress の 1632 には効かない | normal 3 | normal 2 件は移行で継ぐ状態になり low になる。in_progress の 1632 には効かない | 20:08:08〜20:08:59Z |
| 106 | goal に優先度とラベルを持たせて task の優先度を goal から継ぎ、go… | open | planning | normal | 段では「その他」だが、この文書の適用の前提を作る今の goal（request 11）で、今の task の値 normal を保つ。人の確認点 | normal 5・low 1 | low 1 件は個別の指定のまま low（goal より低い）。in_progress の 1640・1642 には効かない | 20:08:08〜20:08:59Z |
| 107 | status の 1 回の呼び出しで同じ run の events を重ねて読まない… | open | reliability | low | その他（読み取りの問い合わせの削減） | low 1 | なし | 20:08:08〜20:08:59Z |
| 108 | goal review が follow-up の所属の判断待ちで起動せず open… | open | planning, review | low | その他 | normal 1 | normal 1 件は移行で継ぐ状態になり low になる | 20:08:08〜20:08:59Z |
| 109 | runtime が適用する人の答え（approve_goal・correct_goa… | open | planning | low | その他 | normal 1・low 1 | normal 1 件は移行で継ぐ状態になり low になる | 20:08:08〜20:08:59Z |
| 110 | worker の中身（session の step）と実行する側の環境（contai… | open | enterprise, observability | normal | エンタープライズ（container・k8s で差し替えられる記録） | normal 6 | なし | 20:08:08〜20:08:59Z |
| 111 | 対話の worker の廃止を CLI・worker の prompt・plugin… | open | cmux, docs | normal | 2026-10-03 の方針の normal（goal 92 から分けた後続の整理） | normal 2・low 1 | low 1 件は個別の指定のまま low（goal より低い） | 20:08:08〜20:08:59Z |
| 112 | worker が変えた挙動を説明する文書の候補を、変えた名前で探して照合し、初回の … | open | review, docs | low | その他 | normal 2 | normal 2 件は移行で継ぐ状態になり low になる | 20:08:08〜20:08:59Z |
| 113 | worker の run ごとに、読んだ指示（runtime の prompt の雛… | draft | observability | normal | goal の記述の人の決定（normal、スループットを直接上げないので high にしない） | normal 2 | なし | 20:08:08〜20:08:59Z |
| 114 | 改善案の効果を A/B で測る仕組みを作る: 実験の宣言・task 単位の決定的な群… | draft | observability | low | goal の記述の人の決定（low） | low 9 | なし | 20:08:08〜20:08:59Z |
| 115 | レイヤー境界の違反を解消し、scripts/check-layer-deps.sh … | open | architecture | normal | 2026-10-03 の方針の normal（後続の整理） | normal 7 | なし | 20:08:08〜20:08:59Z |
| 116 | runtime の planner が人の planner_question の答え… | open | planning | low | その他 | normal 1 | normal 1 件は移行で継ぐ状態になり low になる | 20:08:08〜20:08:59Z |
| 117 | この repository の文書と skill の中の相対 link の切れ（fi… | open | docs | low | その他 | normal 1 | normal 1 件は移行で継ぐ状態になり low になる | 20:08:08〜20:08:59Z |
| 119 | goal 100 の状態の分割に触れない tests/it の判断を unit te… | open | throughput, architecture | high | goal の記述の人の指示（high、interrupt にしない） | high 2・normal 1 | normal 1 件は移行で継ぐ状態になり high になる | 20:08:08〜20:08:59Z |
| 120 | goal 100 の Supervisor の状態の分割の後に、supervise … | open | throughput, architecture | high | goal の記述の人の指示（high、interrupt にしない） | high 6・normal 1 | normal 1 件は移行で継ぐ状態になり high になる | 20:08:08〜20:08:59Z |
| 121 | domain を crate に切り出すかを、試作の枝での測定（ビルド時間・依存の制… | open | architecture | low | 2026-10-03 の方針の low（大きな拡張・条件待ち） | low 2 | なし | 20:08:08〜20:08:59Z |
| 122 | goal review が作った goal_gap の draft を runtim… | open | planning | low | その他 | low 1 | なし | 20:08:08〜20:08:59Z |
| 123 | AGENTS.md の大きさの上限（9,216 byte）に余裕を戻し、短い参照の … | open | docs | low | その他 | low 1 | なし | 20:08:08〜20:08:59Z |
| 124 | plan review が、人の依頼（request）の proposal で人の言… | draft | review, planning | low | その他 | normal 1 | normal 1 件は移行で継ぐ状態になり low になる | 20:08:08〜20:08:59Z |
| 125 | review agent を eval で測って改善する: 外の Spike の成果… | draft | review | low | その他（外の Spike の成果の取り込み） | normal 1 | normal 1 件は移行で継ぐ状態になり low になる | 20:08:08〜20:08:59Z |
| 126 | 計画の依存と検査の端の扱いを正す: abandoned の goal を待つ tas… | draft | planning | low | task 1512 の範囲外の follow-up を集めた goal で、受け皿と同じく low（ADR-t1639-2 決定 1） | low 5 | なし | 20:08:08〜20:08:59Z |
| 127 | 止まりと不足の知らせ（stalled の ask・attention・stats の… | draft | reliability | low | task 1512 の範囲外の follow-up を集めた goal で、受け皿と同じく low（ADR-t1639-2 決定 1） | low 5 | なし | 20:08:08〜20:08:59Z |
| 128 | test の fixture と補助の process の隔離・後片付けを固め、着地… | draft | reliability | low | task 1512 の範囲外の follow-up を集めた goal で、受け皿と同じく low（ADR-t1639-2 決定 1） | normal 2・low 5 | normal 2 件は移行で継ぐ状態になり low になる | 20:08:08〜20:08:59Z |
| 129 | supervisor の周回を止める待ちを減らす: queue DB の write… | draft | reliability | low | task 1512 の範囲外の follow-up を集めた goal で、受け皿と同じく low（ADR-t1639-2 決定 1） | normal 3・low 1 | normal 3 件は移行で継ぐ状態になり low になる | 20:08:08〜20:08:59Z |
| 130 | 着地の前と自動更新の前に host で流す e2e の工程と、podman mach… | draft | reliability | low | task 1512 の範囲外の follow-up を集めた goal で、受け皿と同じく low（ADR-t1639-2 決定 1） | normal 2・low 3 | normal 2 件は移行で継ぐ状態になり low になる | 20:08:08〜20:08:59Z |
| 131 | KPI・日次レポート・完了見込み（forecast）・依存図の読み方の後回しの改善と… | draft | observability | low | task 1512 の範囲外の follow-up を集めた goal で、受け皿と同じく low（ADR-t1639-2 決定 1） | low 5 | なし | 20:08:08〜20:08:59Z |
| 132 | actor・認可・queue service の後続: クライアントモードの表示と断… | draft | enterprise | low | task 1512 の範囲外の follow-up を集めた goal で、受け皿と同じく low（ADR-t1639-2 決定 1） | normal 5 | normal 5 件は移行で継ぐ状態になり low になる | 20:08:08〜20:08:59Z |
| 133 | domain と compose の残りの小さな整理: kind の比較を even… | draft | architecture | low | task 1512 の範囲外の follow-up を集めた goal で、受け皿と同じく low（ADR-t1639-2 決定 1） | normal 1・low 2 | normal 1 件は移行で継ぐ状態になり low になる | 20:08:08〜20:08:59Z |
| 134 | 同じ repository で複数の supervisor が動くときに run の… | draft | reliability | low | task 1512 の範囲外の follow-up を集めた goal で、受け皿と同じく low（ADR-t1639-2 決定 1） | low 1 | なし | 20:08:08〜20:08:59Z |
| 135 | dagq v0.4.0 のリリースの準備: README の marketplace… | draft | enterprise, docs | low | task 1512 の範囲外の follow-up を集めた goal で、受け皿と同じく low（ADR-t1639-2 決定 1） | normal 1 | normal 1 件は移行で継ぐ状態になり low になる | 20:08:08〜20:08:59Z |
| 136 | task-registration.md の dagq.toml の verify … | open | docs | low | その他 | normal 1 | normal 1 件は移行で継ぐ状態になり low になる | 20:08:08〜20:08:59Z |

### 人の確認点（優先度の案）

- **goal 80**: goal 106 の例では interrupt。残る 1223〜1226 は acceptance (1)(5) に要る（observer・復旧の Codex 化）が、2026-10-02 に人が low にしたもので、goal を interrupt にしても個別の指定の low は変わらない。案は low。
- **goal 71**: 計測は段では「その他」だが、今の task 13 件が high。案は high で、人の意図（request 13 で high にしたか）を確かめる。
- **goal 106**: 段では「その他」だが、適用の前提を作る今の goal なので案は normal。
- **goal 37**: goal 106 の例に無いが、2026-10-02 の段の high の「不安定な test・検証の時間切れ」に当たると見て high にした。
- normal の task が多い low の案の goal（32・34・39・105・136 など）は、適用すると継ぐ task が low に下がる。上げたい task は個別の指定で残す（ADR-t1639-1 決定 2）。

## 3. 後回しの判定の表

対象は 2 で high 以上の案の 17 goal（57・73・86・89・36・37・62・65・68・71・72・87・92・100・104・119・120）に残る未完了の task 67 件全部と、goal 87 の follow-up のうち request 15 で既に別の goal へ移された・閉じたもの（1563・1564・1565・1585。1576・1577 は上の 67 件に入る）と、goal 87 に残っていた follow_up でない 1539。

- 問いは ADR-t1639-2 決定 1 のとおり「この task をしなくても、今いる goal の受け入れ条件を満たしたと言えるか」。影響の大小や今の priority では決めない。いる goal に対して判定するのは、高い goal の優先度を継いで後回しの改善が走ること（request 11 の問題）を見るためである。
- follow_up 由来のものは task 1512 の分類（出どころの goal の acceptance に対する判定と置き先）を起点にし、「1512 との関係」の列に一致・不一致と理由を書いた。出どころの goal に対する 1512 の分類（required / out_of_scope / undecided）と、その後の planner の判断の行を覆したものは無い（1512 が undecided とした 1205・773 は、その後 planner が required を記録しており、それに従う。1577 と 1650 は、1512 が出どころの goal 87・3 に対して範囲外とし、この表は今いる goal 92・100 に対して判定したので列が違う）。1512 の後に登録された follow_up（1793・1794・1795・1736）は worker の `membership_proposal` を起点にした。違うのは置き先で、1512 か planner の判断の行が「合う既存の goal」として high 以上の案の goal を置き先にした 14 件（643・1385・1476・1703・1342・1359・1382・1351・1515・847・1514・1628・1729 と 1736）を、その goal の受け入れ条件に要らないので受け皿へ移す案にした。
- 受け皿のラベルは register.md の 4 と ADR-t1639-2 決定 2 のとおり出どころの goal の主なラベル（2 の案の先頭）で選んだ。出どころの goal が閉じていてラベル案が無いもの（94・45・118）は、その goal の主題から選んだ理由を行き先の列に書いた。follow_up でない 1480 は今の goal のラベルで選んだ。
- `goal_gap`（goal review が受け入れ条件の欠けとして登録したもの）は生まれつき必須で、移さない（dagq skill の register.md）。
- 行き先の「受け皿 X」は「4. 受け皿の goal の案」のラベル X の goal。

件数: 必須 46、範囲外 16、判断できない 5（67 件）。

| task | title | 元の goal（優先度案） | 状態・今の priority | 判定 | 該当する受け入れ条件の項目と理由 | 行き先 | 1512 との関係 | 読んだ時刻 |
|---|---|---|---|---|---|---|---|---|
| 643 | runtime: worker と planner の claude の argv に pr… | 57（interrupt） | ready・low | 範囲外 | (1)〜(6) のどれも worker・planner の argv に prompt 全文を載せるかを求めない。(1) の非対話の経路は今の argv で満たされている | 受け皿 reliability（出どころ goal 34 の主なラベル） | 一致（1512 は source goal 34 に対して範囲外で、置き先を goal 57 にした）。置き先だけ違う: goal 57 を interrupt にすると継ぐ優先度で走るので、ADR-t1639-2 決定 1・2 で出どころのラベルの受け皿へ | 20:08:44〜20:08:59Z |
| 1205 | measure: task 1174 の着地後の本番の Codex の非対話の run の記… | 57（interrupt） | ready・low | 必須 | (6) 実 Codex の非対話の worker の手動スモークの結果（確認点の通過）。planner が 1512 の後に required を記録済み（判断の行 37、2026-10-04T15:48Z、needs_recheck false）で、記録し直さない | 元の goal に残す | 1512 は undecided。その後の planner の判断の行（required）と一致 | 20:08:44〜20:08:59Z |
| 1385 | runtime: task_created に provider と worker_mode… | 57（interrupt） | ready・low | 範囲外 | (5) は run の provider と経路を stats・kpi・show で見せることで、task_created に provider と worker_mode を残すことは求めない | 受け皿 headless（出どころ goal 86） | 一致（1512 は source goal 86 に対して範囲外、置き先 goal 57）。置き先だけ違う（理由は 643 と同じ） | 20:08:44〜20:08:59Z |
| 1476 | runtime: Codex の review job で必須の subagent が動くこ… | 73（interrupt） | ready・low | 範囲外 | (1)〜(6) は goal review の Codex 化と provider の記録で、Codex の review job の必須の subagent の確かめは求めない | 受け皿 codex（出どころ goal 94 は閉じていてラベル案が無い。主題の Codex の review から codex） | 一致（1512 は source goal 94 に対して範囲外、置き先 goal 73）。置き先だけ違う（goal 73 を interrupt にすると継ぐため） | 20:08:44〜20:08:59Z |
| 1703 | manual-smoke.md の Codex の run review のスモーク手順 3… | 73（interrupt） | ready・normal | 範囲外 | (1)〜(6) は manual-smoke.md の Codex の run review のスモークの起動引数の記述を求めない | 受け皿 codex（出どころ goal 94、同上） | 一致（1512 は source goal 94 に対して範囲外、置き先 goal 73）。置き先だけ違う | 20:08:44〜20:08:59Z |
| 1342 | measure: task 1340 の着地の後、非対話の Claude の worker … | 86（interrupt） | ready・low | 範囲外 | (1) は切り替えの基準値・評価のコマンド・戻す基準の『案が文書にある』ことで、非対話の Claude での最初の answer の turn と resume の turn の確認は求めない | 受け皿 headless（出どころ goal 57 の主なラベル） | 分類は一致（1512 は source goal 57 に対して範囲外）。1512 は置き先を goal 86（(1) の評価に合う）にしたが、合うことと要ることは別で、goal 86 を interrupt にすると継ぐので受け皿へ | 20:08:44〜20:08:59Z |
| 1374 | measure: 既定を非対話にしてから 1 週間後に、印 61916 の前後を docs/… | 86（interrupt） | draft・interrupt | 判断できない | goal の題と記述は『評価』だが、(1) は評価の基準と戻す基準の『案』だけを求め、1 週間後の評価そのものは acceptance に無い。acceptance を直すか範囲外とするかは人か planner が決める | 判断まで元の goal に残す | follow_up でない（起点なし） | 20:08:44〜20:08:59Z |
| 1409 | measure: 非対話の wrapper を background に切り替えた印から 1… | 89（urgent） | draft・urgent | 必須 | (4)「startup の基準値と切り替え後の比較が文書にあり」に要る 1 週間後の判定 | 元の goal に残す | follow_up でない（起点なし） | 20:08:44〜20:08:59Z |
| 1657 | runtime: background の wrapper の停止に使った signal（S… | 89（urgent） | ready・urgent | 必須 | (2) 止める経路で process が残らないことと (4) の比較の材料（wrapper_stopped の記録）。1409 の前提 | 元の goal に残す | 一致（1512 も required） | 20:08:44〜20:08:59Z |
| 1403 | measure: runtime の planner を非対話に切り替えた印から 1 週間後… | 87（high） | draft・high | 必須 | (4)「1 週間後の評価が文書にある」 | 元の goal に残す | follow_up でない（起点なし） | 20:08:44〜20:08:59Z |
| 1576 | plugin の skill と hook のコメントを dagq plan の廃止に合わせ… | 87（high） | ready・high | 必須 | (5)「plugin の skill と AGENTS.md が新しい流れに合っている」。planner の予備の判定と同じ | 元の goal に残す | 一致（1512 も required。名指しの箇所は task 1400 で直っている見込みで、着手時に残りを確かめる） | 20:08:44〜20:08:59Z |
| 1359 | measure: build script の rerun-if-changed を wor… | 36（high） | ready・normal | 範囲外 | (1) の build の共有の前後の比較は済み、build script の rerun-if-changed を worktree に依らない形にした短縮の測定は (1)〜(4) のどれにも要らない | 受け皿 throughput（出どころ goal 65） | 分類は一致（1512 は source goal 65 に対して範囲外、置き先 goal 68。その後 goal 36 に移された）。置き先だけ違う | 20:08:44〜20:08:59Z |
| 1382 | runtime: 待ちと着地待ちの run の worktree の target の大きさ… | 36（high） | ready・low | 範囲外 | (4) の材料は load・build 時間・着地待ちで、待つ run の target の大きさ（disk）は求めない | 受け皿 headless（出どころ goal 86） | 分類は一致（1512 は source goal 86 に対して範囲外、置き先 goal 36）。置き先だけ違う（goal 36 を high にすると継ぐため） | 20:08:44〜20:08:59Z |
| 1480 | docs: worker が負荷の下で落ちる test の再現のために host に負荷を足… | 36（high） | ready・normal | 範囲外 | worker が負荷の下で落ちる test の再現で host に負荷を足さないことの文書で、(1)〜(4) のどれにも要らない | 受け皿 throughput（今の goal 36。follow_up でないので今の goal のラベル） | follow_up でない（起点なし） | 20:08:44〜20:08:59Z |
| 1625 | measure: claim の間隔（task 1479、ADR-t1479-1）の着地の前… | 36（high） | ready・normal | 必須 | (4) 並列数を上げるかの材料（load）を claim の間隔の変更（task 1479）の後の値で揃える | 元の goal に残す | 一致（1512 も required） | 20:08:44〜20:08:59Z |
| 773 | landing recheck: don't send a run held for a h… | 37（high） | ready・low | 必須 | 「integration verification が環境の原因で落ちても resume しない」に landing recheck を含める読み方で、planner が 1512 の後に required を記録済み（判断の行 38、2026-10-04T15:48Z、needs_recheck false）。記録し直さない | 元の goal に残す | 1512 は undecided。その後の planner の判断の行（required）と一致 | 20:08:44〜20:08:59Z |
| 1351 | test: queue_service_reads の 2 本（the_read_roles… | 37（high） | ready・normal | 範囲外 | acceptance は環境の失敗で resume しないことと tests の固定の待ちが無いことで、時刻に依る出力の取り直しが負荷の下で落ちる queue_service_reads の test はどちらにも当たらない | 受け皿 throughput（出どころ goal 65） | 分類は一致（1512 は source goal 65 に対して範囲外）。1512 は置き先を goal 37（合う）にしたが、goal 37 の acceptance には要らないので受け皿へ | 20:08:44〜20:08:59Z |
| 1515 | runtime_waiting::a_returning_run_counts_toward… | 37（high） | ready・normal | 範囲外 | claim 直後の写しの run_dir の unwrap の競合で、固定の wall-clock の待ちではなく acceptance のどちらの項目にも当たらない | 受け皿 throughput（出どころ goal 39） | 分類は一致（1512 は source goal 39 に対して範囲外）。1512 の置き先 goal 37 と違う（理由は 1351 と同じ） | 20:08:44〜20:08:59Z |
| 1793 | runtime_headless::a_turn_that_ends_by_itself_l… | 37（high） | draft・normal | 判断できない | 負荷の下で process group の外の sleep が生きていない原因が未調査で、固定の待ちによるもの（acceptance の後半に当たる）かが分からない | 判断まで元の goal に残す（原因が分かった時点で判定） | 1512 の後に登録（task 1629 の follow_up）。worker の membership_proposal（undecided）と一致。判断の行は無い | 20:08:44〜20:08:59Z |
| 1794 | SystemProcesses.list が ps の出力に UTF-8 でない byte … | 37（high） | draft・normal | 範囲外 | ps の出力の UTF-8 でない byte で SystemProcesses.list が失敗する不具合で、負荷の下の時間の問題ではなく acceptance に当たらない | 受け皿 throughput（出どころ goal 37 の主なラベル） | 1512 の後に登録（task 1629 の follow_up）。worker の membership_proposal（out_of_scope）と一致。判断の行は無い | 20:08:44〜20:08:59Z |
| 1795 | runtime_headless の a_receipt_written_before_it… | 37（high） | draft・normal | 判断できない | common/service.rs の約 302 秒の期限（固定の待ち）なら acceptance の後半に当たるが、測定の装置の産物の可能性があり、負荷だけで再現するかが分からない | 判断まで元の goal に残す | 1512 の後に登録（task 1629 の follow_up）。worker の membership_proposal（undecided）と一致。判断の行は無い | 20:08:44〜20:08:59Z |
| 1784 | test: 非対話 worker で receipt 後に残る待ちループの自動停止と着地を再… | 62（high） | ready・normal | 必須 | 「438 の型の再現 test がある」。438 型の test は対話の経路の撤去で消え、goal review が欠けとして登録した（origin goal_gap） | 元の goal に残す | follow_up でない（goal_gap。ADR-t1504-1 で判定の対象外） | 20:08:44〜20:08:59Z |
| 962 | docs: 着地の処理を worker の slot から分け、着地中は worker の … | 65（high） | draft・high | 判断できない | acceptance の後半は「進めると決めた場合」の ADR で、進めるかは spike（961）の後に人が決める（2026-10-02 の決定）。進めるなら必須、進めないなら cancel | 判断まで元の goal に残す | follow_up でない（起点なし） | 20:08:44〜20:08:59Z |
| 1785 | measure: task 1629 の着地の後、1583 で変えた headless の境… | 68（high） | ready・normal | 必須 | goal の constraints（足した・変えた test は stress を通す）の欠けを goal review 50 が登録した（origin goal_gap）。goal review の閉じる判定に要る | 元の goal に残す | follow_up でない（goal_gap） | 20:08:44〜20:08:59Z |
| 847 | runtime: REVISE の session span の attempt を rev… | 71（high） | ready・low | 範囲外 | (2) の attempt は run_phase_changed が記録し、(5) のタグの内訳もそれを使う。REVISE の session span の attempt を revise_requested に揃えることは (1)〜(5) のどれにも要らない | 受け皿 throughput（出どころ goal 11） | 分類は一致（1512 は source goal 11 に対して範囲外、置き先 goal 71）。置き先だけ違う（goal 71 を high にすると継ぐため） | 20:08:44〜20:08:59Z |
| 1663 | runtime: supervisor が工程を移る処理を 1 か所の関数に集め、移るたびに… | 71（high） | ready・high | 必須 | (2) 工程を移るたびの run_phase_changed と Phase::tags() | 元の goal に残す | follow_up でない（起点なし） | 20:08:44〜20:08:59Z |
| 1664 | runtime: claim 前の待ちを supervisor 単位の区間（slots_fu… | 71（high） | ready・high | 必須 | (2) claim 前の待ちの区間の event と run_claimed の項目・blocked_by | 元の goal に残す | follow_up でない（起点なし） | 20:08:44〜20:08:59Z |
| 1665 | refactor: RunLog・EventReads・TaskStore を論理ストアの … | 71（high） | ready・high | 必須 | (1) 計測のストアの抽象化（SSOT とビュー）を port に表す | 元の goal に残す | follow_up でない（起点なし） | 20:08:44〜20:08:59Z |
| 1666 | runtime: run と task の区間を組み立てる 1 つの畳む関数（domain）… | 71（high） | ready・high | 必須 | (3) 台帳（ledger_v）と 1 つの畳む関数 | 元の goal に残す | follow_up でない（起点なし） | 20:08:44〜20:08:59Z |
| 1667 | runtime: supervisor の周回に、終わった run と task の台帳を冪… | 71（high） | ready・high | 必須 | (3) supervisor の周回が台帳を冪等に作る、(4) 並走の差 | 元の goal に残す | follow_up でない（起点なし） | 20:08:44〜20:08:59Z |
| 1668 | measure: 台帳と今の stats を 7 日並走させた結果で、unattribute… | 71（high） | ready・high | 必須 | (4) 1 週間の並走で unattributed が 2% 未満、差の説明 | 元の goal に残す | follow_up でない（起点なし） | 20:08:44〜20:08:59Z |
| 1669 | runtime: stats・kpi・report を台帳だけから作り、所要時間と slot… | 71（high） | ready・high | 必須 | (3)(5) stats・kpi・report を台帳だけから作り、2 本の内訳を出す | 元の goal に残す | follow_up でない（起点なし） | 20:08:44〜20:08:59Z |
| 1670 | runtime: timeline・status・forecast の工程と空白の理由を台帳… | 71（high） | ready・high | 必須 | (3) timeline・status・forecast を台帳と畳む関数から作る | 元の goal に残す | follow_up でない（起点なし） | 20:08:44〜20:08:59Z |
| 1671 | runtime: 1 期間残した今の work・wait_to_land・land_phas… | 71（high） | ready・high | 必須 | (5) 今の work・wait_to_land・land_phases・startup を 1 期間の後に廃止する | 元の goal に残す | follow_up でない（起点なし） | 20:08:44〜20:08:59Z |
| 1678 | measure: 段 2 の関門: 台帳の記録・畳む関数・係を最後に変えた build の後… | 71（high） | ready・high | 必須 | (4) 段 2 の関門（unattributed < 2%・不明の差 0 件） | 元の goal に残す | follow_up でない（起点なし） | 20:08:44〜20:08:59Z |
| 1682 | runtime: run を持たない session・job の行と queue 全体の行を… | 71（high） | ready・high | 必須 | (3) stats・kpi・report の欄を台帳の行から作る（run を持たない session・job・queue 全体の行） | 元の goal に残す | follow_up でない（起点なし） | 20:08:44〜20:08:59Z |
| 1685 | runtime: host の負荷の連続の記録を queue.db の NodeSample… | 71（high） | ready・high | 必須 | (3) stats・kpi・report の host の値も台帳から作るための node の記録 | 元の goal に残す | follow_up でない（起点なし） | 20:08:44〜20:08:59Z |
| 1686 | runtime: NodeSampleStore の node の時系列を台帳の node … | 71（high） | ready・high | 必須 | (3) stats の host・kpi の cpu_per_landing などを台帳の node の行から作る | 元の goal に残す | follow_up でない（起点なし） | 20:08:44〜20:08:59Z |
| 1514 | 本番の stats で landing recheck の件数と負荷を前後で確かめる | 72（high） | ready・normal | 範囲外 | acceptance は landing_utilization と cpu_per_landing と毎週の見直しの手順で、landing recheck の件数と負荷の前後の確認は求めない | 受け皿 throughput（出どころ goal 39） | 分類は一致（1512 は source goal 39 に対して範囲外、置き先 goal 72）。置き先だけ違う（goal 72 を high にすると継ぐため） | 20:08:44〜20:08:59Z |
| 1439 | test: runtime の integration test の非対話の worker … | 92（high） | in_progress・high | 必須 | (3) runtime の integration test が run の workspace の fake を使わない | 元の goal に残す（in_progress で動かせない） | follow_up でない（起点なし） | 20:08:44〜20:08:59Z |
| 1440 | runtime: 非対話の session wrapper の workspace の経路を… | 92（high） | ready・high | 必須 | (1)(4) 非対話の wrapper を background だけにし、run の workspace の掃除を消す | 元の goal に残す | follow_up でない（起点なし） | 20:08:44〜20:08:59Z |
| 1441 | runtime: runtime の planner の対話の経路と、対話の planner… | 92（high） | ready・high | 必須 | (2) runtime の planner の対話の経路が無い | 元の goal に残す | follow_up でない（起点なし） | 20:08:44〜20:08:59Z |
| 1442 | runtime: supervisor・queue service・observer が c… | 92（high） | ready・high | 必須 | (4) supervisor・queue service・observer が cmux を呼ばない | 元の goal に残す | follow_up でない（起点なし） | 20:08:44〜20:08:59Z |
| 1443 | runtime: in-cmux mode を廃止し、launchd mode の cmux… | 92（high） | ready・high | 必須 | (4) in-cmux mode が無い・supervisor を cmux なしで常駐 | 元の goal に残す | follow_up でない（起点なし） | 20:08:44〜20:08:59Z |
| 1444 | measure: goal 92 の前後で、着地の関門の test 段（test の数・合計… | 92（high） | ready・high | 必須 | (6) 前後の数字が docs/plans にある | 元の goal に残す | follow_up でない（起点なし） | 20:08:44〜20:08:59Z |
| 1577 | ADR-t1433-2 決定 5: 廃止前から開いている人の planner の行を閉じた扱… | 92（high） | ready・high | 必須 | (4)「廃止前から開いている人の planner の行も cmux を呼ばずに閉じた扱いにする」（ADR-t1433-2 決定 5）。goal 87 の (2) の dagq plan の廃止には要らない（task 1399 が ADR-t1394-1 決定 9 で満たした） | 元の goal（goal 92）に残す | 一致（1512 は source goal 87 に対して範囲外、置き先 goal 92）。goal 92 に対しては必須 | 20:08:44〜20:08:59Z |
| 1628 | planner_screen_idle の person の planner の test … | 92（high） | ready・normal | 範囲外 | (2) は対話に固有の test を消すことで、人の planner の画面の idle の test の時計を直すことは要らない。1441・1577 の後にこの test は消えるか意味を失う見込み | 受け皿 throughput（出どころ goal 45 は閉じていてラベル案が無い。主題の着地待ちから throughput。1441 の着地の後に消えていれば cancel の候補） | 分類は一致（1512 は source goal 45 に対して範囲外、置き先 goal 92）。置き先だけ違う | 20:08:44〜20:08:59Z |
| 1552 | refactor: Supervisor の共有の状態から観測と分析・host 運用の co… | 100（high） | ready・high | 必須 | (4) Supervisor の共有の状態を context ごとに分ける | 元の goal に残す | follow_up でない（起点なし） | 20:08:44〜20:08:59Z |
| 1553 | refactor: Supervisor に残った実行と着地の状態（slots・worker… | 100（high） | ready・high | 必須 | (4) 同上（実行と着地） | 元の goal に残す | follow_up でない（起点なし） | 20:08:44〜20:08:59Z |
| 1554 | refactor: application/ports.rs を context ごとの m… | 100（high） | ready・high | 必須 | (4) ports.rs を context ごとの module に分ける | 元の goal に残す | follow_up でない（起点なし） | 20:08:44〜20:08:59Z |
| 1555 | refactor: 観測と分析・host 運用の use case（lifecycle・up… | 100（high） | ready・high | 必須 | (4) 分けた use case が要る port だけを取る | 元の goal に残す | follow_up でない（起点なし） | 20:08:44〜20:08:59Z |
| 1556 | refactor: compose.rs を context ごとの組み立ての module… | 100（high） | ready・high | 必須 | (4) compose.rs を context ごとの module に分ける | 元の goal に残す | follow_up でない（起点なし） | 20:08:44〜20:08:59Z |
| 1557 | refactor: supervise の revise・reopen・resume・ses… | 100（high） | ready・high | 必須 | (4) revise・reopen・resume の時間の判断を値で受ける関数と unit test へ | 元の goal に残す | follow_up でない（起点なし） | 20:08:44〜20:08:59Z |
| 1558 | refactor: supervise の stall・stall_recovery・ado… | 100（high） | ready・high | 必須 | (4) stall の判断を値で受ける関数と unit test へ | 元の goal に残す | follow_up でない（起点なし） | 20:08:44〜20:08:59Z |
| 1559 | measure: goal 100 の構造の分割（task 1552〜1558）の前後で、構… | 100（high） | ready・high | 必須 | (5) 構造の前後の比較が docs/plans にある | 元の goal に残す | follow_up でない（起点なし） | 20:08:44〜20:08:59Z |
| 1650 | application::queue_reads::answer を context ごとの… | 100（high） | ready・high | 判断できない | (1) は今の違反が行き先の task を持つこと（この task があれば満たす）、(4) は ports.rs・compose.rs の分割と分けた use case が要る port だけを取ることで、queue_reads::answer の分割が (4) の『分けた use case』に入るかの読み方で変わる。planner の採用の理由は (1)(4) に属すると読んでいる | 判断まで元の goal に残す（範囲外なら goal 115 へ） | 分類は 1512 と同じ向き（source goal 3 に対して範囲外、置き先 goal 100）。goal 100 に対して要るかは決めきれない | 20:08:44〜20:08:59Z |
| 1593 | config: dagq.toml の [supervisor] に light_chang… | 104（high） | ready・normal | 必須 | (3) この repository の dagq.toml が docs と config を軽い change に決めている | 元の goal に残す | follow_up でない（起点なし） | 20:08:44〜20:08:59Z |
| 1710 | refactor: plan review の verdict・失敗・編集との競合・revi… | 119（high） | ready・high | 必須 | (1)(2) 対象の module の判断を unit test に移し、前後の時間を出す | 元の goal に残す | follow_up でない（起点なし） | 20:08:44〜20:08:59Z |
| 1714 | measure: goal 119 の移し替え（1709〜1713）の着地の前後で、本番の関… | 119（high） | ready・high | 必須 | (3) 全 task の前後の比較が it-reduction.md にある | 元の goal に残す | follow_up でない（起点なし） | 20:08:44〜20:08:59Z |
| 1736 | config: .config/it-slow-allow.toml から tests/it… | 119（high） | ready・normal | 範囲外 | (1)〜(4) は判断の移し替えと時間の比較で、.config/it-slow-allow.toml の古い項目の掃除と警告は求めない | 受け皿 throughput（出どころ goal 118 は閉じていてラベル案が無い。主題の it の削減から throughput） | 1512 の後に登録（task 1707 の follow_up）。worker の membership_proposal と planner の判断の行（source goal 118 に対して範囲外、置き先 goal 119）と分類は一致。置き先だけ違う | 20:08:44〜20:08:59Z |
| 1715 | refactor: goal 100 の状態の分割の後に、後始末（cleanup）の頼みの合… | 120（high） | ready・high | 必須 | (1)(2) cleanup の判断を host 運用の context の unit test へ | 元の goal に残す | follow_up でない（起点なし） | 20:08:44〜20:08:59Z |
| 1716 | refactor: goal 100 の状態の分割の後に、着地の枠の解放・landing r… | 120（high） | ready・high | 必須 | (1)(2) 着地の枠・recheck・release の判断を実行と着地の context へ | 元の goal に残す | follow_up でない（起点なし） | 20:08:44〜20:08:59Z |
| 1717 | refactor: goal 100 の状態の分割の後に、broker の mode・tok… | 120（high） | ready・high | 必須 | (1)(2) broker・disk の判断を host 運用の context へ | 元の goal に残す | follow_up でない（起点なし） | 20:08:44〜20:08:59Z |
| 1718 | refactor: goal 100 の状態の分割の後に、run の review の質問・… | 120（high） | ready・high | 必須 | (1)(2) review の質問・evidence・着地の answer の判断を実行と着地の context へ | 元の goal に残す | follow_up でない（起点なし） | 20:08:44〜20:08:59Z |
| 1719 | refactor: goal 92 と goal 100 の状態の分割の後に、引き継ぎ（ha… | 120（high） | ready・high | 必須 | (1)(2) handoff と待ちの段の判断を行き先の context へ | 元の goal に残す | follow_up でない（起点なし） | 20:08:44〜20:08:59Z |
| 1720 | measure: goal 120 の移し替え（1715〜1719・1729）の着地の前後で… | 120（high） | ready・high | 必須 | (3) 全 task の前後の比較が it-reduction.md にある | 元の goal に残す | follow_up でない（起点なし） | 20:08:44〜20:08:59Z |
| 1729 | test: goal 100 の状態の分割の後に、runtime_heartbeat::th… | 120（high） | ready・normal | 範囲外 | acceptance は各 task の対象の module の移し替えを求めるが、goal の記述の対象の module（cleanup・landing・broker・review・handoff ほか）に runtime_heartbeat は無く、移さなくても (1)〜(4) を満たせる。1720 の description が 1729 を名指すので、移すなら 1720 の範囲から外す | 受け皿 throughput（出どころ goal 119） | 分類は一致（1512 は source goal 119 に対して範囲外、置き先 goal 120）。置き先だけ違う | 20:08:44〜20:08:59Z |

### goal 87 の follow-up のうち今は別の goal にあるもの（と 1539）

request 15（2026-10-04T08:41Z〜08:42Z の判断の行）で 1563・1564・1565・1585 は goal 87 から出され、1577 は goal 92 へ移された（1577 は上の表）。1539 は follow_up ではない goal 87 の task で、in_progress のまま goal 87 にあったが、読んだ時点では completed。

| task | title | 元の goal（優先度案） | 状態・今の priority | 判定 | 該当する受け入れ条件の項目と理由 | 行き先 | 1512 との関係 | 読んだ時刻 |
|---|---|---|---|---|---|---|---|---|
| 1539 | runtime: 計画の依頼の CLI に --text-file と stdin の入力を… | 87（high） | completed・high | — | follow_up ではない（origin が無い）goal 87 の task。task 1642 の context では in_progress だったが、読んだ時点で completed。所属は変えられず判定は要らない | —（goal 87 の completed） | 1512 の対象外（未完了でない） | 20:08:44〜20:10:00Z |
| 1563 | status の inbox 向けの欄に planner を待つ open の依頼を出す | 87 → 今は 48（low） | ready・low | 範囲外 | goal 87 の (1)〜(5) に要らない（request 15 の判断の行と同じ）。今の goal 48（inbox を唯一の入口に）は低い案の goal で、合う既存の goal なので受け皿に移さない | 今の goal 48 のまま（request 15 で移し済み） | 一致（1512 も範囲外、置き先 goal 48） | 20:08:44〜20:10:00Z |
| 1564 | --request の planner_question を依頼の planner 自身だけ… | 87 → 今は 55（normal） | ready・normal | 範囲外 | goal 87 の (1)〜(5) に要らない。今の goal 55（capability 認可）が合う既存の goal で、案は normal | 今の goal 55 のまま（request 15 で移し済み） | 一致（1512 も範囲外、置き先 goal 55） | 20:08:44〜20:10:00Z |
| 1565 | finding_planner_exhausted を ATTENTION_KINDS に入… | 87 → 今は 31（low） | canceled・low | — | canceled（task 1450 の commit で済んでいた）。判定は要らない | —（canceled） | 1512 は範囲外・goal 31。その後 cancel された | 20:08:44〜20:10:00Z |
| 1585 | 非対話の runtime の planner の起動の失敗で控えを待つ integratio… | 87 → 今は 29（low） | ready・low | 範囲外 | goal 87 の (3) は非対話の planner の経路と turn の届け方で、起動の失敗の控えの integration test は要らない。今の goal 29（planner と plan review）が合う既存の goal で案は low | 今の goal 29 のまま（request 15 で移し済み） | 一致（1512 も範囲外、置き先 goal 29） | 20:08:44〜20:10:00Z |

planner の予備の判定との照合: 1576 は (5) に必須で元に残す（同じ）。1563・1564・1565・1585 は goal 87 の (1)〜(5) のどれにも要らず範囲外（同じ）だが、request 15 で既に合う既存の goal（48・55・31・29）に移っているので、受け皿へは移さない（ADR-t1639-2 決定 2 は固有の既存の goal を先にする）。1577 は ADR-t1433-2 決定 5 の実装で、goal 87 の (2) の dagq plan の廃止には要らず（task 1399 が ADR-t1394-1 決定 9 で満たした）、goal 92 の (4) に必須。

### 判断できないものの問い

| task | 誰が決めるか | 問い |
|---|---|---|
| 1374 | 人（acceptance を変えるなら人） | goal 86 の acceptance (1) は「案」だけで、1 週間後の評価そのものを含めるか（含めるなら acceptance を直して必須、含めないなら範囲外で受け皿 headless） |
| 1793・1795 | planner（原因が分かってから） | 負荷の下の失敗が固定の待ちによるものか（よるなら goal 37 の後半に必須、よらないなら範囲外で受け皿 throughput） |
| 962 | 人（2026-10-02 の決定のとおり） | spike 961 の後に goal 65 の後半（CPU の取り分の ADR と実装）へ進めるか。進めないなら cancel して goal 65 を閉じる |
| 1650 | planner | queue_reads::answer の分割が goal 100 (4) の「分けた use case」に入るか。入らないなら範囲外で goal 115（レイヤー境界の違反）へ |

## 4. 受け皿の goal の案

ADR-t1639-2 決定 2 のとおり受け皿はラベルごとに 1 つで、優先度は low。今回 3 で移すものがあるラベルだけ作り、ほかのラベルは最初に要るときに同じ形で作る。

| ラベル | 案 | title の案 | acceptance の案 | 今回移す task |
|---|---|---|---|---|
| `headless` | 新しく作る | headless の後回しの改善（受け皿） | ラベル headless の goal の受け入れ条件に要らない改善の受け皿。各 task がそれぞれの受け入れ条件で着地するか、根拠を残して cancel されている。新しい task は ADR-t1639-2 の判定で入る | 1385・1342・1382 |
| `codex` | 新しく作る | codex の後回しの改善（受け皿） | 同じ形（ラベル codex） | 1476・1703 |
| `throughput` | 新しく作る | throughput の後回しの改善（受け皿） | 同じ形（ラベル throughput） | 1359・1480・1351・1515・1794・847・1514・1628・1736・1729 |
| `reliability` | 新しく作る | reliability の後回しの改善（受け皿） | 同じ形（ラベル reliability） | 643 |
| `cmux`・`observability`・`enterprise`・`planning`・`architecture`・`review`・`docs` | 今は作らない | — | — | なし（判断できない 1650 が範囲外になれば、置き先は固有の既存の goal 115） |

- 受け皿の acceptance は閉じることを目標にしない形にした（ADR-t1639-2 決定 2: 受け皿は閉じずに後回しの改善を受け続けうる）。goal review がこの acceptance で判定し、未完了がある限り open に残る。constraints の案: 「他の goal の受け入れ条件に要る task を入れない。ここへ移すことは採用も優先も意味しない（ADR-t1639-2 決定 4）。」
- 既存の goal 126〜135（task 1512 の範囲外の follow-up を集めた goal）は受け皿に使わない。それぞれの acceptance は特定の問題の列で、ラベルの受け皿にするには acceptance を書き換えることになり、acceptance の版が上がって 1512 の適用で記録した判断の行が `needs_recheck` になる（register.md の 7）。2 では low の案にしてラベルを付けるだけにした。

## 5. 適用の手順

前提: task 1640（goal の優先度と継承）と 1641（goal のラベルと語彙、goal list の並びと `--tag`）が固定バイナリに入り、1643 がラベルの語彙を `dagq.toml` に書いた後。綴り（`goal edit --priority` / `--tag`、`goal add` の同じ flag、個別の指定を外す `set-priority` の綴り）はその task の `docs/design/` が決めたものに合わせる。下は ADR-t1639-1 の言葉で書いた並び。適用の前に各 task の `show ID --full` の状態・goal・`membership_judgements` と、goal の `acceptance_version` が読んだ時点から変わっていないかを確かめる。

1. **人が goal の優先度の値と、今の task の優先度の並びとの調整を決める**（この文書は決めない）。2 の「人の確認点」と 3 の「判断できないものの問い」も人か planner が答える。
2. **受け皿の goal を作る**（4 の 4 件）:

   ```sh
   dagq goal add "headless の後回しの改善（受け皿）" --description "ADR-t1639-2 のラベル headless の受け皿。docs/plans/goal-priority-inventory.md の 4" --acceptance "ラベル headless の goal の受け入れ条件に要らない改善の受け皿。各 task がそれぞれの受け入れ条件で着地するか、根拠を残して cancel されている。新しい task は ADR-t1639-2 の判定で入る" --constraints "他の goal の受け入れ条件に要る task を入れない。ここへ移すことは採用も優先も意味しない（ADR-t1639-2 決定 4）" --priority low --tag headless
   # codex・throughput・reliability も同じ形（title・acceptance のラベルと --tag を替える）
   ```

3. **goal のラベルと優先度を付ける**（1 で人が決めた値で。下は案の値。93 goal）:

   ```sh
   dagq goal edit 3 --priority normal --tag architecture
   dagq goal edit 8 --priority low --tag reliability
   dagq goal edit 11 --priority low --tag throughput
   dagq goal edit 12 --priority low --tag reliability
   dagq goal edit 13 --priority low --tag planning
   dagq goal edit 14 --priority low --tag planning
   dagq goal edit 17 --priority low --tag cmux --tag reliability
   dagq goal edit 21 --priority low --tag observability
   dagq goal edit 29 --priority low --tag planning
   dagq goal edit 30 --priority low --tag reliability --tag headless
   dagq goal edit 31 --priority low --tag observability
   dagq goal edit 32 --priority low --tag reliability --tag enterprise
   dagq goal edit 34 --priority low --tag reliability
   dagq goal edit 36 --priority high --tag throughput
   dagq goal edit 37 --priority high --tag throughput --tag reliability
   dagq goal edit 38 --priority normal --tag enterprise
   dagq goal edit 39 --priority low --tag throughput
   dagq goal edit 40 --priority low --tag observability
   dagq goal edit 46 --priority low --tag reliability
   dagq goal edit 48 --priority low --tag planning
   dagq goal edit 51 --priority low --tag observability
   dagq goal edit 52 --priority normal --tag enterprise
   dagq goal edit 53 --priority normal --tag enterprise --tag observability
   dagq goal edit 54 --priority low --tag reliability
   dagq goal edit 55 --priority normal --tag enterprise
   dagq goal edit 56 --priority low --tag reliability --tag headless
   dagq goal edit 57 --priority interrupt --tag headless --tag codex
   dagq goal edit 59 --priority normal --tag enterprise
   dagq goal edit 61 --priority low --tag reliability
   dagq goal edit 62 --priority high --tag throughput
   dagq goal edit 64 --priority low --tag observability
   dagq goal edit 65 --priority high --tag throughput
   dagq goal edit 66 --priority low --tag throughput
   dagq goal edit 68 --priority high --tag throughput
   dagq goal edit 70 --priority low --tag throughput --tag planning
   dagq goal edit 71 --priority high --tag observability --tag throughput
   dagq goal edit 72 --priority high --tag throughput
   dagq goal edit 73 --priority interrupt --tag codex
   dagq goal edit 74 --priority low --tag reliability
   dagq goal edit 75 --priority low --tag reliability
   dagq goal edit 76 --priority low --tag observability
   dagq goal edit 77 --priority low --tag codex
   dagq goal edit 79 --priority low --tag reliability
   dagq goal edit 80 --priority low --tag codex
   dagq goal edit 82 --priority normal --tag enterprise
   dagq goal edit 83 --priority normal --tag enterprise
   dagq goal edit 86 --priority interrupt --tag headless
   dagq goal edit 87 --priority high --tag headless --tag planning
   dagq goal edit 88 --priority low --tag codex --tag headless
   dagq goal edit 89 --priority urgent --tag headless --tag cmux
   dagq goal edit 90 --priority low --tag review --tag throughput
   dagq goal edit 92 --priority high --tag cmux --tag throughput
   dagq goal edit 93 --priority low --tag throughput
   dagq goal edit 95 --priority low --tag observability
   dagq goal edit 96 --priority low --tag planning
   dagq goal edit 97 --priority low --tag planning
   dagq goal edit 98 --priority low --tag review
   dagq goal edit 99 --priority low --tag planning --tag headless
   dagq goal edit 100 --priority high --tag architecture
   dagq goal edit 101 --priority low --tag reliability
   dagq goal edit 102 --priority low --tag reliability
   dagq goal edit 104 --priority high --tag throughput
   dagq goal edit 105 --priority low --tag reliability
   dagq goal edit 106 --priority normal --tag planning
   dagq goal edit 107 --priority low --tag reliability
   dagq goal edit 108 --priority low --tag planning --tag review
   dagq goal edit 109 --priority low --tag planning
   dagq goal edit 110 --priority normal --tag enterprise --tag observability
   dagq goal edit 111 --priority normal --tag cmux --tag docs
   dagq goal edit 112 --priority low --tag review --tag docs
   dagq goal edit 113 --priority normal --tag observability
   dagq goal edit 114 --priority low --tag observability
   dagq goal edit 115 --priority normal --tag architecture
   dagq goal edit 116 --priority low --tag planning
   dagq goal edit 117 --priority low --tag docs
   dagq goal edit 119 --priority high --tag throughput --tag architecture
   dagq goal edit 120 --priority high --tag throughput --tag architecture
   dagq goal edit 121 --priority low --tag architecture
   dagq goal edit 122 --priority low --tag planning
   dagq goal edit 123 --priority low --tag docs
   dagq goal edit 124 --priority low --tag review --tag planning
   dagq goal edit 125 --priority low --tag review
   dagq goal edit 126 --priority low --tag planning
   dagq goal edit 127 --priority low --tag reliability
   dagq goal edit 128 --priority low --tag reliability
   dagq goal edit 129 --priority low --tag reliability
   dagq goal edit 130 --priority low --tag reliability
   dagq goal edit 131 --priority low --tag observability
   dagq goal edit 132 --priority low --tag enterprise
   dagq goal edit 133 --priority low --tag architecture
   dagq goal edit 134 --priority low --tag reliability
   dagq goal edit 135 --priority low --tag enterprise --tag docs
   dagq goal edit 136 --priority low --tag docs
   ```

4. **範囲外の task を受け皿へ移す**（3 の範囲外 16 件。draft・ready だけが動く。ADR-0009）:
   - follow_up 由来で既に判断の行があるもの（643・1385・1476・1703・1342・1359・1382・1351・1515・847・1514・1628・1736・1729）: 判定は 1512 か planner の判断の行と同じ out_of_scope で、置き先だけを受け皿に変える。`dagq judge-follow-up T --classification out_of_scope --acceptance-item '<出どころの goal の項目>' --reason '<1512 の理由>。今の goal の受け入れ条件にも要らず、継ぐ優先度で走るので受け皿へ（docs/plans/goal-priority-inventory.md の 3）' --evidence 'docs/plans/goal-priority-inventory.md の 3' --destination-goal <受け皿>`。register.md の 6 は required と out_of_scope の間の訂正に `--corrects` を求め、`set-goal` で判断の行と違う goal へ移すのは拒まれる。out_of_scope のまま置き先だけを変える記録を runtime が受け付けるか（`--corrects` が要るか）は適用の前に確かめる。
   - follow_up 由来で判断の行が無いもの（1794。出どころは goal 37 自身）: `dagq judge-follow-up 1794 --classification out_of_scope --acceptance-item 'goal 37 の acceptance 全体' --reason '<3 の理由>' --evidence 'docs/plans/goal-priority-inventory.md の 3' --destination-goal <受け皿 throughput>`。
   - follow_up でないもの（1480）: `dagq set-goal 1480 <受け皿 throughput>` と、判定の根拠を task の context か計画の記録に残す（ADR-t1639-2 決定 4）。
   - 移した task の個別の優先度の指定を外して受け皿の low を継がせる（ADR-t1639-2 決定 1。外す綴りは task 1640 の `docs/design/`）。残すなら理由を task に書く。
5. **必須の task は元の goal に残し、goal の優先度を継がせる**: 移行の後に個別の指定が goal の優先度と違うもの（例: goal 57 の 1205 は low、goal 37 の 773 は low）は、残すか外すかを 1 の人の調整で決める。
6. **判断できない 5 件**（1374・1793・1795・962・1650）は答えを待つ。follow_up 由来のもの（1793・1795・1650）は答えの後に required か out_of_scope を記録する（1793・1795 は判断の行が無く、1650 は今の out_of_scope の行からの訂正になる）。

### 適用しないこと

- 2026-10-02 の段と 2026-10-03 の方針に沿って今の task に付いている個別の優先度の値は、この文書では変えない（上の 4 で移す task の指定を外すことと 5 の調整は、適用する人か planner が 1 の決定に従って行う）。
- goal の acceptance は変えない（register.md の 7。acceptance を弱めて範囲外にしない）。1374 のように acceptance を直すかどうかの問いは人に上げる。
- goal の入れ子・rank・roadmap は作らない（ADR-t1639-1 決定 8）。

## 気づいたこと（適用の前に planner が確かめる）

- ADR-t1639-2 決定 2 は、範囲外の改善の置き先を「固有の適切な既存の goal → 無ければ同じラベルの受け皿」と決めるが、固有の既存の goal が高い優先度の goal（受け入れ条件には要らないが主題が合う）のときにそこへ置いてよいかは書いていない。1512 と planner の判断の行はその形で 14 件を high 以上の案の goal に置いており、goal の優先度を継ぐと後回しの改善が高い優先度で走る（request 11 の問題）。この文書は決定 1 の「要らない改善は元の goal から出す」を優先して受け皿へ移す案にした。skill の手順（task 1644）で「置き先の goal の受け入れ条件に要らないなら、置き先の優先度が受け皿より高いときは受け皿を選ぶ」を書くかは planner と人が決める。
- goal 73 と goal 72 は、3 の範囲外を移すと未完了が 0 件になり、goal review で閉じられるかを見られる。goal 57 は必須の 1205 だけが残る。
- この文書は 3 の判定を、follow_up の所属の判定（出どころの goal に対する ADR-t1504-1 の問い）とは別に、今いる goal の受け入れ条件に対して行った。高い goal の優先度を継いで後回しの改善が走ることを見るための読み方で、ADR-t1639-2 決定 1 の「元の goal」を今いる goal と読んでいる。出どころと今いる goal が違う follow_up でこの読み方を採るかは planner と人が確かめる。
- goal 8・11・12・21 は未完了 0 件（goal 14 は task 0 件）で open のまま。goal review が起動しない理由（所属の判断待ちなど）は goal 108 の範囲。

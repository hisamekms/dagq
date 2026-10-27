---
id: plan-landing-it-after-t2892-spike
type: plan
title: スパイク：着地の IT の選び方（ADR-t2892-1）は、入った後の 7 日の実測で全部流す割合と verify の時間を下げ、CI で見つかる見逃しを許容の範囲に収めるか
status: completed
created: 2026-10-27
owners:
  - hisamekms
tags:
  - testing
  - performance
  - measurement
related:
  - adr-t1925-1
  - adr-t2892-1
  - plan-landing-it-alternatives
  - plan-landing-it-selection
---

# スパイク：着地の IT の選び方（ADR-t2892-1）の入った後の 7 日の実測

問いは「[ADR-t2892-1](../adr/2026-10-20-t2892-1-landing-it-leaves-added-sources-and-broad-entries-to-ci.md) の規則は、入った後の 7 日の実測で保てるか」。
(a) 全部流す割合と verify の時間が前の窓より下がり、かつ (b) 着地の絞った IT が流さず CI で見つかった見逃しが、CI を最終関門として許容する範囲（[ADR-t1925-1](../adr/2026-10-07-t1925-1-landing-verifies-unit-tests-and-selected-integration-tests-and-ci-is-the-final-gate.md) 決定 5）に収まるかを、前後の窓の landing-it のログと main の CI の結果から測った。

**判定: holds。** 全部流した割合は 93.4% → 15.7%、landing-it のコマンドの中央値は 690 秒 → 233 秒。
見逃し（既知の不安定なテストを除く）は 2 件（確かな壊れ 1 件と、壊れか不安定か分けられない 1 件）で、後の窓の landing-it の 230 回の 0.9%。

## 環境

- **対象の commit**: 調査した main は `483282704370165f952da2a027dea7625e5cfac3`（この run の base）。`scripts/landing-it.py` は task 2892 の着地 `36a72691` の後に変わっていない（`git log 36a72691..HEAD -- scripts/landing-it.py` が空）。ログの理由の行も、前の窓は旧規則（`a source file not in the table's tree` で全部）、後の窓は新規則（`its IT left to CI` の行）だけで、窓の間に規則は揺れていない。
- **道具の版**: Python 3.9.6、gh 2.86.0、dagq 0.4.0-dev+91cc7fa82（固定バイナリ `~/.local/bin/dagq`）、git。
- **provider と経路**: Claude（claude-opus-5-5）の headless の worker（run b4317c79）。`dagq events` は worker のクライアントモード（queue service の socket）で、状態を変えないコマンドだけを打った。gh はこの run の host の認証で CI の結果とログを読んだだけ。
- **host**: macOS Darwin 23.6.0（arm64、MacBook Air）。検証のログはこの host の run の directory（`log_path`）から読んだ。
- 読んだ時刻: 2026-10-27T04:10Z〜04:20Z（UTC）。

## 打ったもの

repository の root で（DATA は `$TMPDIR` の下の dir）:

```sh
sh docs/plans/landing-it-after-t2892-spike/collect.sh "$DATA"
```

- [`landing-it-after-t2892-spike/collect.sh`](landing-it-after-t2892-spike/collect.sh): `dagq events --full --kind verification_command` を前後の窓で 12 時間ずつ、`--kind run_integrated` を 2026-10-19 から後の窓の終わりまで読み、main の push の CI の run・job ごとのテストの step の結論・failure の run の落ちたテストを [landing-it-selection/collect.sh](landing-it-selection/collect.sh) の手順 2・3・6（`SINCE=2026-10-19T00:00:00Z`）で読み、`measure.py` を流す。
- [`landing-it-after-t2892-spike/measure.py`](landing-it-after-t2892-spike/measure.py): 各回のログから全部か絞ったか・理由・段の秒・流したテストを読み、`attempts.csv`・`new-failures.csv`・`summary.json` を書く。
  前の測定の `replay.py` は窓が固定で選び方を書き直したものなので使わず、着地の時に `landing-it.py` が出した選択と流したテストをログから直接読んだ（再現の誤差が無い）。`stages.py` の段の読み方（cargo の `Finished` の和と nextest の `Summary`）は同じ正規表現で写した。
- [`landing-it-after-t2892-spike/classify.py`](landing-it-after-t2892-spike/classify.py): 絞って新たに赤の IT を流さなかった着地ごとに、その着地が使った表と差分から流さなかった理由を区分し、`new-failures.csv` に `it_reason` の列を足す。
- 新たに赤のテストのうち見逃しの 4 本は、`gh` のログの行（`ci-failed-tests.tsv` の FLKY-FL の行）と `git log`（直しの commit）で、壊れか不安定かを確かめた。`dagq ci failures` は、着地のログの `left out, fails on main already` の行がその時点の既知の失敗を持つので使わなかった。
- **使った時間と回数**: 約 1 時間 10 分（上限 3 時間）。`collect.sh` の通しの実行は 1 回で、前後の窓を 1 回で読んだ（窓ごとに 1 回、上限 3 回）。review の差し戻しで、同じ材料に `classify.py` を 1 回流した（表 4 本の取得を含む。`collect.sh` の手順 5 と同じ）。

## 測ったもの

### 窓と値の式

- **前の窓**: `2026-10-13T02:12:03Z` 以上 `2026-10-20T02:12:03Z` 未満。**後の窓**: `2026-10-20T02:12:03Z` 以上 `2026-10-27T02:12:03Z` 未満。区切りは `verification_command` の `created_at`（コマンドの終わり）。
- **landing-it の回**: `command` が `sh scripts/landing-it.sh` の `verification_command`。ログは各 event の `log_path`（`~/.local/share/dagq/77067154921b9014/runs/<run>/integrate-<n>-verify-<m>.log`）で、前後とも全部残っていた。
- **全部流した割合** = ログに `landing-it: every IT runs:` がある回 ÷ ログに選択の行（`every IT runs` か `narrowed IT`）がある回。1800 秒の時間切れ（exit 128）は前の窓に 5 回・後の窓に 1 回あり、うち前の窓の 2 回は選択の行の前に切れたので分母から外した（時間には入れた）。
- **verify の時間** = landing-it のコマンドの `duration_secs`。参考に、同じ run・同じ `attempt` の integration の `verification_command` 全部の和（fmt・clippy・文書の検査を含む）も出す。
- **build** = ログの cargo の `Finished ... in` の秒の和、**テスト** = nextest の `Summary` の秒（unit test と IT は 1 回の nextest で同時に流れるので分けない）。
- **中央値・p90**: 中央値は件数が偶数なら中の 2 つの平均、p90 は最近順位法（昇順の ceil(0.9×n) 番目）。

### 全部流した割合と時間

`landing-it-after-t2892-spike/attempts.csv` が各回を、`summary.json` が下の値を持つ。

| 値 | 前の窓 | 後の窓 |
|---|---|---|
| landing-it の回（run） | 139（119） | 230（214） |
| 通った回（exit 0） | 121 | 212 |
| 選択の行がある回 | 137 | 230 |
| 全部流した回（割合） | 128（93.4%） | 36（15.7%） |
| コマンドの時間 中央値 / p90（全部の回） | 689.6 / 1213.6 | 232.9 / 1093.0 |
| 同、通った回 | 652.5 / 1025.3 | 217.6 / 643.1 |
| build 中央値 / p90 | 171.2 / 456.0 | 103.6 / 221.2 |
| テスト（nextest）中央値 / p90 | 501.7 / 655.5 | 93.1 / 978.3 |
| integration の検証全部の和 中央値 / p90 | 805.9 / 1705.9 | 309.8 / 1157.6 |

| 値（中央値 / p90） | 前: 全部（128 回） | 前: 絞った（9 回） | 後: 全部（36 回） | 後: 絞った（194 回） |
|---|---|---|---|---|
| コマンドの時間 | 707.8 / 1211.4 | 242.2 / 700.0 | 1194.2 / 1523.2 | 195.7 / 367.6 |
| build | 185.4 / 460.2 | 157.2 / 301.2 | 108.0 / 238.0 | 103.1 / 221.2 |
| テスト（nextest） | 510.2 / 658.3 | 56.0 / 455.2 | 1098.4 / 1268.7 | 83.0 / 191.8 |

全部流した理由（1 回に複数の理由がありうるので、理由ごとの件数の和は全部流した回より多い）:

| 理由（landing-it.py の出力） | 前の窓 | 後の窓 |
|---|---|---|
| 閾値超え（`the narrowed IT's estimated wall time ... exceeds`） | 83 | 3 |
| 表の tree に無い src のファイル（`a source file not in the table's tree`、旧規則だけ） | 34 | — |
| 共通のファイル（`a common file`） | 17 | 33 |
| 表が無い・読めない・古い | 0 | 0 |
| 組み合わせ | 閾値のみ 83・tree のみ 28・共通のみ 11・共通と tree 6 | 共通のみ 33・閾値のみ 3 |

- 後の窓の絞った 194 回のうち、158 回が広い項目のファイルを、31 回が表の tree に無いファイルを `its IT left to CI` として CI に任せた。
- 後の窓の全部流した回は前の窓の全部流した回より遅い（テストの中央値 510 → 1,098 秒）。規則は全部流すかどうかしか変えないので、IT そのものが遅くなったことによる（[代替案の比較](landing-it-alternatives.md)の「全部流した run の段」が前の期間で読んだ伸びの続き）。

### CI で新たに赤になった IT と見逃し

- **新たに赤**と**範囲**は [landing-it-selection](landing-it-selection.md) の「値の式」と同じ: job（macOS・linux）ごとに、テストの step が success か、failure で落ちたテストの行がある run を順に並べ、前の結果のある run で落ちておらずその run で落ちたテスト（FLKY-FL は数えない）。範囲は前の run の `headSha`（含まない）からその run の `headSha`（含む）までの main の first-parent の commit。後の窓に作られた CI の run を数えた（338 run、結果のある run は macOS 159・linux 165）。
- **流したか**: 範囲の各着地について、その run が着地する前の最後の landing-it のログの PASS・FAIL などの行にそのテストがあれば「流した」。どの着地も流していなければ**見逃し**とした。そのテストを `left out, fails on main already` で外した着地は「既知の失敗として外した」とした。
- **流さなかった理由（テストごと）**: 絞って流さなかった着地ごとに、そのログの `landing-it: table` の行が名指す表（夜間の job の artifact を `gh run download` で取った。4 本、run 39337639088・39506124653・39684774315・39863147906）と、その着地の `run_integrated` の `main_before..commit` の差分（`landing-it.sh` が読む `DAGQ_LANDING_BASE..HEAD`）を当てた（`classify.py`、`new-failures.csv` の `it_reason` の列）。区分は、差分のファイルのうち表がそのテストに当てるものが全部広い項目（IT の 25% より多くに当たる）なら「広い項目で外した」、当てるファイルが無く、表の tree に無い src のファイルが差分にあれば「表に無い」（そのファイルがテストに届くかを表は言えない）、当てるファイルが無く tree に無いファイルも無ければ「CI に任せた（差分が表で届かない）」。表・ログ・差分のどれかが取れない着地は「未取得」とする。
  再現の確かめ: 10 件の着地とも、差分と表から求めた広い項目のファイルの集合が、そのログの `its IT left to CI`（広い項目）の行と一致し、ログの任せたファイルは全部差分の中にあった。未取得は 0 件。
  `measure.py` の `per_landing` の括弧（`broad`・`not_in_tree`・`not_reached`）はログにその種の行があるかだけで、テストごとの理由ではない。
- 一斉の赤（1 run で 50 本以上）・人の直接の commit を含む範囲・CI のログや着地のログが読めない範囲（未取得）は 0 件だった。
- 明細は `landing-it-after-t2892-spike/new-failures.csv`（job・テスト・前の run・赤の run・範囲の着地と各着地の区分・判定・次の結果でも赤か）。

| テスト | job | 赤の run（時刻） | 範囲の着地（task: 区分） | 判定 |
|---|---|---|---|---|
| `runtime_integrate::moved_queue_directory_resolves_run_paths_and_lands_awaiting_runs` | macOS・linux | 39556709746（10-21T06:38Z） | 2936: 広い項目で外した（差分の 12 ファイルが当て、どれも広い項目。`src/application/integrate.rs` を含む）、3014: CI に任せた（差分が表で届かない） | **見逃し（変更による壊れ）**。直しの commit `ce5bbbff4` が task 2936 の変更が原因と書く。main で 10-21T06:38Z〜10-22T08:51Z の 38 run で赤だった |
| `runtime_sweep::a_live_supervisor_prunes_the_registration_a_dead_one_left_behind` | macOS・linux | 39875549154（10-23T00:01Z） | 3108: 広い項目で外した（差分の 30 ファイルが当て、どれも広い項目。差分には表の tree に無いファイルもあったが、表が当てていたので区分は広い項目）、3171・1496: IT を流さない | **見逃し（壊れか不安定か分けられない）**。次の結果のある run 39881738075 で緑に戻り、間の commit（`dee8b6d82`・`62a359936`）に直しが無い |
| `runtime_headless::a_later_turn_is_the_runs_agent_and_its_helper_no_idle_process` | linux | 39720453195（10-22T04:05Z） | 2966: 広い項目で外した（`src/application/supervise/planning/plan_review.rs` 1 ファイル） | 不安定（前の run 39718840725 の linux で FLKY-FL、テストの待ちを直した `7682a6f8f` の後は赤くならない）。見逃しに数えない |
| `planner_headless::a_headless_draft_planner_…_from_the_answer`・`planner_headless::a_headless_request_planner_…_that_declines`（既知の不安定） | macOS | 11 件（10-20〜10-22） | 着地ごとに違う | 不安定な既知のテスト。規則の上の区分は、絞って流さなかった 6 件が広い項目で外した 5 件（2229・2936・3042・2520・2951。範囲の別の着地 3014・2956 は CI に任せた）と CI に任せた 1 件（2264）、既知の失敗として外した 4 件、流した 1 件。見逃しに数えない |
| `runtime_cleanup::a_process_running_…`・`run_container::the_supervisor_starts_the_sccache_server_…`・`cli_kpi::report_out_through_the_service_is_written_by_the_client`・`location::move_the_queue_directory_then_rebind`・`location::rebind_with_db_then_move_the_queue_directory` | macOS・linux | 7 件（10-24〜10-25） | 3105・2181・3421・3106: 流した | 着地の絞った IT が流して通り、CI で赤（host と CI の環境の違い）。見逃しでない |

- 見逃し 2 件の理由は、どちらも「広い項目で外した」（表に無い 0 件、差分が表で届かない 0 件）。
- 新たに赤の 23 件（行）は、見逃し 4 行（2 件）・不安定 12 行（planner_headless 11、runtime_headless 1）・流した 7 行。
- **見逃しの割合**（既知の不安定なテストを除く）= 見逃しの件数（1 つの壊れを job をまたいで 1 件）÷ 後の窓の landing-it の回 = 2 ÷ 230 = 0.9%（確かな壊れだけなら 1 ÷ 230 = 0.4%。分母を `attempts.csv` の後の窓で landing-it が通った run の 210 件にしても 1.0%）。

## 判定

**holds**。

| 条件 | 物差し | 2892 の当て直しの見込み | 実測 | 満たすか |
|---|---|---|---|---|
| (a) 全部流す割合 | 後の窓で 40% 以下 | 21.9%（前の窓の再現 92.9%） | 15.7%（36 / 230）。前の窓 93.4% | 満たす |
| (a) verify の時間 | 後の窓の中央値が前の窓の中央値より短い | IT の見込みの中央値 31 秒（表の秒、前 462 秒） | landing-it のコマンドの中央値 232.9 秒（前 689.6 秒）、検証全部の和 309.8 秒（前 805.9 秒） | 満たす |
| (b) 見逃し | 既知の不安定なテストを除き、landing-it の回の 5% 以下 | 比べられなかった（新たに赤が不安定な 2 本だけ） | 2 件 / 230 回 = 0.9% | 満たす |

- (a) 全部流す割合は見込みより低い。残る全部の 9 割は共通のファイルで、閾値超えは 83 回から 3 回に減った。見込みの 31 秒は表の秒の IT だけの値で、実測の絞った回は build の約 100 秒と unit test を含むテストの約 80 秒で中央値 196 秒になった（代替案の比較が「絞った回の時間の見込み」で約 149 秒と見込んだ形に近い）。
- (b) 見逃しは件数が 230 回に対し 2 件で、物差しの 5%（約 11 件）より十分小さい。確かな壊れの 1 件（task 2936）は、表が差分の広い項目からだけ当てていたので外した IT で、CI が最初の push で赤にし、修正の commit が約 27 時間後に着地した。CI を最終関門として事故を直す経路が働いた例である。
- 件数は判定に足りる: 前後とも landing-it のログは全部読め（未取得 0）、CI は後の窓の 338 run を読めた。

## 後続の計画への推奨

- 判定が holds なので、ADR-t2892-1 を保つ。規則（`BROAD_SHARE`・閾値・共通のファイル）を変える follow_up は挙げない。
- 見逃しの代償は「CI の後で見つかる遅れ」として残った（task 2936 の壊れは main で約 26 時間・38 run で赤）。これは規則の外の、CI の見張りから修正 task までの経路の速さの問題で、ADR-t2892-1 を直す理由にはならない。
- 全部流す回の時間が前の窓の全部流す回より伸びた（中央値 708 → 1,194 秒）のは IT 1 本あたりの遅さで、選び方では縮まない。今の verify の p90（1,093 秒）を決めているのは、共通のファイルで全部流す回のこの時間である。

## 限界

- 流さなかった理由は、表のファイルの粒度での区分。「広い項目で外した」は表がそのファイルからテストに当てていたことを言うだけで、その変更がテストの通る行に触れたかは分からない。表の artifact の保持（30 日）が切れると `classify.py` は表を取れず、その着地は未取得になる。
- 範囲が複数の着地にまたがるものは「どれかが流せば見逃しでない」と数えるので、見逃しの数は範囲の粒度での値（[landing-it-selection](landing-it-selection.md)の数え方と同じ）。後の窓の CI は cancel が多く、範囲が 2〜3 件の着地にまたがるものがある。
- 既知の不安定なテストは、窓の間に赤と緑を繰り返したこと（planner_headless）か、CI に FLKY-FL の行があり、テストの直しの後は赤くならないこと（runtime_headless）で決めた。runtime_sweep の 1 件は 1 回だけの赤で、壊れか不安定か決められないので見逃しに数えた。
- 前後の窓は着地の数も中身も違い（landing-it の回 139 対 230）、時間の比較は同じ差分での比較でない。全部流す回が遅くなった分は後の窓に不利に働くので、時間が短くなった結論の向きは変わらない。

## 区分

`attempts.csv`・`new-failures.csv`・`summary.json` は queue の event・CI の記録・検証のログ・表から `collect.sh` で作り直せるビューで、DATA に集めたものは消えてよい材料。host の run の directory と CI のログの保持（GitHub の既定 90 日）が切れると作り直せないので、CSV と JSON を正として残す。

---
id: plan-landing-it-selection
type: plan
title: 過去の着地の差分に IT の対応表を当てた、絞った IT の時間と見逃しの測定と、全部流す閾値・共通のファイル・表の古さの上限
status: active
created: 2026-10-06
owners:
  - hisamekms
tags:
  - testing
  - performance
  - measurement
related:
  - plan-it-reduction
  - plan-nextest-measurement
  - plan-coverage-at-landing
  - design-measurement
---

# 過去の着地の差分に IT の対応表を当てた、絞った IT の時間と見逃しの測定と、全部流す閾値・共通のファイル・表の古さの上限

goal 157 の段 3a（spike、task 1924）。task 1923 の CI の job が作った「repository のファイル → 統合テスト（IT）」の対応表を、2026-09-29 以降に着地した run の差分に当て、着地の検証を絞った IT にしたときの時間と見逃しを測った。結果から、IT を全部流す閾値と共通のファイルと表の古さの上限を決めた（段 3b の ADR と design が使う）。src/・tests/・scripts/ は変えていない。

## 要点

- **母集団は 431 件の着地（git に無く除いたものは 0 件）。** 期間は 2026-09-29T00:00:00Z から T_end = 2026-10-06T00:00:00Z（UTC）。表は 1 本（commit `8d208e2a`、2026-10-06T04:54:42Z、IT 1,138 本、IT 全部で直列の和 2,632.5 秒・見込みの壁時計 438.7 秒）。
- **閾値（見込みの壁時計 220 秒 = IT 全部の見込み 438.7 秒の半分 219.35 秒を丸めた値。219.35〜220 秒の着地は無い）を当てると、全部流した着地は 247 件（57.3%）**: 閾値超え 196・共通のファイル 33・その他（表の tree に無い src のファイル）18。絞れた 184 件のうち 106 件は IT に当たるファイルを変えておらず 0 本、IT を 1 本以上選んだ 78 件は見込みの壁時計の中央値 4.2 秒・p90 45.8 秒。
- **IT の時間（見込みの壁時計）は全着地の中央値・p90 とも 438.7 秒（全部流したのが過半のため IT 全部と同じ）、平均は 254 秒（全部流すより 42% 短い）。** 絞った IT の大きさは二山で、閾値の前の見込みの壁時計は 60 秒以下が 182 件、350 秒超が 206 件、その間は 10 件。どの閾値（60〜350 秒）でも結果はほぼ変わらない。
- **見逃しは 0 件。** 期間に main の CI で新たに赤くなった IT 1,655 件（範囲が複数の着地にまたがるもの 1,165 件。どれもその範囲のどれかの着地の絞った IT に入っていた）。範囲に dagq を通らない人の直接の commit を含み、それが原因で着地のどれにも選ばれなかったものが 6 件（見逃しと別に数える）。閾値を外すと、2026-09-29 の CI の環境の壊れ（fixture の template の cache）で一斉に赤くなった 682 本のうち 57 本が task 1046 の絞った IT に入らなかった。
- **決めたこと（段 3b へ）**: 全部流す閾値は「絞った IT の見込みの壁時計が IT 全部の見込みの半分を超える」。全部流す共通のファイルは `Cargo.lock`・`Cargo.toml`・`crates/*/Cargo.toml`・`build.rs`・`crates/*/build.rs`・`rust-toolchain.toml`・`.config/nextest.toml`・`migrations/**`・`src/migration_numbers.rs`・`tests/common/**` と、表の tree に無い src・crates の `.rs`。表の古さの上限は生成から 48 時間で、表に無い IT（表の後に足されたテスト）は必ず流す。
- 比べる今の値: `land_phases.verify` の中央値 516 秒・p90 982 秒（goal 157 の記述、`dagq stats --full`、2026-09-29 以降の着地 439 件）。これは build・unit test・coverage の計測を含む verify 全体で、ここの値は IT のテストの時間だけの見込み（build を含まない）なので、そのまま引き算はしない。

## 母集団と記録の取り方

- **期間**: 2026-09-29T00:00:00Z 以上、T_end = 2026-10-06T00:00:00Z 未満（測った日 2026-10-06 の UTC 00:00:00Z）。
- **着地**: `dagq events --full --kind run_integrated --since 2026-09-29T00:00:00Z --until 2026-10-06T00:00:00Z --limit 100` を、返った `cursor` を `--after <cursor>` に渡して `events` が空になるまでページを進めて全部読んだ（1 ページは 100 件で切れる）。431 件。各着地の差分は payload の `main_before` と `commit` から `git diff --name-only <main_before> <commit>`。両方とも git にあり、除いた着地は 0 件。
- **CI の履歴**: `gh run list --workflow CI --branch main --event push --created 2026-09-29..2026-10-05 --json databaseId,headSha,conclusion,createdAt,url --limit 1000`。返ったのは 418 件で `--limit` に達しなかったので、期間は分けていない（達したら日ごとに `--created <日>` で読み直す。collect.sh がそうする）。cancel された 357 件は飛ばし、残る 61 件（failure 60・success 1）を使った。CI は期間のほぼ全体で赤だった（success は 2026-09-29T00:08 の 1 件だけ）。
- **落ちたテスト**: failure の各 run の `gh run view --log-failed` から、job ごとに nextest の最初の `Summary [` の行の後の FAIL などの行（linux の job の Failed tests の step が要約に出すのと同じ状態の集合）を読んだ。FLKY-FL（流し直しで通った）は、CI では `.config/nextest.toml` の `flaky-result = "fail"` で失敗に数えられ run を赤くするが、変更による壊れの印ではないので、この測定では意図して赤に数えない。job ごとのテストの step の結論は `gh run view --json jobs` から読み、step が success か、failure で落ちたテストの行があるものを「その job のテストの結果がある」とした（macOS の job 61 件・linux の job 35 件。linux の job は 2026-10-01T23:15 の run から）。ci-failure の issue は 4 件で、落ちたテストの名前を持たず（job と step だけ）、期間内の #2（2026-10-02T01:48 に開き 2026-10-06 に閉じた）は赤の期間の照合にだけ使った。
- **対応表**: `gh run download 37413267459 -n it-coverage-map`（task 1923 の job の最新で、測った時点で保持中の唯一の artifact）。commit `8d208e2a3703e2624805f0ed6eab680c157052f2`、generated_at 2026-10-06T04:54:42Z、<https://github.com/hisamekms/dagq/actions/runs/37413267459>。IT 1,138 本（`dagq::it` 1,088・`dagq::plugin` 17・broker の crate 33）、ファイル 499。所要時間はこの job（macos-14 の runner、テストごとに coverage の計測つき）の nextest の JUnit の値。

**1 回だけ読む測定にした理由**: 入力は queue の event・git・CI の履歴・artifact の記録で、値はそれらと表から計算するだけで決まる。host の負荷や流す順に依らず、何度流しても同じ値になるので、周回も交互の比較もしない（host の上で build や test を流す測定と違い、揺れの源が無い）。

## 絞った IT の選び方（測定で当てた規則）

1. 差分に共通のファイル（下の「決めたこと」の一覧）があれば IT を全部流す（理由: 共通のファイル）。
2. そうでなければ、差分の各ファイルについて和をとる:
   - 表にあるファイル（src・crates・`tests/it/runtime_support/**`・crates の `tests/common` などのテスト側の補助を含む）→ 表がそのファイルに当てたテスト。
   - 差分で足した・変えたテストのファイル → そのファイルが定めるテスト（`tests/it/<m>.rs` → `dagq::it::<m>::*`、`tests/plugin.rs` → `dagq::plugin::*`、`crates/<c>/tests/<t>.rs` → `<c>::<t>::*`）。
   - テストが repository から読むファイル → そのテスト（`plugins/**` → `dagq::plugin::*` と `dagq::it::installed_plugin::*`、`scripts/check-migration-numbers.sh` → `dagq::it::queue_schema::*`、`dagq.toml`・`.dagq/agents/**` → `dagq::it::review_subagents::this_repository_names_only_agents_it_defines`）。
   - src・crates の `.rs` で表に無いもの: unit test の module（`tests.rs`・`tests/`・`*_tests.rs`）は IT を選ばない（unit test は全件流す）。表の commit の tree にあるが表に無いもの（`src/lib.rs`・`mod.rs` の mod の行だけなど、IT がどの行も実行しないファイル）も選ばない。表の tree に無いもの（その後に消えたか名前が変わった）は表が判断できないので IT を全部流す（理由: その他）。
   - それ以外（docs・`.github/`・その他の scripts など）は IT を選ばない。
3. 選んだ IT の見込みの壁時計が閾値を超えたら IT を全部流す（理由: 閾値超え）。

## 値の式

- **直列の和** = 選んだ各テストの所要時間（表の `duration_secs`）の和。
- **見込みの壁時計** = max(直列の和 ÷ 6, 選んだテストの最長の所要時間)。6 は `dagq.toml` の `[run.env]` の `NEXTEST_TEST_THREADS`。build とテストの割り振りの偏りを含まない下限の見込み。全部流した着地は IT 全部の和（2,632.5 秒）と壁時計（max(2,632.5 ÷ 6, 49.9) = 438.7 秒）で数える。
- **中央値**: 値を昇順に並べ、件数 n が奇数なら (n+1)/2 番目、偶数なら n/2 番目と n/2+1 番目の平均。
- **p90**: 最近順位法で、昇順の ceil(0.9×n) 番目。
- どちらも母集団の全着地（全部流したもの・IT を 1 本も選ばないものを含む）で出し、絞れた着地だけの値を別に添える。
- **全部流した割合** = 全部流した着地の件数 ÷ 母集団の着地の件数（差分が src に無い着地も分母に含む）。
- **新たに赤くなったテスト**: job ごとに、テストの結果がある run を順に並べ、ある run で落ち、その job の前の結果のある run では落ちていなかったテスト。その範囲は、前の run の `headSha`（含まない）からその run の `headSha`（含む）までの main の first-parent の commit で、その中の着地が「最後の緑から最初の赤までの範囲の着地」。両方の job で新たに赤なら短い方の範囲をとる。
- **見逃し** = 新たに赤くなった IT（表にあるテスト）のうち、範囲のどの着地の絞った IT にも入っていなかったもの。範囲が 1 件の着地ならその着地の絞った IT に入っていなければ見逃し、範囲が複数の着地にまたがるときはどれかに入っていれば見逃しでない（その件数を別に数える）。範囲に dagq を通らずに main に入った commit（人の直接の commit）があり、どの着地にも選ばれていないものは、選択が当たらない commit が原因になりうるので見逃しと別に数える。表に無いテスト（その後に消えたか名前が変わった IT、unit test）は見逃しの判定から外して件数だけ書く。
- **一斉の赤**: 1 つの run で 50 本以上が新たに赤くなったものは、着地の選択と関係ない 1 つの原因（環境）として別に数える。期間には 2 回あった: 2026-09-29T00:18 の run 36502435811（macOS、682 本。restore した cache の空の fixture の template。ci.yml の comment が書く「9/29 から main の CI が赤」）と、2026-10-05T00:09 の run 37246370940（linux、879 本）。

## 結果

### 着地ごとの表

`landing-it-selection/landings.csv` が全 431 件を持つ。列: `event_id`・`landed_at`・`task`・`commit`・`diff_files`（差分のファイル数）・`full`（全部流したか）・`reason`（`threshold`・`common`・`other`）・`selected_tests`（絞った IT の本数。全部流したら 1,138）・`serial_secs`（直列の和）・`wall_secs`（見込みの壁時計）・`narrowed_tests_before_threshold` と `narrowed_wall_secs_before_threshold`（閾値を当てる前の絞った IT。共通のファイルで全部流したものは空。閾値を変えたときの再計算に使う）・`missed_tests`（範囲がその着地 1 件の見逃しのテスト。今回は全部空）。

抜粋（先頭の 2 件と、絞れた最初の 3 件）:

| task | commit | 差分のファイル数 | 全部流したか（理由） | 絞った IT の本数 | 直列の和（秒） | 見込みの壁時計（秒） | 見逃しのテスト |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 997 | d12de1f1 | 36 | 全部（共通のファイル） | 1,138 | 2,632.5 | 438.7 | なし |
| 1046 | 3a3d9618 | 11 | 全部（閾値超え。前は 939 本・421.3 秒） | 1,138 | 2,632.5 | 438.7 | なし |
| 835 | abe81186 | 7 | 絞った | 65 | 127.9 | 28.2 | なし |
| 1052 | d056faa4 | 4 | 絞った | 22 | 12.5 | 4.2 | なし |
| 915 | 146a617c | 4 | 絞った | 9 | 8.7 | 5.6 | なし |

### 全体の要約（閾値 220 秒）

| 対象 | 件数 | 直列の和 中央値 | 直列の和 p90 | 見込みの壁時計 中央値 | 見込みの壁時計 p90 | 本数 中央値 | 本数 p90 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 全着地 | 431 | 2,632.5 | 2,632.5 | 438.7 | 438.7 | 1,138 | 1,138 |
| 絞れた着地だけ | 184 | 0.0 | 75.1 | 0.0 | 13.3 | 0 | 22 |
| 絞れて 1 本以上選んだ着地 | 78 | 12.5 | 268.0 | 4.2 | 45.8 | 22 | 111 |
| （閾値の前の絞った IT。共通のファイルで全部流したものを除く） | 398 | — | — | 365.1 | 428.2 | — | — |

- 全部流した割合: 247 / 431 = 0.573。理由ごと: 閾値超え 196、共通のファイル 33（`tests/common/**` 19・`migrations/**` 14・`Cargo.toml` 5・`Cargo.lock` 2・`crates/*/Cargo.toml` 2・`build.rs` 2・`crates/*/build.rs` 1。重なりあり）、その他 18（表の tree に無い `src/throughput_review.rs`・`src/observer.rs`・`src/application/workspace_cleanup.rs` など、その後に消えたか移ったファイル）。
- 全着地の見込みの壁時計の平均は 254.2 秒（全部流すと 438.7 秒）。
- 閾値の比較（`landings.csv` の `reason` と `narrowed_wall_secs_before_threshold` から。共通のファイル・その他・閾値超えを IT 全部の 438.7 秒とし、CSV の丸めた秒で計算）:

| 閾値（見込みの壁時計） | 全部流した割合 | 中央値 | p90 | 平均 |
| --- | --- | --- | --- | --- |
| 60 秒 | 0.582 | 438.7 | 438.7 | 257.2 |
| 120 秒 | 0.573 | 438.7 | 438.7 | 254.2 |
| 220 秒（決めた値） | 0.573 | 438.7 | 438.7 | 254.2 |
| 350 秒 | 0.559 | 438.7 | 438.7 | 252.4 |
| 無し（共通・その他だけ全部） | 0.118 | 378.0 | 438.7 | 239.4 |

閾値の前の絞った IT が大きくなるのは、多くの着地が中心のファイルに触れるため。表の src・crates のファイル 326 のうち 147 は IT の半分以上に当たる（最大は `src/domain/mod.rs` の 1,052 本 = 92%）。

### 見逃し

| 区分 | 新たに赤くなった IT | 見逃し（範囲が 1 件） | 見逃し（範囲が複数） | 範囲が複数で、どれかの着地に選ばれた | 人の直接の commit を含む範囲で選ばれなかった |
| --- | --- | --- | --- | --- | --- |
| 全部 | 1,655 | 0 | 0 | 1,165（うち範囲の一部の着地だけが選んだ 1,155） | 6 |
| 一斉の赤（2 run） | 1,345 | 0 | 0 | 869 | 0 |
| それ以外 | 310 | 0 | 0 | 296 | 6 |

- 新たに赤くなったテストは 1,940 件で、IT 1,655・表に無い IT 279（一斉の赤 216・それ以外 63）・unit test 6。明細は `landing-it-selection/ci-new-failures.csv`（テスト・前の run・赤の run・その run で新たに赤の本数・一斉の赤か・範囲の着地の task・区分・選んだ着地・見逃しか・範囲が複数か・範囲の人の直接の commit・次の結果でも赤か）。
- 人の直接の commit を含む範囲の 6 件: 2026-10-05T15:43 の run 37335082493 の `dagq::it::runtime_slot_order::*` 5 本と `dagq::it::cli_authorization::only_user_and_inbox_set_the_priority_of_a_task_in_progress`。範囲は task 1619（`src/application/planner_handoff.rs` の unit test だけの変更）と、slot の順を変えた人の直接の commit `a8056a3a` で、原因は後者と読める。選択は着地にだけ当たるので見逃しに数えない。
- 閾値を外した場合: 一斉の赤の run 36502435811 の範囲（task 1046 の 1 件）で 57 本が見逃しになる（環境の壊れで、task 1046 の変更とは関係ない）。それ以外の run では閾値の有無によらず見逃し 0。閾値を外すと人の直接の commit `0e8c4020` を含む範囲（task 1174）の `dagq::it::lifecycle_up::no_claude_up_*` 2 本も選ばれない。

## 古さの材料

表が 1 本しか無い（測った時点で保持中の `it-coverage-map` の artifact は 1 本）ので、日付の違う表を同じ差分に当てた選択の差は出せない。近似として、各着地について、その着地の前 N 日（N = 1・2・3・7）に main に入った変更を git から数えた（`landing-it-selection/staleness.csv`）:

- **前 N 日に足されたテスト**: 着地の `main_before` の tree の IT の関数（`tests/it`・`tests/plugin.rs`・`crates/*/tests` の `#[test]` の関数）のうち、N 日前の main（`git rev-list -1 --first-parent --before=<着地の時刻 − N 日> <main_before>`）の tree に無いもの。`path::関数名` で数えた値（移ったテストを含む。表の鍵は module を含むので、移ったテストも古い表には無い）と、関数名だけで数えた値（名前を変えたテストを含む）。
- **変わった src・crates のファイル**: N 日前の main と `main_before` の差分の、`src/**/*.rs` と `crates/*/src/**/*.rs` のうち着地の時点にあるもの（消えたファイルを除く）の数と、`main_before` の tree のそれらの全体に対する割合。

| N（日） | 足されたテスト（path::関数名）中央値 / p90 | 足されたテスト（関数名）中央値 / p90 | 変わった src のファイル 中央値 / p90 | 割合 中央値 / p90 |
| --- | --- | --- | --- | --- |
| 1 | 96 / 132 | 89 / 132 | 133 / 156 | 0.400 / 0.521 |
| 2 | 188 / 284 | 188 / 256 | 186 / 241 | 0.550 / 0.843 |
| 3 | 265 / 493 | 265 / 471 | 207 / 274 | 0.625 / 0.965 |
| 7 | 912 / 1,103 | 912 / 1,103 | 307 / 319 | 0.991 / 1.000 |

2026-09-26〜28 に IT が `tests/it` の 1 つの binary に移った（ADR-0078）ので、それをまたぐ窓は移動で大きくなる（7 日の窓は 2026-10-05 の着地でもまだ移動をまたぐ）。3 日の窓が移動の後に始まる 2026-10-03 以降の着地 199 件だけでは、N = 1・2・3・7 で足されたテスト（関数名）の中央値が 90・190・274・676、変わった src のファイルの割合の中央値が 0.392・0.558・0.630・0.924。

**この近似で確かめられる範囲**: 古い表が選び得ないテスト（表に無いので、どのファイルの変更からも選ばれないテスト）の量の上限。差分で足した・変えたテストは必ず流すので、その着地自身のテストは古さに依らず入る。漏れうるのは他の着地が N 日のあいだに足したテストで、1 日で約 90 本（IT の約 8%）、2 日で約 190 本（約 17%）、3 日で約 270 本（約 24%）、7 日でほぼ全部が入れ替わる。

**確かめられない限界**: ファイルとテストの対応の変化そのもの（既にあるテストが新しく別のファイルを通るようになったか）は、表が 1 本では分からない。変わった src のファイルの割合（1 日で約 40%）は、対応が変わりうるファイルの量の上限の目安にしかならない（変わったファイルの大半は対応が変わらないと見込むが、測っていない）。表が 2 本以上たまったら、同じ差分に日付の違う表を当てて選択の差と見逃しへの効き方を測り直す（`analyze.py` は data の `maps/` に複数の表があれば差を出す）。

## 決めたこと（段 3b の ADR と design が使う）

1. **全部流す閾値**: 絞った IT の見込みの壁時計（max(直列の和 ÷ `NEXTEST_TEST_THREADS`, 最長のテスト)）が、同じ表の IT 全部の見込みの壁時計の半分を超えたら IT を全部流す（今の表では 220 秒）。根拠: 絞った IT は 60 秒以下と 350 秒超の二山で、60〜350 秒のどこに置いても全部流す割合は 0.559〜0.582、平均は 252〜257 秒でほぼ変わらない。半分を超える選択は全部流すのに比べて節約が半分未満で、見逃しの危険（閾値を外すと一斉の赤で 57 本を見逃した）を負う価値が小さい。表の秒は coverage の計測つきの CI の runner の値なので、秒の絶対値でなく IT 全部に対する割合で決める。
2. **IT を全部流す共通のファイル**: `Cargo.lock`・`Cargo.toml`・`crates/*/Cargo.toml`・`build.rs`・`crates/*/build.rs`・`rust-toolchain.toml`・`.config/nextest.toml`・`migrations/**`・`src/migration_numbers.rs`（build.rs が読む）・`tests/common/**`（共通の fixture。`mod.rs`・`template.rs` は IT の 88〜89% に当たる）。加えて、表の tree に無い src・crates の `.rs` に触れたら全部流す。`tests/it/runtime_support/**` と crates の `tests/common` は表で選ぶ（各ファイルに当たるテストが表にあり、大きいものは閾値が全部にする）。`tests/it/main.rs` は mod の行だけで、足した module はテストのファイルの変更として選ばれるので共通にしない。中心の src のファイル（`src/domain/mod.rs` など）は一覧に入れず閾値に任せる（どれが中心かは表とともに変わる）。
3. **表の古さの上限**: 表の generated_at から 48 時間。それより古い表しか取れなければ IT を全部流す。あわせて、表に無い IT（表の後に足されたか名前の変わったテスト）は必ず流す。根拠: 夜間の job は毎日なので、ふだんの古さは 1 日以内で、48 時間は 1 回の失敗を許す。表に無いテストを必ず流せば足されたテストの漏れ（2 日で約 190 本の上限）は消え、残るのは確かめられない対応の変化だけになる。対応の変化の上限の目安（変わった src のファイルの割合）は 2 日で約 0.55、3 日で約 0.63、7 日でほぼ 1.0 で、7 日の表は選択の根拠にならない。

## 限界

- 今日（2026-10-06）の表を過去（2026-09-29〜10-05）の差分に当てた近似。表の後に名前が変わった・消えたファイルとテストは判定できない（全部流した理由「その他」18 件、表に無い IT の赤 279 件）。当時の対応は今日と違いうる。
- 期間の main の CI はほぼずっと赤で、cancel が 357 件あったので、範囲が複数の着地にまたがるものが多い（1,165 件）。範囲が広いほど「どれかが選んだ」で見逃しでないとされやすく、見逃しの 0 件は上限ではなく、この範囲の粒度での値。
- 所要時間は CI の coverage つきの runner の値で、本番の host の着地の検証の秒と違う。見込みの壁時計は build とテストの割り振りの偏りを含まない下限。
- 新しい形の着地の検証では build・clippy・unit test 全件も流れるので、verify の全体はここの IT の時間より長い。前後の実測は段 4 が行う。

## 実行のコマンドとファイル

repository の root で（T_END = `2026-10-06T00:00:00Z`、DATA は repository の外の dir）:

```sh
sh docs/plans/landing-it-selection/collect.sh 2026-10-06T00:00:00Z "$DATA"
python3 docs/plans/landing-it-selection/analyze.py "$DATA" "$DATA/maps/37413267459.json" --threshold-wall 220
```

- `landing-it-selection/collect.sh`: 着地の event（`--after` でページング）・CI の run（`--limit` に達したら日ごと）・落ちたテスト・job ごとのテストの step の結論・ci-failure の issue・保持中の対応表を DATA に集める（dagq の読み取りのコマンド・git・認証済みの gh）。13 MB の表などの材料は repository に置かない。
- `landing-it-selection/analyze.py`: 選択の規則・値の式・見逃し・古さの材料を計算し、`landings.csv`・`ci-new-failures.csv`・`staleness.csv`・`summary.txt` を書く（既定ではこの dir）。2026-10-06 の値は CI の artifact の保持（30 日）が切れると同じ表で作り直せないので、CSV を正として残す。

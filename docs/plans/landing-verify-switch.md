---
id: plan-landing-verify-switch
type: plan
title: 着地の検証を unit test 全件と絞った IT に切り替えた前後 7 日の verify・landing_queue・着地の数・main が赤だった時間・修正 task・事故・他人の壊れによる resume と ask の比較と、全部流す条件を見直す要否
status: completed
created: 2026-10-20
owners:
  - hisamekms
tags:
  - testing
  - performance
  - measurement
related:
  - adr-t1925-1
  - adr-t1920-1
  - plan-landing-it-selection
  - plan-coverage-at-landing
  - plan-landing-lane-cpu-share
---

# 着地の検証の切り替えの前後比較

goal 157 の段 4（task 1929）。
[ADR-t1925-1](../adr/2026-10-07-t1925-1-landing-verifies-unit-tests-and-selected-integration-tests-and-ci-is-the-final-gate.md) で着地の検証を fmt・clippy・軽い検査・unit test 全件・影響範囲で絞った IT（`scripts/landing-it.sh`）にし、最終関門を CI にした切り替えの前後 7 日を比べた。
測って書くだけで、設定・`dagq.toml`・queue の状態は変えていない。

## 要点

- **verify は短くならなかった。** `land_phases.verify` の中央値は 517 秒 → 554 秒（×1.07）、p90 は 942 秒 → 1,260 秒（×1.34）。
  IT を絞れたのは `landing-it.sh` の 183 回のうち 13 回（7%）だけで、そのコマンド単体は中央値 149 秒・p90 223 秒、その回で着地した 12 件の verify（fmt・clippy・build を含む）は中央値 192 秒・p90 859 秒で、見込みの 1〜3 分にほぼ届いた程度。
  残りの 170 回（93%）は IT を全部流し、その時間（中央値 714 秒）は前の窓の `cargo llvm-cov nextest`（中央値 569 秒）より長かった。
- **全部流した理由**（`landing-it.py` の選び方を各着地の差分と当時の表で再現。閾値は他の理由が無いときだけ判定するので閾値超えは他と重ならず、重なるのは共通のファイルと表の tree に無いファイルの 13 回だけ）: 表の tree に無い src のファイル（差分で足したファイル）81 回、閾値超え 74 回、共通のファイル 28 回。
  段 3a の測定（[landing-it-selection](landing-it-selection.md)）は全部流すのを 57%（表の tree に無いファイルは 4%）と見込んでいた。
- **landing_queue の待ちは「ほぼ消える」に届かなかった。** 中央値は 2 秒のまま、p75 は 340 秒 → 231 秒、p90 は 931 秒 → 776 秒。
  ただし後の窓は着地そのものが 41% 少なく（1 時間あたり 2.55 → 1.51 件）、着地の slot の利用率が 0.8 以上の時間は 8 時間 → 4 時間だったので、待ちの減りは切り替えの効果と分けられない。
- **main が赤だった時間**は 149.8 時間（89%）→ 104.3 時間（62%）。
  どちらの窓も 1 本の長い赤（前は 09-30〜10-06 の 135 時間、後は 10-09〜10-11 の 44 時間）が大半で、それを除くと 14.6 時間 → 60.8 時間に増えた。
- **CI の見張りが登録した修正 task は 0 件。** 後の窓に `ci_failure` の finding（`propose: true`）は 19 件記録されたが、どれにも proposal も finding の planner も付かなかった。
- **事故**（固定バイナリを `.previous` に戻した回数）は 1 → 0。
- **他人の壊れによる resume と ask** は 12 → 7（resume 7 → 5、ask 5 → 2）。
  環境（sandbox の拒否）によるものは 26 → 1 に減ったが、run の review の program が main の script を読むために他の task の着地を待つ ask が後の窓に 13 件あった。
- **結論**: 切り替えは verify と待ちを見込みどおりには縮めておらず、IT を全部流す条件（特に「表の tree に無い src のファイル」）を見直す要がある。
  見直しの案は人か planner が決めることなので、receipt の follow_up（decision）にした。

## 切り替えの時点と窓

- **T**: 印 123128「landing verification: unit tests + selected IT (ADR-t1925-1)」の `at` = `2026-10-07T10:27:40.972Z`（task 1927 の着地、commit `ec310498`。planner が着地の時刻に打った）。
- **前の窓**: `2026-09-30T10:27:40.972Z` 以上 T 未満。
- **後の窓**: T 以上 `2026-10-14T10:27:40.972Z` 未満。
- どちらも `--since` / `--until` の [from, to) で、168 時間ずつ。
- **読んだ時刻**: 2026-10-20T00:10Z〜00:40Z（UTC）。
- **固定バイナリ**: `~/.local/bin/dagq` `0.4.0-dev+d837e2ed2821f000396e6ca71f56266e02c09fb6`。
  状態を変えないコマンド（`stats`・`kpi`・`events`・`findings`）だけを、worker のクライアントモードで queue service から読んだ。
  planner log と turns/ は読んでいない。
- **周回しない理由**: 入力は queue の event・`stats` の台帳・GitHub の CI と issue の記録・夜間の対応表で、値はそれらから計算するだけで決まる。
  host の上で build や test を流す測定と違い揺れの源が無く、何度読んでも同じ値になるので、周回数は 1。

## 比べた値

差は「後 − 前」、比は「後 ÷ 前」。
時間は秒。
p75 と p90 は最近順位法（昇順の ceil(p×n) 番目）、中央値は件数が偶数なら中の 2 つの平均。

| 値 | 前の窓 | 後の窓 | 差 | 比 |
|---|---|---|---|---|
| (a) verify の中央値（着地 429 / 253） | 517 | 554 | +37 | 1.07 |
| (a) verify の p90 | 942 | 1,260 | +318 | 1.34 |
| (a) 同、人の答え待ちを除く（ask が 0 の着地 323 / 182）の中央値 | 512 | 497 | −15 | 0.97 |
| (a) 同、人の答え待ちを除く の p90 | 840 | 1,159 | +319 | 1.38 |
| (b) landing_queue の中央値 | 2 | 2 | 0 | 1.00 |
| (b) landing_queue の p75 | 340 | 231 | −109 | 0.68 |
| (b) landing_queue の p90 | 931 | 776 | −155 | 0.83 |
| (b) 同、人の答え待ちを除く の中央値 | 2 | 2 | 0 | 1.00 |
| (b) 同、人の答え待ちを除く の p75 | 118 | 38 | −80 | 0.32 |
| (b) 同、人の答え待ちを除く の p90 | 432 | 568 | +136 | 1.31 |
| (c) 着地の数（1 時間あたり） | 2.55（429 件） | 1.51（253 件） | −1.04 | 0.59 |
| (c) 混む時間帯（slot の利用率 0.8 以上）の 1 時間あたり | 3.75（8 時間） | 2.25（4 時間） | −1.50 | 0.60 |
| (d) main が赤だった時間 | 149.8 時間（89.2%） | 104.3 時間（62.1%） | −45.5 時間 | 0.70 |
| (d) 同、最長の赤 1 本を除く | 14.6 時間 | 60.8 時間 | +46.2 時間 | 4.17 |
| (e) CI の見張りが登録した修正 task | 0（見張りは窓の最後の 1.3 時間だけ。finding 5 件、proposal 0。参考: ci-failure の issue 10 件） | 0（`ci_failure` の finding 19 件、proposal 0。参考: issue 36 件） | 0 | — |
| (f) 事故（固定バイナリを `.previous` に戻した回数） | 1 | 0 | −1 | 0 |
| (g) 他人の壊れによる resume（main_broken・existing_flaky） | 7 | 5 | −2 | 0.71 |
| (g) 他人の壊れによる worker_question（main_broken） | 5 | 2 | −3 | 0.40 |
| (g) 合計（100 着地あたり） | 12（2.80） | 7（2.77） | −5 | 0.58 |

参考の値:

| 値 | 前の窓 | 後の窓 |
|---|---|---|
| 着地の検証の test のコマンド単体（前: `cargo llvm-cov nextest` 313 回、後: `sh scripts/landing-it.sh` 183 回）の中央値 / p90 | 569 / 807 | 700 / 962 |
| 同、後の窓の IT を全部流した 170 回 / 絞れた 13 回 | — | 714 / 972、149 / 223 |
| test のコマンドを含む着地だけの verify の中央値 / p90 | 620 / 1,092（274 件） | 778 / 1,473（161 件） |
| 同を含まない着地（docs・config など）の verify の中央値 | 4（155 件） | 7.5（92 件） |
| 着地の slot の利用率（`stats` の `landing_utilization`）の平均 / 最も混んだ 1 時間 | 0.335 / 0.978 | 0.251 / 0.98 |
| 1 時間の着地の最大 | 13 | 10 |
| 自動更新の関門の失敗（`update_failed`）/ 入れ替え（`update_installed`） | 16 / 207 | 5 / 138 |
| 環境による resume / worker_question | 22 / 4 | 0 / 1 |
| 他の着地との組み合わせによる resume（interaction） | 4 | 7 |
| 他の task の着地を待つ worker_question（waits_other_task） | 0 | 13 |
| 他人の壊れによる worker_question の人の答え待ちの合計 | 71,067 | 15,416 |

## 値の式と出所

集計の script と CSV は [landing-verify-switch/](landing-verify-switch/) にある（区分は「集計の材料」）。

### (a) verify と (b) landing_queue

- 出所: `dagq stats --full --since <from> --until <to>` の `overall.land_phases.verify`・`landing_queue`（中央値・p90）と、p75 と人の答え待ちを除く値のための `runs[]`（`status` が `integrated` の run）の `land_phases`。
- `stats` の工程は重ならない区間なので、verify と landing_queue は人の答え（工程 `ask`）を含まない。
  「人の答え待ちを除く」は、着地までに人の答えを待たなかった run（`land_phases.ask` が 0）に絞った値にした。
- コマンド単体の時間は `verification_command` の event（`command` が `cargo llvm-cov nextest …` か `sh scripts/landing-it.sh`）の `duration_secs`。
- `kpi --compare 123128 --window 7` の `strata`（`kpi_compare_used.json`）も同じ値（`land_phase.verify` 517 → 554、verdict worsened。`land_phase.landing_queue` は unchanged）。

### IT を全部流したかと理由

- `landing-it.sh` は理由をログの冒頭に出し、event の `output_tail` に残らないので、`replay_choice.py` で再現した。
- 各回の差分は、同じ run のその回の `integration_started` の `main` から `integration_rebased` の `head_after` まで（`git diff --name-only`）。
- 表は、その回より前に終わった `IT coverage map` の成功した run の artifact（`gh run download <id> -n it-coverage-map`）の最新で、表の tree は表の `commit` の `git ls-tree`。
- 選び方は `scripts/landing-it.py` の `full_reasons` と閾値の判定をそのまま呼んだ（このファイルは T から後の窓の終わりまで変わっていない）。
- 再現の判定は、nextest の Summary の skipped の数（200 未満なら全部）と 181 回中 176 回で一致した。
  一致しない 5 回は 10-14 の着地で、既に落ちているテスト（277 本）を外したために skipped が増え、数で全部と読めなかったもので、再現の判定を使った。

### (c) 着地の数

- 着地の数は `stats` の着地（429 / 253）÷ 168 時間。
  `run_integrated` の event で数えると 428 / 254 で、T ちょうどに着地した 1 件が event の時刻では後の窓に入る違い。
- 混む時間帯: 窓の始まりから 1 時間ずつ区切り、着地の slot の使用を `integration_started` から同じ run の `run_integrated`・`integration_deferred`・`integration_error`・`integration_held`・`run_recovered` まで（次の `integration_started` で切る）とし、1 時間の使用が 0.8 以上の時間の着地の数 ÷ その時間数（`hours.csv`）。
  使用の合計（202,889 秒 / 151,748 秒）は `stats` の `landing_utilization.busy_secs`（202,888 / 151,746）と合う。

### (d) main が赤だった時間

- 定義: CI（`ci.yml`）の main の push の実行を `createdAt` の順に並べ、緑の後に最初に落ちた実行の `createdAt` から次の緑の実行の `createdAt` までを赤の区間とし、窓で切った和。
  cancel・skip・終わっていない実行は飛ばす。
  CI の見張りの `ci_turned_green` の `red_secs` と同じ定義（例: 赤 37545363130 と緑 37546875560 の `createdAt` の差 974 秒）。
- 出所: 両方の窓とも `gh run list -R hisamekms/dagq --workflow CI --branch main --event push --created <日> --json databaseId,headSha,conclusion,status,createdAt,updatedAt,url --limit 1000` を日ごとに読んだ（どの日も 1000 件に届かない）。
- 後の窓は CI の見張りの event（`ci_turned_red` 36 件・`ci_turned_green` の `red_secs`）からも同じ定義で 102.8 時間で、gh の 104.3 時間に近い。
  ただし打ち消し合う差を含む: 窓の終わりに開いたままの赤（gh で 9.9 時間）は見張りの記録に入らず、その分ほかの区間は見張りのほうが約 8 時間長い（見張りが緑の判定を待った分など）。
  前の窓の見張りの記録は見張りを有効にした 10-07T09:12Z からしか無い。

### (e) 修正 task の数

- 後の窓: CI の見張りが記録した `finding_recorded`（`kind` が `ci_failure`、`propose: true`）と、それに付いた `finding_status_changed`（`to: proposed`）・`finding_planner_opened` の数。
  `dagq findings --all` でも、これらの finding の `proposal_id` はどれも null で、修正 task になる経路に乗ったものは無かった。
- 前の窓: 見張りは 10-07T09:12Z に有効になり、窓の最後の 1.3 時間に `ci_failure` の finding が 5 件（234〜237・240）記録されたが、proposal は 0。
  参考に `gh issue list -R hisamekms/dagq --label ci-failure --state all --limit 500` の、窓に開いた ci-failure の issue の数（10 件。後の窓は 36 件）。

### (f) 事故

- `update_restored`（自動更新・install が固定バイナリを `.previous` に戻した）の event の数。
  前の窓の 1 件は 2026-10-03T11:43Z（`0045125d` から `151a4955` に戻した）。
- 参考に `update_failed`（自動更新の関門の失敗。戻しはしない）と `update_installed` の数。

### (g) 他人の壊れによる resume と ask

対象:

- resume: `resume_started` のうち、`reason` が着地の検証の失敗（`verification command "…" exited with …`）か landing recheck の検証の失敗（`the landing recheck found that "…" exits with …`）のもの（前 38 件、後 19 件）。
  rebase の衝突・review の差し戻し・receipt の催促の resume は対象外。
- ask: `ask_opened` の `kind` が `worker_question` のもの（前 12 件、後 21 件）。

分類の規則（上から順に当てる）:

1. **environment**: 失敗の出力か理由に `Operation not permitted`・`No space left`・`sandbox` がある（sandbox や host が拒んだ）。
   worker_question は、その問いが sandbox の拒否や host でしか示せないことを理由にするもの。
2. **main_broken**: そのコマンドが main だけでも落ちる（worker が main だけの tree で確かめた、または「main 自体」「この変更ではない」と原因を示した）。
   worker_question は、main で落ちている検査か既に落ちているテストのために受け入れ条件を満たせないと問うもの。
3. **existing_flaky**: 差分が触れていない既存のテストの時刻や競合による失敗（worker がテストの作りの競合と示した）。
4. **interaction**: 他の着地が run の差分の使うもの（関数の引数・テストの helper・migration の番号・同じ文書の大きさの上限）を変え、組み合わせで落ちた。
   どちらの差分も単独では落ちないので「他人の壊れ」に数えない。
5. **waits_other_task**（worker_question だけ）: run の review の program が main の script を読むため、その script を直す他の task の着地を待つ問い。
   main は壊れていないので「他人の壊れ」に数えない。
6. **own**: 上のどれでもない（run 自身の差分、または原因を示す記録が無い）。

「他人の壊れ」は main_broken と existing_flaky。
原因の根拠は、event の `reason`（と `integration_deferred`・`landing_recheck_failed` の `failure`・`output_tail`）と、ask の本文の代わりに、その resume か ask の後の worker の `turn_finished` の `message`（worker が原因と問いを要約する）。
ask の本文は event に無く、worker は `dagq show` を打たないため。
規則 1 は `compute.py` が文字列で当て、2〜6 は message を読んで `RESUME_LABELS`・`LABELS` に event と ask の ID ごとに書いた（`verification_resumes.csv`・`worker_questions.csv` に message の頭と分類がある）。
`same_main_hint` は、同じ main の commit で同じコマンドが他の run でも落ちた印で、規則 2 の確かめに使った。

## 交絡

`kpi --compare 123128 --window 7` は T で窓を分けられる（`split.separable: true`）とし、次を `confounders` に挙げた。

- 前の窓の印 6 件: Codex の既定モデルの変更（51928・51978）、run の review を Codex に（60152）、Claude の worker の既定を非対話に（61916）、runtime の planner の非対話（78166）、非対話の wrapper の background（82010）。
- 後の窓の印 2 件: 148902「list」（planner、note なし）、184541「supervisor max_spikes 1, max_investigations 2」（10-13）。
- 後の窓の `claude_version` の変化 6 回（2.1.289 → 2.1.299）と、`supervisor_started` 145 回（前の窓 214 回。多くは自動更新の入れ替え）。

`kpi` の他の値も、後の窓は着地の上流が細ったことを示す: `revise_rate` 0.42 → 0.68、`first_pass_rate` 0.366 → 0.257、`phase.work` の中央値 805 → 1,187 秒、`slot_usage` 2.41 → 1.84、`landings` 429 → 253。
着地の slot が詰まった時間は後の窓で 4 時間しか無く、(c) の減りは着地の検証でなく供給の減りと止まった日による。

後の窓に入った、着地の検証・CI・IT の数と時間を変えた着地（`git log`）:

- `4120791b`（10-08）: docs だけの push で CI の Rust の job を飛ばし、ci-failure.yml を Rust の job に絞った。(d) と (e) の参考の issue の数に効く。
- `d374aba8`（10-08）: CI で `landing-it.sh --self-test` を流す。
- `bc4dee62`・`2734bbe4`・`fa0fa4de`（10-07〜08）: CI の見張りが終わる順・re-run・skipped の job・外した job を扱う修正。後の窓の (d) の見張りの記録に効く。
- `6e9342bd`（10-08）: `[recheck]` の paths を広げた（landing recheck の `cargo check` を流す範囲）。
- `4cdd183b`・`26184a7b`（10-08〜09）: resource broker の crate と `[workspace]` を外した（`Cargo.toml`・`Cargo.lock` は共通のファイル。IT の構成も変わった）。
- `53e10e1c`（10-12）: 既に落ちているテストを worker の prompt と review の材料に載せた（goal 157 の段 5）。(g) に効く。
- `81447765`（10-12）: toolchain の profile と検証の失敗の分類。
- `89b6f5d2`（10-13）: e2e と e2e の関門から cmux を外した。
- `a21a11b3`（10-14）: `dagq.toml` に `[toolchains]` を足した。
- `92164506`（10-08）: `[supervisor] light_changes = ["docs", "config"]`。docs・config の run の slot の取り方が変わり、(b) と test のコマンドを含まない着地の verify（p90 32 → 377 秒）に効きうる。
- `fe9143ff`・`75d039cb`・`b20865a9`・`61221c69`（10-12）: `[review.programs.*]` を足した。run の review の program が main の script を読むので、(g) の waits_other_task の 13 件の原因になった。
- `8ed8c02a`（10-13）: 検証の出力の印（時間切れと落ちたテストの名前）の宣言。検証の失敗の分類に効く。

印に無い交絡:

- **IT の数の伸び**: nextest の Summary の test の数（日ごとの中央値、`summary.json` の `gate.daily`）は、前の窓の llvm-cov で 2,246（10-01）〜2,989（10-07）、後の窓の全部流した回で 3,095（10-07）〜3,584（10-13）と伸び続けた。
  test 段の時間の日ごとの中央値は、前の窓 398〜668 秒、後の窓 449〜952 秒。
  前の窓の値は落ちた回だけのもの（llvm-cov は通ると `output_tail` が coverage の表で終わり、Summary が残らない）なので、通った回の test の数と時間は比べられない。
  後の窓の全部流した回が前の窓の llvm-cov より長いのは IT の伸びが効いていると見るが、この限りで確かめきれていない（load の中央値は llvm-cov の回 13.3、全部流した回 12.5 でほぼ同じ）。
- **10-14 の既に落ちているテスト 277 本**（finding 617）: 着地の検証から外れた分だけ test の数が減った。
- **止まった日**: 10-10 は着地も CI の実行も 0 件、10-13・10-14 は 13 件・11 件。
  前の窓も 09-30 は 1 件だった（Claude が使えなかった期間）。
- **前の窓の長い赤**: 09-30T00:33Z から 10-06T01:41Z まで main は赤のままで（ci-failure の issue #2）、queue は CI を見ていなかった。
  (d) の前後の差の大半はこの 1 本で、切り替えの効果ではない。

## 見込みとの比較と結論

| 見込み（goal 157） | 結果 | 判定 |
|---|---|---|
| verify は 1〜3 分 | 絞れた 13 回のコマンド単体は中央値 149 秒・p90 223 秒、その着地 12 件の verify は中央値 192 秒・p90 859 秒。全体は中央値 554 秒・p90 1,260 秒 | 届かない（絞れた着地でも中央値は 3 分をやや超え、93% の回は IT を全部流した） |
| landing_queue の待ちはほぼ消える | 中央値 2 秒のまま、p75 231 秒・p90 776 秒 | 届かない（着地の数も減ったので効果は分けられない） |
| llvm-cov と worker の CPU の取り合いの解消 | llvm-cov は着地から消えた。`cpu_per_landing` は 2,489 → 2,973 秒、`load_per_core` の中央値は 1.08 → 0.62 | coverage の計測は消えたが、全部流す IT の時間が残った。着地 1 件あたりの CPU が増えたのは、全部流す IT と、着地が減ってその他の CPU（全体の約半分）を割る件数が減ったためと見るが、分けていない |
| 事故は許容（CI で分かる壊れは後から直す） | 戻しは 0 回、main が赤の時間は最長の 1 本を除くと 14.6 → 60.8 時間 | 壊れは増えたが、直す修正 task が 1 件も登録されていない |

結論:

1. **全部流す条件を見直す要がある。**
   全部流した理由の最多は「表の tree に無い src のファイル」（81 回）で、段 3a の見込み（4%）よりずっと多い。
   後の窓は module の分割や新しいファイルを足す着地が多く、足したファイルは表が知らないので、どれも IT を全部流した。
   閾値超えの 74 回は、絞った見込みが IT 全部の見込みの半分（上限）の 1.41〜1.98 倍（中央値 1.9 倍）で、絞っても IT のほぼ全部に当たる差分だった。
   閾値を 0.5 から多少動かしても全部流す回は減らず、効くのは足したファイルの扱いと、表がファイルを多くの IT に当てる粒度のほうである。
2. **全部流したときの時間そのものが伸びている。**
   IT の数の伸びで、全部流す回は前の llvm-cov より長くなった。
   絞れない着地が大半のままなら、verify は IT の数とともに伸び続ける。
3. **CI の見張りから修正 task ができていない。**
   後の窓の `ci_failure` の finding 19 件はどれも proposal にならず、壊れは「後から直す」経路に乗っていない（goal 157 の受け入れ条件 1 の「修正 task が登録される」）。
4. **他人の壊れによる resume と ask は減った（12 → 7）が、100 着地あたりでは同じ（2.8）。**
   段 5（`53e10e1c`）の効果は後の窓の終わりの 2 日分しか入っていない。

見直しの案（どれを採るかは人か planner が決める。receipt の follow_up の decision）:

- 差分で足した src のファイルは、IT を全部流す理由にせず、同じ directory か親の module（`mod` を足したファイル）の表の当たりで選ぶか、表に無い IT と差分のテストだけにして見逃しを CI に任せる。
- 表がファイルを当てる粒度（ファイル単位で多くの IT に当たる共有の module）を見直すか、絞り込みの代わりに IT の数を減らす（[ITの削減](it-reduction.md)）ほうを先にする。
- 修正 task が登録されない原因を調べる（finding が proposal にならない）。

## 集計の材料

区分はどれも材料（1 回きりの測定の出力）で、他の仕組みは読まない。
`collect.sh` が入力を repository の外の directory に読み、`compute.py` がこの directory の CSV と `summary.json` を作る。
入力の events は後から変わらないが、`stats` の台帳の畳み直し・gh の artifact の保持期間（夜間の表）・issue の状態で、読み直した値は変わりうる。

- `collect.sh`: 入力を 1 回読み、再現と集計を流す。
- `fetch_events.py`: 窓の event を 6 時間ずつ読む（queue service の応答の上限のため）。
- `replay_choice.py`: `landing-it.py` の選び方の再現 → `choice.csv`。
- `compute.py`: (a)〜(g) の集計 → 下の CSV と `summary.json`。
- `runs.csv`: 着地した run ごとの verify・landing_queue・ask と test のコマンドの種類。
- `gate_commands.csv`: test のコマンドの回ごとの時間・load・nextest の test の数。
- `choice.csv`: `landing-it.sh` の回ごとの全部か絞ったかと理由・使った表・見込み。
- `hours.csv`: 1 時間ごとの着地の slot の使用と着地の数。
- `ci_main_runs.csv`・`red_intervals.csv`: main の CI の実行と赤の区間（gh と見張りの event）。
- `ci_failure_findings.csv`・`ci_failure_issues.csv`: (e) の finding と issue。
- `verification_resumes.csv`・`worker_questions.csv`: (g) の対象と分類。
- `kpi_compare_used.json`: `kpi --compare 123128 --window 7` のうち文書が引く値（`split`・`strata` の一部・`confounders`）。

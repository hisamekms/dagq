---
id: plan-docs-candidate-search
type: plan
title: 変えた名前で文書の候補を探す指示の前後測定
status: completed
created: 2026-10-05
related:
  - plan-acceptance-check
  - adr-t1688-1
  - design-measurement
---

# 変えた名前で文書の候補を探す指示の前後測定

初回 review の docs_drift の差し戻しは前後とも **14/40（35%）**。主理由だけなら **10/40（25%）→7/40（17.5%）** だが、文書指摘全体の減少は示していない。worker の作業時間の中央値は **889.5→1267.5 秒（+378 秒）**。探索だけの追加時間は記録されず、仕事の内容・負荷・並行した prompt と review の変更が混ざるので、この差を探索の費用や因果効果と断定できない。

## 区間と境界

締切 C = **2026-10-05T09:05:36.831Z**（この測定 run f387f7b2-4cf0-444d-98a4-27a5577db53c の run_claimed、event 103389、build b4f75bac20660c042b72799001e373ce980602d4）。全 event は created_at < C。選択は stats の着地時刻ではなく、各 run の最初の run_claimed の build と attempt=1 の review_finished の存在で決める。claim の古い順（同時刻は event ID 順）に並べ、前は直近最大40件、後は最初の最大40件を採る。最初の review を C までに終えていない run は含めない。区切りを時刻だけで推定せず、acceptance-check の fetch.holds / version_commit と compute.find_t（--t-task の仕組み）を再利用し、claim に記録された build に着地 commit が含まれるか git merge-base --is-ancestor で run ごとにも検査した。

前の条件は task 1429 と subagent 導入 9384cb25 の両方を含み、1688 を含まない build。後は1688を含む build。境界の着地と最初の該当 build の記録は以下（UTC）。完全な SHA と event ID は [meta.json](docs-candidate-search/out/meta.json)、各 claim の build は [runs.csv](docs-candidate-search/out/runs.csv)。

| 境界 task | 着地 commit | 着地 event / 時刻 | 最初の含む build / 起動 event / 時刻 |
| --- | --- | --- | --- |
| 1429 | 9edb8fd8404a2486a031792869bc160e9a760afb | 72010 / 2026-10-03T03:30:14.619Z | 0.4.0-dev+9edb8fd8404a2486a031792869bc160e9a760afb / 72360 / 2026-10-03T03:51:20.204Z |
| 1460 | 9384cb2599062a51742dd5dd8cbf719601924872 | 72702 / 2026-10-03T04:12:16.449Z | 0.4.0-dev+2449e081f229b0e661160e095298a840d6c7dc96 / 73137 / 2026-10-03T05:02:15.681Z |
| 1688 | adda99a602531acae52738f39601f418a423fb84 | 93780 / 2026-10-04T13:03:45.267Z | 0.4.0-dev+adda99a602531acae52738f39601f418a423fb84 / 93981 / 2026-10-04T13:08:20.963Z |

| 区間 | 件数 | 選んだ claim の最初〜最後 | attempt 1 review_finished の最初〜最後 |
| --- | --- | --- | --- |
| before | 40 | 2026-10-04T01:01:20.074Z〜2026-10-04T13:05:16.478Z | 2026-10-04T01:27:36.137Z〜2026-10-04T13:46:58.129Z |
| after | 40 | 2026-10-04T13:19:17.469Z〜2026-10-05T01:00:12.054Z | 2026-10-04T14:04:27.048Z〜2026-10-05T01:36:15.200Z |

後は40件で20件の条件を満たす。標本不足を理由とする再測の時期と measurement follow_up は不要。これは固定された C での測定であり、以後の run は結果に足さない。

## 指標の取得元と式

- **初回 docs_drift 率**: dagq events --full の attempt=1 の review_finished。分母 N は区間の run 数（1 run 1 event）。payload.route があれば destination=send_back のみを差し戻しとする（verdict=concern も含む）。route が無ければ verdict=revise、または concern で同じ run の次の review_started より前かつ C より前に revise_requested があるものを数える。ask、route 無し concern で revise_requested が無いものは「人の判断へ」。送れなかった差し戻し・取り消されたものも率には含む。(a) 分子は差し戻しで reason_codes の配列の配列のどこかに docs_drift、(b) 分子は同じ差し戻し判定で primary_code=docs_drift。全選択で (b)⊆(a) を assert した。参考は差し戻しを問わず reason_codes に docs_drift の attempt 1 の数。task 251/event 78650 と 655/event 78994 のような concern→send_back を verdict だけで落とさない。
- **照合の記録なし**: compute.mapping() と共有する validation_receipt_before() で取った最後の pre-review validation_finished の payload.receipt.summary。receipt の dict を持つ validation_finished を ID 順で探す。summary が無い・空、またはこの validation が無いものは欠測として分母の外へ出す。分母 V は summary が取得できた run。分子は summary に文書 path または不要理由の句が無い run。率=分子/V。receipt_observed と integration_receipt は使わない。取得用 CLI は dagq events --full --all --task ID --kind validation_finished --until C（実際の fetch は --task ID --all で全 kind を取得し、その中を抽出）。
- **summary の判定規則**: compute.py の PATH は docs/...、AGENTS.md、CLAUDE.md、plugins/...、単体の *.md を認める。UNNEEDED は英語の docs/documents と no change/update、not needed/required、unchanged 等、日本語の文書と不要/変更なし/更新なし、照合/更新と不要を認める。path の有無を数える規則で、照合の正しさは判定しない。受け入れ条件の根拠や作成した ADR の path だけでも陽性になるため、記録の充足を過大に見積もりうる。節名だけ（「役割の表を見た」）、代名詞だけ、.rst など docs/ の外の別の拡張子、別の語による不要理由は見落としうる。採用された最初の手がかりは runs.csv の check_evidence。後の「探した名前」は同じ summary の SEARCH（names searched、searched names/terms/for、search terms、探した名前、検索した名前/語、検索語の後に非空文字）で数える。検索の実行や語と差分の対応を保証せず、「Docs search: grepped ...」だけ等の違う言い方は見落とす。
- **最初の1往復（秒）**: (a) の集合（(b) の部分集合も別列）。attempt 1 review_finished 後、次の review_started 前の最初の取り消されていない revise_requested を始まりとする。revise=次の review_started.created_at−revise_requested.created_at。再 review=その started と同 attempt の最初の review_finished.payload.duration_secs。和は両側が取得できた run のみ。revise_unsent は同 attempt の依頼を取り消し、後に再送された revise_requested があればそれから測る。中央値は数値のソートの中央（偶数は中央2値の平均）、合計は取得値の和。
- **時間の除外**: 取り消しだけなら両側無し、未送信（次の started 前に request 無し）も両側無し。revise 未完了（request の後に started 無し）は両側無し。再 review 未完了/失敗（started の後に同 attempt の finish 無し、または review_failed）は revise のみ。duration_secs が無い/非数・非有限・bool、時刻が読めない場合は取得不能側のみ除く。C 後を読まない。2往復目以降は、再 review が差し戻された後の同じ計算（理由 code 不問）を追加往復として別に出す。途中に上の除外がある run は追加往復数・合計から外す。最初の取り消し後の再送が完了した run は最初の時間を出すが、追加往復の参考値からは外す。
- **worker 作業時間（秒）**: dagq stats --full --since 2026-10-01T00:00:00Z --until C の runs[].work（[stats](../design/supervisor-lifecycle/stats.md) の run_claimed→最初の receipt_observed）。land_phases は使わない。待ちを引いた work_excl_wait は acceptance-check compute.run_rows / overlap を再利用し、work の区間と run_waiting_started→次の run_waiting_ended の組の重なりだけを引く（元の計算どおりミリ秒から整数秒へ切り捨て）。receipt より前に開いた未終了の待ちは unfinished とし、中央値から除く。claim/receipt が無ければ両側欠測、stats.work の未取得も除く。run_waiting_deferred が work の区間内にある run は別に数える（待ちに入らなかった ask は引かれない）。今回 work は各 run の event の区間と一致。閉じた待ちの重なりを引いた run は前の task 1583（4643→1978 秒、2665 秒を除外）と後の task 1660（10760→1699 秒、9061 秒を除外）の2件。全体の中央値はどちらの区間も work と excl が同じだが、個々の値は異なる。
- **層**: change は fetch.task_at_claim（今の show --full の change/acceptance から claim 以後の task_edited を巻き戻す）。area は compute.areas_of の着地 commit の file と締切前の main の [areas]。複数 area の run は各行に重複して入り、area 行は足さない。未着地は unknown。重さは compute.terciles / bins の着地 commit の added+deleted の三分位で、前の選択40件から閾値を決め後にも固定して適用する。未着地/未取得は重さの別行で、三分位には入れない。これは完成後の変更量であって着手時の難しさの推定ではない。

## 件数・率・worker の時間（層ごと）

a/N と b/N は同じ分母・同じ差し戻し判定。参考は軽い指摘を含む。照合なし/V、探した名前/V は同じ初回 receipt。work と excl は取得件数/中央値（秒）。欠測列は summary / work / unfinished wait / waiting_deferred の件数。空の時間は — で、0秒とは区別する。各率の数値と時間の取得件数も [table.csv](docs-candidate-search/out/table.csv) にある。

重さの閾値: [150, 685] 行。

| 区間 | 層 | 値 | N | a/N | b/N | 参考 | 照合なし/V | 探した名前/V | 人へ | route 無し | 欠測等 | work n/中央値 | excl n/中央値 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| before | all | all | 40 | 14/40 | 10/40 | 16 | 0/40 | 1/40 | 0 | 0 | 0/0/0/0 | 40/889.5 | 40/889.5 |
| before | area | ci | 5 | 1/5 | 1/5 | 1 | 0/5 | 0/5 | 0 | 0 | 0/0/0/0 | 5/635 | 5/635 |
| before | area | config | 4 | 0/4 | 0/4 | 0 | 0/4 | 1/4 | 0 | 0 | 0/0/0/0 | 4/365.5 | 4/365.5 |
| before | area | docs | 34 | 14/34 | 10/34 | 15 | 0/34 | 1/34 | 0 | 0 | 0/0/0/0 | 34/948.5 | 34/948.5 |
| before | area | migrations | 1 | 1/1 | 0/1 | 1 | 0/1 | 0/1 | 0 | 0 | 0/0/0/0 | 1/2152 | 1/2152 |
| before | area | other | 2 | 0/2 | 0/2 | 0 | 0/2 | 0/2 | 0 | 0 | 0/0/0/0 | 2/440 | 2/440 |
| before | area | plugin | 10 | 5/10 | 3/10 | 6 | 0/10 | 0/10 | 0 | 0 | 0/0/0/0 | 10/1474.5 | 10/1474.5 |
| before | area | runtime | 23 | 12/23 | 10/23 | 14 | 0/23 | 1/23 | 0 | 0 | 0/0/0/0 | 23/1521 | 23/1521 |
| before | area | tests | 22 | 11/22 | 8/22 | 13 | 0/22 | 0/22 | 0 | 0 | 0/0/0/0 | 22/1601.5 | 22/1601.5 |
| before | change | config | 4 | 0/4 | 0/4 | 0 | 0/4 | 0/4 | 0 | 0 | 0/0/0/0 | 4/365.5 | 4/365.5 |
| before | change | docs | 9 | 2/9 | 0/9 | 2 | 0/9 | 0/9 | 0 | 0 | 0/0/0/0 | 9/376 | 9/376 |
| before | change | feature | 12 | 7/12 | 5/12 | 8 | 0/12 | 1/12 | 0 | 0 | 0/0/0/0 | 12/1908.5 | 12/1908.5 |
| before | change | fix | 6 | 4/6 | 4/6 | 4 | 0/6 | 0/6 | 0 | 0 | 0/0/0/0 | 6/1519 | 6/1519 |
| before | change | measure | 1 | 0/1 | 0/1 | 0 | 0/1 | 0/1 | 0 | 0 | 0/0/0/0 | 1/245 | 1/245 |
| before | change | refactor | 4 | 1/4 | 1/4 | 2 | 0/4 | 0/4 | 0 | 0 | 0/0/0/0 | 4/1106 | 4/1106 |
| before | change | test | 4 | 0/4 | 0/4 | 0 | 0/4 | 0/4 | 0 | 0 | 0/0/0/0 | 4/645 | 4/645 |
| before | complexity | 151-685 | 13 | 4/13 | 4/13 | 5 | 0/13 | 0/13 | 0 | 0 | 0/0/0/0 | 13/1015 | 13/1015 |
| before | complexity | <=150 | 14 | 2/14 | 0/14 | 2 | 0/14 | 1/14 | 0 | 0 | 0/0/0/0 | 14/431.5 | 14/431.5 |
| before | complexity | >685 | 13 | 8/13 | 6/13 | 9 | 0/13 | 0/13 | 0 | 0 | 0/0/0/0 | 13/1738 | 13/1738 |
| before | detection | D0 | 26 | 12/26 | 9/26 | 14 | 0/26 | 0/26 | 0 | 0 | 0/0/0/0 | 26/1215 | 26/1215 |
| before | detection | D3 | 2 | 1/2 | 1/2 | 1 | 0/2 | 0/2 | 0 | 0 | 0/0/0/0 | 2/2069.5 | 2/2069.5 |
| before | detection | D2 | 7 | 0/7 | 0/7 | 0 | 0/7 | 1/7 | 0 | 0 | 0/0/0/0 | 7/882 | 7/882 |
| before | detection | D1 | 5 | 1/5 | 0/5 | 1 | 0/5 | 0/5 | 0 | 0 | 0/0/0/0 | 5/275 | 5/275 |
| after | all | all | 40 | 14/40 | 7/40 | 15 | 0/40 | 20/40 | 0 | 0 | 0/0/0/0 | 40/1267.5 | 40/1267.5 |
| after | area | broker | 3 | 3/3 | 1/3 | 3 | 0/3 | 1/3 | 0 | 0 | 0/0/0/0 | 3/1647 | 3/1647 |
| after | area | ci | 7 | 2/7 | 1/7 | 2 | 0/7 | 5/7 | 0 | 0 | 0/0/0/0 | 7/1228 | 7/1228 |
| after | area | config | 1 | 0/1 | 0/1 | 0 | 0/1 | 1/1 | 0 | 0 | 0/0/0/0 | 1/646 | 1/646 |
| after | area | docs | 37 | 13/37 | 7/37 | 14 | 0/37 | 18/37 | 0 | 0 | 0/0/0/0 | 37/1264 | 37/1264 |
| after | area | migrations | 1 | 1/1 | 0/1 | 1 | 0/1 | 0/1 | 0 | 0 | 0/0/0/0 | 1/2157 | 1/2157 |
| after | area | other | 3 | 1/3 | 1/3 | 1 | 0/3 | 3/3 | 0 | 0 | 0/0/0/0 | 3/646 | 3/646 |
| after | area | plugin | 8 | 3/8 | 2/8 | 4 | 0/8 | 3/8 | 0 | 0 | 0/0/0/0 | 8/1246 | 8/1246 |
| after | area | runtime | 26 | 13/26 | 7/26 | 14 | 0/26 | 12/26 | 0 | 0 | 0/0/0/0 | 26/1363.5 | 26/1363.5 |
| after | area | tests | 27 | 13/27 | 7/27 | 14 | 0/27 | 12/27 | 0 | 0 | 0/0/0/0 | 27/1374 | 27/1374 |
| after | area | unknown | 1 | 1/1 | 0/1 | 1 | 0/1 | 1/1 | 0 | 0 | 0/0/0/0 | 1/2383 | 1/2383 |
| after | change | config | 2 | 0/2 | 0/2 | 0 | 0/2 | 2/2 | 0 | 0 | 0/0/0/0 | 2/884 | 2/884 |
| after | change | docs | 2 | 0/2 | 0/2 | 0 | 0/2 | 1/2 | 0 | 0 | 0/0/0/0 | 2/188.5 | 2/188.5 |
| after | change | feature | 10 | 4/10 | 2/10 | 5 | 0/10 | 4/10 | 0 | 0 | 0/0/0/0 | 10/1312 | 10/1312 |
| after | change | fix | 7 | 4/7 | 1/7 | 4 | 0/7 | 3/7 | 0 | 0 | 0/0/0/0 | 7/1085 | 7/1085 |
| after | change | measure | 4 | 0/4 | 0/4 | 0 | 0/4 | 2/4 | 0 | 0 | 0/0/0/0 | 4/1227.5 | 4/1227.5 |
| after | change | refactor | 3 | 2/3 | 2/3 | 2 | 0/3 | 2/3 | 0 | 0 | 0/0/0/0 | 3/2217 | 3/2217 |
| after | change | test | 6 | 0/6 | 0/6 | 0 | 0/6 | 3/6 | 0 | 0 | 0/0/0/0 | 6/1693.5 | 6/1693.5 |
| after | change | unknown | 6 | 4/6 | 2/6 | 4 | 0/6 | 3/6 | 0 | 0 | 0/0/0/0 | 6/1246 | 6/1246 |
| after | complexity | 151-685 | 19 | 4/19 | 1/19 | 5 | 0/19 | 9/19 | 0 | 0 | 0/0/0/0 | 19/1137 | 19/1137 |
| after | complexity | <=150 | 6 | 0/6 | 0/6 | 0 | 0/6 | 3/6 | 0 | 0 | 0/0/0/0 | 6/243 | 6/243 |
| after | complexity | >685 | 14 | 9/14 | 6/14 | 9 | 0/14 | 7/14 | 0 | 0 | 0/0/0/0 | 14/1664.5 | 14/1664.5 |
| after | complexity | 未着地 | 1 | 1/1 | 0/1 | 1 | 0/1 | 1/1 | 0 | 0 | 0/0/0/0 | 1/2383 | 1/2383 |
| after | detection | D4 | 31 | 10/31 | 5/31 | 11 | 0/31 | 12/31 | 0 | 0 | 0/0/0/0 | 31/1228 | 31/1228 |
| after | detection | D3 | 9 | 4/9 | 2/9 | 4 | 0/9 | 8/9 | 0 | 0 | 0/0/0/0 | 9/1653 | 9/1653 |

## 最初の1往復の時間（層ごと）

各セルは **取得件数 / 中央値 / 合計**（秒）。a は reason_codes の docs_drift の差し戻し全体、b は primary_code の部分集合。合計の取得件数は revise や再 review 単独の件数と異なり、両側が取れた run だけ。

| 区間 | 層 | 値 | a revise | b revise | a 再 review | b 再 review | a 和 | b 和 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| before | all | all | 14 / 140.449 / 11848.74 | 10 / 136.062 / 10143.65 | 11 / 130 / 1320 | 9 / 130 / 1106 | 11 / 226.943 / 3249.513 | 9 / 226.943 / 2440.782 |
| before | area | ci | 1 / 76.512 / 76.512 | 1 / 76.512 / 76.512 | 1 / 106 / 106 | 1 / 106 / 106 | 1 / 182.512 / 182.512 | 1 / 182.512 / 182.512 |
| before | area | config | 0 / — / — | 0 / — / — | 0 / — / — | 0 / — / — | 0 / — / — | 0 / — / — |
| before | area | docs | 14 / 140.449 / 11848.74 | 10 / 136.062 / 10143.65 | 11 / 130 / 1320 | 9 / 130 / 1106 | 11 / 226.943 / 3249.513 | 9 / 226.943 / 2440.782 |
| before | area | migrations | 1 / 454.013 / 454.013 | 0 / — / — | 1 / 141 / 141 | 0 / — / — | 1 / 595.013 / 595.013 | 0 / — / — |
| before | area | other | 0 / — / — | 0 / — / — | 0 / — / — | 0 / — / — | 0 / — / — | 0 / — / — |
| before | area | plugin | 5 / 167.449 / 1118.571 | 3 / 167.449 / 523.84 | 5 / 141 / 709 | 3 / 180 / 495 | 5 / 313.383 / 1827.571 | 3 / 313.383 / 1018.84 |
| before | area | runtime | 12 / 153.815 / 11647.57 | 10 / 136.062 / 10143.65 | 10 / 131 / 1247 | 9 / 130 / 1106 | 10 / 263.196 / 3035.795 | 9 / 226.943 / 2440.782 |
| before | area | tests | 11 / 167.449 / 11571.596 | 8 / 149.696 / 9926.958 | 9 / 132 / 1153 | 7 / 132 / 939 | 9 / 299.449 / 2865.821 | 7 / 299.449 / 2057.09 |
| before | change | config | 0 / — / — | 0 / — / — | 0 / — / — | 0 / — / — | 0 / — / — | 0 / — / — |
| before | change | docs | 2 / 100.585 / 201.17 | 0 / — / — | 1 / 73 / 73 | 0 / — / — | 1 / 213.718 / 213.718 | 0 / — / — |
| before | change | feature | 7 / 226.008 / 10893.807 | 5 / 167.449 / 9389.887 | 5 / 141 / 692 | 4 / 156 / 551 | 5 / 313.383 / 1727.032 | 4 / 306.416 / 1132.019 |
| before | change | fix | 4 / 136.062 / 677.251 | 4 / 136.062 / 677.251 | 4 / 112.5 / 449 | 4 / 112.5 / 449 | 4 / 214.062 / 1126.251 | 4 / 214.062 / 1126.251 |
| before | change | measure | 0 / — / — | 0 / — / — | 0 / — / — | 0 / — / — | 0 / — / — | 0 / — / — |
| before | change | refactor | 1 / 76.512 / 76.512 | 1 / 76.512 / 76.512 | 1 / 106 / 106 | 1 / 106 / 106 | 1 / 182.512 / 182.512 | 1 / 182.512 / 182.512 |
| before | change | test | 0 / — / — | 0 / — / — | 0 / — / — | 0 / — / — | 0 / — / — | 0 / — / — |
| before | complexity | 151-685 | 4 / 94.561 / 363.561 | 4 / 94.561 / 363.561 | 4 / 78 / 342 | 4 / 78 / 342 | 4 / 182.72 / 705.561 | 4 / 182.72 / 705.561 |
| before | complexity | <=150 | 2 / 100.585 / 201.17 | 0 / — / — | 1 / 73 / 73 | 0 / — / — | 1 / 213.718 / 213.718 | 0 / — / — |
| before | complexity | >685 | 8 / 298.439 / 11284.009 | 6 / 196.728 / 9780.089 | 6 / 152 / 905 | 5 / 163 / 764 | 6 / 359.696 / 2330.234 | 5 / 313.383 / 1735.221 |
| before | detection | D0 | 12 / 136.062 / 11337.153 | 9 / 131.943 / 9772.781 | 9 / 130 / 1084 | 8 / 118 / 943 | 9 / 226.943 / 2501.926 | 8 / 214.062 / 1906.913 |
| before | detection | D3 | 1 / 370.869 / 370.869 | 1 / 370.869 / 370.869 | 1 / 163 / 163 | 1 / 163 / 163 | 1 / 533.869 / 533.869 | 1 / 533.869 / 533.869 |
| before | detection | D2 | 0 / — / — | 0 / — / — | 0 / — / — | 0 / — / — | 0 / — / — | 0 / — / — |
| before | detection | D1 | 1 / 140.718 / 140.718 | 0 / — / — | 1 / 73 / 73 | 0 / — / — | 1 / 213.718 / 213.718 | 0 / — / — |
| after | all | all | 14 / 97.834 / 4125.452 | 7 / 88.065 / 608.34 | 13 / 150 / 2023 | 6 / 134 / 973 | 13 / 254.557 / 6121.576 | 6 / 272.913 / 1554.464 |
| after | area | broker | 3 / 136.557 / 483.553 | 1 / 136.557 / 136.557 | 3 / 157 / 433 | 1 / 118 / 118 | 3 / 254.557 / 916.553 | 1 / 254.557 / 254.557 |
| after | area | ci | 2 / 208.474 / 416.948 | 1 / 136.557 / 136.557 | 2 / 138 / 276 | 1 / 118 / 118 | 2 / 346.474 / 692.948 | 1 / 254.557 / 254.557 |
| after | area | config | 0 / — / — | 0 / — / — | 0 / — / — | 0 / — / — | 0 / — / — | 0 / — / — |
| after | area | docs | 13 / 92.34 / 1630.592 | 7 / 88.065 / 608.34 | 12 / 137.5 / 1799 | 6 / 134 / 973 | 12 / 239.081 / 3402.716 | 6 / 272.913 / 1554.464 |
| after | area | migrations | 1 / 280.391 / 280.391 | 0 / — / — | 1 / 158 / 158 | 0 / — / — | 1 / 438.391 / 438.391 | 0 / — / — |
| after | area | other | 1 / 26.876 / 26.876 | 1 / 26.876 / 26.876 | 0 / — / — | 0 / — / — | 0 / — / — | 0 / — / — |
| after | area | plugin | 3 / 136.557 / 666.049 | 2 / 112.311 / 224.622 | 3 / 118 / 427 | 2 / 116 / 232 | 3 / 254.557 / 1093.049 | 2 / 228.311 / 456.622 |
| after | area | runtime | 13 / 92.34 / 1630.592 | 7 / 88.065 / 608.34 | 12 / 137.5 / 1799 | 6 / 134 / 973 | 12 / 239.081 / 3402.716 | 6 / 272.913 / 1554.464 |
| after | area | tests | 13 / 92.34 / 1630.592 | 7 / 88.065 / 608.34 | 12 / 137.5 / 1799 | 6 / 134 / 973 | 12 / 239.081 / 3402.716 | 6 / 272.913 / 1554.464 |
| after | area | unknown | 1 / 2494.86 / 2494.86 | 0 / — / — | 1 / 224 / 224 | 0 / — / — | 1 / 2718.86 / 2718.86 | 0 / — / — |
| after | change | config | 0 / — / — | 0 / — / — | 0 / — / — | 0 / — / — | 0 / — / — | 0 / — / — |
| after | change | docs | 0 / — / — | 0 / — / — | 0 / — / — | 0 / — / — | 0 / — / — | 0 / — / — |
| after | change | feature | 4 / 122.298 / 551.864 | 2 / 84.072 / 168.144 | 3 / 150 / 399 | 1 / 150 / 150 | 3 / 291.268 / 923.988 | 1 / 291.268 / 291.268 |
| after | change | fix | 4 / 65.25 / 604.766 | 1 / 32.839 / 32.839 | 4 / 112.5 / 503 | 1 / 83 / 83 | 4 / 177.75 / 1107.766 | 1 / 115.839 / 115.839 |
| after | change | measure | 0 / — / — | 0 / — / — | 0 / — / — | 0 / — / — | 0 / — / — | 0 / — / — |
| after | change | refactor | 2 / 91.368 / 182.735 | 2 / 91.368 / 182.735 | 2 / 254 / 508 | 2 / 254 / 508 | 2 / 345.368 / 690.735 | 2 / 345.368 / 690.735 |
| after | change | test | 0 / — / — | 0 / — / — | 0 / — / — | 0 / — / — | 0 / — / — | 0 / — / — |
| after | change | unknown | 4 / 112.311 / 2786.087 | 2 / 112.311 / 224.622 | 4 / 137.5 / 613 | 2 / 116 / 232 | 4 / 239.081 / 3399.087 | 2 / 228.311 / 456.622 |
| after | complexity | 151-685 | 4 / 65.25 / 260.705 | 1 / 26.876 / 26.876 | 3 / 100 / 316 | 0 / — / — | 3 / 194.329 / 549.829 | 0 / — / — |
| after | complexity | <=150 | 0 / — / — | 0 / — / — | 0 / — / — | 0 / — / — | 0 / — / — | 0 / — / — |
| after | complexity | >685 | 9 / 107.192 / 1369.887 | 6 / 97.629 / 581.464 | 9 / 157 / 1483 | 6 / 134 / 973 | 9 / 291.268 / 2852.887 | 6 / 272.913 / 1554.464 |
| after | complexity | 未着地 | 1 / 2494.86 / 2494.86 | 0 / — / — | 1 / 224 / 224 | 0 / — / — | 1 / 2718.86 / 2718.86 | 0 / — / — |
| after | detection | D4 | 10 / 95.697 / 1355.517 | 5 / 88.065 / 425.605 | 9 / 118 / 1166 | 4 / 116 / 465 | 9 / 223.605 / 2494.641 | 4 / 228.311 / 863.729 |
| after | detection | D3 | 4 / 99.766 / 2769.935 | 2 / 91.368 / 182.735 | 4 / 227.5 / 857 | 2 / 254 / 508 | 4 / 345.368 / 3626.935 | 2 / 345.368 / 690.735 |

## 打ち切り・欠測・追加の往復

初回1往復の (i) 取り消し（取り消した依頼の件数）、(ii) 未送信、(iii) revise 未完了、(iv) 再 review 未完了/失敗、(v) revise 時刻/再 review duration 欠測を別に示す。率の分子は変えない。0件の ID 一覧は空（—）。追加往復の除外は first の異常も含む。詳細は [anomalies.csv](docs-candidate-search/out/anomalies.csv)。

| 区間 | 場合（first または run） | 件数 | run ID |
| --- | --- | --- | --- |
| before | cancelled | 0 | — |
| before | not_sent | 0 | — |
| before | revise_unfinished | 0 | — |
| before | rereview_unfinished_or_failed | 3 | 3d21d841-867f-4a87-873e-0e95cd5a83e9; 76c5dd97-fde3-4a96-8784-4f4e3ad22fb0; 56dfaeaa-897d-47c2-b6b8-6907f23c7f0d |
| before | revise_missing_time | 0 | — |
| before | rereview_missing_duration | 0 | — |
| before | human | 0 | — |
| before | route_absent | 0 | — |
| before | summary_missing | 0 | — |
| before | work_missing | 0 | — |
| before | wait_unfinished | 0 | — |
| before | waiting_deferred | 0 | — |
| after | cancelled | 0 | — |
| after | not_sent | 0 | — |
| after | revise_unfinished | 0 | — |
| after | rereview_unfinished_or_failed | 1 | d97216b8-eb3a-415f-8716-b087c588f086 |
| after | revise_missing_time | 0 | — |
| after | rereview_missing_duration | 0 | — |
| after | human | 0 | — |
| after | route_absent | 0 | — |
| after | summary_missing | 0 | — |
| after | work_missing | 0 | — |
| after | wait_unfinished | 0 | — |
| after | waiting_deferred | 0 | — |

再 review 未完了/失敗の4件は、次の started の同 attempt が C までに review_finished/review_failed を持たないもの。revise は残し、再 review と和は除いた。

| 区間 | 追加往復から除く場合 | 件数 | run ID |
| --- | --- | --- | --- |
| before | excluded | 3 | 3d21d841-867f-4a87-873e-0e95cd5a83e9; 76c5dd97-fde3-4a96-8784-4f4e3ad22fb0; 56dfaeaa-897d-47c2-b6b8-6907f23c7f0d |
| before | cancelled | 0 | — |
| before | not_sent | 0 | — |
| before | revise_unfinished | 0 | — |
| before | rereview_unfinished_or_failed | 0 | — |
| before | revise_missing_time | 0 | — |
| before | rereview_missing_duration | 0 | — |
| after | excluded | 4 | 8db1a6cd-f914-40d3-9709-893408d1b449; d97216b8-eb3a-415f-8716-b087c588f086; 1dc0d5cf-fe32-4579-ba47-616af532ec57; 068c08e3-eb24-4437-b01f-de7f6dd028c1 |
| after | cancelled | 0 | — |
| after | not_sent | 3 | 8db1a6cd-f914-40d3-9709-893408d1b449; 1dc0d5cf-fe32-4579-ba47-616af532ec57; 068c08e3-eb24-4437-b01f-de7f6dd028c1 |
| after | revise_unfinished | 0 | — |
| after | rereview_unfinished_or_failed | 0 | — |
| after | revise_missing_time | 0 | — |
| after | rereview_missing_duration | 0 | — |

対象 run ごとの最初の時間と追加往復（追加0回も表示）。b=1は主理由の部分集合。

| 区間 | task | run ID | b | revise | 再 review | 和 | 追加往復数 | 追加合計秒 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| before | 1339 | 161e4b24-906e-4711-8a84-a5566683e4d8 | 1 | 34.259 | 130 | 164.259 | 0 | 0 |
| before | 1549 | e080a3ed-ad7d-47c6-bc29-2c89b6b89cda | 1 | 76.512 | 106 | 182.512 | 0 | 0 |
| before | 1505 | 0de6cd72-bb17-45b8-815a-67df22a1101b | 0 | 454.013 | 141 | 595.013 | 1 | 1047.155 |
| before | 1189 | 9874facf-b0e8-47dd-9d93-5d4052d079a2 | 1 | 131.943 | 95 | 226.943 | 0 | 0 |
| before | 1507 | cc801ef6-5ffe-4fc8-8644-f30ba3626b24 | 1 | 167.449 | 132 | 299.449 | 1 | 368.774 |
| before | 1509 | e6cc9e1f-9055-4d2c-beb5-df48acb81614 | 1 | 130.383 | 183 | 313.383 | 1 | 323.654 |
| before | 1572 | 31ae076a-affc-4cf0-b2e7-5e7eaea69ded | 1 | 57.179 | 56 | 113.179 | 0 | 0 |
| before | 1437 | 3d21d841-867f-4a87-873e-0e95cd5a83e9 | 1 | 8808.868 | — | — | — | — |
| before | 1591 | 3c5a55c1-72a1-4f78-adda-9e13bc3937ed | 1 | 226.008 | 180 | 406.008 | 0 | 0 |
| before | 1354 | 76c5dd97-fde3-4a96-8784-4f4e3ad22fb0 | 0 | 1049.907 | — | — | — | — |
| before | 1649 | e474159d-70bb-4e7c-af2f-1de46aac5b38 | 1 | 140.18 | 61 | 201.18 | 0 | 0 |
| before | 1487 | 56dfaeaa-897d-47c2-b6b8-6907f23c7f0d | 0 | 60.452 | — | — | — | — |
| before | 1468 | 0cd16a3b-520c-4ce0-a502-b4e3293c236b | 0 | 140.718 | 73 | 213.718 | 0 | 0 |
| before | 1596 | 6ad77ab1-2138-40c6-96a2-a406d5947bc4 | 1 | 370.869 | 163 | 533.869 | 1 | 453.324 |
| after | 1712 | 44ad0456-3946-4fab-a9db-27c09385314b | 1 | 75.543 | 231 | 306.543 | 0 | 0 |
| after | 1713 | 853c5ae1-26d2-40da-af5d-fc158c669810 | 1 | 107.192 | 277 | 384.192 | 0 | 0 |
| after | 1683 | ae1fdf72-6ec2-462f-8eaa-e80583371076 | 0 | 92.34 | 125 | 217.34 | 0 | 0 |
| after | 838 | 8db1a6cd-f914-40d3-9709-893408d1b449 | 0 | 2494.86 | 224 | 2718.86 | — | — |
| after | 839 | bbd11ba3-0056-4621-96a4-fac3d4058ca6 | 1 | 88.065 | 114 | 202.065 | 1 | 253.122 |
| after | 1594 | 62a47b7c-22e8-4840-9342-0fffdf1352cf | 0 | 38.16 | 100 | 138.16 | 0 | 0 |
| after | 840 | f3de8eb0-a0f2-4231-a8a9-7e78ae407a2c | 0 | 66.605 | 157 | 223.605 | 0 | 0 |
| after | 1451 | d97216b8-eb3a-415f-8716-b087c588f086 | 1 | 26.876 | — | — | — | — |
| after | 1609 | a256a156-f5d4-44b4-8baf-4ca064aa9b4d | 1 | 32.839 | 83 | 115.839 | 0 | 0 |
| after | 1632 | 1dc0d5cf-fe32-4579-ba47-616af532ec57 | 0 | 280.391 | 158 | 438.391 | — | — |
| after | 1156 | 5746a32e-64dd-4d3e-8352-93e67af75358 | 0 | 103.329 | 91 | 194.329 | 0 | 0 |
| after | 838 | d6790cac-3649-4b76-8600-6c6d902e44e1 | 1 | 136.557 | 118 | 254.557 | 0 | 0 |
| after | 1633 | 857eea61-452f-489f-b956-b9edf208e5c5 | 1 | 141.268 | 150 | 291.268 | 0 | 0 |
| after | 1481 | 068c08e3-eb24-4437-b01f-de7f6dd028c1 | 0 | 441.427 | 195 | 636.427 | — | — |

## 検出条件と並行した変更

REVIEW_DOCS_CHECK の定数は、対象を含む build 70個で同じ SHA256（3247d97974206c0d9c189b9e050aedddf478b9e6ca919f3d99fe09da145b64da、const 宣言全体）だった。一方、.dagq/review-agents/ は以下の変更があるので同一条件と呼べない。review_started.payload.subagents.commit が示す、実際に定義を読んだ repository の commit から全定義 directory の tree を取って D0〜D4 に分け、上の detection 行に全指標を並べた。実際に path に選ばれた agent 名と digest は [detection.csv](docs-candidate-search/out/detection.csv)。選択対象の差分によって agent の組は違う。定義が同じでも、対象の仕事内容や review の provider が同じという意味ではない。

| 条件 | 定義を変えた commit | 着地 UTC | tree | 内容 |
| --- | --- | --- | --- | --- |
| D0 | 9384cb2599062a51742dd5dd8cbf719601924872 | 2026-10-03T04:12:16.449Z | 03e8845aba911957a464cd5365db5a6e706625b5 | config: この repository の review の subagent の定義（検査項目と開発文書への参照）を置き、dagq.toml で path ごとに有効にする |
| D1 | 00493c308584c062cdc58c52ae346d690140eaa3 | 2026-10-04T10:45:19.400Z | 9549e5225f899e8da1b15350a6eec51389c01571 | test-rules の review subagent に、判断と境界の test と shell_path の検査項目を足す |
| D2 | 64ef873dfc94caeb8aa931b80f55aac894ed20fc | 2026-10-04T11:18:10.851Z | 76dd9a0d152fd81db67cde84f354eaf89a00ab70 | config: レイヤーとコンテキストの境界を確かめる review の subagent の定義（検査項目と設計文書への参照だけ）を .dagq/review-agents/ に置き、dagq.toml で src と crates の差分に有効にする |
| D3 | adda99a602531acae52738f39601f418a423fb84 | 2026-10-04T13:03:45.267Z | 3562351ba77b91e25b42658676f77f49ca64b1f4 | feature: worker の prompt の文書の照合（DOCS_CHECK）に、変えた名前で文書の候補を探して summary に探した名前を書く句を足し、この repository の探す範囲を documents.md に、確かめ方を design-consistency の review の subagent に書き、ADR で ADR-t1428-1 決定 4 を amends する |
| D4 | cfe2fd07eed13953fe3cfc79c8fc569e7c6c568e | 2026-10-04T16:58:44.985Z | 2e8f2dd397d8e0e0a2af8526eda301255685deeb | config: scripts/slow-tests.sh に test binary ごとの本数と時間の合計を足し、throughput-review の日次の見直しで本番の関門の dagq::it の本数と合計を前の 7 日と並べ、test-rules の review に it の時間の関門の検査項目を足す |

前の40件も D0=26、D1=5、D2=7、D3=2 に分かれる。1688を含まない build で claim された最後の2件は review の時点では1688の定義 D3 を読んでいた（境界を claim と review の両方で記録する理由）。後は D3=9、D4=31。task 1688 は worker の探索と design-consistency の確かめ方の強化を同時に着地した。後の値はその和であり、探索だけを受けた対照群が無いので分けられない。D3 の前後2対9件だけでも対象と標本が小さく、worker の探索の効果を断定できない。

以下は最も早い選択 claim から C までの git log --first-parent HEAD で、src/application/prompt.rs、src/application/review.rs、src/application/supervise/jobs.rs、src/domain/review_subagents.rs、src/domain/review_reason.rs、.dagq/review-agents/、dagq.toml に触る着地を列挙したもの。これは広く候補を拾う一覧で、復旧・planner の変更だけの行も含む。元の commit/時刻/event は [concurrent-changes.csv](docs-candidate-search/out/concurrent-changes.csv)。

| commit | 着地 UTC | task | 変更 |
| --- | --- | --- | --- |
| b4f75bac | 2026-10-05T08:07:38.531Z | 1618 | application の SystemTime::now（supervise/jobs.rs:220・headless_session.rs:531）を注入した Clock に替える |
| 832fc43a | 2026-10-05T05:40:04.129Z | 1540 | runtime: keep_draft で残した draft に再検討の時刻を付けられるようにし、時刻を過ぎたら runtime の planner の対象に戻して前回の判断を初期 prompt に載せる（ADR を書く） |
| f78d2dff | 2026-10-05T04:03:27.194Z | 1641 | runtime: goal にラベル（tag）を足し、語彙を dagq.toml に置いて外のラベルを拒み、dagq goal list を優先度順に並べて --tag で絞れるようにする（ADR-t1639-1） |
| 5d22997f | 2026-10-05T01:12:39.253Z | 1633 | runtime: 復旧 job の入力に今の固定バイナリの build 識別子と run の後の入れ替え（update_installed・supervisor_handed_off）を載せ、入れ替えで直る failed の run を人に聞かずに retry できるようにする |
| 051f1b65 | 2026-10-05T00:59:53.288Z | 840 | runtime: broker の package backend（package.install）を設定した少数のコマンドだけで実行する |
| 055ab721 | 2026-10-05T00:20:49.455Z | 1632 | runtime: 固定バイナリが依存先の着地を含むことを要する task を登録で宣言でき、supervisor が自分の build がそれを含むまで claim しない |
| 6f625dec | 2026-10-04T23:25:31.604Z | 838 | runtime: [broker] mode = required で worker と resume の組み込みの道具を拒んで broker の道具に限り、broker が使えないときは claim せず inbox に知らせて直接の実行に戻らない |
| af5b8d10 | 2026-10-04T23:06:16.126Z | 1640 | runtime: goal に優先度を足し、個別の指定の無い task の優先度を goal から継いで candidates・graph・supervisor の claim 順に効かせ、移行で既存の task の効く優先度を変えない（ADR-t1639-1） |
| 47c5fe83 | 2026-10-04T22:35:39.548Z | 1386 | show に task とその run の ask を並べる asks 欄を足し、recommendation と confidence を載せる（ADR-t451-1 決定 1 の表示） |
| cfe2fd07 | 2026-10-04T16:58:44.985Z | 1708 | config: scripts/slow-tests.sh に test binary ごとの本数と時間の合計を足し、throughput-review の日次の見直しで本番の関門の dagq::it の本数と合計を前の 7 日と並べ、test-rules の review に it の時間の関門の検査項目を足す |
| e70840a9 | 2026-10-04T15:06:48.486Z | 1712 | refactor: review の subagent の選択・結果の集約・判断の重さによる振り分けと、goal review の数え方・answer の対応・候補の選び方を副作用のない関数の unit test に移し、review_subagents と goal_review の群を境界の代表に絞る |
| d751a0c9 | 2026-10-04T14:31:09.494Z | 1437 | runtime: 対話の worker の run を起こさないようにし、対話の run だけが使う画面・ダイアログ・/exit・打鍵の処理と、それだけを確かめる integration test を消す |
| adda99a6 | 2026-10-04T13:03:45.267Z | 1688 | feature: worker の prompt の文書の照合（DOCS_CHECK）に、変えた名前で文書の候補を探して summary に探した名前を書く句を足し、この repository の探す範囲を documents.md に、確かめ方を design-consistency の review の subagent に書き、ADR で ADR-t1428-1 決定 4 を amends する |
| d8e5e25c | 2026-10-04T12:34:11.385Z | 1615 | application::prompt から crate::throughput_review::review_prompt への参照と、infrastructure::adapters の crate::throughput_review::ACCESS への参照を無くす |
| 64ef873d | 2026-10-04T11:18:10.851Z | 1547 | config: レイヤーとコンテキストの境界を確かめる review の subagent の定義（検査項目と設計文書への参照だけ）を .dagq/review-agents/ に置き、dagq.toml で src と crates の差分に有効にする |
| 00493c30 | 2026-10-04T10:45:19.400Z | 1608 | test-rules の review subagent に、判断と境界の test と shell_path の検査項目を足す |
| dc93fcfa | 2026-10-04T07:16:01.852Z | 1571 | runtime: goal review・run の review・復旧 job・runtime の 4 種類の planner の prompt に節ごとと全体の上限を付け、決まった順で選び、省いた件数と読む方法を書き、prompt の byte 数を event に記録する（ADR-t1566-1） |
| 5f4e4cd4 | 2026-10-04T05:57:06.625Z | 1508 | runtime: receipt の follow_ups に acceptance との関係の提案（該当の項目・必須か範囲外か不明か・根拠）を持たせ、worker の prompt に書き方を、runtime の planner の draft の prompt に所属の判断の記録の手順を足す（ADR-t1504-1・ADR-t1504-2） |
| f7619e64 | 2026-10-04T05:27:09.035Z | 1506 | runtime: submit が所属の判断の無い follow_up の draft を拒み、lint が同じ欠けを出し、plan review の資料と prompt に planner の対応づけを起点に検査し疑わしいものは周辺の証拠も読む指示を足す（ADR-t1504-2） |
| 45913133 | 2026-10-04T05:00:30.888Z | 1507 | runtime: 未判定か acceptance の変更後に確かめ直していない follow-up のある goal を goal review と goal close が achieved で閉じず、範囲外に分類した follow-up は閉じるのを妨げず、判定と close の間の並行・再起動・引き継ぎでも条件を検査し直す（ADR-t1504-2） |
| cd4d7def | 2026-10-04T04:09:25.966Z | 1408 | config: 固定バイナリが task 1405・1406 を含んだ後に、この repository の dagq.toml で非対話の session wrapper を background に切り替える |
| 4855bc84 | 2026-10-04T03:49:16.544Z | 1505 | runtime: follow_up の draft の所属の判断（元 goal に必須・範囲外で別 goal・未判定）を必須の欄と判定時の acceptance の版つきで記録する CLI と行を足し、出どころ（source goal）と深さを所属の変更で消さず、set-goal で ADR-t808-1 の人の adopt を迂回できないようにする（ADR-t1504-2） |

特に cd4d7def の background worker への切替、dc93fcfa の review 資料の上限、d751a0c9 の対話経路の廃止、00493c30/64ef873d/cfe2fd07 の subagent 項目の変更は費用・検出の比較に混ざる。e70840a9 は判断の unit test への移動で、定義の変更とは分けて扱った。後の選択の最後の review は01:36 UTCだが、後続の往復は C まで見るため、それ以後の着地も候補一覧に残した。

過去の [review-sendback-reasons](review-sendback-reasons.md) の docs_out_of_sync は手で分類した主8/付いた9件で、現在の docs_drift と検出条件・分母・期間が違う。今回の比較に合算せず、見落としの型の以前の参考値としてのみ引く。0/40の照合記録なしは、名前で候補を探せた証拠ではない（summary の path の有無という弱い判定）。探索句は1/40→20/40だが、違う書き方の見落としもある。work 中央値の+378秒は、実際の探索に費やした秒の上限を直接証明せず、探索以外も入る費用の保守的な見立てである。理由全体の35%が同じ、主理由だけが下がったことから、この標本では改善の根拠を得なかった。

## 選んだ run の一覧

全80 run の ID、claim の build/時刻、review_finished/validation_finished の event ID は runs.csv にあり、以下にも ID を載せる。events の選択一覧は [selected-events.csv](docs-candidate-search/out/selected-events.csv)（ID・kind・時刻・attemptのみ、summaryや理由の原文を公開しない）。

| 区間 | task | run ID | claim UTC | build |
| --- | --- | --- | --- | --- |
| before | 1339 | 161e4b24-906e-4711-8a84-a5566683e4d8 | 2026-10-04T01:01:20.074Z | ee1399416b9e3a65e3b5a7b67fce1302e93965fd |
| before | 1467 | ec42db79-7815-44ae-979a-6c3a6adb9f3e | 2026-10-04T01:16:53.926Z | 3c4f15deab22e3afddce0b45e8eb3711c12db3db |
| before | 1549 | e080a3ed-ad7d-47c6-bc29-2c89b6b89cda | 2026-10-04T01:53:48.323Z | f02e0f9ae02fa95a54003b732929f6b8c7c23762 |
| before | 1505 | 0de6cd72-bb17-45b8-815a-67df22a1101b | 2026-10-04T02:30:17.609Z | 8ab60f61aab27e0416664e4a6b3c701dbfdec01f |
| before | 1583 | 6357a771-18de-44db-a425-e73f6efbaead | 2026-10-04T02:33:17.950Z | 8ab60f61aab27e0416664e4a6b3c701dbfdec01f |
| before | 1189 | 9874facf-b0e8-47dd-9d93-5d4052d079a2 | 2026-10-04T02:38:33.078Z | 076bbb727da3fcbd1c883d8e986c8599eee1408d |
| before | 1639 | 719a11ef-7343-4e3e-9641-f510e9d4e03f | 2026-10-04T03:19:31.387Z | 076bbb727da3fcbd1c883d8e986c8599eee1408d |
| before | 1408 | 779a05bb-8929-4ca6-88d0-476412309ef3 | 2026-10-04T03:49:32.319Z | 076bbb727da3fcbd1c883d8e986c8599eee1408d |
| before | 1507 | cc801ef6-5ffe-4fc8-8644-f30ba3626b24 | 2026-10-04T04:09:32.183Z | 076bbb727da3fcbd1c883d8e986c8599eee1408d |
| before | 1506 | 280d9606-8784-412a-b886-91c8a18abf84 | 2026-10-04T04:26:00.258Z | 076bbb727da3fcbd1c883d8e986c8599eee1408d |
| before | 1658 | 3e3e6758-831f-4bbf-91a0-b301f2320489 | 2026-10-04T04:47:50.500Z | 076bbb727da3fcbd1c883d8e986c8599eee1408d |
| before | 1508 | 04500685-3fa3-405c-a5ec-092d09f5058f | 2026-10-04T05:01:07.724Z | 076bbb727da3fcbd1c883d8e986c8599eee1408d |
| before | 1509 | e6cc9e1f-9055-4d2c-beb5-df48acb81614 | 2026-10-04T05:12:22.021Z | 076bbb727da3fcbd1c883d8e986c8599eee1408d |
| before | 1571 | 11752111-14af-46ce-b530-96cc9d9b5f3e | 2026-10-04T05:27:22.643Z | 076bbb727da3fcbd1c883d8e986c8599eee1408d |
| before | 1572 | 31ae076a-affc-4cf0-b2e7-5e7eaea69ded | 2026-10-04T06:50:15.946Z | f7619e64273f220dc0462e3b8312eb013a9ea8f6 |
| before | 1437 | 3d21d841-867f-4a87-873e-0e95cd5a83e9 | 2026-10-04T06:53:16.510Z | f7619e64273f220dc0462e3b8312eb013a9ea8f6 |
| before | 1591 | 3c5a55c1-72a1-4f78-adda-9e13bc3937ed | 2026-10-04T07:16:53.029Z | 1035491baa5935f05c46691d77a8424fae8bb049 |
| before | 1548 | eebd09ed-5007-4a60-a53b-5470758bfc34 | 2026-10-04T07:38:10.689Z | dc93fcfa3a4ddc4451d2d77832d4c1386d8a2020 |
| before | 1631 | 1c707f55-f5b3-4a7c-a720-7f17f0ef1db7 | 2026-10-04T07:59:58.380Z | 95181d50f621ce122bbe9e46bf6bc187c87dfeab |
| before | 1354 | 76c5dd97-fde3-4a96-8784-4f4e3ad22fb0 | 2026-10-04T08:20:12.197Z | a35286332287ac0c1f48f9cc57e1580cdb14daef |
| before | 1649 | e474159d-70bb-4e7c-af2f-1de46aac5b38 | 2026-10-04T08:36:45.347Z | 6d4b1fa7769a040d2cb749c66931344d55224d7a |
| before | 1485 | 6bf5c1a8-3a86-4377-9681-1753512dc040 | 2026-10-04T09:23:43.654Z | 6c0d72d3edcafef023a2a9b260e92efe40b12c73 |
| before | 1570 | a1b7032b-e911-42b9-950e-e5d6aa749a93 | 2026-10-04T09:37:28.910Z | 4f3a3d192e5e36f8b9e5afd60225fc5bf996feaa |
| before | 1487 | 56dfaeaa-897d-47c2-b6b8-6907f23c7f0d | 2026-10-04T10:19:53.548Z | 3686c60974e9f576f57f50d5200d652e25e46f03 |
| before | 1381 | 895ad40d-e308-4cb3-b908-be8bdddf4844 | 2026-10-04T10:31:13.759Z | d080d0174d289505b2ab2be894caaa0809a0acff |
| before | 1607 | 097be999-9251-4f76-82fe-78371d489a73 | 2026-10-04T10:38:08.141Z | 12286976b89c433516b41bd3d9248160957b4e1f |
| before | 1608 | 8b92d5e3-af28-4c7d-a4e5-28b2b78958df | 2026-10-04T10:43:45.219Z | 12286976b89c433516b41bd3d9248160957b4e1f |
| before | 1687 | 9f13bdd0-227e-41be-b8b3-9450e08aa31f | 2026-10-04T10:46:46.422Z | 12286976b89c433516b41bd3d9248160957b4e1f |
| before | 1486 | d1f5eca1-bfa7-4f8a-8c13-917aca77aefd | 2026-10-04T10:49:50.424Z | 12286976b89c433516b41bd3d9248160957b4e1f |
| before | 1468 | 0cd16a3b-520c-4ce0-a502-b4e3293c236b | 2026-10-04T11:08:35.064Z | 12286976b89c433516b41bd3d9248160957b4e1f |
| before | 1547 | 95194a89-4306-4d5c-8e61-6eada4e9c0fe | 2026-10-04T11:12:10.137Z | e66bc70529db2161dda24d3fc5e967ac16a20253 |
| before | 1510 | c9db74e7-7802-4f62-bd01-e9ff853dec21 | 2026-10-04T11:18:14.687Z | e66bc70529db2161dda24d3fc5e967ac16a20253 |
| before | 1584 | b25072c7-3d35-4e9c-a3fa-87b8b5208f6f | 2026-10-04T11:33:25.138Z | e66bc70529db2161dda24d3fc5e967ac16a20253 |
| before | 1627 | b57b4f8c-9142-432c-9438-c1430de2936d | 2026-10-04T11:36:25.280Z | e66bc70529db2161dda24d3fc5e967ac16a20253 |
| before | 1615 | e2445fdc-63fe-4151-bdc6-4d5ecf73d863 | 2026-10-04T12:00:47.055Z | e66bc70529db2161dda24d3fc5e967ac16a20253 |
| before | 1662 | 2471cbba-42b0-4419-9055-a4d80bd42600 | 2026-10-04T12:15:45.031Z | e66bc70529db2161dda24d3fc5e967ac16a20253 |
| before | 1688 | 43ca40ac-f40a-4c91-8048-b4630765c2e2 | 2026-10-04T12:34:52.639Z | 4723af34fbf6deab8e533ffc56aa5f90766ae894 |
| before | 1706 | 39eb9c87-c07b-41d2-947b-11fd37083707 | 2026-10-04T12:38:22.925Z | 4723af34fbf6deab8e533ffc56aa5f90766ae894 |
| before | 1596 | 6ad77ab1-2138-40c6-96a2-a406d5947bc4 | 2026-10-04T12:44:29.353Z | d8e5e25cc3fc3ddc317b4ce054cbd0e051f932ea |
| before | 1636 | 49d87965-e2f6-4fc4-8cb7-b8d96414ae80 | 2026-10-04T13:05:16.478Z | d8e5e25cc3fc3ddc317b4ce054cbd0e051f932ea |
| after | 1709 | 278d3f15-ff01-425e-8af0-33b628edcaf9 | 2026-10-04T13:19:17.469Z | adda99a602531acae52738f39601f418a423fb84 |
| after | 1712 | 44ad0456-3946-4fab-a9db-27c09385314b | 2026-10-04T14:01:44.550Z | 8646ac612dffa28e2040e587b1e03ea3560a4f0a |
| after | 1713 | 853c5ae1-26d2-40da-af5d-fc158c669810 | 2026-10-04T14:17:48.127Z | 8646ac612dffa28e2040e587b1e03ea3560a4f0a |
| after | 1683 | ae1fdf72-6ec2-462f-8eaa-e80583371076 | 2026-10-04T14:31:26.468Z | 6e53bc4dc8643fc81c0ab2091b368c6c7f176d2e |
| after | 1707 | f2ff7df5-4fe1-41e7-8de7-afe6b8637ac7 | 2026-10-04T15:07:29.005Z | d751a0c93304aece21c4de99fe41cc0bb3aac76b |
| after | 1512 | 542e7443-f137-453b-8966-1bf7ef3d547c | 2026-10-04T15:22:25.598Z | e70840a90ad29812c855146c54be13f18649b173 |
| after | 838 | 8db1a6cd-f914-40d3-9709-893408d1b449 | 2026-10-04T15:44:35.956Z | 60972db31b70c2b1e607185f6cb4555957d28b64 |
| after | 1711 | dd5e9347-5c16-4aff-a749-d94f1be503a8 | 2026-10-04T16:03:52.410Z | 60972db31b70c2b1e607185f6cb4555957d28b64 |
| after | 1439 | 4383f305-aa23-40a4-bb0f-b4ef177cb158 | 2026-10-04T16:16:35.340Z | 04d8bb4e527a81a749fb0dcf0efe02215d7bdb4c |
| after | 1708 | eff71627-6434-4e5f-a4c9-4da107644012 | 2026-10-04T16:46:25.982Z | 04d8bb4e527a81a749fb0dcf0efe02215d7bdb4c |
| after | 1109 | 5f34c076-84d6-4e90-8457-1a33d44bfc1a | 2026-10-04T16:59:12.043Z | 824619bff7cc9f6bc467a079471de9838fecb66b |
| after | 839 | bbd11ba3-0056-4621-96a4-fac3d4058ca6 | 2026-10-04T17:24:01.617Z | 824619bff7cc9f6bc467a079471de9838fecb66b |
| after | 1594 | 62a47b7c-22e8-4840-9342-0fffdf1352cf | 2026-10-04T17:35:16.339Z | 824619bff7cc9f6bc467a079471de9838fecb66b |
| after | 1634 | b8869c02-7458-4f2e-855b-0b703b598d5e | 2026-10-04T17:45:34.254Z | 824619bff7cc9f6bc467a079471de9838fecb66b |
| after | 840 | f3de8eb0-a0f2-4231-a8a9-7e78ae407a2c | 2026-10-04T18:16:28.113Z | 3e74b981ce668a934caab639f5143276691efc30 |
| after | 1451 | d97216b8-eb3a-415f-8716-b087c588f086 | 2026-10-04T18:25:44.046Z | 3e74b981ce668a934caab639f5143276691efc30 |
| after | 1590 | c9c14221-6127-4713-bf1e-447c83a29823 | 2026-10-04T18:56:00.647Z | ed2a9c21e79e7dbc4efe44ec93773b7fe9a4c8ab |
| after | 1592 | 45da0904-5054-4f78-a685-8afd374f386e | 2026-10-04T18:59:01.522Z | ed2a9c21e79e7dbc4efe44ec93773b7fe9a4c8ab |
| after | 1609 | a256a156-f5d4-44b4-8baf-4ca064aa9b4d | 2026-10-04T19:03:58.351Z | ed2a9c21e79e7dbc4efe44ec93773b7fe9a4c8ab |
| after | 1629 | 64a3dfee-3dd3-4df9-9a96-2117817b76ec | 2026-10-04T19:19:16.726Z | 6cb5e894fcc5beedf3673c3cb95291f7ad621e47 |
| after | 1632 | 1dc0d5cf-fe32-4579-ba47-616af532ec57 | 2026-10-04T19:34:02.158Z | 6cb5e894fcc5beedf3673c3cb95291f7ad621e47 |
| after | 1642 | 5d5b2b28-90a5-4c69-bf5e-622650d717e7 | 2026-10-04T20:07:56.104Z | 0df576f25d12e1b5fc1b789cdde93e47756b9e9e |
| after | 1646 | 4bc7f1b4-c735-4c29-9551-541ba6022c86 | 2026-10-04T20:12:51.209Z | 0df576f25d12e1b5fc1b789cdde93e47756b9e9e |
| after | 1660 | b826f03c-b395-4222-ae6e-367909c6e270 | 2026-10-04T20:26:54.678Z | 0df576f25d12e1b5fc1b789cdde93e47756b9e9e |
| after | 688 | 2837a347-c25e-4eb2-8401-1d398fc2fe86 | 2026-10-04T20:33:50.902Z | 7738410640251ecd1524de863a91406a25df4a90 |
| after | 747 | 05b78767-59d9-4379-933c-23e6316db145 | 2026-10-04T20:36:51.535Z | 7738410640251ecd1524de863a91406a25df4a90 |
| after | 1156 | 5746a32e-64dd-4d3e-8352-93e67af75358 | 2026-10-04T20:39:52.924Z | 7738410640251ecd1524de863a91406a25df4a90 |
| after | 1238 | f4c6e714-798f-4b98-8e79-c9fc271e5869 | 2026-10-04T20:46:27.405Z | 7738410640251ecd1524de863a91406a25df4a90 |
| after | 1311 | b11084f7-fb68-46cf-951c-36ca6cd7f42f | 2026-10-04T20:49:28.474Z | 7738410640251ecd1524de863a91406a25df4a90 |
| after | 1312 | 115d7cf9-e41a-4a81-b031-4ecb1f092481 | 2026-10-04T21:02:04.673Z | 7738410640251ecd1524de863a91406a25df4a90 |
| after | 1321 | b373d37b-7175-4e7f-a3bf-ccfaf7049def | 2026-10-04T21:12:00.262Z | 06cf5074493cadc5c74d6908c71a8c22ce340c95 |
| after | 1352 | 542fa1a4-e38b-4205-85a7-a1199be31b94 | 2026-10-04T21:30:21.595Z | 1a43a77811b0ed141c9f6f742e6148e9f74338ff |
| after | 1359 | 8578ff75-632a-4d9d-9013-0a7e76b5648f | 2026-10-04T21:44:03.775Z | 1a43a77811b0ed141c9f6f742e6148e9f74338ff |
| after | 1386 | 9207fcfd-e0f3-43cf-a572-2aa8fe5f366d | 2026-10-04T22:08:43.670Z | 445f23e6c8714e9e24c08fcce1f4f2cbefdfcb08 |
| after | 1480 | 64b1a4f1-92c9-4eb7-9a1c-392d2bbd4fb1 | 2026-10-04T22:21:02.548Z | 464857c5ba060704e04a1969a8aa0f9d4b7998e2 |
| after | 838 | d6790cac-3649-4b76-8600-6c6d902e44e1 | 2026-10-04T22:36:20.099Z | 464857c5ba060704e04a1969a8aa0f9d4b7998e2 |
| after | 1633 | 857eea61-452f-489f-b956-b9edf208e5c5 | 2026-10-05T00:10:00.071Z | eabfabbbac2f46167c1d343f72890171566dc6f5 |
| after | 1315 | 371aa87a-d085-41b6-8827-ec4503307ae7 | 2026-10-05T00:21:22.959Z | 30e6a7b77ff196cb5e0e80e657cc66f616dcf235 |
| after | 1481 | 068c08e3-eb24-4437-b01f-de7f6dd028c1 | 2026-10-05T00:50:50.396Z | 055ab7219aedc8b0e958b974a4ee5bddee792beb |
| after | 1794 | 1db376a4-489a-478d-a932-969346224528 | 2026-10-05T01:00:12.054Z | 055ab7219aedc8b0e958b974a4ee5bddee792beb |

## 再現のコマンドと取得件数

Python 標準ライブラリのみ。固定バイナリを PATH に置き repository の worktree で実行する。queue は queue service 経由の読み取り CLI のみで、DB、locate、--db、接続先を変える env は使わない。acceptance-check/fetch.py は変更していない。compute.py は初回 review 前の receipt 取得を validation_receipt_before() として抽出し、mapping() とこの測定が共有する。既存の出力は変更していない。新しい fetch.py は既存 fetch.main() にページ記録の wrapper を付けるだけ。この測定の compute.py は既存の関数を import し、不足する差し戻し/summary/往復の指標と選択条件だけを追加した。history.py は git のみ、render.py は出力 CSV のみを読む。#1444 の文書と script は変更していない。

```sh
C=2026-10-05T09:05:36.831Z
SNAP="$TMPDIR/candidate-snapshot"
PYTHONDONTWRITEBYTECODE=1 python3 docs/plans/docs-candidate-search/fetch.py \
  --since 2026-10-03T04:12:16.449Z --until "$C" --cutoff "$C" \
  --stats-since 2026-10-01T00:00:00Z \
  --watch-task 1429 --watch-task 1460 --watch-task 1688 --out "$SNAP"
PYTHONDONTWRITEBYTECODE=1 python3 docs/plans/docs-candidate-search/compute.py "$SNAP" --cutoff "$C"
PYTHONDONTWRITEBYTECODE=1 python3 docs/plans/docs-candidate-search/history.py "$SNAP"
python3 docs/plans/docs-candidate-search/render.py
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s docs/plans/docs-candidate-search -p 'test_*.py'
```

既存 fetch は events --full --limit 1000 --after 0 から、非空ページの最大 ID を次の --after に渡し、空ページまで全件を読む。初期の review_finished/review_failed を探し、候補 run とその task の全 kind、境界 task、supervisor/update の event を取得し、ID で重複を除く。task_at_claim の task_edited は C 後も読むが、claim 時点への巻き戻しだけに使う。取得時点の reference は stats・show（層の復元）・marks・status を含む。大きな原文 snapshot は TMPDIR にのみ保存し commit しない（acceptance-check の公開制約に従う）。再取得時の show は後の編集を巻き戻す。C 時点で進行中だった run の stats/work が後から利用可能になった場合は結果の欠測が変わりうるが、今回80件は全て work が取得できた。

events の CLI 呼出し（空ページを含む）は **1053ページ**、非空 **518ページ**、返却 **46500件**（重複込み）、snapshot の ID 重複除去後 **25689件**。取得記録は [pages.json](docs-candidate-search/out/pages.json)。候補は177 run、177 task、162着地 commit。前後選択は各40件。

検証は13本の unit fixture（concern/route優先、legacy concernの次のstartedでの区切り、secondary docs_drift、unsent/再送/別attempt、未送信/未完了/失敗、durationと時刻の片側欠測、待ちの重なり/開いた待ち、summary欠測の分母、初回 receipt と後の最終 receipt の区別）と、同じ snapshot での再集計の全出力一致、cargo fmt --all --check と cargo clippy --locked --all-targets -- -D warnings。Rust の変更は無く、Rust unit test・stress・e2e は追加実行していない。e2e は必要なら runtime が review 通過後に host で実行する。

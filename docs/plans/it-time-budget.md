---
id: plan-it-time-budget
type: plan
title: IT の時間予算のための着地ログの測定
status: completed
created: 2026-11-18
tags:
  - testing
  - measurement
related:
  - adr-t2892-1
  - adr-t3920-1
---

# IT の時間予算のための着地ログの測定

## 読む範囲と算出方法

入れ替え待ちを fixture から縮める口が入った後の main の着地ログを読み、IT の予算と 1 本の上限を決める材料にする。ビルド・測定のための test 実行は行っていない。CI の JUnit による代用もない。

起点は `Dagq-Task: 4984` の main commit `95c933e3383df011ce980925a48e94cb8af5a875`（2026-11-15T18:01:21Z）、`Dagq-Run: 03dba482-fe9d-4f50-bc82-7264428f7a80`。main の履歴を新しい順に、その後の各 trailer の run directory で番号が最大の integrate 試行だけを調べ、unit stage と IT stage を持ち、全件選択を明記した最新 2 件を選んだ。実際の接頭辞は依頼文の `landing-it.sh:` ではなく `landing-it:` で、`scripts/landing-it.sh` が出す同じ段の印である。より新しい 5215 と 4716 は `narrowed IT: 0 filters` で IT の段がなく、対象から外れた。

以下の path は queue directory `/Users/shinnosukeooyama/.local/share/dagq/77067154921b9014` からの相対 path。記録の UTC 時刻はログファイルの最終更新時刻（記録完了の目安）で、test 開始時刻ではない。main commit の日時とは区別する。3 件とも host の着地の検証ログであり、並列度は各時点の `dagq.toml` の `NEXTEST_TEST_THREADS = "6"`。他の run と同時の負荷を分離した測定ではなく、4984 だけの因果効果は示さない。

「CPU 合計」は依頼に従う PASS 行の秒の和で、OS の CPU 使用時間ではない。待ちも含む test の経過秒の累計である。IT stage の PASS だけを抽出し、unit stage を混ぜない。分位点は昇順の nearest-rank（ceil(p × 本数) 番目）、閾値は厳密な超過（>）で集計する。CPU 割合の分母はその記録の PASS 合計。表示は秒を小数 3 桁、割合を小数 2 桁に丸める。module は `dagq::it` の test 名の最初の `::` より前、別 binary は binary ID とする。

CPU 合計 ÷ 6 は同じ実行集合を均等に並列化した見込みで、実測 wall time や除外された test を含む全件時間ではない。Summary は nextest の実測秒で build 時間を含めない。全件選択でも既知の失敗等は別に除外されるので、全登録数の予算達成とは区別する。

## 出典

### A（4984 後、最新）

- 記録の種類: host の integrate 検証ログ
- path: `runs/2ab1f507-ed92-49bf-85da-aaf20ab283fe/integrate-3-verify-3.log`
- commit: `392743eec1b079f4bde260e4d6f5ee2bc15ff04f`
- Dagq-Task: 5067
- Dagq-Run: `2ab1f507-ed92-49bf-85da-aaf20ab283fe`
- 並列度: 6
- 記録の UTC 時刻: 2026-11-17T16:06:31+00:00

commit は main の着地 commit（trailer を照合）。

```text
     Summary [ 998.894s] 6076 tests run: 6076 passed (6 slow), 0 skipped
     Summary [1474.352s] 1831 tests run: 1831 passed (5 slow), 16 skipped
```

### B（4984 後、次に新しい）

- 記録の種類: host の integrate 検証ログ
- path: `runs/1e5dcc30-c42b-441f-9471-ae664e7516f0/integrate-2-verify-3.log`
- commit: `5ff97f77e68d1493968c187ba5295e6aaf331c19`
- Dagq-Task: 5177
- Dagq-Run: `1e5dcc30-c42b-441f-9471-ae664e7516f0`
- 並列度: 6
- 記録の UTC 時刻: 2026-11-17T09:19:06+00:00

commit は main の着地 commit（trailer を照合）。

```text
     Summary [ 424.286s] 5008 tests run: 5008 passed (8 slow), 1055 skipped
     Summary [1189.819s] 1560 tests run: 1560 passed (4 slow), 287 skipped
```

### 比較列 4844（4984 前）

- 記録の種類: host の integrate 検証ログ
- path: `runs/02efea29-e725-4e2a-b4a0-7201b77f5b5c/integrate-3-verify-3.log`
- commit: `81a6329f5800344aea0c75ad0c67d9413c77d902`
- Dagq-Task: 4844
- Dagq-Run: `02efea29-e725-4e2a-b4a0-7201b77f5b5c`
- 並列度: 6
- 記録の UTC 時刻: 2026-11-15T11:47:30+00:00

commit は指定された旧 run の receipt の head。4844 の最終 main commit は `d86607854903c370acb52f18c9e9ddcd507a7ca8`、trailer の run は `1ccbd6eb-22d9-4aae-a2ee-c1d69cc74ce6` であり、この比較に指定された `02efea29` の試行とは異なる。旧ログは 1 本の失敗で完走していない。目標説明の 1,598 本・約 7,413 秒は PASS の集合として再集計する。

```text
     Summary [1782.511s] 5701 tests run: 5701 passed (25 slow), 0 skipped
     Summary [1436.173s] 1599/1833 tests run: 1598 passed (12 slow), 1 failed, 13 skipped
```

## IT の時間と UT との比

| 指標 | A: 5067 | B: 5177 | 4844 |
| --- | --- | --- | --- |
| PASS 本数 | 1831 | 1560 | 1598 |
| IT Summary 秒 | 1474.352 | 1189.819 | 1436.173 |
| CPU 合計 秒 | 8,720.761 | 7,134.813 | 7,413.261 |
| CPU 合計 ÷ 6 秒 | 1,453.460 | 1,189.136 | 1,235.543 |
| p50 秒 | 1.985 | 1.856 | 1.838 |
| p90 秒 | 12.265 | 11.968 | 10.138 |
| p99 秒 | 25.157 | 24.196 | 30.639 |
| UT Summary の実行本数 | 6076 | 5008 | 5701 |
| IT PASS / UT 実行本数 | 1831/6076 = 30.13% | 1560/5008 = 31.15% | 1598/5701 = 28.03% |
| 出力の既知失敗の除外指定数 | 4 | 275 | 1 |
| 1 秒超 本数 / PASS 本数の割合 / CPU 割合 | 1092 / 59.64% / 96.80% | 911 / 58.40% / 96.78% | 931 / 58.26% / 97.01% |
| 5 秒超 本数 / PASS 本数の割合 / CPU 割合 | 634 / 34.63% / 83.23% | 514 / 32.95% / 82.41% | 475 / 29.72% / 79.80% |
| 10 秒超 本数 / PASS 本数の割合 / CPU 割合 | 277 / 15.13% / 53.09% | 214 / 13.72% / 51.22% | 164 / 10.26% / 50.14% |
| 30 秒超 本数 / PASS 本数の割合 / CPU 割合 | 14 / 0.76% / 9.22% | 9 / 0.58% / 8.51% | 24 / 1.50% / 25.74% |

UT は `cargo nextest list` による総登録本数ではなく、unit stage の Summary の実行本数。B は ledger_range の 1,055 本を除外し、A はその組も実行したため、比の分母も違う。IT の選択集合は A が 1,831 本、B が 1,560 本（それぞれ skipped 16・287）。旧ログは選択 1,833 本のうち実行 1,599 本（PASS 1,598・失敗 1）で中断し、skipped 13 は未開始分とは別。旧ログの失敗 `lifecycle_install_service::install_s_drain_reaches_the_queue_only_through_its_service` の TRY 1 FAIL 600.079 秒と、未完了の試行は PASS 合計に入らない。したがって旧値は全件の CPU を過小に表す。

A の実測 IT 段は 24.57 分、B は 19.83 分で、600 秒の予算を両方超える。CPU 合計も 3,600 秒を超える。旧値との増減には集合の変更・負荷・中断が混ざり、待ちの口の効果の比較実験とは解釈しない。

## 全件選択の理由と CI に任せた項目

### 5067

```text
landing-it: every IT runs:
landing-it:   a common file: migrations-postgres/supervisor_disk_status.sql
landing-it:   a common file: migrations/0122_supervisor_disk_status.sql
```

共通の build 入力である migration に触れたため、ADR-t2892-1 決定3 の共通ファイル条件で全件選択。表が古すぎることや広い helper は、この出力が挙げた全件選択の理由ではない。

ADR-t2892-1 決定4 の `its IT left to CI:` 行はこの記録にはない。全件を選んだ回の出力に広い項目を CI に任せた列挙はなく、広い src を全件実行の理由に読み替えない。

### 5177

```text
landing-it: every IT runs:
landing-it:   the narrowed IT's estimated wall time 477.1 s exceeds 404.4 s (0.5 of every IT's)
```

絞った IT の見込みが全件の見込みの 50% の閾値を超えたため、ADR-t2892-1 決定3 の見込み条件で全件選択。表が古すぎることや広い helper は、この出力が挙げた全件選択の理由ではない。

ADR-t2892-1 決定4 の `its IT left to CI:` 行はこの記録にはない。全件を選んだ回の出力に広い項目を CI に任せた列挙はなく、広い src を全件実行の理由に読み替えない。

### 4844

```text
landing-it: every IT runs:
landing-it:   the narrowed IT's estimated wall time 870.4 s exceeds 647.5 s (0.5 of every IT's)
```

絞った IT の見込みが全件の見込みの 50% の閾値を超えたため、ADR-t2892-1 決定3 の見込み条件で全件選択。表が古すぎることや広い helper は、この出力が挙げた全件選択の理由ではない。

ADR-t2892-1 決定4 の `its IT left to CI:` 行はこの記録にはない。全件を選んだ回の出力に広い項目を CI に任せた列挙はなく、広い src を全件実行の理由に読み替えない。

古い表の 48 時間超と、全件に落とす 7 日超は異なる（[ADR-t3920-1](../adr/2026-11-01-t3920-1-landing-it-keeps-using-an-old-table-and-widens-the-files-it-does-not-know.md)）。上の理由は選び方の出力そのものを採り、表の古さから推測していない。

## module の CPU 合計 上位 20

同じ順位を並べる。各セルは module・本数・CPU 合計秒で、同じ module の横比較ではない。

| 順位 | A: module / 本数 / 秒 | B: module / 本数 / 秒 | 4844: module / 本数 / 秒 |
| --- | --- | --- | --- |
| 1 | `runtime_handoff` / 30 / 360.785 | `runtime_handoff` / 30 / 399.755 | `host_client` / 12 / 376.606 |
| 2 | `runtime_resume` / 22 / 330.976 | `runtime_resume` / 22 / 334.736 | `queue_binary` / 8 / 346.549 |
| 3 | `runtime_replan_park` / 26 / 287.843 | `runtime_replan_park` / 26 / 291.686 | `host_control` / 14 / 336.285 |
| 4 | `cli_version` / 9 / 263.109 | `cli_version` / 9 / 265.311 | `cli_version` / 10 / 316.608 |
| 5 | `runtime_cleanup` / 17 / 237.019 | `runtime_cleanup` / 17 / 245.171 | `runtime_resume` / 22 / 236.546 |
| 6 | `runtime_integrate` / 20 / 227.124 | `runtime_integrate` / 20 / 192.447 | `runtime_handoff` / 30 / 219.022 |
| 7 | `runtime_recheck` / 9 / 156.910 | `runtime_recheck` / 9 / 140.211 | `lifecycle_service` / 8 / 213.752 |
| 8 | `verification_container` / 18 / 143.360 | `runtime_claim_defer` / 11 / 138.621 | `runtime_replan_park` / 26 / 212.737 |
| 9 | `runtime_landing_release` / 11 / 141.518 | `runtime_claim` / 27 / 135.835 | `lifecycle_install_service` / 13 / 210.078 |
| 10 | `runtime_task_hold_park` / 12 / 136.867 | `runtime_fresh_session` / 8 / 123.236 | `runtime_cleanup` / 17 / 180.976 |
| 11 | `runtime_claim` / 27 / 124.264 | `runtime_landing_release` / 11 / 122.302 | `runtime_integrate` / 20 / 146.824 |
| 12 | `runtime_light_slots` / 5 / 124.017 | `runtime_headless` / 12 / 121.784 | `run_container` / 32 / 136.971 |
| 13 | `runtime_claim_defer` / 11 / 123.458 | `runtime_claim_class` / 12 / 119.818 | `runtime_recheck` / 9 / 105.415 |
| 14 | `runtime_headless_stall` / 11 / 121.911 | `run_container` / 32 / 113.277 | `runtime_headless` / 12 / 98.204 |
| 15 | `runtime_fresh_session` / 8 / 116.508 | `runtime_headless_stall` / 11 / 110.511 | `runtime_headless_stall` / 11 / 95.172 |
| 16 | `runtime_headless` / 12 / 113.717 | `runtime_replan_diagnosis` / 15 / 106.039 | `runtime_task_hold_park` / 12 / 95.100 |
| 17 | `run_container` / 32 / 112.898 | `runtime_light_slots` / 5 / 96.825 | `queue_service` / 13 / 90.273 |
| 18 | `runtime_claim_class` / 12 / 109.737 | `runtime_codex` / 12 / 90.048 | `runtime_landing_release` / 11 / 88.187 |
| 19 | `supervisor_service_ledger` / 3 / 100.949 | `recovery_codex` / 14 / 88.863 | `runtime_claim` / 27 / 82.958 |
| 20 | `recovery_codex` / 14 / 100.032 | `cli_install_release` / 1 / 87.237 | `runtime_claim_defer` / 11 / 82.432 |

## 1 本の時間 上位 20

同じ順位を並べる。名前は binary ID と test 名を結び、各セル末尾が秒。

| 順位 | A: test / 秒 | B: test / 秒 | 4844: test / 秒 |
| --- | --- | --- | --- |
| 1 | `dagq::it::cli_version::install_hands_a_running_supervisor_over_under_its_pid_and_rolls_back` / 145.831 | `dagq::it::cli_version::install_hands_a_running_supervisor_over_under_its_pid_and_rolls_back` / 128.401 | `dagq::it::host_control::with_no_service_each_command_says_so_and_opens_no_queue` / 300.311 |
| 2 | `dagq::it::runtime_handoff::an_adopted_resume_keeps_its_recorded_exit_request` / 96.983 | `dagq::it::runtime_handoff::an_adopted_resume_keeps_its_recorded_exit_request` / 99.042 | `dagq::it::queue_binary::only_the_queues_binary_migrates_the_queue` / 247.562 |
| 3 | `dagq::it::supervisor_service_ledger::what_builds_the_ledger_crosses_the_services_limits_in_parts` / 93.165 | `dagq::it::cli_install_release::install_release_installs_with_cargo_and_puts_the_binary_in_place` / 87.237 | `dagq::it::cli_version::auto_update_installs_each_runtime_landing_and_puts_a_broken_build_back` / 136.114 |
| 4 | `dagq::it::cli_version::a_supervisor_handed_over_by_an_exec_keeps_its_owner` / 75.291 | `dagq::it::cli_version::a_supervisor_handed_over_by_an_exec_keeps_its_owner` / 79.922 | `dagq::it::host_client::requests_replans_ci_failures_and_planners_are_read_through_the_service` / 120.435 |
| 5 | `dagq::it::cli_install_release::install_release_installs_with_cargo_and_puts_the_binary_in_place` / 71.888 | `dagq::it::cli_version::install_of_a_build_that_dies_puts_the_binary_back_and_asks_the_inbox` / 51.155 | `dagq::it::lifecycle_install_service::install_and_its_rollback_reach_the_queue_only_through_its_service` / 120.216 |
| 6 | `dagq::it::runtime_cleanup::a_withdrawn_handoff_resumes_the_cleanup` / 44.191 | `dagq::it::queue_binary::install_lets_its_new_build_migrate_and_names_it_the_queues_binary` / 49.714 | `dagq::it::lifecycle_service::down_wait_ends_when_the_drained_supervisor_s_pid_runs_another_process` / 90.564 |
| 7 | `dagq::it::queue_binary::install_lets_its_new_build_migrate_and_names_it_the_queues_binary` / 40.397 | `dagq::it::runtime_cleanup::a_withdrawn_handoff_resumes_the_cleanup` / 44.109 | `dagq::it::lifecycle_service::down_wait_takes_the_service_the_drain_stopped_for_the_drain_s_end` / 90.551 |
| 8 | `dagq::it::runtime_precheck::precheck_conflicts_up_to_the_conflict_only_limit_retry_the_task_with_its_branch` / 36.078 | `dagq::it::runtime_cleanup::draining_runs_the_rest_of_a_cleanup_for_room_before_deciding_a_landing` / 34.437 | `dagq::it::host_client::the_user_the_inbox_and_a_planner_go_through_the_service_and_never_open_the_db` / 90.463 |
| 9 | `dagq::it::runtime_cleanup::draining_runs_the_rest_of_a_cleanup_for_room_before_deciding_a_landing` / 35.033 | `dagq::it::runtime_precheck::precheck_conflicts_up_to_the_conflict_only_limit_retry_the_task_with_its_branch` / 33.311 | `dagq::it::queue_binary::install_lets_its_new_build_migrate_and_names_it_the_queues_binary` / 90.219 |
| 10 | `dagq::it::runtime_spike_replan::the_investigation_limit_holds_a_new_goal_s_spike_until_an_investigation_ends` / 34.670 | `dagq::it::runtime_claim_defer::a_task_meeting_a_run_on_a_hotspot_waits_and_the_next_one_is_claimed` / 28.629 | `dagq::it::cli_version::install_hands_a_running_supervisor_over_under_its_pid_and_rolls_back` / 88.863 |
| 11 | `dagq::it::cli_version::install_of_a_build_that_dies_puts_the_binary_back_and_asks_the_inbox` / 34.594 | `dagq::it::runtime_light_slots::a_light_room_deferral_stays_one_record_while_a_free_slot_comes_and_goes` / 27.022 | `dagq::it::host_client::a_recorded_outage_is_in_status_and_returned_once_to_each_role_s_watch` / 64.337 |
| 12 | `dagq::it::lifecycle_install_service::an_update_job_records_its_failure_before_it_stops_its_own_supervisor` / 32.548 | `dagq::it::runtime_claim_defer::a_changed_conflicts_table_is_read_again_without_a_restart` / 26.675 | `dagq::it::queue_service::status_doctor_and_service_status_report_the_service` / 60.310 |
| 13 | `dagq::it::runtime_light_slots::a_light_room_deferral_stays_one_record_while_a_free_slot_comes_and_goes` / 32.196 | `dagq::it::lifecycle_install_service::an_update_job_records_its_failure_before_it_stops_its_own_supervisor` / 24.867 | `dagq::it::cli_version::a_supervisor_handed_over_by_an_exec_keeps_its_owner` / 58.910 |
| 14 | `dagq::it::runtime_light_slots::a_parked_run_waits_for_a_normal_slot_while_the_landing_queue_fills_it` / 31.537 | `dagq::it::runtime_claim_build::a_task_waits_for_a_build_that_contains_its_dependencies_landings` / 24.772 | `dagq::it::cli_install_release::install_release_installs_with_cargo_and_puts_the_binary_in_place` / 44.495 |
| 15 | `dagq::it::runtime_waiting_stages::a_resume_question_waits_in_its_slot_then_outside_it_with_its_clock_stopped` / 29.153 | `dagq::it::runtime_light_slots::a_parked_run_waits_for_a_normal_slot_while_the_landing_queue_fills_it` / 24.370 | `dagq::it::runtime_cleanup::a_withdrawn_handoff_resumes_the_cleanup` / 32.208 |
| 16 | `dagq::it::runtime_claim_defer::a_task_meeting_a_run_on_a_hotspot_waits_and_the_next_one_is_claimed` / 26.785 | `dagq::it::runtime_resume::a_resumed_session_that_ignores_its_exit_request_is_let_go_after_the_exit_timeout` / 24.196 | `dagq::it::queue_service_client::a_client_mode_dagq_opens_no_queue_and_says_why_it_refused` / 30.639 |
| 17 | `dagq::it::runtime_light_slots::the_landing_queue_leaves_room_for_the_heavy_interrupt` / 25.484 | `dagq::it::runtime_recheck::a_run_held_in_its_slot_for_its_e2e_that_a_landing_conflicts_with_is_parked_before_it_lands` / 23.759 | `dagq::it::lifecycle_service::up_down_and_the_service_s_start_and_stop_reach_the_queue_only_through_it` / 30.497 |
| 18 | `dagq::it::verification_container::a_run_held_in_its_slot_whose_recheck_command_fails_in_its_container_is_parked_before_it_lands` / 25.367 | `dagq::it::runtime_claim::supervisor_claims_by_effective_priority_like_candidates_and_graph` / 23.531 | `dagq::it::lifecycle_install_service::the_update_jobs_record_as_the_supervisor_that_started_them` / 30.487 |
| 19 | `dagq::it::runtime_recheck::a_run_held_in_its_slot_for_its_e2e_that_a_landing_conflicts_with_is_parked_before_it_lands` / 25.157 | `dagq::it::runtime_cleanup::the_rest_of_a_cleanup_for_room_holds_and_asks_nothing_until_it_is_done` / 22.909 | `dagq::it::host_client::a_queue_named_beside_the_served_one_is_refused_and_neither_is_changed` / 30.369 |
| 20 | `dagq::it::runtime_precheck::the_conflict_only_limit_of_dagq_toml_retries_the_task_sooner` / 25.117 | `dagq::it::runtime_claim::supervisor_claims_the_candidate_that_releases_the_most_tasks_first` / 22.395 | `dagq::it::host_client::a_planner_s_ask_on_its_request_goes_through_the_service_as_that_planner` / 30.332 |

## 現在の対応表で 25% を超える src

`scripts/landing-it.py` の `BROAD_SHARE = 0.25` と同じく、対応表の `tests` 全 1835 本を分母とし、`files` の各 src の写る本数が 458.75 本を厳密に超えるものを列挙する（351 files）。tests の実行はせず、`scripts/landing-it.sh` が読む cache の既存 JSON を集計した。これは選んだ各ログ当時の表ではなく、今回読める最新の表である。

- path: `/Users/shinnosukeooyama/.cache/dagq/landing-it/44195285183/it-coverage-map.json`
- commit: `74fc1bb1de9693f52d443ef812dfae47bff706d6`
- generated_at（UTC）: 2026-11-17T23:10:19Z
- CI run: https://github.com/hisamekms/dagq/actions/runs/44195285183
- platform: linux（Linux の coverage。macOS 固有経路の欠落は選択 script の widening の対象）

表の時間は coverage つきの CI の値なので、上の host の時間分布には混ぜない。この一覧は file 単位の対応の広さであり、その全 test がその file だけを検査しているという意味ではない。広い file は ADR-t2892-1 決定2 により表から何も選ばず CI に任せる扱いで、全件を選ぶ直接の条件とは別である。

| src file | 写る IT 本数 / 1835 |
| --- | --- |
| `src/domain/mod.rs` | 1742 |
| `src/domain/e2e_deadline.rs` | 1738 |
| `src/infrastructure/event_actor.rs` | 1737 |
| `src/domain/actor.rs` | 1732 |
| `src/infrastructure/queue_service.rs` | 1731 |
| `src/application/mod.rs` | 1729 |
| `src/infrastructure/sqlite.rs` | 1728 |
| `src/infrastructure/sql_exec/mod.rs` | 1727 |
| `src/infrastructure/sql_exec/sqlite.rs` | 1727 |
| `src/infrastructure/sql_exec/values.rs` | 1727 |
| `src/infrastructure/write_leader.rs` | 1727 |
| `src/infrastructure/location.rs` | 1726 |
| `src/infrastructure/clock.rs` | 1724 |
| `src/application/audit.rs` | 1713 |
| `src/infrastructure/sql_exec/commit_actions.rs` | 1712 |
| `src/infrastructure/sql_dialect.rs` | 1704 |
| `src/infrastructure/store_queue.rs` | 1704 |
| `src/domain/ids.rs` | 1699 |
| `src/domain/audit/redact.rs` | 1694 |
| `src/infrastructure/audit.rs` | 1694 |
| `src/domain/audit/chain.rs` | 1691 |
| `src/domain/audit/mod.rs` | 1691 |
| `src/domain/audit/record.rs` | 1691 |
| `src/domain/error.rs` | 1678 |
| `src/application/ports/shared.rs` | 1672 |
| `src/infrastructure/store_rows.rs` | 1610 |
| `src/domain/event_kind.rs` | 1603 |
| `src/infrastructure/event_bodies.rs` | 1565 |
| `src/domain/sessions.rs` | 1562 |
| `src/infrastructure/sessions.rs` | 1562 |
| `src/domain/authorization.rs` | 1522 |
| `src/compose.rs` | 1520 |
| `src/domain/queue_service.rs` | 1510 |
| `src/application/queue_service.rs` | 1494 |
| `src/infrastructure/runtime_store/mod.rs` | 1493 |
| `src/infrastructure/runtime_store/run_log.rs` | 1488 |
| `src/application/commands/mod.rs` | 1484 |
| `src/compose/host.rs` | 1479 |
| `src/compose/observation.rs` | 1478 |
| `src/infrastructure/language.rs` | 1470 |
| `src/infrastructure/task_store.rs` | 1467 |
| `src/compose/planning.rs` | 1464 |
| `src/infrastructure/queue_service/storage.rs` | 1462 |
| `src/domain/direct_human.rs` | 1458 |
| `src/application/commands/operations.rs` | 1457 |
| `src/domain/write_leader.rs` | 1457 |
| `src/domain/sccache.rs` | 1452 |
| `src/infrastructure/replans.rs` | 1451 |
| `src/infrastructure/task_holds.rs` | 1449 |
| `src/domain/scope.rs` | 1444 |
| `src/infrastructure/kpi_config.rs` | 1443 |
| `src/domain/credential_preference.rs` | 1442 |
| `src/domain/store_setting.rs` | 1442 |
| `src/infrastructure/deployment_config.rs` | 1442 |
| `src/domain/task.rs` | 1440 |
| `src/infrastructure/telemetry.rs` | 1440 |
| `src/infrastructure/store_writes.rs` | 1438 |
| `src/main.rs` | 1438 |
| `src/domain/worker.rs` | 1435 |
| `src/domain/plan_request.rs` | 1434 |
| `src/domain/goal_owner.rs` | 1433 |
| `src/infrastructure/adapters.rs` | 1433 |
| `src/domain/execution_class.rs` | 1431 |
| `src/infrastructure/follow_up_membership.rs` | 1430 |
| `src/domain/plan_review.rs` | 1427 |
| `src/infrastructure/search_writes.rs` | 1424 |
| `src/domain/input.rs` | 1422 |
| `src/infrastructure/github_app.rs` | 1402 |
| `src/infrastructure/runtime_store/coordination.rs` | 1392 |
| `src/domain/task_replan.rs` | 1391 |
| `src/domain/stats.rs` | 1389 |
| `src/infrastructure/queue_binary.rs` | 1386 |
| `src/infrastructure/github_rulesets.rs` | 1384 |
| `src/application/heavy_reads.rs` | 1383 |
| `src/infrastructure/queue_service/write_line.rs` | 1383 |
| `src/infrastructure/queue_service_http.rs` | 1383 |
| `src/infrastructure/startup_reads.rs` | 1383 |
| `src/infrastructure/startup_reads/periods.rs` | 1383 |
| `src/application/queue_reads/mod.rs` | 1365 |
| `src/infrastructure/plan_requests.rs` | 1340 |
| `src/domain/task_hold.rs` | 1318 |
| `src/infrastructure/git_binary.rs` | 1307 |
| `src/domain/run/payload.rs` | 1297 |
| `src/domain/provider_switch.rs` | 1261 |
| `src/domain/provider_trial.rs` | 1261 |
| `src/domain/worker_model.rs` | 1261 |
| `src/infrastructure/draft_planners.rs` | 1225 |
| `src/application/queue_commands.rs` | 1217 |
| `src/application/supervisor_service.rs` | 1215 |
| `src/infrastructure/run_files.rs` | 1211 |
| `src/domain/investigation.rs` | 1210 |
| `src/infrastructure/investigations.rs` | 1200 |
| `src/infrastructure/run_env.rs` | 1180 |
| `src/application/supervisor_service/needs.rs` | 1146 |
| `src/domain/host_metrics.rs` | 1145 |
| `src/domain/views.rs` | 1141 |
| `src/application/supervisor_service/dispatch.rs` | 1138 |
| `src/application/supervisor_service/forward.rs` | 1138 |
| `src/application/supervisor_service/wire.rs` | 1138 |
| `src/infrastructure/process_read.rs` | 1133 |
| `src/infrastructure/agent_dir.rs` | 1127 |
| `src/application/queue_reads/planning.rs` | 1124 |
| `src/domain/write_rules.rs` | 1124 |
| `src/application/ask.rs` | 1119 |
| `src/infrastructure/asks.rs` | 1099 |
| `src/application/install.rs` | 1098 |
| `src/application/supervisor_service/resend.rs` | 1079 |
| `src/domain/replan_detect.rs` | 1067 |
| `src/domain/local_config.rs` | 1057 |
| `src/infrastructure/runtime_store/session_registry.rs` | 1035 |
| `src/domain/claim_hold.rs` | 1032 |
| `src/infrastructure/runtime_store/ask_store.rs` | 1032 |
| `src/infrastructure/runtime_store/transitions.rs` | 1027 |
| `src/domain/actor_model.rs` | 1018 |
| `src/infrastructure/finding_planners.rs` | 1015 |
| `src/domain/claim_facts.rs` | 1014 |
| `src/domain/container_runtime.rs` | 1014 |
| `src/domain/stall.rs` | 1014 |
| `src/domain/run.rs` | 1012 |
| `src/domain/stats/conflicts.rs` | 1008 |
| `src/domain/disk.rs` | 1003 |
| `src/domain/audit/administration.rs` | 999 |
| `src/application/ports/execution.rs` | 998 |
| `src/domain/claim_defer.rs` | 996 |
| `src/domain/landing_branch.rs` | 993 |
| `src/infrastructure/queue_broker.rs` | 985 |
| `src/domain/cancel_reason.rs` | 980 |
| `src/domain/release_update.rs` | 979 |
| `src/infrastructure/release_update.rs` | 979 |
| `src/domain/session_step.rs` | 975 |
| `src/application/health/report.rs` | 971 |
| `src/application/queue_broker.rs` | 971 |
| `src/infrastructure/transcripts.rs` | 971 |
| `src/domain/toolchain.rs` | 968 |
| `src/application/supervisor_service/caller.rs` | 964 |
| `src/domain/supervisor_life.rs` | 962 |
| `src/application/execution.rs` | 960 |
| `src/infrastructure/replan_apply.rs` | 960 |
| `src/domain/ledger/baseline.rs` | 959 |
| `src/infrastructure/runtime_store/session_steps.rs` | 956 |
| `src/domain/ledger/attributes.rs` | 953 |
| `src/application/claim_view.rs` | 952 |
| `src/domain/live_alerts.rs` | 952 |
| `src/application/inbox_watcher.rs` | 948 |
| `src/application/stats.rs` | 947 |
| `src/domain/throughput_review.rs` | 947 |
| `src/infrastructure/goal_reviews.rs` | 946 |
| `src/infrastructure/planners.rs` | 944 |
| `src/infrastructure/plan_reviews.rs` | 941 |
| `src/domain/resume.rs` | 939 |
| `src/domain/pre_claim.rs` | 938 |
| `src/domain/ci_watch.rs` | 937 |
| `src/domain/stats/main_log.rs` | 936 |
| `src/domain/run_env.rs` | 934 |
| `src/compose/execution.rs` | 933 |
| `src/domain/retention.rs` | 933 |
| `src/application/supervise/observation/ledger.rs` | 932 |
| `src/infrastructure/runtime_store/attributes.rs` | 932 |
| `src/domain/ledger/findings.rs` | 931 |
| `src/domain/agent_eval/round.rs` | 930 |
| `src/domain/agent_eval/watch.rs` | 930 |
| `src/domain/exit.rs` | 930 |
| `src/infrastructure/podman_machine.rs` | 926 |
| `src/infrastructure/runtime_store/recovery.rs` | 925 |
| `src/application/step_report.rs` | 923 |
| `src/application/queue_writes.rs` | 921 |
| `src/domain/podman_machine.rs` | 919 |
| `src/infrastructure/codex.rs` | 919 |
| `src/application/telemetry_tokens.rs` | 918 |
| `src/application/supervise/execution.rs` | 899 |
| `src/domain/telemetry.rs` | 899 |
| `src/infrastructure/runtime_store/planning_records.rs` | 899 |
| `src/domain/finding.rs` | 897 |
| `src/domain/transcript.rs` | 896 |
| `src/application/planner.rs` | 893 |
| `src/domain/light_slots.rs` | 890 |
| `src/application/live_inputs.rs` | 887 |
| `src/domain/follow_up.rs` | 886 |
| `src/domain/live_alerts/history.rs` | 886 |
| `src/domain/claim_order.rs` | 885 |
| `src/infrastructure/claude_credential.rs` | 885 |
| `src/domain/slot_limits.rs` | 880 |
| `src/application/review_programs.rs` | 876 |
| `src/infrastructure/environment.rs` | 875 |
| `src/infrastructure/review_programs.rs` | 875 |
| `src/application/main_log.rs` | 872 |
| `src/application/supervise/observation/main_log.rs` | 872 |
| `src/application/supervise/host_ops/host_metrics.rs` | 871 |
| `src/infrastructure/headless_jobs.rs` | 870 |
| `src/application/commands/delegates.rs` | 869 |
| `src/compose/supervisor.rs` | 868 |
| `src/application/supervise/host_ops/update.rs` | 867 |
| `src/application/recording.rs` | 866 |
| `src/application/retention.rs` | 866 |
| `src/application/supervise/execution/jobs.rs` | 866 |
| `src/application/supervise/observation/resources.rs` | 866 |
| `src/application/supervisor_service/end_bound.rs` | 866 |
| `src/infrastructure/held_records.rs` | 866 |
| `src/application/supervise/execution/background.rs` | 865 |
| `src/domain/run_clone.rs` | 865 |
| `src/infrastructure/session_tokens.rs` | 865 |
| `src/application/agent_cases.rs` | 863 |
| `src/application/supervise/contexts.rs` | 863 |
| `src/application/supervise/execution/adopt.rs` | 863 |
| `src/application/supervise/execution/agent_cases.rs` | 863 |
| `src/application/supervise/execution/agent_eval.rs` | 863 |
| `src/application/supervise/execution/claim.rs` | 863 |
| `src/application/supervise/execution/claim_defer.rs` | 863 |
| `src/application/supervise/execution/container.rs` | 863 |
| `src/application/supervise/execution/e2e.rs` | 863 |
| `src/application/supervise/execution/landing.rs` | 863 |
| `src/application/supervise/execution/provider.rs` | 863 |
| `src/application/supervise/execution/queue_hold.rs` | 863 |
| `src/application/supervise/execution/recheck.rs` | 863 |
| `src/application/supervise/execution/reconcile.rs` | 863 |
| `src/application/supervise/execution/recovery.rs` | 863 |
| `src/application/supervise/execution/replan_diagnosis.rs` | 863 |
| `src/application/supervise/execution/replan_park.rs` | 863 |
| `src/application/supervise/execution/run_loop.rs` | 863 |
| `src/application/supervise/execution/satisfied.rs` | 863 |
| `src/application/supervise/execution/session.rs` | 863 |
| `src/application/supervise/execution/slot_limits.rs` | 863 |
| `src/application/supervise/execution/stages.rs` | 863 |
| `src/application/supervise/execution/startup.rs` | 863 |
| `src/application/supervise/execution/stranded.rs` | 863 |
| `src/application/supervise/execution/sweeps.rs` | 863 |
| `src/application/supervise/execution/task_hold.rs` | 863 |
| `src/application/supervise/execution/tick.rs` | 863 |
| `src/application/supervise/execution/triage.rs` | 863 |
| `src/application/supervise/execution/waiting.rs` | 863 |
| `src/application/supervise/host_ops.rs` | 863 |
| `src/application/supervise/host_ops/audit.rs` | 863 |
| `src/application/supervise/host_ops/broker_relay.rs` | 863 |
| `src/application/supervise/host_ops/cleanup.rs` | 863 |
| `src/application/supervise/host_ops/disk.rs` | 863 |
| `src/application/supervise/host_ops/handoff.rs` | 863 |
| `src/application/supervise/host_ops/inbox_nudge.rs` | 863 |
| `src/application/supervise/host_ops/moving_sizes.rs` | 863 |
| `src/application/supervise/host_ops/queue_broker.rs` | 863 |
| `src/application/supervise/host_ops/queue_service.rs` | 863 |
| `src/application/supervise/host_ops/release.rs` | 863 |
| `src/application/supervise/host_ops/sccache.rs` | 863 |
| `src/application/supervise/host_ops/sweep.rs` | 863 |
| `src/application/supervise/observation.rs` | 863 |
| `src/application/supervise/observation/agent_watch.rs` | 863 |
| `src/application/supervise/observation/ci_watch.rs` | 863 |
| `src/application/supervise/observation/forecast.rs` | 863 |
| `src/application/supervise/observation/live_alerts.rs` | 863 |
| `src/application/supervise/observation/observer.rs` | 863 |
| `src/application/supervise/observation/push.rs` | 863 |
| `src/application/supervise/observation/report.rs` | 863 |
| `src/application/supervise/observation/throughput_review.rs` | 863 |
| `src/application/supervise/planning.rs` | 863 |
| `src/application/supervise/planning/goal_review.rs` | 863 |
| `src/application/supervise/planning/plan_review.rs` | 863 |
| `src/application/supervise/repository_config.rs` | 863 |
| `src/domain/agent_eval/production.rs` | 863 |
| `src/domain/kpi/candidates.rs` | 863 |
| `src/domain/slot_order.rs` | 863 |
| `src/domain/stats/window.rs` | 863 |
| `src/application/actor_executor.rs` | 862 |
| `src/application/supervise/execution/resume.rs` | 862 |
| `src/domain/agent_eval/record.rs` | 862 |
| `src/domain/headless_job.rs` | 861 |
| `src/application/main_history.rs` | 860 |
| `src/domain/build_wait.rs` | 860 |
| `src/application/supervise/planning/recurring.rs` | 857 |
| `src/domain/supervisor_prune.rs` | 853 |
| `src/infrastructure/container_image.rs` | 848 |
| `src/application/naming.rs` | 840 |
| `src/infrastructure/process.rs` | 840 |
| `src/application/supervise/planning/planner_turns.rs` | 836 |
| `src/application/supervise/planning/draft_planner.rs` | 835 |
| `src/application/supervise/planning/finding_planner.rs` | 835 |
| `src/application/supervise/planning/replan_apply.rs` | 835 |
| `src/application/supervise/planning/request_planner.rs` | 835 |
| `src/domain/turn.rs` | 827 |
| `src/domain/background_wrapper.rs` | 822 |
| `src/domain/language.rs` | 814 |
| `src/infrastructure/run_container.rs` | 812 |
| `src/domain/command_shape.rs` | 810 |
| `src/domain/run_container.rs` | 802 |
| `src/domain/goal_tag.rs` | 799 |
| `src/domain/waiting.rs` | 797 |
| `src/domain/goal.rs` | 784 |
| `src/infrastructure/runtime_store/queue_records.rs` | 784 |
| `src/domain/reason.rs` | 778 |
| `src/application/prompt_fit.rs` | 772 |
| `src/domain/run/recorded.rs` | 756 |
| `src/domain/run_phase.rs` | 735 |
| `src/application/ci_watch.rs` | 727 |
| `src/domain/run/history.rs` | 726 |
| `src/infrastructure/ci_watch_store.rs` | 720 |
| `src/domain/review_reason.rs` | 713 |
| `src/application/ports/host.rs` | 712 |
| `src/domain/tokens.rs` | 705 |
| `src/infrastructure/adapters/run_clones.rs` | 694 |
| `src/application/supervise/execution/headless.rs` | 687 |
| `src/domain/interned.rs` | 685 |
| `src/infrastructure/claude.rs` | 684 |
| `src/domain/receipt.rs` | 662 |
| `src/application/session.rs` | 661 |
| `src/domain/validation.rs` | 660 |
| `src/application/run_git.rs` | 658 |
| `src/domain/environment.rs` | 654 |
| `src/application/supervise/execution/handoff_slots.rs` | 652 |
| `src/application/integrate.rs` | 651 |
| `src/application/prompt/run_review.rs` | 650 |
| `src/application/prompt/known_failures.rs` | 647 |
| `src/domain/measure.rs` | 647 |
| `src/infrastructure/claude_steps.rs` | 647 |
| `src/application/headless_session.rs` | 643 |
| `src/application/prompt/worker.rs` | 642 |
| `src/application/supervise/execution/slot_end.rs` | 642 |
| `src/application/supervise/execution/container_sccache.rs` | 638 |
| `src/application/supervise/execution/slot_step.rs` | 637 |
| `src/domain/local_checks.rs` | 632 |
| `src/domain/recheck.rs` | 632 |
| `src/infrastructure/claude_turns.rs` | 631 |
| `src/domain/satisfied.rs` | 622 |
| `src/application/supervise/execution/exit.rs` | 604 |
| `src/application/supervise/execution/stall.rs` | 604 |
| `src/application/role_snapshot.rs` | 592 |
| `src/application/supervise/execution/stall_recovery.rs` | 591 |
| `src/domain/review_subagents.rs` | 591 |
| `src/domain/review_builtins.rs` | 587 |
| `src/domain/claim_spacing.rs` | 586 |
| `src/domain/unapplied_decisions.rs` | 586 |
| `src/domain/instructions.rs` | 584 |
| `src/application/supervise/execution/idle.rs` | 582 |
| `src/domain/landing_binding.rs` | 581 |
| `src/domain/review_programs.rs` | 577 |
| `src/domain/stats/thresholds.rs` | 571 |
| `src/application/review.rs` | 562 |
| `src/application/update.rs` | 559 |
| `src/application/supervise/execution/deliver.rs` | 551 |
| `src/application/heavy_turn.rs` | 540 |
| `src/domain/heavy_turn.rs` | 539 |
| `src/application/lifecycle.rs` | 533 |
| `src/infrastructure/passed_env.rs` | 532 |
| `src/domain/agent_eval/review.rs` | 531 |
| `src/application/integrate/binding.rs` | 527 |
| `src/domain/review_programs/secrets.rs` | 522 |
| `src/infrastructure/review_secrets.rs` | 522 |
| `src/domain/review_jobs.rs` | 517 |
| `src/application/supervise/execution/stale.rs` | 512 |
| `src/domain/worker_question.rs` | 503 |
| `src/domain/machine_memory.rs` | 502 |
| `src/domain/stats/revert.rs` | 490 |
| `src/domain/queue_hold.rs` | 465 |
| `src/domain/command_rules.rs` | 464 |


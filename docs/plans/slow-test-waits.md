---
id: plan-slow-test-waits
type: plan
title: 遅いintegration testの時間が使われている待ちの内訳と、修正の候補の見積もり
status: active
created: 2026-09-29
updated: 2026-09-29
owners:
  - hisamekms
tags:
  - performance
  - testing
  - measurement
related:
  - plan-nextest-measurement
  - plan-nextest-test-threads
  - adr-0076
---

# 遅いintegration testの時間が使われている待ちの内訳と、修正の候補の見積もり

goal 68（testの実時間の待ちを減らし、着地の検証のtest段とworkerの手元のtestを縮める）の最初の測定で、task 975が書いた。遅いtestの時間を次の4つの待ちに分け、修正の候補ごとに縮む秒を見積もる。

- (a) 閾値: 秒単位のStallConfigの閾値やtimeoutを越えるまでの待ち。閾値を越えることや越えないことを確かめる、testの固定のsleepも含む
- (b) 秒の境界: 秒単位の時刻やmarkerのmtimeの比較で、次の秒まで待つ分
- (c) fixture: git init、seedのcommit、`SqliteQueue::init`
- (d) 処理: 上のどれでもない残り。supervisorのpass、stubのsession、git、review stubなど

この文書はsrc/とtests/を変えない。修正はfollow-upのtaskが行う（5章）。productionのtimeoutと閾値の値も変えない（goal 68の制約）。

## 要点

- **test段はtestの時間の合計÷並列度で決まる。** 直近32回のintegrateのnextestで、testの時間の合計は中央値2,780秒、test段（`Summary`）は中央値377秒だった。比は7.4で、`NEXTEST_TEST_THREADS=8`にほぼ等しい。この32回のうち最後の3回は6で流れた。以後の見積もりは6で割る。
- **遅い30本（ローカルで1本ずつ流した405秒）の内訳**は次のとおり。(a)と(b)はeventの間隔から測った秒で（cli_versionの2本の(a) 18秒だけは推定）、(d)が一番大きい。
  - (a) 閾値: 113秒（28%）
  - (b) 秒の境界: 15秒（4%）
  - (c) fixture: 44秒（11%）
  - (d) 処理: 234秒（58%）
- **fixtureの大半は`SqliteQueue::init`（53本のmigration）で、hostのloadに強く左右される。** load 10で0.17秒、load 35〜45で最大1.9秒、中央値は0.70秒だった。gitの部分（5回のgitの起動）は中央値0.18秒。migrate済みのDBのファイルのコピーは0.7ミリ秒。fixtureを呼ぶ箇所は569あり、合計の見積もりの幅がいちばん大きい候補になる。
- **修正の候補と、test段が縮む秒の見積もり**（6並列で割った値）
  - F1 fixtureのtemplate: 28〜85秒
  - F2 秒未満の閾値をtestから入れる経路: 18〜25秒
  - F4 固定のsleepの置き換え: 11〜13秒
  - F3 Durationのtimeoutの短縮: 4〜6秒
  - F5 heartbeatと引き継ぎの見張り: 約3秒

  合わせて約64〜132秒で、6並列のtest段の約460秒（2,780÷6）の13〜28%にあたる。(d)の58%は、まだ分けきれていない（F6で測る）。

## 1. 遅いtestの上位30本とmoduleごとの合計

### 手順

1. `dagq locate`の`runs_dir`（`~/.local/share/dagq/77067154921b9014/runs`）にある`*/integrate-*-verify-*.log`のうち、`Summary [`を含むもの（nextestのcoverageの関門）を更新時刻の新しい順に32本選んだ。範囲は2026-09-28 21:44〜2026-09-29 06:09 JST。
2. 各logの`PASS [ Ns]`と`FLAKY n/m [ Ns]`の行から、testごとの時間を取った（`FAIL`は除く）。testごとに32回の中央値を出し、上位30本とmoduleごとの合計を出した。
3. loadは`~/.local/share/dagq-hostmetrics/metrics.csv`（約30秒ごと）の`load1`を使った。範囲は各logのtest段の時間（logの更新時刻から`Summary`の秒を引いた時刻から、更新時刻まで）で、その平均と最大を取った。

集計のスクリプトの中身（Python。goal 68の(3)のCIのscriptはこれを元にしてよい）:

```python
pat = re.compile(r'^\s*(PASS|FLAKY \d+/\d+|SLOW|FAIL)\s*\[\s*([0-9.]+)s\]\s*(?:\(\s*\d+/\d+\)\s*)?(\S+)\s+(\S+)')
# 各logの各行で、FAIL以外の行を per[(binary, test)].append(秒) に足す
med = {k: statistics.median(v) for k, v in per.items()}
# moduleは、binary dagq::itではtest名の最初の`::`の前、それ以外はbinary名で束ねる
```

### 全体

| 項目 | 値 |
| --- | --- |
| log | 32本（2026-09-28 21:44〜09-29 06:09 JST）。`NEXTEST_TEST_THREADS`は8が29本、6が3本（40f693fの着地、09-29 05:26 JST より後） |
| testの時間の合計 | logごとの中央値2,780秒。testごとの中央値の合計は2,754.5秒（1,924本） |
| test段（`Summary`） | 中央値377秒（299〜1,261） |
| load1（test段の平均） | logごとの平均の中央値19.3（9.8〜114.1）。1,000秒を超えた2本はload 100超 |
| 1秒以上のtest（割合の分母はtestごとの中央値の合計2,754.5秒） | 626本、2,616秒（合計の95%） |
| 2秒以上 | 503本、2,431秒（88%） |
| 5秒以上 | 184本、1,365秒（50%） |
| 10秒以上 | 19本、285秒（10%） |
| 15秒以上 | 7本、137秒（5%） |

goal 68の記述（1秒以上の604本で95%、5秒以上の185本で51%）と同じ形である。最長の1本（38秒）はtest段の約10%（6並列の約460秒に対しては8%）で、test段を律速していない。

### 上位30本（32回の中央値、秒）

| # | test | 中央値 | 最大 |
| --- | --- | --- | --- |
| 1 | cli_version::auto_update_installs_each_runtime_landing_and_puts_a_broken_build_back | 37.8 | 66.4 |
| 2 | runtime_resume::a_resumed_session_gets_its_request_only_once_its_input_box_is_ready | 19.0 | 37.6 |
| 3 | runtime_waiting_stages::a_resume_question_past_the_limit_waits_in_its_slot_with_its_clock_stopped | 16.9 | 50.3 |
| 4 | runtime_waiting_stages::a_question_while_resuming_waits_outside_the_slot_and_gets_its_answer | 16.8 | 47.1 |
| 5 | runtime_stall::an_input_the_supervisor_did_not_send_holds_the_nudge_and_is_preempted | 15.7 | 38.3 |
| 6 | dagq-broker backends::git::tests::what_git_leaves_holding_its_pipes_is_stopped_too | 15.3 | 15.8 |
| 7 | runtime_resume::a_request_lost_twice_is_asked_to_the_inbox | 15.1 | 43.8 |
| 8 | runtime_review::a_failed_review_span_ends_with_its_job_not_with_the_exit | 14.5 | 54.4 |
| 9 | runtime_review::a_failed_review_closes_the_session_and_asks_a_person_in_the_same_step | 14.4 | 73.2 |
| 10 | runtime_resume::conflict_only_resumes_are_not_counted_and_a_used_up_run_is_retried_with_its_branch | 14.4 | 38.1 |
| 11 | runtime_waiting_stages::a_question_while_revising_waits_outside_the_slot_with_its_clock_stopped | 14.0 | 34.8 |
| 12 | cli_version::install_hands_a_running_supervisor_over_under_its_pid_and_rolls_back | 13.1 | 16.0 |
| 13 | runtime_screen_idle_input::a_markerless_resumed_session_gets_its_answer_once_its_screen_rests | 12.3 | 38.1 |
| 14 | dagq（lib） infrastructure::e2e_gate::tests::a_failing_or_stopped_e2e_is_not_passed_and_is_cleaned_up | 11.8 | 13.0 |
| 15 | runtime_repair::a_live_escalation_is_recorded_at_the_recovery_jobs_request_unless_the_job_failed | 11.7 | 39.1 |
| 16 | lifecycle_install::a_handoff_looks_again_at_a_supervisor_that_took_it_as_the_wait_ran_out | 10.9 | 28.7 |
| 17 | runtime_evidence::a_diff_outside_the_e2e_paths_lands_without_e2e_unless_the_task_requires_it | 10.7 | 83.8 |
| 18 | runtime_repair::a_long_process_that_uses_cpu_time_is_not_an_idle_process_alert | 10.7 | 25.8 |
| 19 | runtime_precheck::precheck_conflicts_up_to_the_conflict_only_limit_retry_the_task_with_its_branch | 10.5 | 139.7 |
| 20 | runtime_integrate::the_push_follows_the_repository_table_of_dagq_toml | 9.9 | 65.6 |
| 21 | runtime_screen_idle_input::a_markerless_revised_session_gets_its_answer_once_its_screen_rests | 9.9 | 29.3 |
| 22 | runtime_headless::a_silent_turn_is_stopped_and_its_recovery_job_resumes_the_session | 9.6 | 59.3 |
| 23 | runtime_resume::a_resumed_session_that_ignores_exit_is_let_go | 9.5 | 33.9 |
| 24 | runtime_stall::a_session_idle_after_its_nudge_gets_one_stalled_ask_and_its_answers_are_applied | 9.5 | 30.2 |
| 25 | runtime_claim_defer::a_task_meeting_a_run_on_a_hotspot_waits_and_the_next_one_is_claimed | 9.4 | 94.1 |
| 26 | runtime_stall_recovery::a_stall_past_its_three_recovery_jobs_is_asked_without_a_fourth | 9.3 | 21.8 |
| 27 | runtime_repair::a_process_without_cpu_progress_is_an_idle_process_alert_for_the_recovery_job | 8.8 | 32.5 |
| 28 | runtime_review::a_run_the_supervisor_lands_closes_its_landing_ask | 8.8 | 58.2 |
| 29 | runtime_resume::resuming_stops_after_three_attempts | 8.7 | 55.8 |
| 30 | runtime_stale_receipt::a_stale_receipt_left_during_a_wait_is_unchanged | 8.6 | 56.8 |

binaryの指定の無いものは`dagq::it`。#17は32回のうち5回しか現れていない（新しいtest）。

### moduleごとの合計（`dagq::it`、上位15）

| module | 合計（秒） | 本数 | 5秒以上 | 1本の平均 |
| --- | --- | --- | --- | --- |
| runtime_resume | 218.4 | 29 | 29 | 7.5 |
| runtime_review | 210.6 | 44 | 12 | 4.8 |
| runtime_integrate | 142.3 | 31 | 12 | 4.6 |
| runtime_session | 88.4 | 30 | 3 | 2.9 |
| runtime_triage | 73.0 | 23 | 2 | 3.2 |
| runtime_stall_recovery | 69.7 | 11 | 11 | 6.3 |
| runtime_screen_idle_input | 65.8 | 8 | 7 | 8.2 |
| plan_review | 64.2 | 29 | 0 | 2.2 |
| runtime_adopt | 62.7 | 19 | 2 | 3.3 |
| cli_version | 57.3 | 7 | 3 | 8.2 |
| runtime_repair | 56.3 | 7 | 7 | 8.0 |
| runtime_stall | 55.9 | 8 | 5 | 7.0 |
| runtime_waiting_stages | 54.5 | 4 | 4 | 13.6 |
| runtime_claim | 54.1 | 24 | 4 | 2.3 |
| runtime_resume_exit_retry | 53.5 | 7 | 7 | 7.6 |

`dagq::it`全体では823本・2,656秒で、うち`runtime_*`が474本・2,116秒。`dagq::it`以外（libのunit test、brokerのcrate）は98秒。束ねた群の合計は次のとおり。

| 群 | 本数 | 合計（秒） |
| --- | --- | --- |
| stall系: runtime_stall・runtime_stall_recovery・runtime_screen_idle・runtime_screen_idle_input・runtime_repair・runtime_queue_hold*・runtime_headless | 62 | 372 |
| resume/exit系: runtime_resume・runtime_resume_exit_retry・runtime_exit_retry・runtime_resume_adopt・runtime_adopt | 72 | 421 |
| lifecycle_*とcli_version | 92 | 187 |

## 2. 上位の30本の待ちの内訳

### 手順

- 2026-09-29 06:34〜06:41 JSTに、このrunのworktree（base 68eb828）で、1章の集計の上位32本のうち`dagq::it`の30本（1章の表の#6のbrokerと#14のlibを除き、31位の`runtime_resume_exit_retry::used_up_retries_over_a_dialog_…`と32位の`worker_escalation::missing_evidence_…`を足した）を1本ずつ直列に流した。コマンドは`cargo nextest run --locked --test it --no-capture -E '<30本のtest(=…)>'`。`--no-capture`なので直列になる。
- instrumentなしのdebug buildで流した。loadは開始時10.4、終了時35.7で、並行する他のrunの負荷の下にある。
- 30本の合計は405.4秒（nextestの`Summary`は405.6秒）で、integrateの中央値の合計377.2秒の1.08倍だった。instrumentなし・直列・loadが高め、という条件の違いが打ち消し合い、ほぼ同じ規模になっている。内訳の割合はintegrateにも当てはまるとみてよい。
- **commitしない計測用の変更**を`tests/it/runtime_support/mod.rs`の`fixture()`と`tests/common/lifecycle.rs`の`fixture()`に入れた。内容は、git部分・`SqliteQueue::init`・taskの追加のミリ秒をstderrに出すことと、`MEASURE_KEEP`でtempdirを消さないこと。計測後に`git checkout -- tests/`で戻した。
- (a)と(b)は、残したqueue DBの`run_events`（`created_at`はミリ秒の精度）で、隣り合うeventの間が0.45秒以上の区間を拾った。それをtestとsrcの閾値と照らして振り分けた。
- (d)は測った全体から(a)(b)(c)を引いた残りである。
- cli_versionの2本はfixtureを使わず、DBも残らない。testのsourceにある固定のsleep、`--update-interval 1`、引き継ぎの見張りの条件（次の段落）から見積もった。

`cli_version`と`lifecycle_install`の引き継ぎの見張りは、新しいsupervisorの`heartbeat_at`（unix秒）が最初に見た値より大きくなるまで待つ（`src/application/update.rs`の`observe`）。heartbeatは2秒おきなので、新しいsupervisorが起動して最初のheartbeatを書いた後に、最大約2秒（pollとunix秒の切り上げを足した分）待つ。下の表のcli_versionの(a)は、この待ちを1回2〜4秒と置いた上限寄りの推定である。

### testごとの内訳（ローカルで直列、秒）

| test | 全体 | (a) 閾値 | (b) 秒の境界 | (c) fixture | (d) 処理 | (a)(b)の中身 |
| --- | --- | --- | --- | --- | --- | --- |
| cli_version::auto_update_… | 36.7 | 13（上限寄りの推定） | 0 | 0.4（推定） | 23.3 | 固定のsleep 3秒×2、`--update-interval 1`の待ち、引き継ぎの見張りの2〜4秒×約2 |
| cli_version::install_hands_… | 12.2 | 5（上限寄りの推定） | 0 | 0.4（推定） | 6.8 | 引き継ぎの見張り |
| lifecycle_install::a_handoff_looks_again_… | 12.1 | 0 | 1.1 | 2.5（5回） | 8.5 | 次のunix秒のheartbeatを待つ1.1秒のsleep |
| runtime_claim_defer::a_task_meeting_a_run_on_a_hotspot_… | 10.1 | 1.1 | 0 | 0.67 | 8.3 | `defer_max_secs = 1`を越える1.1秒のsleep |
| runtime_evidence::a_diff_outside_the_e2e_paths_… | 7.5 | 0 | 0 | 1.13（2回） | 6.4 | — |
| runtime_headless::a_silent_turn_is_stopped_… | 9.7 | 5.3 | 0 | 0.63 | 3.8 | `turn_silence_secs: 5` |
| runtime_integrate::the_push_follows_… | 12.1 | 0 | 0 | 3.03（3回） | 9.1 | — |
| runtime_precheck::precheck_conflicts_up_to_… | 11.2 | 0 | 0 | 0.57 | 10.6 | — |
| runtime_repair::a_live_escalation_… | 11.7 | 2.0 | 2.0 | 1.93（2回） | 5.8 | `background_alert_secs: 1`。切り捨ての比較で約2秒かかる（2回） |
| runtime_repair::a_long_process_that_uses_cpu_time_… | 10.3 | 7.2 | 0 | 0.73 | 2.4 | `idle_process_secs: 2`を何度か越えるまでstubが動く |
| runtime_repair::a_process_without_cpu_progress_… | 8.0 | 4.0 | 1.0 | 0.47 | 2.5 | `idle_process_secs: 2`、recoveryの前の1秒 |
| runtime_resume::a_request_lost_twice_… | 17.3 | 6.3 | 0 | 1.44 | 9.6 | `send_confirm_secs = 1`×6（送信の確認と再送） |
| runtime_resume::a_resumed_session_gets_its_request_only_once_… | 9.4 | 2.1 | 0 | 0.60 | 6.7 | `registration_timeout` 2秒 |
| runtime_resume::a_resumed_session_that_ignores_exit_… | 9.9 | 1.0 | 0 | 0.75 | 8.2 | `exit_timeout` 1秒 |
| runtime_resume::conflict_only_resumes_… | 15.2 | 5.4 | 0 | 1.04 | 8.8 | resumeのtimeout 1秒×5 |
| runtime_resume::resuming_stops_after_three_attempts | 10.4 | 1.1 | 0 | 1.43 | 7.9 | resumeのtimeout |
| runtime_resume_exit_retry::used_up_retries_over_a_dialog_… | 9.7 | 1.0 | 0 | 1.28 | 7.4 | `exit_timeout` 1秒 |
| runtime_review::a_failed_review_closes_the_session_… | 15.7 | 0 | 0 | 2.84（4回） | 12.9 | — |
| runtime_review::a_failed_review_span_ends_… | 15.4 | 4.2 | 0 | 3.58（4回） | 7.6 | stubの`slow_exit`の`sleep 1`×4 |
| runtime_review::a_run_the_supervisor_lands_… | 15.3 | 0 | 0 | 2.45（2回） | 12.9 | — |
| runtime_screen_idle_input::a_markerless_resumed_… | 14.5 | 2.0 | 3.8 | 2.07 | 6.6 | `screen_idle_secs: 1`×2と、2回のcaptureが秒の境界をまたぐ待ち、stubの`sleep 1.1` |
| runtime_screen_idle_input::a_markerless_revised_… | 11.5 | 2.0 | 3.9 | 1.82 | 3.8 | 同上 |
| runtime_stale_receipt::a_stale_receipt_left_during_a_wait_… | 12.0 | 0 | 1.1 | 1.07 | 9.8 | 秒単位の時計の1.1秒のsleep（task 931と同じ種類） |
| runtime_stall::a_session_idle_after_its_nudge_… | 11.3 | 6.2 | 1.0 | 1.33 | 2.8 | `idle_without_receipt_secs: 1`を段ごとに約6回 |
| runtime_stall::an_input_the_supervisor_did_not_send_… | 21.8 | 10.5 | 0 | 3.97（2回） | 7.3 | 閾値3秒を越えるstubの`sleep 5`×2 |
| runtime_stall_recovery::a_stall_past_its_three_recovery_jobs_… | 13.2 | 9.6 | 0 | 1.54 | 2.1 | `idle_without_receipt_secs: 1`×9段 |
| runtime_waiting_stages::a_question_while_resuming_… | 18.4 | 8.0 | 0 | 1.10 | 9.3 | `HELD` 8秒の固定のsleep（`STAGE_TIMEOUT` 6秒を越えるため） |
| runtime_waiting_stages::a_question_while_revising_… | 15.6 | 7.9 | 1.1 | 1.33 | 5.3 | `HELD` 8秒と1.1秒のsleep |
| runtime_waiting_stages::a_resume_question_past_the_limit_… | 18.3 | 8.0 | 0 | 0.71 | 9.6 | `HELD` 8秒 |
| worker_escalation::missing_evidence_and_a_scope_violation_… | 8.9 | 0 | 0 | 1.00（2回） | 7.9 | — |
| **合計** | **405.4** | **112.9（28%）** | **15.0（4%）** | **43.8（11%）** | **234.0（58%）** | |

読み方:

- **(a)は閾値を1秒にしたtestで効く。** stall系のtestは、1秒の閾値を段ごとに何回も待つ。runtime_stall_recoveryは9段で9.6秒、runtime_stallは約6段で6.2秒だった。閾値そのものは1秒でも、「越えないこと」を確かめる固定のsleep（1.5秒・2.5秒）と、stubの`sleep 5`がそれに上乗せされる。
- **(b)は1本あたり1〜4秒と小さい。** ただし、sleepの長さに1.1秒・2.5秒といった「次の秒へ」の余裕を足す形で(a)にも混ざっている。例: screen idleは、unix秒のspanに2回以上のcaptureが要るので、testは`SCREEN_IDLE_SECS`×2.5秒寝る。
- **(c)は1回0.3〜2.3秒で、複数のcaseをloopするtestで効く。** runtime_reviewの2本は4回呼ぶので2.8〜3.6秒、runtime_stallの1本は2回で4.0秒。
- **(d)の中の決まった区間**: どのtestでも、fixtureの後からsupervisorの`stall_config_loaded`までの0.45〜2.7秒が繰り返し現れる（supervisorの起動と、testの前段のsupervise）。review stubがverdictを出さずにretryする区間（0.3〜0.6秒）も多い。この区間を含め、(d)はまだevent単位でしか分けていない。

## 3. fixtureの1回あたりの時間と、templateの効果の見積もり

### 測った値（2章と同じ流し方。load 10〜45）

| 部分 | 回数 | 中央値 | p90 | 範囲 |
| --- | --- | --- | --- | --- |
| `runtime_support::fixture()`のgit（`init`・`config`×2・`add`・`commit`の5回の起動と書き込み） | 40 | 176ms | — | 130〜466ms |
| `runtime_support::fixture()`の`SqliteQueue::init`（53本のmigration、WAL） | 40 | 698ms | 1,561ms | 168〜1,869ms |
| `add_ready_task` | 40 | 2ms | — | 1〜6ms |
| `common::lifecycle::fixture()`のgit | 5 | 160ms | — | 140〜250ms |
| `common::lifecycle::fixture()`の`SqliteQueue::init` | 5 | 287ms | — | 229〜585ms |

`SqliteQueue::init`は、最初の1本（load 10）では168msだった。loadが35〜45になった後半の本では0.9〜1.9秒になり、loadに強く左右される。migrationごとのcommitのfsyncなど、I/Oを待つ部分が多いとみられる（未確認）。

templateのコピーの時間を、残したfixtureで測った（06:42 JST、load 35）。20回の中央値は次のとおり。

| コピーするもの | 大きさ | 中央値 |
| --- | --- | --- |
| migrate済みのDB（`queue's data.db`） | 336KB | 0.7ms（`shutil.copy`） |
| seedのcommit済みのrepository | `.git` 192KB。多くはgitの既定のhookのsample | 65ms（`copytree`） |

同じ時刻（06:42 JST、load 35）に、同じ手順のgitのfixtureを10回流すと中央値133msだった。hookを含めないtemplate（`git init --template=`の空のもの）にすれば、repositoryのコピーはさらに縮む見込みである。

### templateの案と注意

[nextest-measurement](nextest-measurement.md)の案は、共通のtemplateのrepositoryとmigrate済みのDBを1回作り、testごとにそれをコピーするものである。nextestはtestを1本ずつ別processで流すので、`OnceLock`のようなprocessの中の共有では効かない。templateはdiskに1つ置く。

- 置き場所はtarget dirか一時dirにし、`SqliteQueue::SCHEMA_VERSION`とmigrationの中身のhashを名前に入れる
- 作るときはfile lockで1つのprocessだけが作る
- DBはWALのcheckpointの後にcloseした単一のfileをコピーする

seedのcommitのhashは全testで同じになる。testは別々のtempdirで流れるので、hashが同じでも衝突しない。ただ、hashを数字で書いているtestがないかは、F1のtaskで確かめる。

### 見積もり

- **fixtureを呼ぶ回数**: `tests/it`と`tests/common`の`fixture()`の呼び出し箇所は569。caseのloopで呼ぶ回数は数えていないので、`SqliteQueue::init`は1回の全体のtestで約650回と置く。`tests/common/queue.rs`のfixtureと`plan_review.rs`のfixtureも含む。
- **`SqliteQueue::init`をコピーに置き換える**: 1回0.17〜0.70秒が縮む（上のload 10と中央値）。650回で110〜455秒。
- **gitをrepositoryのコピーに置き換える**: 1回約0.1秒（176msから65ms）。約570回で約60秒。
- **testの時間の合計**: 170〜515秒（合計2,780秒の6〜19%）。
- **test段**: 6並列で28〜85秒。

幅が大きいのは、integrate（instrumentあり・6〜8並列・load 20前後）での`SqliteQueue::init`の時間を、まだ直接測っていないためである。F1のtaskは、最初にinitの時間を関門と同じ条件で測ってから直す。

## 4. tests/の固定のsleepと、秒単位の閾値・timeoutの棚卸し

base 68eb828のtests/を読んで数えた。値の名前と行は変わりうるので、修正のtaskは同じgrepで数え直す。

### 固定のsleep（Rustの`thread::sleep`の定数）

| 値 | 箇所 | 主な中身 |
| --- | --- | --- |
| 10〜300ms | 88 | ほぼpollのloopの間隔（`TEST_TICK` 20msなど）。固定の待ちとしては小さい |
| 500〜600ms | 15（合計約8秒） | runtime_reviewの`HOLD_PERIOD` 600ms×6、runtime_adopt・runtime_session・runtime_screen_idle_inputの500ms |
| 1.0秒 | 4 | runtime_sessionの`prompt_wait`（300ms）を越えてもcaptureしないことの確認×3、runtime_exit_retry×1 |
| 1.1秒 | 12 | 次のunix秒へ進める待ち。runtime_review・runtime_review_adopt・runtime_waiting_stages・runtime_stale_receipt（idle markerや依頼の時刻より後の秒）、runtime_stall・runtime_stall_recovery（`idle_without_receipt_secs` 1を越える）、runtime_claim_defer（`defer_max_secs` 1）、plan_review×2（plannerのtimeout）、lifecycle_installのhelper×3（次の秒のheartbeat） |
| 1.5秒 | 8 | 1秒の閾値を越えても何も起きないことの確認（runtime_stall×3、runtime_stall_recovery×2、runtime_review、runtime_adopt、runtime_open_turn） |
| 2.0秒 | 1 | runtime_handoff（次のprocessが何周かしても何も起きない） |
| 2.5秒 | 9 | `screen_idle_secs` 1の2.5倍（runtime_screen_idle×2、runtime_screen_idle_input×3）、1秒の閾値の後に何も起きない確認（runtime_stall×2、runtime_adopt、runtime_stall_recovery） |
| 3.0秒 | 4 | runtime_review_questions（`resume_timeout`を越えるidle）、inbox_watcher（watcherが生きている確認）、cli_version×2（無関係なcommitで更新が始まらない確認） |
| 8.0秒 | 3 | runtime_waiting_stagesの`HELD`（`STAGE_TIMEOUT` 6秒を越える） |

1秒以上の固定のsleepは41箇所で、各箇所が1回ずつ走るとして合計約90秒になる（helperやloopの中の箇所は1回の全体のtestで何度も走りうるので、実際はそれ以上）。多くは、閾値を越えても何も起きないことを確かめる「負の確認」である。

### stubのscriptの中の、待ち切られるsleep

pollの`sleep 0.05`（約100箇所）と、killされるまでの`sleep 30`〜`600`は除く。

| 場所 | sleep | 中身 |
| --- | --- | --- |
| runtime_stall | `sleep 5` | `idle_without_receipt_secs: 3`の下で5秒働く（2 case） |
| runtime_review_questions | `sleep 3` | `slow_revise`でreviewのverdictを3秒遅らせる |
| runtime_screen_idle_input×2 | `sleep 1.1` | 問いをmarkerより後の秒にする |
| runtime_session×3 | `sleep 1` | sessionが自分で終わる・answerの後にidleに戻る |
| runtime_review | `sleep 1` | `slow_exit`（4 caseで4秒） |
| runtime_adopt | `sleep 1` | idleの後に自分で終わる |

### testが入れる秒単位の閾値（StallConfig、`*_secs: i64`）

StallConfigの値は`src/domain/stall.rs`のi64の秒である。

- `dagq.toml`の`[stall]`から読むときは`src/infrastructure/run_env.rs`の`parse_positive`が正の整数に限る。test（`SuperviseOptions.stall`）から直接入れる値は検査されない。
- 比較は秒に切り捨てる: `secs_between`は`as_secs`で、screen idleのspanはunix秒で比べる。
- そのため、閾値を1にしても実際の待ちは1〜2秒になる。

| 閾値 | testで使う値 | 使うtestの例 |
| --- | --- | --- |
| `idle_without_receipt_secs` | 1（一部2・3） | runtime_stall 8本、runtime_stall_recovery 約9本、runtime_queue_hold*、runtime_provider_switch 1本 |
| `send_confirm_secs` | 1 | runtime_stall_recovery 2本、runtime_resume 2本。`dagq.toml`の`[stall]`経由で入れるものもある（runtime_review・runtime_resume・runtime_session） |
| `screen_idle_secs` | 1 | runtime_screen_idle 9本、runtime_screen_idle_input 8本（planner系の5は偽の時計で進めるので待たない） |
| `background_alert_secs` | 1 | runtime_repair |
| `idle_process_secs` | 2 | runtime_repair |
| `turn_silence_secs` | 5 | runtime_headless 1本 |
| `turn_limit_secs` | 1 | runtime_headless 1本 |

stall以外の秒単位の設定:

| 設定 | 値 | 使うtest |
| --- | --- | --- |
| `[conflicts] defer_max_secs` | 1 | runtime_claim_defer |
| `--update-interval` | 1 | cli_version |
| plannerのtimeout | i64の秒で比べる | plan_review |
| supervisorのheartbeat | 2秒おき、`heartbeat_at`はunix秒 | 引き継ぎの見張り |

### TestWorkspaceのtimeout（`Duration`）

`tests/it/runtime_support/mod.rs`の`TestWorkspace`の既定の値は次の4つで、どれもsrcでは期限（`elapsed() >= timeout`）として使われる。

| timeout | 既定 | 普段の流れでの扱い |
| --- | --- | --- |
| `exit_timeout` | 120秒 | 上限で、待ち切られない |
| `registration_timeout` | 45秒 | 同上 |
| `resume_timeout` | 120秒 | 同上 |
| `prompt_wait` | 90秒 | 最初のdialogのcaptureまでの猶予。dialogを扱うtestは300msにする |

待ち切るのは、testが短くしたものだけである。

| timeout | 短くした値 | 使うtest |
| --- | --- | --- |
| `exit_timeout` | 1秒 | runtime_exit_retry×7、runtime_adopt×4、runtime_resume_exit_retry×3、runtime_triage×3など約20本 |
| `exit_timeout` | 500ms | runtime_session×2、runtime_waiting×2 |
| `exit_timeout` | 3秒 | runtime_triage |
| `resume_timeout` | 1秒 | runtime_resume×2、runtime_review_questions |
| `resume_timeout` | 2秒 | runtime_review_questions |
| `resume_timeout` | 3秒 | runtime_review |
| `resume_timeout` | 6秒（`STAGE_TIMEOUT`） | runtime_waiting_stages×3 |
| `registration_timeout` | 1秒 | runtime_ask |
| `registration_timeout` | 2秒 | runtime_resume |

これらはもう`Duration`なので、srcを変えずにtestで短くできる。ただし値の多くは「負荷の下でもsessionが間に合う」ことから決めてある（例: `STAGE_TIMEOUT`）。短くすると負荷の下でflakyになりうる。

## 5. 修正の候補と見積もり

見積もりの前提は次のとおり。

- testの時間の合計は1章の中央値（2,780秒）。
- test段は、今の`NEXTEST_TEST_THREADS=6`で割る。1章のtestごとの時間は29本が8並列で流れたlogで、6並列ではtest 1本の時間がやや変わりうる。
- 「縮むtestの秒」はtestの時間の合計の減り。2章の割合を、同じ待ちを持つ群（1章の束ね）に当てて広げた値である。
- 候補どうしで重なる分（例: F2で閾値を短くすれば、F4の固定のsleepも短くできる）は、それぞれに数えた。合計は上限の目安として読む。

| 候補 | 原因 | 中身 | srcの変更 | 縮むtestの秒（合計） | test段（÷6） |
| --- | --- | --- | --- | --- | --- |
| F1 | (c) | fixtureのtemplate（migrate済みのDBとseedのcommit済みのrepositoryを、disk上に1回作ってコピーする。3章） | なし（tests/だけ） | 170〜515 | 28〜85 |
| F2 | (a)(b) | 秒未満のStallConfigの閾値をtestから入れる経路と、秒の切り捨てで比べている箇所のミリ秒化。productionの既定値と`[stall]`の書式（正の整数の秒）は変えない | あり（test用の経路の追加） | 110〜150 | 18〜25 |
| F3 | (a) | `Duration`のtimeoutの見直し（`exit_timeout` 1秒→短く、`HELD`/`STAGE_TIMEOUT`をeventで確かめる形に） | なし | 25〜35 | 4〜6 |
| F4 | (a)(b) | 「越えても何も起きない」の固定のsleepを、supervisorのpassの回数（例: `candidates_sampled`の増え）を待つ形に置き換える。stubの`sleep 5`・`sleep 3`・`slow_exit`を閾値に合わせて縮める | なし（passの数を読む口が無ければ小さく足す） | 65〜75 | 11〜13 |
| F5 | (a)(b) | 引き継ぎの見張りとheartbeat（2秒おき・unix秒）のtest用の間隔、cli_versionの3秒のsleep | あり（test用の経路） | 15〜20 | 約3 |
| F6 | (d) | 処理の内訳の測定。supervisorの起動からstall_config_loadedまでの0.5〜2.7秒、1回のpassの中のgitと`ps`の起動の回数、review stubのretry | 測定だけ | —（F6の結果で決める） | — |

各候補の見積もりの根拠:

- **F1**は3章のとおり。
- **F2**の対象はstall系の群（62本・372秒）と、runtime_resumeの`send_confirm_secs`のtest。
  - 2章のstall系の9本では、(a)と(b)が112秒のうち60.5秒（54%。(a)だけで48.8秒、44%）だった。群全体では閾値の短いtestばかりではないので、(a)(b)を控えめに35〜45%と置くと130〜170秒になる。
  - 閾値を1秒から0.2秒前後にすると、その約80%が縮む。runtime_resumeの`send_confirm`の分（約6秒×2本）も足す。
- **F3**のうち大きいのは`HELD` 8秒×3（24秒）で、eventで確かめる形にすれば約15秒縮む見込み。`exit_timeout` 1秒の約20本は、0.5秒にして約10秒縮む見込み。
- **F4**の対象は、1秒以上の固定のsleepの約90秒と、stubで待ち切るsleepの約20秒。そのうち60〜70%（66〜77秒）をpassの回数の待ちに置き換えられるとみて、65〜75秒とした。F2と重なる分がある。
- **F5**の対象はcli_versionとlifecycle_installの引き継ぎのtestで、1回の引き継ぎの2〜4秒のうち約半分が縮む。

これらを合わせると、test段で約64〜132秒（6並列の約460秒の13〜28%）になる。(d)が58%を占めるので、F6の測定の結果によってはそれより大きい候補が出うる。

### 守ること（goal 68の制約）

- productionの閾値とtimeoutの値と意味を変えない。F2とF5は、testから差し替える経路を足すだけにする。
- testが確かめる中身を弱めない。F4で固定のsleepを置き換えるときも、「越えても何も起きない」ことは、越えた後のpassを待って確かめる。
- 足した・変えたtestは、AGENTS.mdのstress（5周）を通す。
- 前後の比較は、この文書の1章と同じ手順（直近の32回のintegrateのlog、load1を併記）で行う。

## 6. 残っている不確かさ

- **integrateの条件でのfixtureの時間**: instrumentあり・6〜8並列の下での`SqliteQueue::init`は直接測っていない（3章）。
- **(d)の中身**: supervisorの起動、pass、stubのsessionの時間を分けていない（F6）。
- **cli_versionの2本**: DBが残らないので、(a)は推定である。
- **ローカルの測定の条件**: 直列で、instrumentなしで、load 10〜45だった。integrateの条件（6〜8並列、instrumentあり、load中央値19）とは違う。30本の合計はintegrateの中央値の1.08倍で、規模は合っている。

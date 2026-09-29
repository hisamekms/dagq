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

この文書はsrc/とtests/を変えない。修正はfollow-upのtaskが行う（5章と7章）。7章はtask 1049（F6）が(d)処理の内訳を足したものである。productionのtimeoutと閾値の値も変えない（goal 68の制約）。

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

  合わせて約64〜132秒で、6並列のtest段の約460秒（2,780÷6）の13〜28%にあたる。
- **testの時間の約半分はtest processが起動する`git`の待ちである（7章、F6。wrapperありの回で312秒のうち158秒）。** `/usr/bin/git`のxcrunのshimが1回の起動を約2倍にし、supervisorは毎passで着地のbranchを`git`2本で確かめる。supervisorの起動の大半は、testが書いたばかりの`claude-stub`の最初のexec（macOSが新しい実行fileに0.2〜1.3秒かける）である。候補G1〜G3で、test段は合わせて約53〜99秒（G4を足すとさらに10〜22秒）縮む見込み。

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
| F6 | (d) | 処理の内訳の測定。supervisorの起動からstall_config_loadedまでの0.5〜2.7秒、1回のpassの中のgitと`ps`の起動の回数、review stubのretry | 測定だけ | —（7章。結果は候補G1〜G4） | — |

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
- **(d)の中身**: 7章（F6）で分けた。7章の実験は各3回で、loadによる合計の揺れ（±45秒）が単独の候補の差と同じ規模なので、単独の効果は仕組みごとの数字と合わせて読む。
- **cli_versionの2本**: DBが残らないので、(a)は推定である。
- **ローカルの測定の条件**: 直列で、instrumentなしで、load 10〜45だった。integrateの条件（6〜8並列、instrumentあり、load中央値19）とは違う。30本の合計はintegrateの中央値の1.08倍で、規模は合っている。

## 7. (d)処理の内訳（F6、task 1049）

2章の(d)（上位30本の58%）を、関門と同じ条件（instrumentあり・6並列）で分けた。F1（fixtureのtemplate）とF2（秒未満の閾値）が着地した後のbase a494154で測ったので、2章の数字（F1・F2の前）とは全体の秒が違う。

### 要点

- **testの時間の約半分は、test processが起動する`git`の待ちである。** 30本で`git`を7,873回起動し、合計158秒（1回の平均20ms）だった。同じ回（r4）の30本の合計312秒の51%にあたる。loopの中の`git`は閾値を待つ区間（(a)(b)）にも走るので、全部が(d)に入るわけではない。そのうち4,046回（79秒）は、supervisorが毎passで着地のbranchを確かめる`git symbolic-ref`と`git show-ref`の2本である。1回のpassで起動する`git`は平均4.15回だった（r4の1,555 pass）。
- **`/usr/bin/git`はCommand Line Toolsのxcrunのshimで、1回の起動を約2倍にする。** 単独で測ると中央値16ms、実体の`/Library/Developer/CommandLineTools/usr/bin/git`は8〜10msだった。
- **supervisorの起動（fixtureの後から`stall_config_loaded`まで）の大半は、testが書いたばかりの`claude-stub`の最初のexecである。** macOSは新しく書かれた実行fileの最初のexecに0.2〜1.3秒かける。すでに実行したfileへのsymlinkとhardlinkは6ms前後で済む。1つのtest processの最初のagentのpreflight（`claude-stub --version`）は中央値228〜313ms（r3とbaseの3回）だった。30本の起動の合計18〜27秒のうち、14〜18秒がこれにあたる（stubの実験で縮んだ分）。
- **review stubがverdictを出さずにretryする分は、30本で約15〜22秒だった。** 既定のreviewer（`claude-stub`）は`test provider`としか出さないので、reviewは毎回2回目の試行まで走る。`review_retried`は44回で、1回0.3〜0.4秒だった（2回目以降の試行は54回あり、残りの10回はresumeの後のreviewの試行である）。
- **psは小さい。** 157回・2.8秒（passあたり0.1回）で、修正の候補にしない。fixtureはF1の後で40回・0.7秒になった。
- **検証の実験（commitしない変更、各3回）では、git・着地のbranch・stubの3つを合わせると、30本の合計が中央値296秒から212秒に縮んだ（−29%）。** 合わせたときのloadは、比べた基準と同じか高い（load1の平均22〜32、基準は18〜21）。

### 手順

- 2026-09-29 09:16〜09:36 JSTに、このrunのworktree（base a494154）で流した。対象は2章の30本（1章の上位32本のうち`dagq::it`の30本）である。
- コマンドは`cargo llvm-cov nextest --locked --workspace --no-report --test it -E '<30本のtest(=…)>'`である。`NEXTEST_TEST_THREADS=6`・`RUST_TEST_THREADS=6`と、`dagq.toml`の`[run.env]`の`RUSTC_WRAPPER`・`CARGO_BUILD_JOBS=4`を渡した。関門と同じく、instrumentありで6並列になる。
- loadは`~/.local/share/dagq-hostmetrics/metrics.csv`の`load1`を使い、各回のnextestの開始から終了（前後15秒を含む）の平均と最大を取った。並行する他のrunの負荷の下で流した。
- **commitしない計測用の変更**を次のとおり入れ、計測後に`git checkout -- src tests`で戻した。
  - `src/lib.rs`に、時刻（ミリ秒）・pid・`NEXTEST_TEST_NAME`・印を1行ずつ`$MEASURE_DIR/trace.log`に書く関数を足した。
  - `src/application/supervise/mod.rs`で、supervisorの入口・cmuxとagentのpreflightの後・queueを開いた後・`stall_config_loaded`・loopの始めと終わりに印を書いた。loopの1回のpassの中の区切り（先頭、`check_provider_holds`の後、`return_waiting_runs`の後、`fill_slots`の後、plan reviewの後、`tick`の後）にも印を書いた。
  - `tests/it/runtime_support/mod.rs`で、`fixture()`の始めと終わり、`supervise_with`と`supervise_reviewed`の入口、`StubSpawner`のspawnとstubの終了に印を書いた。`Fixture`のdropでは、queue DBを`$MEASURE_DIR/db/`にcopyした。sessionとreviewの時間は、このDBの`session_opened`・`session_closed`・`review_retried`から読んだ。
- `git`・`ps`・`lsof`の起動は、PATHの先頭に置いた同名の小さなwrapper（実体を子processで起動して待ち、開始時刻・所要時間・親のpid・引数を`spawn.log`に書く）で数えた。runtimeは`git`の場所をcanonicalizeするので、wrapperはsymlinkではなく実行fileのcopyにした。親のpidがtest processのものを「test processの起動」（in-processのsupervisorとtestのhelper）、それ以外を「子の起動」（stubのscriptが打つ`git commit`など）として分けた。
- wrapperは1回の起動に数ms（1回の追加のexec）を足す。wrapperありの回（r2 382秒、r4 312秒）は、なしの回（r3 272秒）より長かったが、wrapperなしのbaseの3回も292〜381秒に揺れたので、差のどれだけがwrapperによるかは分けられない。そのため、起動の回数と所要時間はwrapperありの回（r4）から、それ以外の秒はwrapperなしの回（r3）から取った。所要時間はwrapperの中で実体の起動から終了までを測ったもので、wrapper自身の起動は含まない。

### 30本の内訳（r3: wrapperなし、load1 平均15.5・最大18.9。起動の回数と秒はr4: wrapperあり、load1 平均15.5・最大19.6）

| 部分 | 回数 | 秒 | 1回あたり |
| --- | --- | --- | --- |
| testの時間の合計 | 30本 | 272.5 | — |
| fixture（F1のtemplateのcopy） | 40 | 0.7 | 18ms |
| `supervise_with`・`supervise_reviewed`の入口から、supervisorの入口まで（portsの組み立て。`git`を約6回起動する） | 55 | 5.7 | 103ms |
| supervisorの入口から`stall_config_loaded`まで | 70 | 19.2 | — |
| 　うちtest processの最初のagentのpreflight（`claude-stub --version`の最初のexec） | 29 | 13.9 | 中央値228ms |
| 　うち2回目以降のpreflight | 41 | 5.1 | 中央値17ms |
| 　うちqueueを開いてから`stall_config_loaded`まで | 70 | 0.3 | 4ms |
| supervisorのloop | 1,762 pass | 202.5 | passの中央値69ms・p90 244ms |
| 　pass先頭〜`check_provider_holds`（heartbeat、host metrics、run.env、**着地のbranch**、conflicts、disk、hold） | 1,762 | 59.5（29%） | 平均34ms |
| 　`fill_slots`（claim・resume・triage・着地） | — | 19.7（10%） | — |
| 　`tick`（slotごとのsessionの見張り・validating・reviewの起動） | 1,670 | 45.3（22%） | 平均27ms |
| 　`tick`の後の`TEST_TICK`（20ms）のsleep | 1,670 | 42.4（21%） | — |
| 　slotが空のときのidleのsleep（うち28秒はcli_versionの子のsupervisor） | — | 31.1（15%） | — |
| test processの`git`の起動（r4） | 7,873 | 158.3 | 平均20.1ms |
| 　うち着地のbranch（`symbolic-ref --quiet refs/remotes/origin/HEAD`と`show-ref --verify --quiet`） | 4,046 | 79.0 | 19.5ms |
| 　うちloopの中 | 6,449 | 130.3 | passあたり4.15回（r4の1,555 pass） |
| 子の`git`の起動（stubのscriptのcommitなど、r4） | 375 | 9.6 | 25.6ms |
| test processの`ps`の起動（`running()`、idle processの見張りなど、r4） | 157 | 2.8 | 17.6ms（passあたり0.10回） |
| test processの`lsof`の起動（r4） | 20 | 2.6 | 129ms |
| reviewのjob（`session_opened`から`session_closed`まで） | 115 | 38.7 | 0.34秒 |
| 　うち2回目以降の試行（うち`review_retried`が44回、残りはresumeの後の試行） | 54 | 18.1 | 0.34秒 |
| workerのsession（stubの起動からsessionの終わりまで） | 61 | 63.6 | 1.04秒 |
| resumeのsession | 23 | 45.8 | 1.99秒 |
| reviseのsession | 2 | 14.4 | 7.2秒 |

読み方:

- sessionとreviewのjobの時間は、supervisorのloopと重なる。loopはそれを待っているので、足し合わせない。
- sessionの時間の多くはstubの`sleep`と、閾値を越えるまでの待ち（2章の(a)(b)）である。sessionの中のgitは子の`git`（375回・9.6秒）で、下のG1のshimの分が効く。
- 着地のbranchの確かめは、passごとに毎回2本の`git`を起動する。loopの中の`git`（6,449回）の約6割はこれである。残りは、`tick`のreviewの材料（1回のreviewの起動に`diff`×3・`log`・`rev-parse`・`status`・`worktree list`などの約8本）と、claim・resume・着地の`git`である。
- `git`の1回の起動（20ms）の約半分はxcrunのshimである。load 14〜16で単独に40回ずつ測ると、`/usr/bin/git`は中央値16ms、実体は8〜10msだった。`ps`と`/usr/bin/true`の起動は約4msだった。
- 新しく書いたfileの最初のexecの時間を、単独で測った（load 15、8回ずつ）。書いたばかりのscriptは中央値514ms（187〜795ms）、同じ中身のcopyも525ms、すでに実行したfileへのhardlinkは6ms、symlinkは5msだった。同じpathに同じ中身を書き直したときも10msで、新しいfileだけが遅い。

### testごとの内訳（全体・起動・pass・session・reviewはr3、gitとpsの起動はr4）

| test | 全体（秒） | supervise | supervisorの起動（秒） | うち最初のagentのpreflight（秒） | pass | gitの起動÷pass（r4のpass） | gitの起動（回・秒） | psの起動（回・秒） | worker・resume・reviseのsession（秒） | reviewのjob（秒、retryの回数） |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| cli_version::auto_update_installs_each_runtime_landing_and_puts_a_b… | 37.7 | 4 | 1.93 | 1.81 | 19 | 4.5 | 85・1.6 | 4・0.20 | 0.0 | 0.0（0） |
| runtime_waiting_stages::a_question_while_resuming_waits_outside_the… | 14.2 | 2 | 0.27 | 0.23 | 174 | 3.8 | 482・10.2 | 6・0.02 | 10.8 | 2.0（3） |
| cli_version::install_hands_a_running_supervisor_over_under_its_pid_… | 13.9 | 3 | 2.49 | 2.43 | 8 | 4.2 | 34・0.6 | 3・0.13 | 0.0 | 0.0（0） |
| runtime_waiting_stages::a_resume_question_past_the_limit_waits_in_i… | 13.7 | 2 | 0.41 | 0.40 | 179 | 4.0 | 479・11.0 | 6・0.02 | 18.8 | 1.8（3） |
| runtime_waiting_stages::a_question_while_revising_waits_outside_the… | 12.2 | 1 | 0.23 | 0.23 | 178 | 3.0 | 404・8.5 | 3・0.01 | 11.9 | 0.9（0） |
| runtime_review::a_failed_review_span_ends_with_its_job_not_with_the… | 11.9 | 4 | 1.65 | 0.21 | 107 | 3.2 | 323・6.2 | 5・0.12 | 8.2 | 2.6（1） |
| runtime_resume::conflict_only_resumes_are_not_counted_and_a_used_up… | 11.9 | 2 | 0.22 | 0.21 | 74 | 6.5 | 439・9.0 | 6・0.02 | 6.1 | 1.8（3） |
| runtime_review::a_failed_review_closes_the_session_and_asks_a_perso… | 11.1 | 8 | 0.96 | 0.21 | 78 | 5.4 | 430・8.2 | 7・0.11 | 4.7 | 3.0（2） |
| runtime_headless::a_silent_turn_is_stopped_and_its_recovery_job_res… | 10.9 | 1 | 1.13 | 1.12 | 113 | 3.6 | 338・6.5 | 3・0.04 | 8.0 | 0.3（0） |
| lifecycle_install::a_handoff_looks_again_at_a_supervisor_that_took_… | 9.6 | 0 | 0.00 | 0.00 | 0 | 0.0 | 0・0.0 | 0・0.00 | 0.0 | 0.0（0） |
| runtime_claim_defer::a_task_meeting_a_run_on_a_hotspot_waits_and_th… | 8.3 | 3 | 1.46 | 1.44 | 23 | 12.8 | 295・5.7 | 10・0.03 | 0.6 | 3.8（5） |
| runtime_stall::an_input_the_supervisor_did_not_send_holds_the_nudge… | 7.7 | 2 | 0.46 | 0.23 | 81 | 3.1 | 228・4.2 | 4・0.01 | 6.2 | 1.1（2） |
| runtime_stale_receipt::a_stale_receipt_left_during_a_wait_is_unchan… | 7.6 | 2 | 0.24 | 0.21 | 47 | 6.8 | 293・5.6 | 4・0.02 | 2.7 | 1.5（2） |
| runtime_review::a_run_the_supervisor_lands_closes_its_landing_ask | 7.5 | 4 | 0.51 | 0.24 | 52 | 5.8 | 298・5.3 | 4・0.01 | 3.4 | 1.3（0） |
| runtime_screen_idle_input::a_markerless_resumed_session_gets_its_an… | 7.5 | 2 | 0.30 | 0.28 | 39 | 7.2 | 275・4.9 | 6・0.02 | 3.6 | 2.5（3） |
| runtime_resume::a_request_lost_twice_is_asked_to_the_inbox | 7.2 | 2 | 0.23 | 0.21 | 34 | 8.3 | 274・5.6 | 12・0.39 | 3.1 | 1.3（2） |
| runtime_resume_exit_retry::used_up_retries_over_a_dialog_close_a_re… | 7.2 | 2 | 0.24 | 0.21 | 46 | 6.4 | 280・5.6 | 4・0.01 | 2.6 | 1.5（2） |
| runtime_precheck::precheck_conflicts_up_to_the_conflict_only_limit_… | 6.9 | 1 | 0.26 | 0.26 | 60 | 5.3 | 382・7.8 | 7・0.03 | 5.2 | 1.8（0） |
| runtime_resume::a_resumed_session_gets_its_request_only_once_its_in… | 6.9 | 2 | 0.39 | 0.37 | 58 | 5.8 | 284・5.9 | 4・0.02 | 2.9 | 1.2（2） |
| runtime_resume::a_resumed_session_that_ignores_exit_is_let_go | 6.8 | 4 | 0.27 | 0.21 | 42 | 7.6 | 304・6.1 | 6・0.11 | 2.6 | 1.2（2） |
| runtime_evidence::a_diff_outside_the_e2e_paths_lands_without_e2e_un… | 6.8 | 2 | 2.37 | 1.28 | 25 | 7.0 | 183・3.7 | 4・0.01 | 1.2 | 1.3（2） |
| runtime_stall_recovery::a_stall_past_its_three_recovery_jobs_is_ask… | 6.7 | 1 | 0.21 | 0.21 | 70 | 3.0 | 199・3.8 | 7・0.28 | 5.5 | 0.3（0） |
| runtime_resume::resuming_stops_after_three_attempts | 6.6 | 5 | 0.27 | 0.20 | 28 | 10.5 | 273・5.7 | 6・0.11 | 1.7 | 1.2（2） |
| runtime_integrate::the_push_follows_the_repository_table_of_dagq_toml | 6.1 | 3 | 0.69 | 0.25 | 27 | 10.6 | 287・5.9 | 6・0.02 | 0.3 | 1.6（3） |
| worker_escalation::missing_evidence_and_a_scope_violation_keep_the_… | 5.6 | 2 | 0.45 | 0.23 | 14 | 18.9 | 284・6.7 | 8・0.03 | 0.4 | 2.4（4） |
| runtime_screen_idle_input::a_markerless_revised_session_gets_its_an… | 5.3 | 1 | 0.22 | 0.21 | 52 | 3.6 | 191・3.5 | 2・0.01 | 3.7 | 0.7（0） |
| runtime_repair::a_live_escalation_is_recorded_at_the_recovery_jobs_… | 5.1 | 2 | 0.75 | 0.33 | 49 | 4.7 | 212・4.4 | 6・0.19 | 2.4 | 0.5（0） |
| runtime_stall::a_session_idle_after_its_nudge_gets_one_stalled_ask_… | 3.7 | 1 | 0.23 | 0.22 | 33 | 3.4 | 101・1.8 | 4・0.10 | 3.1 | 0.6（1） |
| runtime_repair::a_process_without_cpu_progress_is_an_idle_process_a… | 3.1 | 1 | 0.20 | 0.19 | 26 | 4.5 | 109・2.3 | 7・0.49 | 2.1 | 0.2（0） |
| runtime_repair::a_long_process_that_uses_cpu_time_is_not_an_idle_pr… | 2.9 | 1 | 0.26 | 0.25 | 26 | 4.5 | 107・2.1 | 3・0.21 | 1.9 | 0.2（0） |
| **合計** | **272.5** | 70 | **19.2** | 13.9 | 1762 | — | 7873・**158.3** | 157・2.8 | 123.7 | 38.7（44） |

lifecycle_install::a_handoff_looks_again_…は`common::lifecycle`のfixtureと`dagq`の子processを使うので、計測用の印を入れていない（0になっている）。cli_versionの2本は、`dagq supervise`を子processで起動する。そのためsupervisorの起動（1本あたり1.9〜2.5秒）には、instrumentありのbinaryの起動と`claude-stub`の最初のexecが入る。

### 検証の実験（commitしない変更、各3回、交互に流した）

内訳の上位を、commitしない変更で外したときの30本の合計を測った。load1の平均と最大を添える。loadで回ごとの秒が±45秒ほど揺れるので、合計の比較は3回の中央値で読む。仕組みごとの数字（passの先頭の区切りまでの時間、supervisorの起動）は、loadの揺れにあまり左右されない。

- base: 変更なし
- landing: `check_landing_branch`を1つのprocessで1秒に1回までにした（測るための変更で、修正案ではない）
- stub: `claude_stub()`が、1回実行済みの共有のscriptへのsymlinkを置く形にした
- clt: PATHの先頭に`/Library/Developer/CommandLineTools/usr/bin`を置き、`git`の実体を直接起動させた
- all: landing・stub・cltを合わせた

| 変種 | 1回目 | 2回目 | 3回目 | 中央値 | passあたりの先頭の区切りまでの時間（平均） | supervisorの起動の合計 |
| --- | --- | --- | --- | --- | --- | --- |
| base | 380.7秒（load 18.4/27.4） | 292.0秒（20.0/25.3） | 296.3秒（20.6/23.0） | **296.3秒** | 38〜58ms | 17.8〜27.3秒 |
| landing | 358.9秒（30.8/35.6） | 305.6秒（28.7/31.2） | 255.3秒（15.2/20.1） | 305.6秒 | 8〜15ms（中央値1〜2ms） | 15.7〜22.3秒 |
| stub | 364.6秒（36.3/39.5） | 286.2秒（25.6/31.2） | 297.8秒（13.2/14.5） | 297.8秒 | 46〜59ms | **3.4〜9.7秒** |
| clt | 271.1秒（33.8/39.5） | 269.6秒（21.0/21.1） | 299.6秒（23.5/31.6） | **271.1秒** | 19〜27ms | 30.5〜47.8秒 |
| all | 196.4秒（23.3/26.8） | 211.6秒（21.9/23.0） | 240.2秒（31.9/32.2） | **211.6秒** | 7〜11ms | 3.3〜4.5秒 |

括弧の中はload1の平均/最大。30本はどの回もすべて通った。

読み方:

- **clt**は、同じかそれより高いloadで、合計が中央値で25秒（8%）縮んだ。passの先頭の区切りまでは38〜58msから19〜27ms、`tick`は平均33〜63msから22〜33msになった。`git`の1回が半分になった分である。supervisorの起動はかえって長く出た。これは`claude-stub`の最初のexecで、loadの高い時間帯に当たった揺れとみる（gitとは関係しない）。
- **landing**は、passの先頭の区切りまでを1/4〜1/5にした。ただし合計は基準との差がloadの揺れの内に収まった。閾値や固定のsleepで壁時計の時間が決まるtestでは、passが速くなると、同じ時間の中でpassの回数が増える（1,364〜1,760回から2,047〜3,227回）だけで、testの時間は縮まない。縮むのは、eventが続けて起きる区間（claim、validating、着地）だけである。
- **stub**は、supervisorの起動の合計を17.8〜27.3秒から3.4〜9.7秒に縮めた（test processの最初のpreflightは中央値237〜313msから13〜17ms）。合計の差（中央値で+1.5秒）はloadの揺れに埋もれた。
- **all**は中央値で85秒（29%）縮んだ。3つを合わせると、passが軽くなり、eventが続く区間が縮む。そのため、1つずつの差の和より大きく出た。

### 次の修正の候補と見積もり

見積もりの前提は次のとおり。

- 全体への広げ方: 30本での割合を、1章の`runtime_*`の群（474本・2,116秒。F1・F2の前の値）に当てる。上位30本は待ちの多いtestなので、群の全体では割合がこれより小さいこともありうる。そのため幅を持たせる。
- test段はtestの時間の合計の減り÷`NEXTEST_TEST_THREADS`（6）。
- workerの手元のtestとstressの5周も、同じ割合で縮む。
- どの候補もproductionのtimeoutと閾値の値と意味を変えない（goal 68の制約）。

| 候補 | 中身 | srcの変更 | 30本で縮んだ・縮む秒 | 縮むtestの秒（全体） | test段（÷6） |
| --- | --- | --- | --- | --- | --- |
| G1 | `git`の実体を1回だけ解決して使う。`/usr/bin/git`がxcrunのshimのときは、`xcrun --find git`か`git --exec-path`で実体を求め、`GitRepository`の`git`とtestのhelperの`git`をそれにする | あり（`executable(Path::new("git"))`の解決。productionのsupervisorと`integrate`の`git`も速くなる） | 25（実験の中央値、8%） | 105〜170（5〜8%） | 18〜28 |
| G2 | 着地のbranchの確かめを毎passで`git`を2本起動する形から、refのfile（`refs/remotes/<remote>/HEAD`・`packed-refs`など）の変化を見て変わったときだけ確かめる形などに変える。`ADR-t615-1`の「解決しなければclaimと着地を止め、解決したらすぐ再開する」は保つ | あり | 単独では揺れの内（passの先頭の区切りまでは1/4〜1/5） | 0〜105（0〜5%）。G1・G3と合わせると効く | 0〜18 |
| G3 | testが書く実行fileのstub（`claude_stub`・`headless_claude`など、`from_mode(0o7…)`の35箇所）のうち中身がtestに依らないものを、F1のtemplateと同じく1回だけ作って実行し、testのdirにはsymlinkかhardlinkを置く。中身がtestごとに違うもの（DBのpathを埋めるもの）は、pathを引数かenvで渡す形にしてから共有する | なし（tests/だけ） | 14〜18（supervisorの起動の合計の差） | 70〜150（supervisorを使う約340本×0.2〜0.45秒） | 12〜25 |
| G4 | 既定のreviewer（`claude-stub`）がverdictを出さないreviewのretryを、testから0回にする経路を足す（retryそのものを確かめるtestは今の回数のまま）。または、approve_landingのaskを待つtestだけがverdictなしのreviewerを使い、他のtestは`verdict("pass", …)`を使う | あり（retryの回数をtestから入れる経路）か、なし（testのreviewerの使い分け） | 15〜22（`review_retried`の44回） | 約65〜130（3〜6%） | 10〜22 |
| G1＋G2＋G3 | 上の3つを合わせたもの | — | 85（実験の中央値、29%） | 320〜590（15〜28%） | 53〜99 |

候補にしないもの:

- `ps`（157回・2.8秒）、queueを開くまで（70回・0.3秒）、fixture（F1の後で40回・0.7秒）は小さい。
- `lsof`は1回129〜424msと重いが、30本で20回・2.6〜8.5秒で、主にruntime_resumeの一部のtestに限られる。G1〜G4の後に残れば見直す。
- `TEST_TICK`（20ms）のsleep（30本で1,670回×20ms＝約33秒。表の42.4秒は次のpassのheartbeatまでを含む）は、task 567で50msから縮めたものである。これ以上縮めるとpassが増え、G1とG2の前ではpassの`git`がかえって増える。
- cli_versionの子のsupervisorのidleの待ち（28秒）は、F5（引き継ぎの見張りと`--update-interval`）が扱う。

進め方:

- G1とG3は独立で、見積もりも実験で確かめた。先に着手する。
- G2はproductionの振る舞い（着地のbranchを確かめる頻度）に触れるので、`ADR-t615-1`と照らしてから変える。productionのsupervisorでも、passごとの`git`の2本が減る。
- G4は、retryを確かめるtestと、verdictのないreviewの後のaskを確かめるtestを弱めないことを先に確かめる。
- どの候補も、前後の比較は1章の手順（直近のintegrateのlog、load1を併記）と、この章の手順（30本、instrumentあり、6並列）の両方で行う。

---
id: design-architecture
type: design
title: レイヤーとコンテキストの境界（contextごとの所有・判断・操作・公開するport・依存の向き・境界をまたぐtransaction・検査できる規則・今の違反）
status: current
created: 2026-10-04
updated: 2026-10-06 # task 1637: nested comments and test-required cfg; task 1619: planner_handoff tests use MemoryFiles, the L3 row removed; task 1616: stats tests use domain fixtures, the L1 row removed; task 1618: the L4 row of jobs.rs and headless_session.rs removed, they read the injected Clock; task 1617: the L2 row of landing_branch removed; task 1564: ask --request reads RequestStore::request_planner; task 1551: domain::run::payload among the run modules, the C6 row no longer names run/history.rs; task 1550: the L4 row of broker_admin removed, AuditFiles among the host operations ports; task 1540: the revisit command and draft_revisits; task 1223: the observer on Codex; task 1641: domain::goal_tag among the planning modules; task 839: domain::broker_usage among the host operations modules; task 1662: the observation and analysis context points to the measurement design; task 1437; task 1632
last_verified: 2026-10-06 # task 1637; task 1619; task 1616; task 1618; task 1617; task 1564; task 1551; task 1550; task 1540; task 1223; task 1641; task 839; task 1615; task 1437; task 1632
scope: system
related:
  - adr-t1545-1
  - adr-0013
  - design-measurement
  - adr-0032
  - adr-0054
  - adr-t1410-1
  - adr-t1453-1
  - design-overview
  - design-domain-model
  - design-persistence
  - design-supervisor-lifecycle-supervise
---

# レイヤーとコンテキストの境界

runtime（`src/`と`crates/`）の責務を、レイヤーとコンテキスト（context）の2軸で分ける今の境界と、それを守る規則の正本。決めた理由は[ADR-t1545-1](../adr/2026-10-04-t1545-1-split-the-runtime-by-layer-and-context.md)（[ADR-0013](../adr/0013-layered-architecture-and-type-function-style.md)決定1をamends）が持つ。レイヤーごとのmoduleの説明は[overview](overview.md)、集約は[Domain model](domain-model.md)、tableとtransactionの詳細は[Persistence](persistence.md)、supervisorのループは[`supervise`](supervisor-lifecycle/supervise.md)が持つ。

reviewのsubagentと禁止依存の検査のscriptは、この文書の節（「[検査できる規則](#検査できる規則)」の規則のIDと「[境界をまたぐtransaction](#境界をまたぐtransaction)」の一覧）を参照し、規則の本文を写さない（ADR-t1545-1決定4）。

この文書の事実は2026-10-04のmain（base `e465d467`）を読んで確かめたものに、follow_upの所属の判断（task 1505）を反映している。goal 92の削除（対話の経路とcmuxの依存）と、goal 100・goal 3の後続のtaskが進むと変わるので、それらのtaskは同じ変更でこの文書を直す。

## 2つの軸

- **レイヤー**（ADR-0013決定1）: `domain`（`src/domain/`）・`application`（`src/application/`）・`infrastructure`（`src/infrastructure/`）・起動部分（`src/compose.rs`と`src/main.rs`）。依存は外から内へだけ向く（起動部分 → infrastructure → application → domain）。
- **context**（ADR-t1545-1決定1）: 計画管理・実行と着地・観測と分析・host運用の4つと、どれにも属さない共有の部品。
- 1つのmoduleは1つのレイヤーと1つのcontextに属する。2026-10-04の時点では、directoryはレイヤーでだけ分かれ、contextはmoduleの名前と下の表で決まる。contextを1つに決められないmoduleは「[混在しているmodule](#混在しているmodule)」に挙げ、分ける先のtaskを持つ。

## 共有の部品

どのcontextにも属さず、全てのcontextが使ってよいもの。業務の判断を持たせない（ADR-t1545-1決定1）。

- IDと値: `domain::ids`（`TaskId`・`GoalId`・`RunId`・`CommitSha`・`EventId`ほか）、`domain::error`（`DomainError`）、`domain::reason`（理由の分類コード）、`domain::views`・`domain::input`の共通の型（taskの着地commit`Landing`を含む。計画管理の`TaskStore::build_waits`が返し、実行と着地のclaimの`domain::build_wait`が読む。task 1632）。
- build識別子: `build_id`（`crates/dagq-broker-protocol`の`build_id`の再export）。識別子の規則（`build_identifier`・`is_prerelease`・`UNKNOWN_COMMIT`）と、それが名乗るcommitを読む`named_commit`はI/Oを持たない関数で、host運用の自動更新（`application::update::build_commit`）と実行と着地の`domain::build_wait`が使うのはこの部分だけ。同じmoduleの`emit`・`compute`はgitを呼ぶbuild scriptの側で、runtimeのcontextからは呼ばない。
- 時刻とID生成: `application::ports`の`Clock`・`IdGenerator`（組は`Generators`）、実装は`infrastructure::clock`。
- eventの記録と種類: `run_events`表への追記（`RunLog::record_runtime_event`・`RunLog::record_queue_event`、`infrastructure::runtime_store`の`run_event`と`infrastructure::sqlite`の`event`）と、種類の定数`domain::event_kind::EventKind`。各contextは自分の種類のeventだけを書く（下の各contextの「所有する状態」）。どの種類も所有するcontextは1つで、接頭辞で書いた種類（末尾が`*`）は、他のcontextが名前で書いた種類を含まない。共有の部品の種類は`ask_*`・`authorization_denied`と、どのcontextの自動修正も数える`auto_repaired`（ADR-0047）。
- 人への問い合わせ: `asks`表と`AskStore`（`infrastructure::asks`）、CLIの`ask`・`answer`・`asks`。askを開くのと、answerを自分の状態に適用するのは、そのaskの`kind`を持つcontextが行う（例: `worker_question`と`approve_landing`は実行と着地、`plan_review`は計画管理、`blocked`のfindingは観測と分析、`update`はhost運用）。
- actorと認可: `domain::actor`・`domain::actor_model`・`domain::authorization`、`infrastructure::event_actor`・`infrastructure::denials`（[Authorization](authorization.md)）。
- 組み立ての共有の欄: `Supervisor`の`queue`・`queues`・`generators`・`layout`・`processes`・`utc_offset`（どのcontextの処理も読む接続・時計・配置）。
- 名前と出力: `application::naming`、`tracing`のマクロ（subscriberは`infrastructure::telemetry`）。
- 共有の規則: `domain::write_rules`（SQLiteのCHECKが持っていた表をまたぐ規則）、`domain::language`（人に向けて書く言語）。
- `src/broker_material.rs`: brokerのimageの材料（std だけを使い、`infrastructure::broker_image`が使う）。`migration_numbers`と同じ扱いの共有の部品で、host運用の中で使う。
- `src/migration_numbers.rs`: migrationのファイル名の規則（`build.rs`と共有する、I/Oを持たない関数だけ）。domainと同じ扱いで、`application::integrate`のmigrationの振り直しが参照してよい。

## 計画管理

goal・task・proposalと、その検査と採否（plan review・goal review・runtimeのplanner・follow_upとfindingのproposal）。

**所有する状態**

- table: `tasks`・`task_dependencies`・`task_goal_dependencies`・`goals`（`acceptance_version`を含む）・`follow_up_judgements`・`proposals`・`plan_reviews`・`goal_reviews`・`planners`・`draft_bundles`・`draft_bundle_members`・`draft_origins`・`draft_reopens`・`draft_revisits`・`plan_requests`・`plan_request_proposals`・`search_index`・`landed_commits`（検索と関連の索引）。
- eventの種類: `task_*`（`task_created`・`task_edited`・`task_status_changed`・`task_submitted`・`task_reopened`・`task_goal_changed`・`task_paths_changed`・`task_priority_changed`・`task_weight_predicted`）、`dependency_*`、`goal_*`（`goal_review_*`を含む）、`proposal_*`、`plan_*`（`plan_review_*`・`plan_decided`・`plan_concern_decided`・`plan_revise_*`）、`planner_*`、`draft_*`、`follow_up_*`、`request_*`、`finding_planner_*`。
- `Supervisor`の欄（`src/application/supervise/mod.rs`）: `plan_review`・`goal_review`・`planner_exits`・`max_improvement_proposals`。

**判断**（domain）

`domain::task`（`Task`・`TaskAction`）、`domain::goal`、`domain::proposal`、`domain::plan_review`、`domain::goal_review`、`domain::plan_request`、`domain::planner`、`domain::follow_up`、`domain::lint`、`domain::plan_quality`、`domain::prediction`、`domain::related`・`domain::search`、`domain::change`（taskの変更の宣言）、`domain::goal_tag`（goalのラベルとその語彙）。

**操作**

- application: `application::commands::planning`・`application::commands::requests`、`application::planner`・`application::planner_request`・`application::planner_handoff`、`application::supervise`の`plan_review`・`goal_review`・`draft_planner`・`finding_planner`・`request_planner`・`planner_turns`、`application::diagram`（依存図）。
- CLI: `add`・`edit`・`ready`・`draft`・`cancel`・`dependency`・`goal`・`set-goal`・`judge-follow-up`・`set-paths`・`set-priority`・`revisit`・`submit`・`proposal`・`lint`・`request`・`requests`・`search`・`related`・`candidates`・`graph`・`plan`・`planners`・`planner`・`planner-session`、読み取りの`list`・`show`。
- infrastructure: `infrastructure::sqlite`（`TaskStore`の実装）、`proposals`・`plan_reviews`・`goal_reviews`・`draft_planners`・`follow_up_membership`（所属の判断とdraft/readyの移動を同じtransactionで記録し、achievedで閉じた元goalへのrequiredではそのtransactionで`correct_goal`のaskを開く。そのaskのanswerの適用（`decide_correction`）も1つのtransaction。[所属の判断](follow-up-membership.md)、下のT8・T9）・`finding_planners`・`plan_requests`・`planners`・`planning`・`search`・`related`・`stranded`。

**公開するport**

- `TaskStore`の読み取り（`show`・`list`・`candidates`・`build_waits`（task 1632）・`predecessors`・`goal_predecessors`・`tasks_in_progress`・`graph_input`・`lint_input`・`show_goal`・`list_goals`）を全てのcontextに公開する。
- `TaskStore::claim`（と`RunTransitions::claim_for_supervisor_in_order`）は実行と着地だけに公開し、越境のtransaction **T1**として扱う。
- `DraftPlannerStore::register_follow_ups`を実行と着地に公開する（着地したrunのfollow_upsを計画管理のdraftとして渡す。transactionは着地と別）。
- `PlanReviewStore`・`GoalReviewStore`・`PlanRequestStore`・残りの`DraftPlannerStore`・`TaskStore`の書き込みは内部のport（このcontextの操作とCLIだけが使う）。CLIの`request add` / `request decline`のport `RequestStore`も内部で、その`request_planner`（依頼の閉じていないruntimeのplanner）は`ask --request`の判定も`DialogueStore::request_planner`から読む（task 1564）。

**許す依存の向き**

- 実行と着地の状態（`task_runs`・`run_leases`・`run_processes`）を書かない。runの結果は`RunLog`の読み取りとeventで読む（goal reviewとplan reviewの材料）。
- 観測と分析の`findings`を書くのはfindingのproposalの採否（越境のtransaction **T6**）だけ。

**境界をまたぐtransaction**: T1・T2・T4（書き手は実行と着地）、T3・T5・T6・T7（[一覧](#境界をまたぐtransaction)）。

## 実行と着地

taskをrunにして動かし、検証し、mainへ着地させること（claim・run・session・review・integrate・resume・triageと復旧・e2eの工程）。

**所有する状態**

- table: `task_runs`・`run_leases`・`run_processes`・`session_workspaces`。ファイルはqueueのdirの`runs/<run-id>/`（receipt・log・idle marker。`RunFiles`）とrunのworktree。
- eventの種類（廃止した worker の画面・打鍵の event は過去の記録を読むためのもの）: `worker_mode_converted`、`run_*`（`run_claimed`・`run_planned`・`run_integrated`・`run_adopted`・`run_recovered`・`run_e2e_*`・`run_waiting_*`ほか。run_tmp_removedはhost運用）、`supervision_finished`（runのrunningからvalidatingへの遷移。`domain::run::recorded`が返す）、`lease_*`、`claim_*`、`worktree_*`、`workspace_*`、`wrapper_*`、`agent_started`、`session_*`、`turn_*`、`receipt_observed`・`validation_finished`・`verification_command`・`scope_violation`・`evidence_missing`、`review_*`、`revise_*`、`resume_*`、`triage_*`、`recovery_*`、`integration_*`、`landing_*`、`conflict_*`、`concern_*`、`approve_withheld`、`exit_*`、`stall_*`、`stale_receipt_*`、`prompt_waiting`・`prompt_cleared`・`screen_*`・`idle_inferred`・`input_not_ready`・`known_dialog_unanswered`、`provider_*`、`queue_hold_applied`・`hold_*`・`usage_limited`・`auth_required`、`submit_*`（sessionへの打ち込み）、`stall_config_loaded`・`conflicts_config_changed`・`run_env_changed`・`run_env_program_*`、`first_commit_observed`、`migration_renumbered`、`push_*`（着地のpush）、`job_restarted`・`runtime_error`・`cleanup_failed`。
- `Supervisor`の欄: `workers`・`slots`・`parallel`・`max_waiting`・`limits`・`slot_flags`・`finished`・`errors`・`claiming`・`provisioning_error`・`triaged`・`stall`・`conflicts`・`conflicts_file`・`conflicts_error`・`job_ends`・`screen_spans`（plannerとinboxの画面のidleの区間。workerのrunには使わない）・`last_turns`・`run_env_missing`・`landing_unresolved`・`landing_stamp`・`run_e2e`・`e2e`・`queue_hold`・`provider_holds`・`moved`・`hold_continue`・`reopens`・`notice_failures`・`rechecks`・`defer`・`loads`・`resume_config`・`retry_unreadable_review`・`review_material`と、使うadapter（`repository`・`remote`・`verifier`・`reviewer`・`codex_jobs`・`signals`・`spawner`・`files`）。

**判断**（domain）

`domain::run`（`TaskRun`と遷移）・`domain::run::history`（`RunHistory`）・`domain::run::payload`（`RunHistory`が読む記録済みのeventのpayloadの型付きの復元の値）・`domain::run::recorded`、`domain::resume`・`domain::recovery`・`domain::receipt`・`domain::validation`・`domain::verify_failure`・`domain::concern`・`domain::review_reason`・`domain::review_subagents`、`domain::claim_defer`・`domain::build_wait`・`domain::claim_hold`・`domain::queue_hold`・`domain::slot_limits`・`domain::waiting`・`domain::recheck`・`domain::stall`・`domain::exit`・`domain::idle_process`・`domain::sessions`・`domain::turn`・`domain::worker`・`domain::worker_question`・`domain::provider_switch`・`domain::run_e2e`・`domain::e2e_quarantine`・`domain::landing_branch`・`domain::landing_release`・`domain::scope`・`domain::run_env`・`domain::headless_job`・`domain::background_wrapper`・`domain::worker_model`（plan reviewの重さの予測とtrialからclaimのときにworkerのmodelを選ぶ）。

**操作**

- application: `application::supervise`のループとslot（`mod.rs`）と工程のsubmodule（`session`・`exit`・`jobs`・`landing`・`revise`・`resume`・`reopen`・`triage`・`adopt`・`recovery`・`stall`・`stall_recovery`・`waiting`・`deliver`・`idle`・`recheck`・`claim_defer`・`queue_hold`・`provider`・`slot_limits`・`e2e`・`stale`・`headless`・`background`・`file_time`）、`application::session`・`application::headless_session`・`application::integrate`・`application::review`・`application::e2e_verdict`・`application::screen`・`application::screen_idle`・`application::recording`・`application::commands::operations`。
- CLI: `supervise`・`run`・`session`・`session-event`（session wrapper）・`integrate`・`review`・`recover`。
- infrastructure: `runtime_store`の`transitions`・`recovery`・`session_registry`・`run_log`と`coordination`のleaseとprocessの部分、`sessions`・`adapters`（`GitRepository`・`ClaudeCode`）・`claude`・`codex`・`run_files`・`run_env`・`e2e_gate`・`process`・`background`。

**公開するport**

- `RunLog`の読み取り（`runs_with_status`・`run`・`run_events`・`all_runs`・`all_events`・`latest_*`・`events_of_between`）を全てのcontextに公開する。
- `RunId`・`TaskRun`のview・型付きのeventを値として公開する。
- `RunTransitions`・`RunRecovery`・`SessionRegistry`のworkerの部分・`RunCoordination`のleaseとprocessの部分は内部のport。

**許す依存の向き**

- 計画管理のtaskを読むのは`TaskStore`の読み取りと、claimのT1だけ。taskの状態を変えるのはT2（着地）とT4（triageのretry・cancel）だけ。
- follow_upsは`DraftPlannerStore::register_follow_ups`（T7）で計画管理に渡す。`tasks`・`draft_origins`をSQLで書かない。
- 観測と分析・host運用の状態を書かない。

**境界をまたぐtransaction**: T1・T2・T4（書き手）、T7（計画管理の公開する関数を呼ぶ）（[一覧](#境界をまたぐtransaction)）。

## 観測と分析

起きたことを読み、数え、予測し、知らせること（events・watch・stats・KPI・forecast・印・observer・スループットの見直し）。

計測の作り直し（goal 71。区間とタグ・台帳とそれを作る係・論理ストアのSSOTとビューの区分・送る口）の予定は[計測](measurement.md)が持ち、新しいportと台帳の係はこのcontextに置く（[ADR-t1662-2](../adr/2026-10-04-t1662-2-measurement-stores-ssot-and-views.md)決定6）。まだ実装は無く、下の節は今の姿のまま。

**所有する状態**

- table: `findings`。ファイルはqueueのdirの日次のKPIのreport（`report_written`が指す）とKPIのpushの待ち。
- eventの種類: `observation`・`observe_*`、`finding_recorded`・`finding_updated`・`finding_status_changed`、`mark_recorded`・`mark_retracted`、`forecast_recorded`、`kpi_breach_*`・`kpi_push_*`、`report_written`、`throughput_review_*`、`candidates_sampled`。
- `Supervisor`の欄: `observer`・`observers_launched`・`observer_again`（Codexを使えなかったobservationを起動し直すmode。task 1223）・`throughput_review`・`report`・`reports`・`forecasts`・`forecast`・`push`（KPIのpush）・`candidates`。

**判断**（domain）

`domain::stats`とその下（`stats::conflicts`・`stats::thresholds`ほか）、`domain::kpi`とその下、`domain::forecast`とその下、`domain::marks`、`domain::timeline`、`domain::measure`、`domain::worktime`、`domain::tokens`、`domain::transcript`、`domain::throughput_review`、`domain::finding`、`domain::areas`（runの差分から導く範囲の分類。`application::areas`が読む）。

**操作**

- application: `application::stats`・`application::areas`・`application::marks`（`mark`・`mark --retract`。`MarkLog`と注入した`Clock`越し。task 1548が`compose`から移した）・`application::kpi`・`application::forecast`・`application::report`・`application::push`、`application::supervise`の`forecast`・`report`・`push`・`throughput_review`（行き先の`job_start_route`はobserverと共有）・`observer`（observerのjobの終わりの`provider_unusable`を読んでCodexを控える。task 1223）。`application::observer`（observerのjob）・`application::watch`（`events`・`timeline`・`watch`。task 251がレイヤーの外から移した）・`application::throughput_review`（スループットの見直しのjob。promptの組み立てと`ACCESS`。portは`ThroughputReviewSources`と`ThroughputReviewHost`。task 1615がレイヤーの外から移した）。組み立ては`compose::throughput_review`・`compose::throughput_review_launch`・`compose::observe`・`compose::observer_launch`（手で打つ`observe`の`[roles.observer]`。task 1223）。レイヤーの外の`src/view.rs`。
- CLI: `events`・`watch`・`stats`・`kpi`・`report`・`forecast`・`mark`・`marks`・`timeline`・`finding`・`findings`・`observe`・`throughput-review`・`note`・`notes`、`status`の読み取り。
- infrastructure: `findings`・`observer`（observerのファイル・設定・headlessのagentのprocess）・`throughput_review`（見直しのdirのファイル・`[roles.throughput_review]`・hostの時間帯・agentのprocess）・`runtime_store::queue_records`・`kpi_config`・`kpi_push`・`report_config`・`d2`・`transcripts`・`claude_turns`・`codex_turns`。

**公開するport**

- `QueueRecords`の読み取り（`findings`・`reports_written`・`kpi_breaches_open`）を全てのcontextに公開する。`task_changes`・`task_goals`・`task_titles`・`draft_origins`・`related_landed_commits`・`related_tasks`・`search_documents`は計画管理の表（`tasks`・`draft_origins`・`landed_commits`・`search_index`）を読むmethodで、task 1554でportを分けるときに計画管理へ移す。`record_*`（report・KPIの目標割れ・push・forecast）は内部。
- findingのIDと`finding_*`のeventを値として公開する（計画管理のfindingのplannerが読む）。
- `ObserverLog`（observerの観察の記録と書いたものの読み取り）と`EventReads`（cursorより後のeventの読み取り）は`application::ports`のportで、observerと`events`・`timeline`・`watch`のユースケースが内部で使う（task 251）。

**許す依存の向き**

- 他のcontextの状態とeventを読むだけで、他のcontextのtableを書かず、他のcontextの操作（claim・遷移・answerの適用）を呼ばない（ADR-t1545-1決定2）。
- 書くのは自分の種類のeventと`findings`と、findingに紐づく`blocked`のask（共有の部品）だけ。

**境界をまたぐtransaction**: 無い（T6はこのcontextの`findings`を計画管理が書くもの）。

## host運用

runtime自身をhostで動かし続けること（up・down・install・自動更新・broker・queue service・compileの共有・diskの後始末・hostの計測）。

**所有する状態**

- table: `supervisors`・`queue_repository`・`schema_floor`・`binary_updates`・`headless_jobs`（jobのprocessの台帳）。ファイルはqueueのdirの`service/`（queue service）・`broker/`（brokerのtokenとaudit）・`logs/`、launchdのplist、sccacheのserver。
- eventの種類: `supervisor_*`、`update_*`・`release_check*`、`broker_*`、`queue_service_*`、`sccache_*`、`build_outputs_removed`・`scratchpad_removed`・`run_tmp_removed`、`inbox_*`、`supervisor_config_changed`、`backend_call_failed`、`headless_job_stopped`。
- `Supervisor`の欄: `token`・`heartbeat`・`supervisor_file`・`supervisor_error`・`exec`・`handoff`・`draining`・`stop_recorded`・`service_up`・`service_access`・`update`・`max_load`・`load_average`・`host_versions`・`host_metrics_port`・`host_metrics`・`broker_port`・`broker_leftovers`・`broker`・`queue_service_port`・`queue_service`・`release_port`・`release`・`disk_config`・`free_space`・`scratchpad_roots`・`disk`・`free`・`cleanup`・`sccache_port`・`sccache`・`last_sweep`・`sweep_failures`・`jobs_swept`・`process_sample`・`no_claude`・`cmux`。

**判断**（domain）

`domain::broker`・`domain::broker_usage`・`domain::disk`・`domain::sccache`・`domain::release_update`・`domain::queue_service`・`domain::host_metrics`・`domain::source_repository`。

**操作**

- application: `application::lifecycle`（`up`・`down`）・`application::install`・`application::update`・`application::release_update`・`application::broker`・`application::broker_admin`・`application::broker_run`・`application::queue_service`・`application::rebind`・`application::workspace_cleanup`・`application::inbox_watcher`・`application::inbox_guardrail`・`application::actor_executor`・`application::execution`（AIのactorを動かす場所と隔離）、`application::supervise`の`update`・`release`・`broker`・`queue_service`・`sccache`・`disk`・`cleanup`・`sweep`・`host_metrics`・`handoff`・`inbox_nudge`。
- CLI: `auto-update`・`release-update`・`init`・`locate`・`up`・`down`・`install`・`migrate`・`rebind`・`broker`・`service`、`doctor`のhostの部分。
- infrastructure: `launchd`・`binaries`・`broker_*`・`queue_service`・`sccache`・`host_metrics`・`release_update`・`location`・`schema`・`telemetry`・`inbox_watchers`・`agent_dir`・`git_binary`・`runtime_store::coordination`のsupervisorの登録と引き継ぎの部分。`crates/`のbrokerのcrate（`dagq-broker`・`dagq-broker-client`・`dagq-broker-protocol`）。

**公開するport**

- `RunCoordination`のsupervisorの登録と引き継ぎ（`register_supervisor`・`heartbeat`・`take_handoff`など）を実行と着地のループに公開する。
- `HeadlessJobStore`（jobのprocessの台帳）を、jobを起動する各contextに公開する。
- `QueueOpener`・`LaunchAgent`・`SccacheServer`・`ProcessControl`・`InstalledPlugin`、AIのactorの起動（`application::actor_executor`）を他のcontextに公開する。
- `AuditFiles`（`application::broker_admin`。brokerのauditの日のファイルを読む。adapterは`infrastructure::broker_audit::AuditDir`。task 1550）は、このcontextの中だけで使う（`dagq broker audit`の`compose::broker_audit`と、tokenの失効の前の数え`infrastructure::broker_token`が注入する）。他のcontextには公開しない。
- `WorkspaceBackend`はinboxのworkspaceのためのport（[ADR-t1433-1](../adr/2026-10-03-t1433-1-cmux-is-used-only-by-the-inbox.md)、goal 92で縮める）。

**許す依存の向き**

- 他のcontextのuse caseを起動・停止してよい（supervisorのループ・handoff・drain）が、他のcontextの状態は公開したportで変える。
- brokerのcrateはrootの`dagq`のcrateに依存しない（`dagq-broker-protocol`だけを共有する。[ADR-t827-1](../adr/2026-09-28-t827-1-broker-crates-binaries-and-version-alignment.md)）。

**境界をまたぐtransaction**: 無い。

## 混在しているmodule

contextを1つに決められず、分ける先のtaskを持つもの。

| module | 混ざっているcontext | 分ける先 |
| --- | --- | --- |
| `src/application/ports.rs` | 全てのcontextのportと`Queue`（13のstoreのportのsupertrait）・`QueueOpener` | task 1554（contextごとのmodule）、task 1555（use caseが要るportだけを取る） |
| `src/application/supervise/mod.rs`の`Supervisor` | 4つのcontextの欄（上の各節）を1つのstructで持ち、`impl Supervisor`のsubmoduleが互いの欄を変える | task 1552（観測と分析・host運用）、task 1553（実行と着地のslotと工程） |
| `src/compose.rs` | 全てのcontextの組み立て | task 1556（contextごとの組み立てのmodule） |
| `runtime_store::session_registry`（`SessionRegistry`） | 実行と着地の`session_workspaces`と、計画管理の`planners`（`open_planner`・`close_planner`・`planner_*`） | task 1554（portの分割） |
| `application::queue_reads` | CLIとqueue serviceの読み取りの入口（`answer`。task 1549で`compose::read_queue`から移した）で、各armは自分のcontextのport（`TaskStore`・`AskStore`・`RunLog`・`EventReads`・`ObserverLog`・`QueueRecords`）と起動部分が組む`QueueReadSources`を読む | 未登録（follow_up） |
| `application::prompt` | workerのprompt（実行と着地）、inbox・plannerのprompt（計画管理）、observerとスループットの見直しのprompt（観測と分析） | 未登録（follow_up） |
| `application::health` | `status`・`doctor`とattention（観測と分析）、`recover`（実行と着地）、`doctor`のhostの部分（host運用） | 未登録（follow_up） |

## 境界をまたぐtransaction

複数のcontextの状態を1つの`BEGIN IMMEDIATE`のtransactionで変えてよいのは、この一覧のものだけ（ADR-t1545-1決定3）。原子性を守るための例外で、[Persistence](persistence.md)の「Runtime ownership」「Transactions and constraints」の規則（claimとlease、`renew_lease`、着地）を弱めない。一覧に足す・外す変更は同じ変更でこの表を直す。

| ID | transaction | 書き手のcontext | 変える状態 | 理由 | コード |
| --- | --- | --- | --- | --- | --- |
| T1 | claim | 実行と着地 | `tasks`を`ready`から`in_progress`に、`task_runs`を作り、`task_runs.supervisor_token`と`run_leases`の行、`run_claimed`・`lease_acquired`（interactiveのtaskを非対話でclaimしたときは`worker_mode_converted`も。task 1437） | 所有者の無いclaimed runとrunの無い`in_progress`を作らない（ADR-0054、ADR-0013決定9） | `infrastructure::runtime_store::transitions`の`SqliteQueue::claim_for_supervisor_in_order`が`infrastructure::sqlite::claim_task`を呼ぶ。人の`claim`は`TaskStore::claim` |
| T2 | 着地の完了 | 実行と着地 | runを`integrated`にし、`run_leases`の行を消し、`tasks`を`completed`に、`run_integrated`・`lease_released`・`task_status_changed`。`run_integrated`の追記でSQLのtrigger（`search_run_integrated`）が`landed_commits`と`search_index`を書く | mainへの着地とtaskの完了を食い違わせない | `infrastructure::runtime_store::transitions`の`SqliteQueue::finish_integration` |
| T3 | plan reviewのverdictの適用 | 計画管理 | `asks`を閉じ、`proposals`を承認・差し戻し・取り消しにし、その`tasks`を`ready`・`draft`・`canceled`にし、`plan_decided` | proposalとそのtaskの状態を1回で揃える（同じcontextの中の表とaskの組で、askは共有の部品） | `infrastructure::plan_reviews`の`decide_plan`（`infrastructure::proposals`の`approve`・`send_back`が`transition_task`を呼ぶ） |
| T4 | triageのanswerの適用 | 実行と着地 | `run_leases`・`task_runs`を見て`asks`を閉じ、`retry`は`tasks`を`ready`に、`cancel`は`canceled`に、`resume`はrunを`needs_session`に、`triage_decided` | runの失敗の扱いとtaskの次の状態を1回で決める（ADR-0047決定40） | `infrastructure::runtime_store::recovery`の`decide_triage`が`infrastructure::sqlite::transition_task`を呼ぶ |
| T5 | goal reviewのverdictの適用 | 計画管理 | `asks`を閉じ、`goals`を閉じるか、足りないtaskを`tasks`に登録し、`goal_reviews`を直し、`goal_decided` | goalの判定と隙間のtaskを1回で揃える（同じcontextの中の表とask） | `infrastructure::goal_reviews`の`decide_goal`（`register_gaps`） |
| T6 | findingのproposalの採否 | 計画管理 | `proposals`を作り`findings`をそのproposalに結ぶ（`submit_linking`）、proposalとそのtaskの状態から`findings`の状態を決める（`settle_findings`） | findingを1つのproposalに結び、二重のproposalを作らず、採否をfindingに戻す | `infrastructure::finding_planners`の`submit_linking`・`settle_findings`（判断は`domain::finding::settle`） |
| T7 | follow_upsのdraftの登録 | 計画管理（実行と着地が`register_follow_ups`で呼ぶ） | `task_runs`と`run_events`を読み、`tasks`（draft）と`draft_origins`を書く | 着地したrunのfollow_upsをそのrunの由来つきで1回だけdraftにする | `infrastructure::draft_planners`の`register_follow_ups`（呼び出しは`application::integrate`。着地のT2とは別のtransaction） |
| T8 | follow_upの所属の判断の記録 | 計画管理 | `follow_up_judgements`に行を足し、draft / readyの`tasks`の`goal_id`を移し（`task_goal_changed`）、achievedで閉じた元goalへのrequiredなら`asks`に`correct_goal`を開き（`ask_opened`）、`follow_up_judged` | 判断と所属と人への問いを食い違わせない（ADR-t1504-2決定1・6・9） | `infrastructure::follow_up_membership`の`SqliteQueue::judge_follow_up`（移動は`infrastructure::sqlite::set_goal_in`、askは`infrastructure::asks::insert_ask`） |
| T9 | achievedの後の訂正のanswerの適用 | 計画管理 | `asks`を閉じ（`ask_closed`）、`reopen`は`goals`を開き直し（`goal_reopened`）、draft / readyのfollow_upの`tasks`の`goal_id`を移し、同じgoalのほかの`correct_goal`と残った`approve_goal`の`asks`をruntimeが答えて閉じ、`goal_correction_decided` | 人の答えとgoalの状態・follow_upの所属・残った問いを1回で揃える（ADR-t1504-2決定9。T5の`decide_goal`の対） | `infrastructure::follow_up_membership`の`decide_correction`（`domain::goal::reopen`、`infrastructure::asks::close_by_runtime`） |

## 検査できる規則

規則のIDは後続の検査のscript（task 1546）とreviewのsubagent（task 1547）が参照するので変えない。規則を変えるときはIDを足すか、古いIDを「廃止」と書いて残す。各規則の「検査」は、機械的な検査（script）で見るか、reviewで見るかを書く。

### レイヤーの規則

- **L1** `src/domain`のコードは`crate::application`・`crate::infrastructure`・`crate::compose`と、レイヤーの外のmodule（`crate::view`・`crate::runtime`・`crate::lifecycle`）を参照しない。`#[cfg(test)]`の中も同じ（domainのtestはdomainの値と関数だけで組む）。検査: script。
- **L2** `src/domain`の本番のコード（`test=false`のビルドに残りうるコード）は`rusqlite`・`std::fs`・`std::process`・`std::net`・`SystemTime::now`・`Instant::now`・`Uuid::new_v4`・`anyhow`を参照しない（ADR-0013のAlternativesの検査の機械化）。検査: script。
- **L3** `src/application`のコードは`crate::infrastructure`・`crate::compose`とレイヤーの外のmoduleを参照しない。`#[cfg(test)]`の中も同じ（testはapplicationのtest double、たとえば`application::memory_files`を使う）。例外は共有の部品の`crate::migration_numbers`だけ。検査: script。
- **L4** `src/application`の本番のコード（`test=false`のビルドに残りうるコード）は`rusqlite`・`std::fs`・`std::process::Command`・`SystemTime::now`・`Uuid::new_v4`を直接使わず、portを通す。`#[cfg(test)]`の中でfixtureを作る`std::fs`と`tempfile`はよい。検査: script。
- **L5** `src/application`の状態の判断（遷移・回数と上限・送るかどうか・待つかどうかを決めるもの）は、時刻を`Clock`か値の引数で受け、`Instant::now`・`SystemTime::now`を判断の中で読まない（[ADR-t1410-1](../adr/2026-10-03-t1410-1-decisions-in-unit-tests-boundaries-in-integration-tests.md)）。検査: review（今の`Instant::now`は数が多く、task 1557・1558が減らすまでscriptには入れない）。
- **L6** `src/infrastructure`のコードは`crate::compose`とレイヤーの外のmoduleを参照しない。検査: script。
- **L7** 起動部分（`src/compose.rs`と、task 1556が作るその下のmodule）はadapterを作ってuse caseに注入する配線だけを持ち、判断・時刻の読み取り・eventのpayloadの組み立てを持たない。検査: review（task 1556の後にscript）。
- **L8** レイヤーの外のmodule（`view`。`observer`と`watch`はtask 251、`throughput_review`はtask 1615がapplicationへ移した）は起動部分と同じ外側に置き、domain・applicationを使ってよいが、domain・application・infrastructureから参照されない（L1・L3・L6）。新しいmoduleをレイヤーの外に足さない。検査: script（L1・L3・L6として）とreview。

### コンテキストの規則

- **C1** あるcontextの`src/infrastructure`のstoreのmoduleは、自分のcontextのtable（上の各節の「所有する状態」）にだけ`INSERT`・`UPDATE`・`DELETE`を書く。他のcontextのtableを書くのは「[境界をまたぐtransaction](#境界をまたぐtransaction)」の一覧にあるものだけ。検査: review（task 1554の後にscript）。
- **C2** 観測と分析のコードは、他のcontextの状態を変えるportのmethod（`TaskStore`の書き込み・`RunTransitions`・`RunRecovery`・`RunCoordination`の書き込み・`PlanReviewStore`・`GoalReviewStore`・`DraftPlannerStore`の書き込み）を呼ばない。検査: review。
- **C3** `application::supervise`のsubmoduleは、自分のcontextの`Supervisor`の欄（上の各節）だけを変える。他のcontextの欄は読むか、そのcontextの関数を呼ぶ。検査: review（task 1552・1553の後にscript）。
- **C4** 新しく足す・変えるuse caseは`Box<dyn Queue>`・`&mut dyn Queue`・`QueueOpener`を取らず、要るportだけを取る（ADR-0013決定1の「portは原則applicationに定義する」のまま、幅を狭める）。検査: review（task 1555の後にscript）。
- **C5** 他のcontextの公開していないport（各節の「公開するport」で内部としたもの）を使わない。検査: review。
- **C6** 他のcontextのeventを読むときは、kindを`domain::event_kind`か所有するdomainのmoduleの定数で名指し、状態の判断に使うpayloadは文字列のkeyではなく型付きの復元の値で読む（`serde_json::Value`の`payload["..."]`で判断しない）。古いeventの読み取りは保つ。検査: review（task 1551・1558の後にscript）。
- **C7** 新しいportは、どのcontextが所有し、どのcontextに公開するかをこの文書の該当の節に足してから置く。`application::ports`に足すときはtask 1554の後のcontextのmoduleに置く。検査: review。

### transactionの規則

- **X1** 複数のcontextの状態を変える1つのtransactionは、上の一覧のIDを持つものだけ。新しく要るときは同じ変更で一覧に足す。検査: review。
- **X2** 境界の規則のためにtransactionを分けない。一覧のtransactionの述語（`WHERE status IN (...)`・`renew_lease`・`BEGIN IMMEDIATE`）を外へ出さない（ADR-0013決定9）。検査: review。
- **X3** 一覧のtransactionの中で、他のcontextの状態を変えるのは、そのcontextのinfrastructureが公開する関数（`infrastructure::sqlite::transition_task`など）を通す。SQLの文を書き写さない。検査: review。今のT1・T2・T6はこの形になっていない（「[今の違反と行き先](#今の違反と行き先)」のX3の行）。

### 検査の範囲

- SQLのtrigger（migrationが作る`search_*`のtrigger）が書く`search_index`・`landed_commits`は、計画管理の検索の索引の書き込みで、C1の違反に数えない（trigger自体は計画管理が所有する）。
- コメントの行（`//`・`//!`・`///`）とdocのlinkは数えない（docのlinkの`crate::application::...`は依存ではない）。
- `#[cfg(test)]`の中は、L1・L3・L6（参照の向き）では数え、L2・L4（I/Oと時刻）では数えない。
- 今ある違反は、理由と行き先のtaskを持つ許可の一覧にだけ置き、直したtaskが同じ変更で一覧から外す（ADR-t1545-1決定4）。
- scriptは`scripts/check-layer-deps.sh`で、L1・L2・L3・L4・L6を`src/domain`・`src/application`・`src/infrastructure`の`.rs`に当てる。CIが流し、`src/`を変えるtaskのverifyに付ける（[taskの登録](../development/task-registration.md)の「推奨の組み合わせ」）。`sh scripts/check-layer-deps.sh --self-test`は`TMPDIR`（未設定なら`target/`）の下の一時directoryに小さなfixture（違反なし・一覧に無い違反・古い項目・task IDの無い項目・コメントと文字列だけの参照・入れ子のブロックコメント・複数行のcfg・testが必須のcfg・本番に残りうるcfg・inline modの中のtestの範囲と終わり）を作って判定を確かめ、終わったら消す。
- 数えるのはpath（`crate::application::timestamp`・`std::time::SystemTime::now`など）で、`use crate::{infrastructure::sqlite, ...}`のような組の`use`も展開して数える。コメント（`//`・`//!`・`///`・入れ子も含む`/* */`）と文字列・文字のliteralの中は数えない。
- testとして扱うcfgは、条件が`test`そのもの、`all(...)`の引数のどれかがtestを必須とするもの、`any(...)`の引数のすべてがtestを必須とするもの。`#[cfg(test)]`（複数行も含む）・`cfg(any(test))`・`cfg(all(test, unix))`・`cfg(all(unix, any(test)))`はtestとして扱う。`cfg(any(test, unix))`・`cfg(any(test, feature = "x"))`は`test=false`でも成立しうるため本番として数える。`cfg(not(test))`と、`not`を含むものや判定できない形も本番として数える（安全側）。
- このcfgの範囲は、属性が付いた項目（`mod tests { ... }`・関数・`use`など）の終わりまでと、testの範囲で`mod name;`が宣言するファイル（inline modの名前を含むmoduleの位置の`name.rs`と`name/`の下）。inline modの中にあるtestのmodも扱い、範囲の終わりでは元の扱いに戻る。ファイル先頭の`#![cfg(test)]`などtestが必須の内側の属性はファイル全体をtestとして扱う。上のとおりL1・L3・L6では数え、L2・L4では数えない。
- 許可の一覧は`.config/layer-deps-allow.txt`。1行が1項目で、`規則 | path | 参照 | 行き先のtask | 理由`の5つを`|`で区切る（`#`で始まる行と空行は読まない）。規則はL1・L2・L3・L4・L6のどれか、pathは`src/`からのファイル、参照はscriptが出す参照（`crate::application`・`std::fs`・`SystemTime::now`・`anyhow`など、規則が禁止する形の先頭）、行き先のtaskはtask IDか`,`で区切った複数のtask ID、理由は空でない。同じファイルの同じ参照は何箇所あっても1項目。一覧に無い参照（新しい違反）、一覧にあるのにもう無い参照（古い項目）、書式の誤りと重複はどれもexit 1。
- 違反を直すtaskは、同じ変更で一覧の項目と下の「今の違反と行き先」の行を消す。
- 「検査: review」の規則と、境界を変えた差分がこの文書と許可の一覧を直しているかは、`src/**`・`crates/**`の差分で選ばれるreviewのsubagent `architecture-boundaries`（`.dagq/review-agents/architecture-boundaries.md`、[Review](supervisor-lifecycle/review.md#reviewのsubagent)）が見て、scriptが見る規則は見ない。

## 今の違反と行き先

2026-10-04のmain（base `e465d467`）で`grep`して見つけたもの（task 1545）に、`scripts/check-layer-deps.sh`が見つけた行（`src/application/supervise/mod.rs`のL3。task 1546）を足した（その行はtask 1631が直して消した）。scriptが検査する規則（L1・L2・L3・L4・L6）の行は、どれも行き先のtaskを持ち、許可の一覧`.config/layer-deps-allow.txt`の項目と一致する（一覧の書式は「[検査の範囲](#検査の範囲)」）。行き先が「未登録」のまま残っているのはreviewで見るX3・C1の行だけで、task 1545のreceiptのfollow_upでplannerに渡した。行き先のtaskは着地したら同じ変更でこの表の行と一覧の項目を消す。

| 規則 | 場所 | 違反 | 行き先 |
| --- | --- | --- | --- |
| L5 | `src/application`の`Instant::now`（2026-10-04で109箇所。多いのは`supervise/resume.rs`・`lifecycle.rs`・`supervise/session.rs`・`supervise/revise.rs`・`supervise/reopen.rs`・`supervise/adopt.rs`） | 判断が実時間を読む | task 1557（revise・reopen・resume・session）、task 1558（stall・stall_recovery・adopt）。残りは計測（task 1559）の後に判断 |
| L6 | `src/infrastructure/queue_service.rs`（`crate::view::task_detail`） | infrastructureがレイヤーの外を呼ぶ | task 1620 |
| C3 | `src/application/supervise/mod.rs`の`Supervisor`と、`impl Supervisor`を持つ`supervise/`の39のsubmodule（2026-10-04） | 全てのcontextの欄を1つのstructで共有し、submoduleが互いの欄を変える | task 1552・1553 |
| C4 | `Box<dyn Queue>`・`&mut dyn Queue`・`QueueOpener`を取るuse case（`application::lifecycle`・`update`・`install`・`health`・`supervise`ほか） | 要るportだけを取っていない | task 1555（観測と分析・host運用）、task 1553（実行と着地） |
| C5 | `SessionRegistry`が計画管理の`planners`を書く | 実行と着地のportに計画管理の状態が混ざる | task 1554 |
| C6 | `src/application/supervise/stall.rs`・`supervise/adopt.rs`ほか | 判断に使うeventのpayloadを`serde_json::Value`の文字列のkeyで読む（`src/domain/run/history.rs`はtask 1551で型付きの復元の値`domain::run::payload`に移した。残る4行はtask 1437で書かれなくなった`exit_unsent`・`prompt_waiting`を読む呼び手の無い関数） | task 1558（stall・adopt）、task 681（domainのkindの比較を定数へ）、history.rsの残る4行は未登録（follow_up） |
| X3・C1 | T1: `src/infrastructure/sqlite.rs`の`claim_task`（計画管理のstore）が`INSERT INTO task_runs`を書く | 計画管理のstoreが実行と着地の表をSQLで直接書く | 未登録（follow_up） |
| X3・C1 | T2: `src/infrastructure/runtime_store/transitions.rs`の`finish_integration`が`UPDATE tasks SET status='completed'`を書く | 実行と着地のstoreが計画管理の表を`transition_task`を通さずSQLで直接書く | 未登録（follow_up） |
| X3・C1 | T6: `src/infrastructure/finding_planners.rs`（`settle_findings`・`link_findings`ほか）が`UPDATE findings`を書く | 計画管理のstoreが観測と分析の表を`infrastructure::findings`を通さずSQLで直接書く | 未登録（follow_up） |

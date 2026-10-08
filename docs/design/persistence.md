---
id: design-persistence
type: design
title: SQLite persistence
status: current
created: 2026-09-21
scope: persistence
related:
  - adr-t1704-1
  - adr-t1639-1
  - adr-t1394-1
  - adr-t1632-1
  - adr-t1340-1
  - adr-t807-1
  - adr-0067
  - adr-t614-2
  - adr-t876-1
  - adr-0062
  - adr-0073
  - adr-0044
  - adr-0040
  - adr-0038
  - adr-0003
  - adr-0006
  - adr-0007
  - adr-0008
  - adr-0009
  - adr-0010
  - adr-0011
  - adr-0012
  - adr-0039
  - adr-0014
  - adr-0045
  - adr-0017
  - adr-0020
  - adr-0024
  - adr-0022
  - adr-0029
  - adr-0037
  - adr-0063
  - design-domain-model
---

# SQLite persistence

## 目的

SQLiteのDBはqueueの正本で、プロセス間の共有と再起動の後の復旧に使う。
stdoutやsessionの画面は正本にしない。
runtimeが状態を変えるのは、このDBへの書き込みトランザクションの中だけである。
列・既定値・eventの欄の意味は定義のそばのdoc comment（主に`src/infrastructure/`のstoreと`src/domain/`の型）と`migrations/*.sql`のコメントが持つ。

## 全体の流れ

```text
CLI / supervisor / integrate / queue service / job
        │  port（application の trait）
        ▼
SqliteQueue（src/infrastructure/）── BEGIN IMMEDIATE ──► queue.db（WAL）
        │  行を読む → domainの集約に復元 → domainのコマンド → 返った集約を UPDATE / INSERT
        │  同じトランザクションで run_events に event を書く
        ▼
読み取り専用のコマンド ── SQLITE_OPEN_READ_ONLY（古いschemaはメモリの複製をmigrate）
```

## 責務と境界

- 判断（遷移を許すか、どのeventを書くか、payloadの中身）はdomainが持ち、storeは読む・復元する・書く・同じトランザクションにまとめるだけを持つ（[ADR-0013](../adr/0013-layered-architecture-and-type-function-style.md)）。
- 例外は全体のグラフが要る依存の循環の検出（再帰CTE）だけで、拒むかの判断はdomainが持つ。
- 時刻とIDは`SqliteQueue`が持つ`Generators`（`Clock`と`IdGenerator`）から取り、SQLの`now`で作らない。
- schemaの形は`migrations/*.sql`、適用と互換の判定は`src/infrastructure/schema.rs`、場所は`src/infrastructure/location.rs`が持つ。
- runのログ本体・receipt・promptはDBに置かず、queueのディレクトリの`runs/<run-id>/`のファイルに置く（下の「Queue location」）。

## 不変条件

- 状態を変える操作は`BEGIN IMMEDIATE`で直列化し、判定と書き込みとeventを1つのトランザクションに入れる。
- taskごとに未完了のrunは1つ、`integrated`のrunは1つ、queue全体で`integrating`のrunは1つ（部分UNIQUE index）。
- `recover`を除き、runの状態を変える書き込みは、そのrunのlease行が自分のtokenのときだけ通る。
- eventと所属の判断は追記だけで、eventは知らないkindも落とさずに読む。
- DBにCHECK制約は無く、規則はdomainの型と書き込みのportが持つ（下の「CHECK制約を使わない」）。
- バイナリは、queueの下限（`schema_floor`）より古ければ開かず、queueより古いschemaしか知らなくても下限以上なら知らない表と列に触れずに動く。

## 表

表ごとの役割と、列についてコードから読めない約束。
列の型と既定値は`migrations/*.sql`と、行を読む関数（`task_row`・`ask_row`など）が持つ。

```text
tasks                  -- task 1つに1行。priorityのnullは所属goalの優先度を継ぐ。worker_modeのnullはproviderの既定（ADR-t1340-1）、
                       --   保存された'interactive'はclaimとresumeで非対話と読む（ADR-t1433-2）。follow_up_depthは人の判断を経ずに続いたfollow-upの段数
                       --   origin*は作成時だけ書き、priority_byは誰が置いたか（ADR-t1975-1）
task_dependencies      -- taskの依存（task → predecessor）
task_goal_dependencies -- taskからgoalへの依存。goalがachievedで閉じるまでclaimしない（ADR-0038）
task_runs              -- run 1つに1行（試行ごとに新しい行）。pathの列は記録時の絶対pathで、読むときは開いたqueueのruns/から解決し直す（ADR-0017）
run_events             -- event（task / goal / run、またはどれにも属さないqueueのevent）と書いたactor（ADR-t728-1）。追記だけ
run_leases             -- 実行中のrunの所有者（supervisorかintegrateのtoken）とheartbeat
run_processes          -- runのwrapperとagentのPID。(run, role)ごとに1回だけ登録
queue_repository       -- queueを束縛するGit common directory（1行）
schema_floor           -- 受け入れるバイナリのschemaの下限（1行。ADR-0045）
supervisors            -- 常駐superviseの登録。runを持たないsupervisorもstatus / doctorに見せる。providers以降の列のnullはその列を知らない古いbinaryの登録
session_workspaces     -- upが開いた常駐sessionのworkspace（role主キー）
goals                  -- goal 1つに1行。acceptance_versionはacceptanceの文が変わったときだけtriggerが増やす（ADR-t1504-2）。priorityは個別の指定の無いtaskが継ぎ、tagsはgoalのラベル（ADR-t1639-1）。origin* / priority_byはtasksと同じ
proposals              -- plan reviewに出したgoalとtaskの束。memberは表を持たず、tasks / goalsのproposal_idが今の所属を指す
plan_reviews           -- plan review job 1つに1行。未完了は1行まで（部分unique index）
goal_reviews           -- goal review job 1つに1行。未完了は1行まで（indexでなくBEGIN IMMEDIATEの中の検査）
headless_jobs          -- headless jobのプロセス。始めたsupervisorが消えた後に別のsupervisorが止めるため
planners               -- plannerのsession 1つに1行。workspace_idはruntimeのplannerならbackgroundのwrapperのhandle、人のplannerと古い行はcmux workspace。answer_wait_atは人の答えだけを待って終わる行（ADR-t1704-1）
plan_requests          -- inboxが記録した計画の依頼。text / note / refsは記録の後に書き換えない（言い直しは新しい依頼）
plan_request_proposals -- 依頼から出たproposal
draft_origins          -- runtimeやjobが作ったdraftの出どころと材料（JSON object）。1つのdraftに1回だけ書く
draft_reopens          -- plan reviewがreopenしたtaskのproposalを取り下げたときの出どころ
draft_revisits         -- draftの再検討の時刻（ADR-t1540-1）。使ったときにopened_atを書く
draft_bundles          -- 束のplanner（同じきっかけのdraftを1人のplannerが持つ。ADR-t807-1）
draft_bundle_members   -- 束のdraftと、plannerを閉じたときに決めた結末。outcomeのnullはplannerが生きている
follow_up_judgements   -- follow-upの所属の判断と訂正。追記だけ（follow-up-membership.md）
asks                   -- 人に答えを求める相談。answered_byの値は書いた者のroleで、権限の出どころはanswer_authorityが別に持つ（Authorization）
findings               -- observerの検出。同じ問題の閉じていない行は1つ（部分unique index）
search_index           -- 全文検索の索引（FTS5）。triggerが保つ
landed_commits         -- 着地したcommit。run_integratedのeventからtriggerが書く
binary_updates         -- 使っていない（自動更新の経過はupdate_*のevent）
```

## 集約の読み書き

`tasks`・`goals`・`task_runs`の行はdomainの集約`Task` / `Goal` / `TaskRun`と行き来する（[domain-model](domain-model.md#集約-taskとgoal)、[集約: TaskRun](domain-model.md#集約-taskrun)）。

- 入口: task・goal・依存・claim・一覧は`src/infrastructure/sqlite.rs`、runの遷移はportごとに`src/infrastructure/runtime_store/`（`transitions.rs`・`recovery.rs`・`coordination.rs`・`session_registry.rs`・`run_log.rs`・`queue_records.rs`）。
- 読む: 行から`TaskRecord`などを組んで`restore`で復元し、復元が拒んだ行は変換エラーになる（既定値に読み替えない）。
- 新しく作る: `next_id`がトランザクションの中で次のIDを決めてから集約を作るので、拒否で巻き戻ればIDも消費しない。
- 変える: 読んだ集約をdomainのコマンドに渡し、返った値を書く。
  `WHERE status=<読んだstatus>`や`AND supervisor_token=<token>`は並行の変更を見つける条件で、どの遷移を許すかの判断ではない。
- 認可の後の変化: 計画系のコマンドは認可に使ったtaskの状態を渡し、storeはトランザクションを開いた直後に照合して、変わっていれば何も書かずに拒む（[Authorization](authorization.md)）。
  supervisorのように認可を経ない書き手は照合しない。
- 遷移に付くevent: domainの記録つきのコマンド（`src/domain/run/recorded.rs`）が更新後のrunとeventの列を返し、storeは同じトランザクションで書くだけにする。
  storeが足すのは自分の表から読むもの（resumeの作業の内訳とtoken）とleaseの解放だけ。
- runのdomainの拒否は、トランザクションが巻き戻ってeventに残せないので、run directoryの`refusals.log`に1行で残す（`runtime_store::REFUSALS_LOG`）。
- 循環は、task依存・goal依存・goalの所属taskを合わせた辺の上の再帰CTE（`waits_for`）で見つけるので、goalを経る循環も拒む。
- claimの順は依存グラフ全体で決めるのでSQLで決めず、`claim_task`が同じトランザクションで`dependency_graph`の順に並べる。
- note（人とplannerのメモ）は表を持たず、`run_events`のkind `observation`として書く。

## asks

`asks`は人の判断を要る相談を1行で持つ（[ADR-0022](../adr/0022-ask-answer-inbox-planner-and-landing-on-doubt.md)、[ask](supervisor-lifecycle/ask.md)）。
入口は`src/infrastructure/asks.rs`と`runtime_store/ask_store.rs`、行の型は`src/domain/`の`Ask`と`NewAsk`。

- openは回答もcloseも無い行で、部分UNIQUE index `asks_open`が（task・run・kind・理由・subject・finding）ごとに1件に限る。
  同じaskを開こうとすると既存の行を返して何も書かない。
- 登録・回答・close・運んだ印は、それぞれ行とevent（`ask_opened`・`ask_answered`・`ask_closed`・`ask_delivered`）を1トランザクションで書くので、`watch`のcursorに乗る。
- askを終えるeventは`ask_answered`だけで、`stats`はそれで未回答を判定する。
  未回答のaskはcloseできない。
- runtimeが自分で書く回答の`answered_by`は`runtime`で、権限の出どころは`answer_authority`が別に持つ（[Authorization](authorization.md)）。
- 回答をsupervisorが適用するaskは、`ask_answered`に`runtime_delivers`を書く。
- 計画の依頼の`planner_question`は、`request_id`に加えて`subject`に依頼を書く。
  `asks_open`は`request_id`を含まず、unique indexの作り直しは互換のmigrationにならないため（`asks::request_ask_subject`）。
- `queue_hold`のaskは止めたrunとjobを`affected`に足していく（[認証と利用上限のaskの待ちとanswer](supervisor-lifecycle/queue-hold.md)）。
- 誰が動かすかは列から導く（`Ask::waits_for`）: openか回答済みで閉じていないものはinbox、閉じたものは誰も待たない。

### draft planners（`draft_planners.rs`）

runtimeやjobが作ったdraftに立てるplannerの記録（[draft planners](supervisor-lifecycle/draft-planners.md)、[domain-model](domain-model.md#draft-planners)）。
入口は`src/infrastructure/draft_planners.rs`（port `DraftPlannerStore`）。

- 出どころ: `draft_origins`は登録のとき1回だけ書き、`draft_reopens`はproposalの取り下げだけが書く。
  `draft_reopens`の行は、材料の`proposal_id`がtaskの今の所属と同じときだけ効く。
- receiptのfollow_upsの登録は1つのreceiptを1トランザクションで書き、途中で失敗すれば全部を巻き戻す。
- 対象（`planner_drafts`）は、閉じていないplannerも未closeの質問も依頼の参照も無く、使い切っていないdraftをID順に返す。
  同じ束の鍵を持つdraftは、その束のplannerが閉じるまで待つ。
- 立てる（`open_draft_planner`）は、対象であることをトランザクションの中で再検査してから行を作るので、2つのsupervisorが同じdraftに立てない。
- 結末: plannerを閉じたトランザクションの中で、束のdraftごとに今のtaskの状態から結末を決めて書く（`settle_bundle`）。
- 人の答え待ちの印（`planner_answer_wait`）はaskが未回答か配送に失敗したときだけ書き、印の付いた行は回答の行き先とdraft・finding・依頼の上限の数から外れる（[ADR-t1704-1](../adr/2026-10-05-t1704-1-human-answer-wait-releases-runtime-planner-slots.md)）。
- 回答の行き先（`planner_answer_route`）は、生きているplanner・新しいplanner・人・閉じるのどれかで、`answer`も同じ判定で`runtime_delivers`を書く。
  依頼のplannerが作り由来もproposalも持たないdraftの回答は、依頼の回答の経路に乗せる（`request_of_draft`。作成のeventのactorで依頼を引く、[ADR-t2015-1](../adr/2026-10-07-t2015-1-answers-about-a-request-planners-draft-go-the-requests-way.md)）。
- runtimeのplannerがfollow-upをsubmitできる深さの上限は[ADR-t808-1](../adr/2026-09-28-t808-1-runtime-planners-submit-follow-ups-up-to-depth-two.md)で、`proposals::submit`の中の`check_adoptions`が拒む。

### planning requests（`plan_requests.rs`）

人がinboxに頼んだ計画の依頼と、依頼ごとに立てるruntimeのplanner（[ADR-t1394-1](../adr/2026-10-03-t1394-1-abolish-person-planners-and-route-planning-through-inbox-requests.md)、[inboxからの計画の依頼](supervisor-lifecycle/plan-planners.md#inboxからの計画の依頼)、[domain-model](domain-model.md#planning-requests)）。
入口は`src/infrastructure/plan_requests.rs`（port `PlanRequestStore`と`RequestStore`）。

- 依頼のeventはtask・goal・runに付かないqueueのeventで、payloadに`request_id`を持つ。
- 立てる（`open_request_planner`）は対象であることをトランザクションの中で再検査し、上限に達した依頼を`exhausted`にする。
- 却下と`ask --request`は依頼のplanner自身だけが打て、却下は判定に使ったplannerがまだその依頼のものかをトランザクションの中で確かめる。
- 依頼のaskの回答は、依頼のplannerが生きていればそのplannerへ、いなければ新しいplannerか閉じるで、人へは回さない（`route_of`）。
  依頼のplannerが作ったdraftの回答は、依頼が`open`でなくても新しいplannerにする（`draft_route_of`）。

## findings

`findings`はobserverが見つけた問題を1行で持つ（[ADR-0044](../adr/0044-findings-proposals-from-findings-and-quiet-observer.md)、[Observer](supervisor-lifecycle/observer.md)）。
入口は`src/infrastructure/findings.rs`、判断は`src/domain/finding.rs`。

- 記録は同じkind・対象・subjectの行に統合し（`finding::merge`）、何も変わらなければ何も書かない。
  `resolved`の問題がまた起きれば`open`に戻す。
- eventは対象のtask・run・goalに付き、queueのfindingはどれにも付かない。
- 一覧は件数が小さいのでSQLで絞らず、domainで絞って並べる。
- CIの見張りの`ci_failure`は下の「CIの見張り」。

## Runtime ownership

runの所有・supervisorの登録・runのファイルの約束（[ADR-0007](../adr/0007-run-level-leases-parallel-execution.md)、[ADR-0039](../adr/0039-adopt-stale-lease-of-live-wrapper-and-renew-own-stale-lease.md)）。
入口は`src/infrastructure/runtime_store/coordination.rs`と`transitions.rs`、`recovery.rs`。

- leaseは実行中のrunの所有で、実行の記録と揮発する所有権を混ぜないよう`task_runs`の列にせず別の表にする（ADR-0007）。
- claimはrun・`supervisor_token`・lease行を1トランザクションで作り、所有者のないclaimed runを作らない。
- runを変える書き込みは同じトランザクションで自分のlease行のheartbeatを更新し（`renew_lease`）、0行なら何も書かずに拒む。
  heartbeatの古さは拒む理由にしない（hostのsleepの後も、引き継がれるまでは自分のもの）。
- 動いているrunのleaseを奪うのは引き継ぎ（`adopt_run`）だけで、他のtokenのstaleなleaseでwrapperが生きているか終了を記録したrunだけを対象にする。
  自分の更新と引き継ぎはどちらもlease行への書き込みなので、SQLiteが直列化して片方だけが通る。
- `supervisor_token`は今そのrunを動かしているsupervisorで、claimしたsupervisorは`run_adopted`のeventに残る。
- `integrate`は登録せずにleaseだけを持つので、`run_leases.token`から`supervisors`へ外部キーを張らない。
- `supervisors`の行はそのプロセスの性質（`mode`・`binary_version`・引き継ぎの印）を持ち、行と寿命を共にする。
  `binary_version`は登録するプロセス自身だけが書き、`mode`は`up`が`launchd`を書く（`in_cmux`は廃止前の登録）。
- 登録の行を消すのは`supervise`自身の終了と`up`・`down`の掃除（`prune_supervisor`）だけで、どちらも同じトランザクションで`supervisor_stopped`を書く。
  heartbeatか停止の記録に失敗したときは消さず、staleとして次の掃除に任せる。
  `status`・`doctor`・`recover`は消さない。
- 生存の証拠は`SUPERVISOR_ALIVE_INTERVAL_SECS`ごとの`supervisor_alive`に残り、その記録の失敗はheartbeatを止めない。
- `recover`だけがtokenなしで未完了のrunを止め、同じトランザクションでleaseとプロセスを再検査する。
- 着地（`integrate`）は自分のtokenでlease行を持ち、`integrating`から出る遷移はどれもそのlease行を要する。
  Gitの操作はトランザクションの外で行い、mainを進める前のエラーではrunを元のstatusに戻す。
- sessionを止めた時刻（`workspace_closed_at`）はeventから導かず列に持ち、止めていないsessionを1つの問いで拾えるようにする。
  失敗すれば列はnullのまま`cleanup_failed`を書く。
- receiptの内容は読んだ時点でevent（`validation_finished`・`integration_receipt`）に写すので、worktreeとrun dirが消えた後も`show`で辿れる。
  `needs_session`をsessionが解消した後のreceiptは、そのrunの最後の`integration_receipt`が持つ。
- backendの呼び出しの失敗とsessionの終了の依頼は、runのstatusを変えずeventだけで表す（[backendの呼び出しの失敗](supervisor-lifecycle/backend-call-failures.md)、[receiptとsessionの終了](supervisor-lifecycle/receipt-and-session-exit.md)）。
- 時刻: 1つの操作は`Clock`を1回読んで使い回し、leaseの鮮度の判定の基準もその時刻にする。

### queue service

queue service（[Queue service](queue-service.md)）もDBを開くプロセスの1つで、要求ごとに接続を開き、要求のprincipalのactorで書く。
DBを直接開く他のプロセスとは、SQLiteのトランザクションとleaseの規則で並ぶ。
serviceのファイル（socket・lock・`state.json`・token）はqueueのディレクトリの`service/`に置き、DBには置かない。

## follow-upの所属の判断

follow-upの所属の判断・訂正とdraft / readyの所属の変更は同じ`BEGIN IMMEDIATE`のトランザクションで行う。
登録のトランザクションで元のtaskのgoalと状態を読み直し、`draft_origins`の材料に残す。
行・遷移・必須の情報・旧schemaの読み方は[所属の判断](follow-up-membership.md)、入口は`src/infrastructure/follow_up_membership.rs`。

## Transactions and constraints

- `BEGIN IMMEDIATE`で状態の変更・依存グラフの検証・claimを直列化する。
  ロックの待ちは最大5秒で、超えればerrorを返す。
  supervisorのheartbeatは`QueueBusy`を次の間隔で書き直す（[supervise](supervisor-lifecycle/supervise.md#supervise)）。
- claimは候補の選択・taskの更新・runの作成・event・lease行を1つのトランザクションにまとめ、同時のclaimは別々のtaskを取る。
- queue全体の実行枠は無く、同時に動くrunの数は`supervise --parallel`が決める。
- 外部キーと依存の複合主キーはDBに置き、自己依存はdomainが拒み、循環はトランザクションの中の再帰CTEで見つける。
- taskの詳細と一覧は1つのread transactionで読むので、task・run・eventのスナップショットが揃う。
- 複数のcontextを1つのトランザクションで変えてよいものは[architecture](architecture.md)の一覧が持つ。
- SQLiteのトランザクションの挙動は[公式仕様](https://www.sqlite.org/lang_transaction.html)に従う。

### CHECK制約を使わない

schemaが安定したと人が決めるまで、SQLiteのCHECK制約を使わない（[ADR-t876-1](../adr/2026-09-28-t876-1-no-sqlite-check-constraints-until-schema-is-stable.md)）。
今のqueueにCHECKは1つも無い。

- 対象はCHECKだけで、NOT NULL・UNIQUE・主キー・外部キー・DEFAULTはDBに残す。
- 規則の置き場所: 値の一覧はdomainの型（`string_enum!`）、型で表せない規則は書き込みのportが書く前に検査し、破れていれば書かずにerrorにする（`src/domain/write_rules.rs`）。
- 読むとき: askとeventのkind、taskの`change`は知らない値を寛容に読む。
  それ以外の列で規則の外の値を読むと、その読み込みはerrorで止まり（fail closed）、DBの行はその場で直さない。
- 値を足すとき: kindの列への追加はmigrationを要さない。
  読む側がfail closedの列への追加は、古いバイナリが読めないので非互換の宣言のmigrationで下限を上げる（表は作り直さない）。
- kindの規則（[ADR-0073](../adr/0073-kind-additions-are-compatible.md)）: 書き込み口はkindを`EventKind`（`src/domain/event_kind.rs`）で受け、kindに結び付く規則は`check_ask_kind` / `check_event_target`（`src/domain/mod.rs`）が書く前に検査する。
  queueのeventのkindは`EventKind::is_queue`にも足す。
- 新しいmigrationにCHECKを書かないことは`scripts/check-migration-numbers.sh`と`schema.rs`の`kind_enumerations`が検査する。

## Queue location

queueはrepositoryごとに1つで、場所は`src/infrastructure/location.rs`の`QueueLocation`が決める（[ADR-0006](../adr/0006-queue-per-repository.md)）。

```text
$XDG_DATA_HOME/dagq/<hash>/          未設定・空・相対pathなら $HOME/.local/share
  queue.db                           SQLite（WALの -wal / -shm も隣に置かれる）
  repository                         束縛先のGit common directory（人向けの逆引き）
  runs/<run-id>/                     prompt、runner、worktree/、receipt、refusals.log、ログなどrunのファイル
  logs/                              processごとのJSON Lines、自動更新の出力、launchd.log、rebind.jsonl
  service/                           queue serviceのsocket・lock・token
  backups/                           非互換のmigrateの前の複製
~/Library/LaunchAgents/com.dagq.<hash>.plist   upが書くsupervisorのLaunchAgent
```

- `<hash>`はcanonicalizeしたGit common directoryのhashなので、symlink経由やworktreeからでも同じqueueになる。
- `--db PATH`は明示のoverrideで、run dirとlog dirはそのDBの隣に置く。
  LaunchAgentのlabelはDBのpathを正規化してからhashするので、相対と絶対のpathが同じlabelを指す。
- repositoryを移すとhashが変わり、新しいqueueに解決される。
  付け替えは明示の`rebind`だけが行う（[ADR-0020](../adr/0020-rebind-queue-to-a-moved-repository.md)、[rebind](supervisor-lifecycle/rebind.md)）。
- queueのディレクトリは丸ごと移せる: runのpathは開いた場所から解決し直し、worktreeのGitの管理情報は`integrate`が直す（[ADR-0017](../adr/0017-resolve-run-paths-from-the-queue-directory.md)）。
- `logs/`の書式と消す時期は[Logs](supervisor-lifecycle/logs.md#logs)。

## Database setup and migrations

入口は`src/infrastructure/schema.rs`（`MIGRATIONS`と互換の判定）、`SqliteQueue::open` / `open_read_only` / `open_watch` / `migrate`（`src/infrastructure/sqlite.rs`）。
migrationを足すtaskの規則は[migrations.md](../development/migrations.md)が持つ。

- `init`だけがDBを作る（cwdのqueueなら`queue_repository`も束縛する）。
  通常の操作は存在しないDBを暗黙に作らない。
- `application_id`でdagqのDBを見分け、他のアプリのDBは書き換えずに拒む。
- schemaの版は適用したmigrationの数（`PRAGMA user_version`）で、`migrations/`の`NNNN_<name>.sql`を置くだけで`build.rs`が一覧に足す（[ADR-0067](../adr/0067-migrations-are-listed-by-build-and-renumbered-on-landing.md)）。
  並行するrunが同じ番号を足したときは`integrate`がrebaseの後に振り直す。
- リリース済みのmigrationは中身も名前も変えない（[ADR-t614-2](../adr/2026-09-27-t614-2-released-migrations-are-immutable.md)、検査は`scripts/check-migration-numbers.sh`）。
- queueを開いただけではmigrateしない。
  migrationを適用するのは空のファイルに最新のschemaを作る`init`と`dagq migrate`だけ（[ADR-0045](../adr/0045-build-identifier-explicit-migrate-schema-compat-handoff-and-auto-update.md)）。
- 互換の宣言: 各migrationは先頭行で`compatible`か`breaking`を宣言し、互換と言える文の範囲は`schema.rs`のunit testが検査する。
  下限（`schema_floor`）は最後に適用した非互換のmigrationの版である。
- open: 下限がバイナリより新しければ止まり、`user_version`がバイナリより古ければ`dagq migrate`を案内して止まる。
  バイナリより新しくても下限以下なら動く。
  このため`INSERT`は必ず列を列挙し、行は列名で読む。
  claimのときに写したバイナリ（`runs/<id>/runner`）も互換のmigrationの後に動き続ける。
- 読み取り専用のopen: 状態を変えないコマンドは読み取り専用の接続で開き、何も書かない（[ADR-0045](../adr/0045-build-identifier-explicit-migrate-schema-compat-handoff-and-auto-update.md)決定18）。
  queueがバイナリより古ければメモリの複製をmigrateして読むので、開発中のバイナリで本番のqueueを読める。
  複製はsnapshotなので、待ち続ける`watch`はファイルを直接開き、古いschemaなら`dagq migrate`を促して終わる。
  複製はGitからcommit messageを埋めないので、その`search`はmessageの無い着地を見つけない。
- `dagq migrate`: 非互換のmigrationが残っていれば、使用中のsupervisor・run・wrapperが1つでもあると何も適用せずに挙げて止まり、通れば`backups/`に複製してから適用する。
- 入れ替えの中のmigrateは[install](supervisor-lifecycle/install.md#install)・[Auto-update](supervisor-lifecycle/auto-update.md#auto-update)・[`up` / `down`](supervisor-lifecycle/up-down.md)。
- 表の作り直し: 全ての列を列挙してrowidと`sqlite_sequence`も写し（消した行のIDを再び払い出さない）、`foreign_keys`はrunnerがトランザクションの外でOFFにしてcommitの前に`pragma_foreign_key_check`を確かめる。
  `run_events`を作り直すときは、actorの列・検索のtrigger・`run_events`のindexも作り直す。
- 落とし穴: `run_events`の式のindex（`events_by_opened_event`）は、queryが同じ式を書きaffinityの掛からない値（bindした値か`+o.id`）と比べるときだけ引かれる。
  `run_id IS NULL`で絞る問いは`+run_id`と書いて`events_by_run`で全部のqueueのeventを辿らない。

## 全文検索（search）

`dagq search`の索引（[ADR-0063](../adr/0063-full-text-search-related-with-mentions-and-search-strength-and-duplicate-of.md)決定1〜3）。
索引とtriggerは`migrations/0026_search.sql`、読むのは`src/infrastructure/search.rs`、問い合わせの解釈と抜粋は`src/domain/search.rs`。

- 1つのFTS5の表にtask・goal・note・着地のcommitをまとめる（表を分けると`bm25`の統計が分かれ、kindをまたいで順位を比べられない）。
- tokenizerは`trigram`で、空白で語を区切らない日本語とpathの部分一致を引く。
  3文字未満の語は索引で引けないので`LIKE`で絞り、`OR` / `NOT` / 括弧とは組み合わせられない。
- 索引はDBのtriggerが同じトランザクションで保つので、索引を知らない古いバイナリの書き込みにも追いつく。
  `tasks` / `goals` / `run_events`を作り直すmigrationはtriggerを作り直す。
- `landed_commits`は`run_integrated`のeventからtriggerが書き（欠けたpayloadは着地を止めずに行を飛ばす）、messageの無い行は`dagq migrate`がGitから埋める。

## 関連（related）

`dagq related TASK`（[ADR-0063](../adr/0063-full-text-search-related-with-mentions-and-search-strength-and-duplicate-of.md)決定4）は表を足さず、既存の表と`search_index`を読む。
読むのは`src/infrastructure/related.rs`、手がかりの取り出しと点数と重みは`src/domain/related.rs`。

- 点数は手がかりの重みの和で、多くのtaskが共有する手がかりほど割り引く（`rarity`）。
- 重みはADR-0063を置き換えずに変えてよく、変えたら`tests/it/related.rs`の確かめと合わせる。

## CIの見張り

[ADR-t1920-1](../adr/2026-10-06-t1920-1-supervisor-watches-main-ci-keeps-known-failures-and-files-fixes-through-findings.md)と[CI watch](supervisor-lifecycle/ci-watch.md)。
入口は`src/infrastructure/ci_watch_store.rs`。

- 既に落ちているtestの一覧は表を持たず、`ci_checked`のeventから求めるビューである。
- 実行1件の記録（event・finding・runtimeの解決）は1つの書き込みトランザクションで、最新の`ci_checked`を読み直し、他のsupervisorが先に記録していれば何も書かない。
- findingの`covered_by_task`は外部キーを持たず、runtimeは読んだばかりのtaskのIDだけを書く。

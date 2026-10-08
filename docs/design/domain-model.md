---
id: design-domain-model
type: design
title: Domain model
status: current
created: 2026-09-21
scope: domain
related:
  - adr-t1879-1
  - adr-t1704-1
  - adr-t1811-1
  - adr-t1639-1
  - adr-t1394-1
  - adr-t1632-1
  - adr-t1340-1
  - adr-t946-1
  - adr-t807-1
  - adr-t791-1
  - adr-0067
  - adr-t813-1
  - adr-t813-2
  - adr-0063
  - adr-0044
  - adr-0038
  - adr-0003
  - adr-0004
  - adr-0007
  - adr-0008
  - adr-0009
  - adr-0016
  - adr-0019
  - adr-0040
  - adr-0024
  - adr-0029
  - adr-0034
  - adr-0037
  - design-persistence
  - adr-t947-4
  - adr-t1992-1
---

# Domain model

queueが扱う業務の型（task・goal・proposal・run・finding・計画の依頼）と、その状態遷移と拒否の規則の概念と地図。
型・欄・既定値・エラー文の詳細は`src/domain/`の定義のそばのdoc commentが持ち、決定の理由はリンクしたADRが持つ。
保存の形は[persistence](persistence.md)、runを動かす流れは[supervisor-lifecycle](supervisor-lifecycle.md)が持つ。

## 目的

- 業務上の判断（遷移してよいか、拒むか、次に何をするか）を、I/Oを持たない1か所の純粋な関数に集める。
- 同じ判断をCLI・supervisor・`integrate`・queue serviceのどこから呼んでも同じ結果と同じ文にする。
- 人とAIが「この状態から何ができ、なぜ拒まれたか」をコードの入口から辿れるようにする。

## 全体の流れ

taskは計画（draft）から始まり、plan reviewを通ってready、claimでrunを持ち、runの着地でcompletedになる。

```text
task:  draft ──submit──▶ submitted ──approve（plan review）──▶ ready ──claim──▶ in_progress ──runの着地──▶ completed
submitted / ready ──draft──▶ draft、ready ──reopen（plan review）──▶ submitted
in_progress（runが全てfailed / interrupted）──retry──▶ ready、──draft──▶ draft
draft / submitted / ready / in_progress（未完了のrunが無い）──cancel──▶ canceled
draft / submitted ──ready --bypass-review（人）──▶ ready
```

runはclaimから着地までの1回の試行で、supervisorが進め、`integrate`だけが着地させる。

```text
claimed ──▶ starting ──▶ running ──▶ validating ──accept──▶ awaiting_integration ──▶ integrating ──▶ integrated
                                         │ reject                 │ review・landingの判断       │ 衝突・再検証の失敗
                                         ▼                        ▼                             ▼
                                failed / needs_session ◀──────────┴─────────────────────── needs_session ──▶（resume）
claimed〜validating ──interrupt──▶ interrupted、integrating ──interrupt──▶ awaiting_integration
failed / interrupted ──復旧jobのresume──▶ needs_session ──resumeを使い切る──▶ failed
```

goalはtaskの束が解く上位の課題で、draft → openの状態と、それと独立な1回のclose（verdict）を持つ。
proposalはplannerがplan reviewに出すtaskとgoalの束で、submitted → accepted / revising / canceledと進む。

## 責務と境界

- domain（`src/domain/`）は状態と遷移の判断、拒否の分類（`DomainError`）、記録するeventのkindとpayloadを決める。
  I/Oをせず、`anyhow`・`rusqlite`に依存しない（[ADR-0013](../adr/0013-layered-architecture-and-type-function-style.md)）。
- applicationとinfrastructureは「読む → domainの関数に渡す → 返った値を保存する」だけで、SQLの中で業務の判断をしない。
  保存の形とトランザクションは[persistence](persistence.md#集約の読み書き)。
- 集約（`Task`・`Goal`・`TaskRun`・`Proposal`・`Receipt`）の欄は非公開で、作る・変える入口はそのmoduleの関数だけ。
  保存済みの行の復元（`restore`）は作成時の規則を当て直さず、どのversionでも守られてきたことだけを確かめる。
- runの状態のうちstatusの外のもの（承認・回数・待ち）は列にせず、`run_events`を`RunHistory`に畳み込んで読む。
- CLIのJSONの欄名とeventのkind名・理由のコードは公開の契約で、名前を変えない（[ADR-0016](../adr/0016-maintainer-notification-and-compact-output.md)、[ADR-0034](../adr/0034-domain-events-carry-reason-codes-actor-and-configuration-changes.md)）。

## 不変条件

- taskは自分自身にも自分の属するgoalにも依存できない。
- 依存のグラフ（task依存の辺、goal依存の辺、goalから`canceled`でない所属taskへの暗黙の辺）は循環しない（[ADR-0038](../adr/0038-task-depends-on-a-goal-until-it-is-achieved.md)）。
- taskは高々1つのgoalに属し、閉じたgoalに属するtaskは増えない。
- goalの`closed_at`と`verdict`は同時にnullか同時に非nullで、verdictを消すのは人の`reopen`だけで、そのときも`goal_closed`のeventは残る。
- `in_progress`はclaimしたtaskだけが持ち、終端（`completed`・`canceled`）のtaskは変わらない。
- taskが`completed`になるのは、その`integrated`のrunを`integrate`がmainへ着地させたときだけ。
  receiptの自己申告だけでは成功せず、base commitの上のcommit、clean worktree、rebase後に1回だけ流す検証コマンドの成功が要る（[ADR-0040](../adr/0040-verify-once-review-run-env-graph-stats-and-task-priority-in-claim-order.md)決定1）。
- mainはtaskごとに1つのsquash commitの直線で、`integrated`のrunはtaskごとに1件、`integrating`のrunはqueueごとに1件。
- 1 runの失敗・中断・復旧は他のrunの状態・lease・process・資源を変えない。
- agentの異常終了だけでtaskを自動で再実行しない（次の一手は復旧jobのverdictか人が決める）。
- sessionのwrapperを止めたことを`workspace_closed_at`に記録するまで、runは閉じていない扱いにする。
- 同じdraftの束と同じ計画の依頼に、runtimeのplannerは同時に2つ立たない（storeが`BEGIN IMMEDIATE`で再検査してから記録する）。

## Entities

pathは`src/domain/`から。
`mod.rs`が各moduleを再公開し、各ファイルの冒頭のdoc commentがそのmoduleの決定とADRを持つ。

| 知りたいこと | コードの入口 |
| --- | --- |
| task | `task.rs`の`Task`・`TaskAction`・`TaskStatus::transition` |
| goal | `goal.rs`の`Goal`・`close`・`reopen`・`list` |
| proposal | `proposal.rs`の`Proposal`・`accept`・`send_back`・`withdraw` |
| run | `run.rs`の`TaskRun`、記録つきは`run/recorded.rs` |
| 入力型・復元用・読み取り用の型 | `input.rs`、`views.rs`の`TaskDetail`・`GoalDetail`・`RunPaths` |
| receipt | `receipt.rs`の`Receipt::parse`・`check_requiring` |
| finding | `finding.rs`の`Finding`・`merge`・`check_transition`・`settle` |
| workerのprovider・変更の種類・goalのラベル・path | `worker.rs`、`change.rs`、`goal_tag.rs`、`scope.rs` |

- sessionとworkspaceは独立のentityにせず、`TaskRun`のIDと`workspace_id`で表し、noteは`run_events`のkind `observation`で表す。
- receiptの`follow_ups`は検証に使わず、着地の後に`integrate`が元のtaskと同じgoalのdraftとして登録する（[ADR-0019](../adr/0019-move-routine-maintainer-work-into-the-runtime.md)決定4）。
  `category`と`membership_proposal`は形が違ってもreceiptを拒まない（[receiptとsessionの終了](supervisor-lifecycle/receipt-and-session-exit.md#follow_upsの分類コード)）。

## Current operations

CLIの各コマンドの引数と出力の欄は`src/main.rs`のclapの定義と`--help`が持ち、ここには判断の入口と約束だけを置く。

### taskの遷移と編集

- 遷移の判断: `TaskStatus::transition`（`TaskAction`ごとに許す元のstatus）。
- `submitted → ready`はplan reviewの経路（`proposal::accept`）だけが行い、bypassの無い`ready`はdraftとsubmittedを拒む。
  人の`ready --bypass-review`は`review_bypassed`を記録して飛ばす。
- `Ready`は失敗・中断したrunだけを持つ`in_progress`のtaskの再試行で、中身が変わらないのでplan reviewを通さない。
- 中身の編集（`edit`）はdraftとsubmittedだけで、readyのtaskはsubmittedに戻してから直す（[ADR-0044](../adr/0044-findings-proposals-from-findings-and-quiet-observer.md)決定9・14）。
  例外として、最新のrunが終わり生きているrunの無い`in_progress`のtaskのverifyだけはuserとinboxが変えられる（[ADR-t883-1](../adr/2026-09-30-t883-1-edit-ended-run-verification-before-inherited-retry.md)）。
- 循環の検出はstoreの再帰CTEが3種の辺を合わせて行い、domainはその結果を受けて拒む（`task::check_acyclic`ほか）。
- 走っているrunは`prompt.txt`の写しのままで、編集は次のclaimから効く。
- 重複のcancel（`cancel --duplicate-of`）の重複先は`canceled`でないtaskに限り、記録が連鎖も循環もしない（[ADR-0063](../adr/0063-full-text-search-related-with-mentions-and-search-strength-and-duplicate-of.md)決定5）。

### 優先度

- 基の値は個別の指定、無ければ所属goalの優先度、goalが無ければ`normal`で、1か所の`domain::base_priority`が決める（[ADR-t1639-1](../adr/2026-10-04-t1639-1-goal-priority-is-the-source-tasks-inherit-and-goals-carry-tags.md)決定2）。
- 個別の指定の無いtaskは状態によらずgoalの今の値に追随し、claimの時に凍らせない（[ADR-t1811-1](../adr/2026-10-05-t1811-1-tasks-without-own-priority-follow-the-goal-in-every-status.md)）。
  runがclaimされた優先度は表示でなく`run_claimed`の記録で読む。
- plan reviewはAI由来のtaskの個別の優先度を外す（人が置いた値と人間由来は残す。[Plan review](supervisor-lifecycle/plan-review.md)の6）。
- 誰が置いたか（`PriorityBy`。`human`は人・inbox・依頼の値）は効く段（`PrioritySource`）と別の軸で、goal・taskの由来（`RecordedOrigin`）は作成時に1回だけ書く（`domain::plan_request`、ADR-t1975-1決定2・5）。
- 効く優先度（`application::effective_priority`）は自分を推移的に待つ`ready`のtaskの値も継ぐが、永久にclaimされないtaskからは継がない（[ADR-t791-1](../adr/2026-09-28-t791-1-effective-priority-ignores-tasks-waiting-on-abandoned-goals.md)）。
- claimの順・`needs_session`の再開・復旧jobは効く優先度で1つの列に並ぶ（`domain::slot_order`、[ADR-t1850-1](../adr/2026-10-06-t1850-1-resumes-recovery-jobs-and-claims-share-one-line-by-effective-priority.md)）。
  goal間のrankは作らない。

### proposalとplan review

- `submit`はdraftのtaskとgoalを1つのproposalにする入口で、判断は`proposal.rs`、保存は`TaskStore::submit`。
- taskとgoalは同時に1つのactiveな（`submitted` / `revising`の）proposalにだけ属する（`proposal::check_task_joins` / `check_goal_joins`）。
- 所属の判断の無いfollow_upのdraftは、`submit`・`lint`・`ready --bypass-review`が同じ条件で止める（[ADR-t1504-2](../adr/2026-10-04-t1504-2-runtime-records-and-enforces-follow-up-membership-judgements.md)決定7、判定は[所属の判断](follow-up-membership.md)の「submit・lint・plan review」）。
- `lint`はrepositoryに依存しない決まった規則だけを持つ純粋関数`domain::lint::lint`で、repository固有の規則はplan reviewのpromptが文書から読む。
- approveは、submitの後に所属goalが閉じたtaskを`ready`にせず`draft`に戻す（閉じたgoalのtaskはclaimされないため）。
- `proposal withdraw`は閉じていない`approve_plan`のaskも閉じる。
  開いたままだと、taskが次に入ったproposalにその回答が当たるため。

### claimと実行の順

- `claim`だけが`ready → in_progress`へ動かし、同じトランザクションで`TaskRun::new`がrunを作る。
- 候補は依存が全て`completed`、goal依存の先が全て`achieved`で閉じ、draftのgoalに属さないreadyのtask（storeの`READY_QUERY`）。
- 順は1つの関数が決める: `dependency_graph`の`candidates`（効く優先度 → 解放する数 → ID）。
  `fill_slots`の`queue.candidates()`とstats・KPIの標本は控えを除かない。
  順は記録せず`candidates --ignore-deferrals`で読む。
- CLIの`candidates`と`graph`の`candidates`は共有の`claim_view`で、supervisorが最後に記録したclaimの控えを除き`deferred`に理由・ファイル・原因のrunと出す（[ADR-t1992-1](../adr/2026-10-07-t1992-1-candidates-show-the-order-as-claimed-from-the-supervisor-s-deferral-records.md)）。
  CLIは記録を読むだけで、claimを止める条件と生きたsupervisorの不在は`candidates`だけの`held`に出す。
- `wait_for_build`のtaskは、判定したsupervisorが順に名指したときだけclaimされ、順を渡さないclaimは取らない（[ADR-t1632-1](../adr/2026-10-05-t1632-1-claim-waits-for-a-build-that-contains-the-dependencies-landings.md)）。
- canceled・失敗・中断・統合待ち・着地中・セッション待ちは、依存の完了の条件を満たさない。

### goal

- close: `goal::close`が所属taskのstatusをverdictが許すかを決め、`goal::check_follow_ups`が`achieved`の前に元goalのfollow_upの所属の判断を求める（ADR-t1504-2決定8）。
  `goal close`・goal reviewの`achieved`・`approve_goal`の答えは同じトランザクションの検査を通る。
- 開き直し: `achieved`で閉じたgoalだけを、`correct_goal`のaskへの人の`reopen`で開き直す（`goal::reopen`、ADR-t1504-2決定9）。
  開き直したgoalを待つtaskのうちまだclaimされていないものは再び待ち、走っているrunは止めない。
- `abandoned`で閉じたgoalへの依存は新しく張れない（`goal::check_accepts_dependents`）。
  張ったtaskは永久にclaimされないため。
- acceptanceの版は集約の欄ではなく`GoalDetail`の欄で、文が変わったときだけ増え、所属の判断が判定時の版を持つ。
- 所属taskが全部終わったopenのgoalは、supervisorのgoal reviewが閉じるか、足りないものをdraftにするか、人に聞く（[Goal review](supervisor-lifecycle/goal-review.md)）。

### 取り残された依存

- 閉じたgoalに残った未完了のtaskを開いたgoalのtaskが待つと、待つ側は永久にclaimされない（`domain::StrandedDependency`）。
- runtimeは知らせるだけで、待っているtaskを自動で保留にしない。
  依存を外すか、cancelするか、取り直すかは計画の判断で、決まった規則では選べないため。
- 同じ待ちの集合は重ねて知らせず、鎖になっているときは根のtaskだけを知らせる。

### 予定: taskのhold

- holdはdraftと別の印で、taskのstatusを変えず、plan reviewを通った扱いを失わせない（ADR-t1879-1、まだ実装していない）。
- 印はdraft・editでは外れず、終端で効きを失う。
  状態ごとの効果・解除・権限は[taskのhold](supervisor-lifecycle/task-hold.md)が持つ。

### attentionと読む口

- attentionの判定は`event_attention`・`run_attention`・`supervisor_attention`、次の一手は`AttentionNext`（`src/domain/mod.rs`）。
  attentionは全てinboxのもので、plannerのものは無い（[ADR-0044](../adr/0044-findings-proposals-from-findings-and-quiet-observer.md)決定17）。
- `show`・`goal show`の圧縮形（`view`）は`--full`のキー名を変えずに省くか切り、全文に無い`claim_deferral`などを足す。
- `ci failures`は`ci_checked`のeventを畳み込んだビュー（`domain::ci_watch::WatchState::fold`）で、表を持たない（[CI watch](supervisor-lifecycle/ci-watch.md)）。

### 役割の柵

- observerの環境（`DAGQ_ROLE=observer`）は読み取りとfinding・`blocked`のaskだけを許し、一覧に無いコマンドは後から足したものも拒む（ADR-0044決定4）。
  表は[Authorization](authorization.md)の「Policy」が持つ。
- この判定は`DAGQ_ROLE`の申告に依る柵で、悪意ある実行は防がない。

## IDとcommitのnewtype

- 入口は`src/domain/ids.rs`で、task IDとgoal IDのように意味の違う値を型で分ける（ADR-0013決定4）。
- `RunId`と`CommitSha`の読み込みは生成と同じ検証を通し、空のrun IDや不正なcommitの行は変換のエラーになる。
- 落とし穴: receiptの`run_id`と`commit`は文字列のまま持つ。
  `Receipt::check`が決まった順で検証し、最初に外れた項目の文を`last_error`に書くため、parseの時点では検証しない。
- 落とし穴: Gitの`rebase`・`diff_*`などの引数は`main`やrefも受けるrevisionなので、`CommitSha`でなく`&str`のまま。

## 集約: TaskとGoal

- 作成は`Task::new` / `Goal::new`（作成時の規則を通す）、復元は`Task::restore` / `Goal::restore`。
- `updated_at`はDBが更新のたびに書き、`goal::close`だけは`closed_at`と揃えるため時刻を引数に取る。
- 集約は`Serialize`だけを持ち、`Deserialize`を持たない（作る入口を1つにするため）。
  入力型（`NewTask`・`NewGoal`・`GoalEdit`）は`Deserialize`を持つ。
- follow_upの深さ（`tasks.follow_up_depth`）は集約に持たせず、storeの列として読み書きする（下の「Draft planners」）。

## cancelの理由の分類コード（未実装）

[ADR-t947-4](../adr/2026-09-28-t947-4-cancel-carries-a-reason-code.md)の決定で、まだ実装していない。
実装までは`cancel`は理由を持たず、`--duplicate-of`だけが構造の理由である。
一覧と定義はこの節が持つ（ADR-t947-4決定5、元の分析は[cancel-reasons](../plans/cancel-reasons.md#ラベルの定義案)）。

- `dagq cancel TASK --reason <code> [--duplicate-of X] [--note <text>]`で、`--reason`か`--duplicate-of`のどちらかを必須にする。
- `duplicate`・`already_done`・`absorbed`・`re_registered`は`--duplicate-of`を必須にし、`other`は`--note`を必須にする。
- `--duplicate-of`だけのcancelは、runtimeがXの状態から`duplicate`（開いている）か`already_done`（`completed`）を補う。
- 一覧に無い値は拒まずに記録し、重複の組としては読まない。
- 重複の組として読む（`related`・`search`・`show`・`stats`）のは`duplicate`と`already_done`だけ。
- runtimeが適用するcancelは経路から理由を付け、askの`cancel`の答えは`answered_cancel`とaskのIDを記録する。
- 理由の無い過去のcancelは書き換えず、集計は`duplicate_of`があれば上の規則で補い、無ければ`unrecorded`と数える。

| コード | 定義 | `--duplicate-of` |
| --- | --- | --- |
| `duplicate` | 同じ中身の開いたtaskがある | 必須（開いたtask） |
| `already_done` | 中身は着地したtaskかmainで既に満たされている | 必須（着地したtask） |
| `absorbed` | 中身を別のtaskに移して閉じる | 必須（移した先） |
| `re_registered` | 同じ意図を新しいIDで登録し直した | 必須（新しいtask） |
| `superseded` | 前提の決定や方針が変わり要らなくなった（決定を`--note`に書く） | 後継があれば |
| `not_worth` | 中身は正しいが、変更と検証の費用に見合わない | — |
| `decision_moot` | 判断を求めるtaskで、答えが既に出たか、判断しないことにした | — |
| `not_repo_work` | repositoryの変更でなく、人かinboxがhostや外で行う作業か観察だけ | — |
| `stale` | 前提（役割・ファイル・コマンド）が消え、後継の決定も無い | — |
| `answered_cancel` | runtimeがaskの`cancel`の答えを適用した（人とplannerは選ばない） | — |
| `other` | どれにも当たらない（`--note`で説明する） | — |

## 集約: TaskRun

- 入口は`src/domain/run.rs`で、どのstatusからどのstatusへ移れるかの判断は全てここにあり、`runtime_store/`と`sqlite.rs`のSQLは判断しない。
- コマンドは`fn(run, ...) -> Result<TaskRun, DomainError>`の形で、許さない遷移を`RunTransitionNotAllowed`で拒む。
  各コマンドが許すstatusはその関数のdoc commentが持つ。
- eventを伴う遷移は記録つきの形（`run::Recorded`、`src/domain/run/recorded.rs`）が記録するkindとpayloadを決めて返し、storeは同じトランザクションで書くだけ（[persistence](persistence.md#集約の読み書き)）。
- 落とし穴: storeはrunのコマンドの拒否をCLIと`last_error`の従来の文に置き換えて返し、domainの理由はrun directoryの`refusals.log`に残す。
- 落とし穴: runのファイルのpathはDBの値を信じず、読むたびにqueueの今の`runs/`から解決し直す（`RunPaths`、[ADR-0017](../adr/0017-resolve-run-paths-from-the-queue-directory.md)）。
- resume・review・reviseの進みは新しい状態を足さず、eventで表す（[needs_session](supervisor-lifecycle/needs-session.md#needs_session)、[review](supervisor-lifecycle/review.md#review-supervisor)、[ADR-0027](../adr/0027-keep-worker-session-through-review-revise-verdict-and-merge-tree-precheck.md)）。
- 復旧のラウンドの状態は`domain::triage_state`が最後の`resume_started`以降のeventから導く（[triage](supervisor-lifecycle/triage.md#triage-supervisor)）。
  providerを使えずに落ちた復旧jobは`Pending`と読む（次のラウンドがもう一方のproviderで、`[provider_fallback] jobs = false`なら控えが解けた後に同じproviderで始まる）。

### receiptの検証（`domain::validation`）

- 受理か拒否かは純粋関数`validation::judge`が決め、次に要る事実を1つずつ求める。
  applicationの`integrate::check_receipt`は求められた事実だけをrun filesとGitから集めて渡し直す。
- 判定の順と各拒否の理由のコードは`judge`と`Fact`のdoc commentが持つ。
- `Validation::resumable()`（evidenceの欠けかscopeの違反だけ）が`needs_session`か`failed`かを決める。
- 検証コマンドはvalidatingでは流さず、`integrate`がrebase後に1回だけ流す（[Validation](supervisor-lifecycle/validation.md)）。

### runの履歴（`RunHistory`）

- 入口は`src/domain/run/history.rs`の`RunHistory::from_events`で、1 runのeventを借りて畳み込む読み取り専用の型。
- 回数の上限と承認で決まる判断（`decide_conflict`・`decide_revise`・`ResumeCount::exhausted`・`after_validation`）はdomainの関数だけが定数と比べ、supervisor・`integrate`はその結果に従ってI/Oをする。
- resumeの数え方（数える試行、衝突だけの試行、killだけの試行）と引き継ぐretryの判定は`domain::resume`（[ADR-t946-1](../adr/2026-09-29-t946-1-kill-only-resumes-have-their-own-limit.md)）。
- 判断に使うpayloadは文字列のkeyでなく型付きの復元（`src/domain/run/payload.rs`、[architecture](architecture.md)の規則C6）で読み、keyの綴りは型の定義の1か所だけにある。
- 落とし穴: 復元は過去の版が書いたものを読むので拒まず、欄の欠け・`null`・別の型の値は欄が無いと読む。
- eventのkind名は`domain::event_kind`か機能のmoduleの定数で名指す。

## DomainError

- 入口は`src/domain/error.rs`の`DomainError`で、業務上の拒否だけをvariantにし、汎用の`Other`を持たない。
- `Display`の文はCLIが`{"error": ...}`に出し、runtimeが`last_error`に書く文そのもので、文はdomainの単体testが固定する。
- I/Oをする層は境界で`?`により`anyhow::Error`へ変える。
- DBの文字列が既知のenumの値でなければ、`enum_col`が`UnknownValue`を`rusqlite`の変換エラーの原因として包む。

## Draft planners

runtimeやjobが作ったdraftに、runtimeが同じきっかけの束ごとに1つplannerを立てる（ADR-0044決定16、[ADR-t807-1](../adr/2026-09-28-t807-1-bundle-drafts-of-one-piece-of-work-for-one-runtime-planner.md)）。
規則と型は`src/domain/follow_up.rs`、storeは`src/infrastructure/draft_planners.rs`、流れは[Draft planners](supervisor-lifecycle/draft-planners.md#draft-planners-supervisor)。

- 出どころ（`DraftOrigin`）と材料はdraftごとに1回記録する。
  `revisit`は記録する出どころでなく、再検討の時刻が来た人のdraftをstoreがplannerの対象として読むときの名前（[ADR-t1540-1](../adr/2026-10-05-t1540-1-a-kept-draft-returns-to-runtime-planners-at-its-revisit-time.md)）。
- `reopened`のdraftはreadyに戻さない。
  食い違いを見つけたplan reviewを飛ばさないため。
- 束の鍵は`BundleKey::of`が材料から決め、作成時刻では束ねない。
- 上限（`MAX_DRAFT_PLANNERS`）はdraftごとに数え、達したdraftは計画の依頼を待つ（attentionの`DecideDraft`）。
  人のanswerを運ぶときと、人・inboxが付けた再検討の時刻が来たときは1回越える。
- 人の答えだけを待って終わったplanner（`PlannerSession::answer_wait_at`、draftの結末`DraftOutcome::AnswerWait`）はどの上限にも数えない（[ADR-t1704-1](../adr/2026-10-05-t1704-1-human-answer-wait-releases-runtime-planner-slots.md)決定5）。
- 同時に立つplannerの数の上限は束1つを1と数え、他の種類のplannerと共有する。
- 人を経ずに採用できるfollow_upの上限は`adopt_needs_person`（深さは[ADR-t808-1](../adr/2026-09-28-t808-1-runtime-planners-submit-follow-ups-up-to-depth-two.md)）。
- 落とし穴: follow_upの深さは`add`・人のsubmit・人の`adopt`を経たsubmit・`ready --bypass-review`で0に戻り、runtimeのplannerが人を経ずにsubmitしたtaskではそのまま残る。
- 知らない`AskKind`は`Other`として読み、記録はするがruntimeは適用しない（[ADR-0073](../adr/0073-kind-additions-are-compatible.md)決定21）。

## Planning requests

人がinboxに頼んだ計画の依頼で、supervisorが依頼ごとにruntimeのplannerを立てる（[ADR-t1394-1](../adr/2026-10-03-t1394-1-abolish-person-planners-and-route-planning-through-inbox-requests.md)決定2〜7）。
規則と型は`src/domain/plan_request.rs`、storeは`src/infrastructure/plan_requests.rs`、流れは[inboxからの計画の依頼](supervisor-lifecycle/plan-planners.md#inboxからの計画の依頼)。

- 記録できるのは`inbox`と`user`だけで、人の言葉と参照は記録の後に変わらない。
- `open`から`proposed` / `declined` / `exhausted`へ進み、終わりの状態はどこにも戻らない。
  `proposed`はproposalのその後の結末で変えない。
- 依頼を名指せるaskは`planner_question`だけで、その答えは人に回らず、依頼のplannerか新しいplannerに運ぶか閉じる。

## `TaskRun.last_error`

- 意味・上書き・読み方（statusとの組）は`TaskRun`の`last_error`の欄のdoc commentが持つ。
- 書く文は書くコマンドのdoc commentと、そのrunのeventの`reason` / `message`が持つ。
- コードは列を持たず、runのeventから導く（下の「理由の分類コード」）。

## providerの切り替えの理由（`SwitchReason`）

- 入口は`domain::provider_switch::SwitchReason`の閉じた集合で、`provider_switched`と`provider_held`の`reason`に入る（[ADR-t813-2](../adr/2026-09-28-t813-2-provider-per-task-and-mutual-fallback.md)）。
- 理由のコード（`ReasonCode`）とは別の集合で、どれも失敗ではなく「そのproviderが使えない」ことを言う。
- 控えの長さは`SwitchReason::hold_secs`で、利用上限の文が解ける時刻を言えばそれを優先する。
- 失敗したtestやturnでは切り替えず、使えないと安全に判定できるときだけ切り替える。
- 必須のreviewのsubagentを動かせない行き先は、reviewの起動の理由にだけ入りproviderは控えない（動かせる側の控えは待つ。[Review](supervisor-lifecycle/review.md#reviewのsubagent)）。

## headless jobの種類・権限の意図・失敗（`JobKind`・`JobAccess`・`JobFailure`）

- 入口は`domain::headless_job`の閉じた集合（[ADR-t1063-1](../adr/2026-09-29-t1063-1-headless-job-provider-per-role-with-intent-permissions.md)決定2・4、ADR-t1895-1決定1）。
- `JobFailure`の4つは`SwitchReason`と同値、認証と利用上限は人だけが動かす壁。
- 値の意味とClaude Codeへの訳は[Agent provider lifecycle](provider-lifecycle.md#headless-jobのinterface)。

## 理由の分類コード（`code`）

- 失敗・保留・中断を記録するeventは、payloadに`code`（`domain::ReasonCode`）を持ち、自由文の`reason` / `message` / `error`はそのまま残す（ADR-0034決定1）。
- コードの一覧と意味は`ReasonCode::ALL`と`meaning()`が正で、名前を変えるにはADRが要る。
- コードは「なぜ」、eventのkindは「どの工程で」を言うので、同じコードが別のkindに付く。
- コードに添える値にpath・workspace ID・pidなどマシンに依る値を入れない（[ADR-0032](../adr/0032-classify-records-into-domain-events-diagnostics-coordination-and-bodies.md)）。
- どのeventがどのコードを書くかは記録する経路のコードが持ち、cmuxの失敗の`backend_*`は`application::reason_of_error`が決める。
- 落とし穴: コードが入る前のeventには`code`が無く、読む側はそれを許す。
- 落とし穴: validationの保留に添えるeventと`backend_call_failed`は、別のeventと同じコードを重ねて持つので、`stats`の`reason_codes`は数えない（`REPEATED_CODE_KINDS`）。
- runの`last_error`のコードは`domain::reason::last_error_code`が、`last_error`を書いたか中断したeventのうち最新のものから導く（どのeventが当たるかは`explains_last_error`）。
  `status`・`show`の`last_error_code`と`stats`の`reason_codes`がこれを読む（[status](supervisor-lifecycle/status.md#status)）。

## CIの見張り（ADR-t1920-1）

- 着地先のbranchで既に落ちているtestの一覧は`domain::ci_watch::WatchState`が持つ（[ADR-t1920-1](../adr/2026-10-06-t1920-1-supervisor-watches-main-ci-keeps-known-failures-and-files-fixes-through-findings.md)）。
- `finding dismiss --covered-by`は`ci_failure`のfindingにだけ、閉じていないtaskだけを受ける（`domain::finding::check_covered_by`）。
- 見張りの流れと設定は[CI watch](supervisor-lifecycle/ci-watch.md)。

---
id: design-provider-lifecycle
type: design
title: Agent provider lifecycle
status: current
created: 2026-09-21
scope: provider
related:
  - design-supervisor-lifecycle-task-hold
  - adr-t1857-1
  - adr-t1486-1
  - design-execution-tokens
  - adr-t1570-1
  - adr-t655-1
  - adr-t1228-2
  - adr-t1340-1
  - adr-t1433-2
  - adr-0004
  - adr-t813-1
  - adr-t813-2
  - adr-t2080-1
  - adr-t813-3
  - adr-t1215-1
  - adr-t2086-1
  - adr-t1063-1
  - plan-headless-worker-spike
  - adr-0040
  - adr-0048
  - adr-0027
  - design-supervisor-lifecycle
  - adr-t803-1
---


# Agent provider lifecycle

## 目的

AI agent（Claude Code・Codex）の起動・出力の読み取り・失敗の分類・記録を、providerに依らない契約にまとめる。
applicationはCLIの引数や出力の形式を直接扱わず、providerの違いはadapterだけが持つ。
providerが使えないとき（実行ファイルが無い・起動できない・認証・利用上限）は、runやjobを失敗にせず、もう一方のproviderへ移るか待つ。
引数・出力の欄・既定値の意味は定義のそばのdoc comment（主に`src/application/ports/`・`src/infrastructure/adapters.rs`・`claude.rs`・`claude_turns.rs`・`codex.rs`・`codex_turns.rs`・`src/domain/provider_switch.rs`・`src/domain/actor_model.rs`・`src/application/supervise/provider.rs`）が持つ。

## 全体の流れ

```text
claim（domain::provider_switch::routes）── taskのproviderか、使えなければもう一方 ──► run（requested / actual provider）
        │
session wrapper ── AgentProvider::turn_command ──► claude -p / codex exec（1 turn = 1回の呼び出し）
        │  TurnReader が出力を TurnSignal / TurnResult に写す
        ▼
turn_finished（failure・tokens・model）── authentication / usage_limit / launch ──► supervisor の turn_at_wall
        │                                                    ├─ もう一方へ切り替え（provider_switched）
        │                                                    ├─ 控えが解けたら同じsessionへもう一度
        │                                                    └─ 人が要る壁は queue_hold の ask
        ▼
session_opened / session_turns / session_closed（区間・稼働時間・tokens・model・作業の内訳）

headless job（runのreview・復旧・plan review・goal review・observerなど）
  ── domain::actor_model::job_route ──► headless_command / review_command（promptはstdin）
  ── job_reply・job_failure・job_session ──► verdict の適用 / 失敗の分類と控え
```

## 責務と境界

- applicationは`AgentProvider`（起動するコマンド）と`AgentSignals`（idle marker・jobの出力の壁）と`TurnReader`（非対話のturnの出力）の3つのportだけでproviderにつながる（`src/application/ports/`）。
- コマンドは`std::process::Command`でなく値の`CommandSpec`で返し、起動は`Spawner` port（実装は`infrastructure::process::LocalSpawner`）が行う。
  supervisorとsession wrapperのユースケースはプロセスを直接扱わない。
- AI actorの起動は全て`ActorExecutor`（`HostActorExecutor`、`src/application/actor_executor.rs`）を通り、roleとcapabilityと環境はroleごとに1か所で決まる（[Roles](supervisor-lifecycle/roles.md#actorの起動actorexecutor)）。
- Claude Codeのsettings（hook・`permissions.deny`）はClaude Codeの実装のものなので、どれを書くかもadapter（`src/infrastructure/adapters.rs`の`agent_settings`・`write_settings`）が持つ。
  headless jobの側は権限の意図（`JobAccess`）だけを渡す（[ADR-t1063-1](../adr/2026-09-29-t1063-1-headless-job-provider-per-role-with-intent-permissions.md)）。
- どの失敗がproviderを切り替えるかの判断は`domain::provider_switch`と`domain::actor_model`の副作用の無い関数が持ち、supervisorの`supervise::provider`は観測を読んでそれを呼び、記録と依頼だけを行う。

## 不変条件

- 人の設定（`~/.claude.json`・`~/.codex/config.toml`・`auth.json`・`CODEX_HOME`）をruntimeは書かない。
  runごとの設定は起動の引数とrun dirのファイル（Claudeの`--settings`、Codexの`-c`とworktreeのrules）だけにする。
- providerの切り替えは使えない理由（`SwitchReason`）のときだけで、`model`・`sandbox`・`other`の失敗、止めたturn、receiptの`failed`は切り替えずに今までの復旧に回す（[ADR-t813-2](../adr/2026-09-28-t813-2-provider-per-task-and-mutual-fallback.md)決定3）。
- 1つのrunの切り替えは`MAX_PROVIDER_SWITCHES`回まで。
- 区間は開閉を決めたeventと同じトランザクション・同じ時刻で書き、1つの区間は1回だけ閉じる。
- transcriptは書き込みトランザクションの中で読まない。
- 稼働時間・トークン数・modelを読めなくても、event・run・jobの結果は変わらない。

## headless jobのinterface

worker以外のheadless jobは、providerに権限の意図・promptの渡し方・最終の返答・失敗の分類・sessionだけでつながる（[ADR-t1063-1](../adr/2026-09-29-t1063-1-headless-job-provider-per-role-with-intent-permissions.md)決定2〜4）。
どのproviderで動かすかは`dagq.toml`の`[roles.<role>]`の`provider`（[Actor model](supervisor-lifecycle/actor-model.md#provider)）。

| 知りたいこと | コードの入口 |
| --- | --- |
| 権限の意図とproviderごとの訳 | `domain::headless_job::JobAccess`、Claudeは`adapters.rs`の`claude_tools`、Codexは`codex.rs`の`JOB_SANDBOX` |
| jobとreviewのコマンド | `AgentProvider::headless_command`・`review_command`（`adapters.rs`・`codex.rs`） |
| promptをstdinに渡す仕組み | `CommandSpec::stdin`と`infrastructure::process::LocalSpawner` |
| 最終の返答 | `AgentProvider::job_reply`（Codexは`codex::last_message`） |
| 失敗の分類 | `domain::headless_job::JobFailure`、起動は`application::job_start_failure`、出力は`claude::job_failure`・`Codex::job_failure` |
| sessionのidと実際のmodel、jobのトークン数 | `AgentProvider::assign_session_id`（Claude）と`job_session`、`domain::headless_job::JobSession` |

約束と落とし穴:

- promptはargvに載せずstdinで渡す。
  引数とenvの合計はhostの`ARG_MAX`（macOSで約1MiB）を超えられず、大きなplan reviewのpromptが起動できなかったため。
  workerとplannerのturnは今もpromptを位置引数で渡す。
- 引数の上限（`E2BIG`）とstdinを用意できない失敗はjobの入力かsupervisorの環境の問題なので、そのjobだけの失敗にしてproviderを控えない。
  実行ファイルが無いなどproviderそのものの起動の失敗は控える。
- runのreviewを`headless_command`と別のportにするのは、reviewがrunに属し、run dirの設定・debug fileと、workerのsessionが持つworktreeを変える道具の拒否を要るため。
- Claudeのreviewは`--setting-sources ""`で、workerが変えられるworktreeの`.claude`・`.mcp.json`・`CLAUDE.md`とuserの設定を読まない（[ADR-t1470-1](../adr/2026-10-03-t1470-1-all-claude-run-reviews-load-no-setting-sources.md)）。
  reviewの設定は`Stop` hookを持たない。
  reviewの間もworkerのsessionは開いていて、reviewがidle markerを書くとsupervisorのidle判定を誤らせるため。
- Claudeの他のjobはdagqのsettingsを持たず、道具を`--allowedTools`で絞る。
  予約の道具はreviewと同じく`--disallowedTools`で拒む（`PRINT_MODE_DENIED_TOOLS`、`src/infrastructure/adapters.rs`。拒む理由は定数のdoc comment）。
- jobの子プロセスは`DAGQ_QUEUE`を持たず、queue serviceのsocketとjobのtokenでクライアントモードの`dagq`を使い、serviceがroleのpolicyで判定する（[Queue service](queue-service.md#クライアントモード)）。

### Codexのheadless job

`provider = "codex"`の役割のjobは、supervisorが見つけて`--version`の通ったCodex（`Ports::codex_jobs`）の`infrastructure::codex::Codex`で起動する（設定の最小形は[codex-headless-jobs-spike](../plans/codex-headless-jobs-spike.md)）。
役割ごとの流れは[Plan review](supervisor-lifecycle/plan-review.md#codexで動かす)・[Throughput review](supervisor-lifecycle/throughput-review.md#codexで動かすtask-1220)・[Observer](supervisor-lifecycle/observer.md#codexで動かす)・[Triage](supervisor-lifecycle/triage.md#codexで動かす)・[生きているsessionの復旧job](supervisor-lifecycle/background-recovery-job.md#codexで動かす)と[ADR-t1207-1](../adr/2026-09-30-t1207-1-codex-run-review.md)が持つ。

- 入口: `Codex::headless_command`・`review_command`・`apply_launch`（modelとeffort）、`codex::last_message`、`codex_turns::rollout_model`。
- どの`JobAccess`も読み取りだけのsandboxに訳す。
  queue serviceに届く必要があるときは、executorがsandboxをそのsocketだけを許すpermission profileに置き換える（[Queue service](queue-service.md#codexのsandboxからの到達adr-t1233-5決定4)）。
- `--skip-git-repo-check`をどのjobにも付ける。
  スループットの見直し・observer・復旧jobのcwdはGitの外で、flagが無いと`codex exec`が起動を拒むため。
  flagはGitの検査を飛ばすだけで、sandbox・承認・trustは変えない。
- jobにはtrustを渡さず、人の`~/.codex/config.toml`のtrustを継ぐ。
  runのreviewだけはworktreeを`untrusted`にする（`codex::distrust_config`）。
  main checkoutのtrustを継ぐと、workerが書けるworktreeの`.codex/config.toml`と`.codex/rules`がreviewに読まれるため（[ADR-t1570-1](../adr/2026-10-04-t1570-1-codex-run-review-distrusts-the-worktree-project.md)）。
- Codexにはsession idを付けられないので、開始のeventの`session_id`はnullで、終わりのeventがthreadのidと実際のmodelを持つ。

## Claude sessionの区間

runtimeが起動するsessionとjobは、kindごとの区間（`session_opened` / `session_turns` / `session_closed`のrun_events）として記録する（[ADR-0048](../adr/0048-record-claude-sessions-by-kind-with-open-and-active-time.md)）。

| 知りたいこと | コードの入口 |
| --- | --- |
| どのeventがどのkindの区間を開き閉じるか | `domain::sessions::changes`（純粋関数） |
| 区間を書く場所 | `infrastructure::sessions::follow`・`follow_goal`（`infrastructure::sqlite`のevent書き込みの後に呼ぶ） |
| hookが記録するinboxと人のplannerの区間 | `domain::sessions::hook_changes`、`infrastructure::sessions::record_hook`、hookは[plugin-integration](plugin-integration.md#sessionの区間hookadr-0048) |
| 失敗したreviewの区間を閉じる | `SessionRegistry::close_review_session`（`infrastructure::sessions::close_review`） |

約束と落とし穴:

- 終わりのeventの無いまま次の開始が来た区間は推定（`inferred`）で閉じ、時刻はtranscriptの最後のレコードに寄せる。
- 失敗したreviewの`review_failed`はworkerの`/exit`の後に書かれるので、supervisorはjobが終わった時点で区間を閉じ、区間に`/exit`の待ちを入れない。
- hookの区間は`SessionEnd`を取り逃しうる。
  workspaceの無い区間は同じkindの別sessionの開始で閉じる（ADR-t655-1）。
  plannerの行・wrapper・後の別sessionのinboxから終わった区間は、supervisorがcmuxを呼ばず推定で閉じて取り込む（`inferred_hook_closes`、ADR-t2022-1）。
  常駐sessionの長いidleを終了と取り違えないため、経過時間では閉じない。
- `/clear`の`SessionEnd`と次の`SessionStart`が両方来ても、閉じた区間への2回目の終了は何も書かないので二重に数えない。
- 非対話のruntimeのplannerの区間はhookでなくturnから記録し、turnの`claude -p`がhookを走らせても`record_hook`は書かない。
- Claude以外のproviderの区間はtranscriptを読まず、閉じるeventのpayloadからsession idとmodelを写す。

### transcriptと稼働時間

区間の稼働時間は、区間に属するturn（実際の入力から次の実際の入力の前の最後の出力まで）の長さの合計。

- 入口: `infrastructure::transcripts::ClaudeTranscripts`（port`Transcripts`）、`domain::transcript`（レコードとturn）、閉じは`infrastructure::sessions::read_before`と`close`。
- transcriptは書き込みのロックの外で前もって読み、解析までしておく（`read_before`）。
  workerのtranscriptは10 MBほどになり、ロックの中で読むと他のprocessの書き込みがbusy timeoutを超えるため（ADR-0048決定10）。
  前もって読めなかった区間は稼働時間なしで閉じる。
- 前もった解析は閉じの実際の終わりへ動かすだけで、トランザクションの中でtranscriptを解析し直さない。
- `worktime.jsonl`の明細はトランザクションのcommitを確かめてから書く（`sessions::watch_commits`）。
  SQLiteはrollbackしたeventのidを次の書き込みに振り直すので、閉じをeventのidで見分けると行が二重になりうるため。
- hookの`SessionEnd`はtranscriptを読まずに先に閉じ、稼働時間・tokens・modelは後でsupervisorの取り込みが最終の`session_turns`（`final: true`）に書く（ADR-t655-1）。
  短い`SessionEnd`の実行枠をファイルの読み取りに使わないため。
- 開いている区間の終わったturnは、supervisorが一定間隔とobserverの起動前に`session_turns`に書く（`SessionRegistry::record_session_turns`）。
- 非対話の区間（`route: headless`）とClaude以外のproviderの区間はtranscriptでなくturnのeventから取る（下の[非対話のworkerの区間](#非対話のworkerの区間)）。

### 作業の内訳

runのsessionの区間は、閉じるときに作業の内訳（foregroundのtool > backgroundのコマンド > subagent > model > idleの分類と、コマンドの分類ごとの時間）を`session_closed`の`work`とrun dirの`worktime.jsonl`に記録する。

- 入口: 分類は`domain::worktime`（`classify`・`breakdown`・`headless_breakdown`）、落ちたtestの名前は`domain::verify_failure::failed_tests`。
- hook（PreToolUse / PostToolUse）は使わない。
  backgroundのコマンドの終わりが取れないため。
- コマンドの分類はコマンド位置の語だけで見るので、引数や引用符・heredocの本文に`cargo`を含むだけのコマンドは数えない。
- cargo専用の分類（`e2e`・`llvm_cov`・`test`）はqueueのrepositoryがdagqのソース（[Source repository](supervisor-lifecycle/source-repository.md)）のときだけ付ける。
  判定はGitを実行するので`read_before`がトランザクションの前に済ませる。
- 落ちたtestの名前はtestを流すforegroundのコマンドの出力からだけ読む。
  ファイルの表示や検索の出力はtestの印を引用しうるため。
- Codexにはtranscriptが無いので、wrapperがturnごとに書くコマンドのfile（`turns/turn-NNNNNN.commands.jsonl`）から同じ形の内訳を作る（`domain::turn::codex_span_turns`）。
  Codexの出力は時刻を持たず、時刻は読んだ時刻なので約1秒の粒度になる（`TurnReader::stamp`）。
  この記録より前に閉じたCodexの区間は、時刻を後から決められないので遡って作らない。

### トークン数とコスト

区間で使ったトークン数を`session_closed`の`tokens`に記録する。

- 入口: `domain::tokens`（`TokenUsage`・`span_usage`）。
  非対話のturnとheadlessのjobの1回ごとの記録（contextの大きさとcompactionを含む）、inboxと人のplannerの区切り、今の穴は[Executionのトークン数](execution-tokens.md)。
- transcriptでは同じ`message.id`のレコードを1つのmessageとして1回だけ数え、sidechain（subagent）も数える。
- コストはClaude Codeがレコードに`costUSD`を書いた版だけ記録し、単価表からは計算しない。

### 非対話のworkerの区間

非対話の区間（`route: headless`）は、transcriptを読まずにrunの`turn_started` / `turn_finished`から記録する（[ADR-t813-2](../adr/2026-09-28-t813-2-provider-per-task-and-mutual-fallback.md)決定7）。
Claudeもtranscriptは読めるが、区間の数え方をproviderで分けないためstreamから取る。

- 入口: `domain::turn::HeadlessSpan`、`infrastructure::sessions::close_headless`、turnのトークンは`TurnResult::tokens`、コストは`domain::turn::turn_own_cost`。
- Codexのturnのトークン数はthreadのrolloutから数え、数えられないときだけ`turn.completed`のthreadの累計と前のturnの累計との差に落とす（規則は[Executionのトークン数](execution-tokens.md#codexの数える元)）。
- Claudeの`total_cost_usd`と`modelUsage`はsessionの累計なので、同じく差をturnの分にする（`TurnResult::cost_cumulative`、規則は[Executionのトークン数](execution-tokens.md#claudeの数える元)）。
  差を取る前に記録されたturnのcostは補正しないので、その期間のcostはresumeのあるrunを重ねて数える。
- Codexの区間はmodelを区間に書かず、各turnの`turn_finished`の`model`に持つ。

### modelとeffort

区間を閉じるとき、区間のmessageを書いたmodelとeffortをtranscriptから読んで記録する（[ADR-0079](../adr/0079-record-task-weight-predictions-and-trial-model-effort-selection.md)決定7）。

- 入口: `domain::tokens::span_models`、非対話のruntimeのplannerは`domain::turn::turns_model`。
- 既定で起動したjobの`launch`はmodelもeffortも持たないので、実際の値はここで読む値になる（[Actor model](supervisor-lifecycle/actor-model.md)）。
- 読み口は[stats](supervisor-lifecycle/stats.md#claude-session)。

## workerのproviderと経路

workerのproviderはtaskが選び、経路は非対話だけ（[ADR-t813-2](../adr/2026-09-28-t813-2-provider-per-task-and-mutual-fallback.md)決定1、[ADR-t1340-1](../adr/2026-10-02-t1340-1-claude-worker-defaults-to-headless.md)、対話の経路の廃止は[ADR-t1433-2](../adr/2026-10-03-t1433-2-abolish-the-interactive-route.md)）。
流れ（依頼・idle marker・終了・止め方・復旧）は[非対話のworker](supervisor-lifecycle/headless-worker.md)が持つ。

| 知りたいこと | コードの入口 |
| --- | --- |
| taskのworkerの型と既定 | `domain::worker`（`Worker`・`Worker::resolve`・`refuse_interactive`） |
| workerごとのadapterの組 | `WorkerAdapters`（`ports/`）、組み立ては`compose::worker_adapters` |
| claimの経路 | `domain::provider_switch::routes` |
| turnのコマンド | `AgentProvider::turn_command`・`turn_session_exists`（`adapters.rs`・`codex.rs`） |
| turnの出力の読み手 | `TurnReader`、`claude_turns::ClaudeTurnReader`・`codex_turns::CodexTurnReader` |
| providerごとの文面 | [Prompt](supervisor-lifecycle/prompt.md#経路とproviderごとの文面) |
| 実行ファイルの解決 | [Provider executables](provider-executables.md)、`up`での固定は[運用](../development/operations.md)の「`up`のコマンド」 |

約束と落とし穴:

- `tasks.worker_mode`は経路を明示したときだけ値を持ち、NULLは読むときにproviderの既定に解く。
  古い固定バイナリはNULLを対話のClaudeとして読む。
- `interactive`と記録されたtaskと過去のrunは書き換えず、claimとresumeで非対話で動く。
- Codexが見つからなくても`up`とsupervisorは止まらず、Codexのtaskを非対話のClaudeで始める。
- Claudeのsettingsの`permissions.deny`は名前やパターンでsignalを送るコマンドを拒む（`SIGNAL_BY_NAME_DENIED`）。
  拒めるのはコマンドの先頭だけなので、pidで止める規則をpromptでも伝える。
- `subagent_review`を要るtaskをCodexで動かしたときは、要るevidenceから外し、receiptの`not_applicable`を理由つきで受ける（`domain::required_of`）。
- 予定: holdの解除でproviderを変えると、成果を引き継ぐ新しいrunにする（[taskのhold](supervisor-lifecycle/task-hold.md)）。

### Codexの非対話のworker

`--provider codex`のturnは`infrastructure::codex::Codex`が組み立て、`CodexTurnReader`が読む（権限は[ADR-t813-3](../adr/2026-09-28-t813-3-codex-worker-permissions.md)、測定は[headless-worker-spike](../plans/headless-worker-spike.md)）。

- 入口: `Codex::turn_command`、権限の`-c`は`codex.rs`の`SANDBOX_CONFIG`と`writable_roots`、trustは`trust_config`、一時ファイルは`run_tmp_dir`、rulesは`RULES_PATH`、失敗の分類は`codex_turns::classify`、modelは`codex_turns::rollout_model`。
- `codex`の名前は、cmuxがPATHの先頭に置く一時的なshimを飛ばして解決する。
- `exec resume`は最初の呼び出しの権限を引き継がないので、権限とtrustの`-c`は毎回同じものを渡す。
- 書ける場所はworktree・worktreeの管理dirとdagqのbranchのref・run dir・cargoのregistry・一時ファイルの場所で、repository全体・`main`のref・`$HOME`・queueのdirは入れない。
- turnの`TMPDIR`はrun dirの`tmp`にし、元の`$TMPDIR`も書けるままにする。
- networkを開けるのはsupervisorがsandboxの外で起動したsccacheのserverに接続するためで、sandboxの中からの起動は拒む（ADR-t1215-1・ADR-t2086-1、[Run environment](supervisor-lifecycle/run-environment.md#sccacheのserver)）。
- worktreeのtrustを`trusted`で渡す。
  渡さないと`codex exec`がthreadを始めるときに人の`~/.codex/config.toml`へmain checkoutのtrustを書き込むため。
- `pkill` / `killall`を止める主の防御はsandboxで、worktreeに書くrules（`info/exclude`でcommitから外す）は補助。
  rulesは`sh -c`で回避できる。
- codexはcommandを別のprocess groupで走らせるので、turnを止めるときは子孫をpidで集めてから止める（[wrapperがturnを止めるとき](supervisor-lifecycle/headless-worker.md#wrapperがturnを止めるとき)）。
- Codexはsessionのidを起動時に決められないので、`thread.started`のidをwrapperが記録して次のturnでresumeする。
  resumeしたthreadをCodexが持っていなければ、そのthreadを忘れて新しいthreadで1回だけやり直す。
- Codexの失敗は構造化されていないので文で分類する。
  裸の`429`はrequest idにも現れるので見ない。
  認証の失敗は再試行で直らないので最初の再試行で止め、利用上限は短いrate limitを乗り越えうるので再試行では止めない。
- Codexの出力はmodelを持たないので、threadのrolloutを読み取りだけで開いてmodelを取る（トークン数も同じ）。
- supervisorとwrapperはsandboxの外で動くので、run dirのファイルはlinkを辿らず、開いた記述子に対して読み書きする（`infrastructure::agent_dir::Directory`）。
  sandboxがworkerに止めていることをworkerの代わりにしないため。
- Codexのsandboxは書き込みとsignalを止めるが隔離ではない（[Security](security.md#codexのworkerのsandbox)）。

## 使えないproviderからの切り替え

[ADR-t813-2](../adr/2026-09-28-t813-2-provider-per-task-and-mutual-fallback.md)決定2〜6と、切り替えを止める設定の[ADR-t1857-1](../adr/2026-10-06-t1857-1-provider-fallback-can-be-turned-off-for-workers-and-jobs.md)。

| 知りたいこと | コードの入口 |
| --- | --- |
| 使えない理由と控えの長さ | `domain::provider_switch::SwitchReason`（`hold_secs`）、[domain-model](domain-model.md#providerの切り替えの理由switchreason) |
| 控えとその解ける時刻 | `ProviderHold`、`provider_switch::reset_at` |
| claimの経路と止める設定 | `provider_switch::routes`・`stopped_by_fallback`、`ProviderFallback`、読み手は`infrastructure::run_env::load_provider_fallback` |
| turnの壁での判断 | `provider_switch::wall_move`・`wall_to_raise`、supervisorは`supervise::provider`の`turn_at_wall` |
| 切り替えとやり直しの文 | `supervise::provider::switch_text`・`retry_text` |
| headless jobの行き先 | `domain::actor_model::job_route`・`job_wait_text`、runのreviewは`supervise::provider::review_route` |
| timer job（observer・スループットの見直し）の使えない終わり | `domain::actor_model::records_unusable`、`supervise::observer::retries_unusable`、`supervise::provider::hold_unusable` |

約束と落とし穴:

- Claudeの認証と利用上限の控えはqueueの`queue_hold`のaskそのもので、Claudeだけで動く役割も止まるので常に人に届ける（[Queue hold](supervisor-lifecycle/queue-hold.md)）。
  Codexの控えと、Claudeが起動できない控えは`ProviderHold`でaskを開かず、解ける時刻の後に確かめ直す。
- 解ける時刻はproviderの文がunix時刻か長さで言うときだけ読み、時計の時刻はtime zoneが分からないので読まない。
- turnが壁に当たってもwrapperはsessionを終えずに次の依頼を待ち、supervisorが切り替えるか、控えが解けたら同じsessionへもう一度送るか、待つ。
  切り替えられないときもrunは失敗にしない。
- 切り替えた後は会話が引き継がれないので、文がcommitと変更を見て続けるよう言い、promptも新しいproviderの文面で書き直す。
- 両方が起動できないときと、Codexの壁でClaudeが使えるのに切り替えの上限に達したときは、人を待つものが無いのでaskを開かない。
- runtimeのplannerは同じ壁で同じsessionを待たせ、もう一度送るが、providerは切り替えない（[Plan planners](supervisor-lifecycle/plan-planners.md#runtimeのplannerの経路)）。
- headless jobは1回きりなので、起動の前に行き先を決め、起動の後に使えないと分かったjobは失敗を記録して次のpassでもう一方で起動し直す。
  runのreviewは行を持たないので、同じpassで起動し直す（[Review](supervisor-lifecycle/review.md)、goal reviewは[Goal review](supervisor-lifecycle/goal-review.md)の3と6）。
- `[provider_fallback]`の`workers` / `jobs`がfalseでも、失敗の分類と控えは変わらず、もう一方へ移ることだけを止めて控えが解けるのを待つ。
  `jobs`が止めるのは`[roles.<role>]`に`provider`を書いたheadlessのjob（`claude`を書いたもの・復旧jobを含む）で、書かないjobは対象の外。
  `--no-claude`と能力による選択（reviewのsubagent）は止めない。
  設定はsupervisorが各passで読み直し、読めないときは使っている値のまま続ける（書式は[Run environment](supervisor-lifecycle/run-environment.md)）。
- timer jobの使えない終わりの読み取りは観測と分析の側（`supervise::observer`）が値にし、控えは実行と着地の側（`supervise::provider`）が行う（[Architecture](architecture.md)）。
- 記録と集計（`provider_switched`・`stats`の`provider_switches`・`kpi`の`provider=`）は[stats](supervisor-lifecycle/stats.md)と[kpi](supervisor-lifecycle/kpi.md)。
  集計のproviderは切り替えの後のactualで数える。

## 大きなcontextからの新しいsession

[ADR-t2080-1](../adr/2026-10-08-t2080-1-fresh-session-on-send-back-and-resume-when-peak-context-exceeds-threshold.md)。

判定は`domain::fresh_session::next_session`（直前のturnの`peak_context`と閾値だけ）、入口は`supervise::session`の`start_fresh_session`で、区切りと理由を`session_renewed`に残す。

- wrapperは`provider_switched`と同じ区切りとし、providerを変えず新しいsessionにする。
  どちらの後もClaudeのsessionは新しい名前（`domain::turn::session_name`。Claude Codeは使われた`--session-id`を拒む）。

## Trust prompt

Claude Codeのfolder trust dialogがrun worktreeで出るかは、worktreeの親repository（main checkout）のrootが`~/.claude.json`の`projects`に承認済みで記録されているかで決まる。
worktreeの置き場所、adapterが渡す引数、`--dangerously-skip-permissions`は関係しない。
承認のときに書かれるkeyもrepositoryのrootで、worktreeのpathは書かれない。

- 入口: `adapters.rs`の`claude_trusts_repository`と`claude_global_config`、`up`のpreflight（[`up` / `down`](supervisor-lifecycle/up-down.md)）。
- dialogを抑止するflagは無く、runtimeが承認を書くのは人の判断の代行になるので、`up`が未信頼のrepositoryを案内つきのerrorで止める。
- main checkoutの決め方は[Run environment](supervisor-lifecycle/run-environment.md#main-checkoutの決め方)、止まったsessionの扱いは`dagq-recover` skillの`reference/session.md`。
- `supervise`自体は検査しないので、`up`を経ない起動は未信頼ならdialogで止まり`prompt_waiting`になる。
- 非対話のworker・runtimeのplanner・jobは`-p`で動くのでdialogに当たらず、検査が要るのは対話のsession（inbox）だけ。

## 起動時のダイアログ

Claude Codeの起動直後に出うるダイアログと扱い:

- folder trust: 上の[Trust prompt](#trust-prompt)のとおり`up`のpreflightで止める。
- auto modeの初回案内（Teach auto mode）: run sessionの設定の`autoMode.environment: ["$defaults"]`で出なくなる（`stop_hook_settings`のdoc comment）。
- LSP pluginの推奨: settingsでもflagでも抑止できず、30秒で閉じるので何もしない。
- 抑止したダイアログは画面の兆候を見る`prompt_waiting`の検知に当たらない（[prompt-waiting](supervisor-lifecycle/prompt-waiting.md)）。

## Claude を使わない運転

`up --no-claude` / `supervise --no-claude`はClaudeを起動しない明示的な運転方針で、既定は無効（[ADR-t1204-1](../adr/2026-09-30-t1204-1-explicit-no-claude-operation.md)）。

- 方針の違うsupervisorは再利用・引き継ぎせず、先にdrainを求める。
- workerはClaudeを頼んだtaskもCodexへ振り分け、Codexの障害をClaudeの`queue_hold`にせず、Claudeへ戻さない。
- `[roles.<role>] provider = "codex"`の役割のjobはCodexで動き、失敗してもClaudeへ戻さず手動の経路（plan review by hand・`approve_landing`のask・`triage by hand`など）に渡す。
  Claudeを選ぶ役割は起動せず、`provider_disabled`で同じ手動の経路に渡す。
- plannerとinboxの自動起動を止め、人が代行する。
- `HostActorExecutor`はworkspaceとコマンドを作る前にClaudeを拒む。

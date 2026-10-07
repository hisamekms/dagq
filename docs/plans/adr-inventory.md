---
id: plan-adr-inventory
type: plan
title: ADR 0001〜0034の決定・後継ADR・designの対応表
status: completed
created: 2026-09-25
owners:
  - hisamekms
tags:
  - architecture
  - documentation
depends_on:
  - adr-t598-1
related:
  - adr-index
  - design-overview
---

# ADR 0001〜0034の決定・後継ADR・designの対応表

[ADR-t598-1](../adr/2026-09-26-t598-1-adr-id-is-task-id-small-adrs-and-design-holds-current-state.md)決定4・5・11に基づく、0001〜0034の各決定が有効か、どのADRに上書きされたか、今の姿をどの`docs/design/`の文書が持つかを引く対応表。なぜそうしたかは後継のADRを、今どうなっているかはdesignを読む。

2026-09-26のask 117で人がadoptと答え、plannerが棚卸しを組み直すことを決めた。旧方針のADR-0042はADR-t598-1に置き換え済みで、ここでは前提にしない。統合ADRの作成計画から対応表への組み直しを完了したため`status: completed`とする。下の候補の採否・実施が完了したという意味ではない。この変更では新しいtaskを登録せず、既存ADRの本文・frontmatterも変えない。

## 読み方

- **決定**: ADRのDecisionの番号。番号の無いADRはDecisionの箇条（または段落）を上から数えた番号（「箇条N」）で示す。Consequences・切替手順は、決定として読まれうるものだけ取り上げる。
- **有効**: 棚卸し時点でそのとおり。表現の中の旧称（SV、maintainerなど）を用語集で読み替えれば正しいものも、内容が生きていれば有効とし、根拠にそう書く。
- **上書き**: 後のADRの決定が内容を変えた、または無くした。後継の欄に上書きしたADRと決定を書く。上書きしたADRがさらに置き換えられていれば、今の後継（例: ADR-0023 → ADR-0040）を併記する。
- **失効**: 一時的な制約や一度きりの手順で、今は効かない（後のADRが変えたものも含む）。
- **記録**: 実装の割り当て・schemaの番号など、その時点の事実の記録で、決定ではない。現在の設計として引き継ぐ対象ではない。
- **未実装**: 後継の決定は`accepted`だが実装がまだのもの。ADRとしては後継の決定が現在の決定なので、上書きとして扱う。
- 対象外: 0032〜0034は`proposed`のまま（acceptedになるときにADR-t598-1とfrontmatter仕様に従う）。0012・0014・0023・0024は置き換え済み（0012 → [ADR-0039](../adr/0039-adopt-stale-lease-of-live-wrapper-and-renew-own-stale-lease.md)、0014 → [ADR-0045](../adr/0045-build-identifier-explicit-migrate-schema-compat-handoff-and-auto-update.md)、0023 → [ADR-0040](../adr/0040-verify-once-review-run-env-graph-stats-and-task-priority-in-claim-order.md)、0024 → [ADR-0041](../adr/0041-on-demand-planners-proposals-submitted-and-plan-review-job.md)）で、表には後継だけを書く。ADR-0041は2026-09-26に[ADR-0044](../adr/0044-findings-proposals-from-findings-and-quiet-observer.md)に丸ごと置き換えられ、決定1〜17は同じ番号で引き継がれた（決定4だけ内容が変わった）ので、この文書の「ADR-0041 決定N」はADR-0044の決定Nと読む。ADR-0044とADR-0019・ADR-0043は2026-09-26に[ADR-0047](../adr/0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)に丸ごと置き換えられた。ADR-0044の決定NはADR-0047の決定N、ADR-0019の決定NはADR-0047の決定23+N、ADR-0043の決定NはADR-0047の決定29+Nと読む（ADR-0047のContextの対応表）。ADR-0019の決定はADR-0047の決定24〜29から辿る。
- **判定の時点**: 表の「現在」「上書き」「根拠」は2026-09-25〜26の棚卸しの判定を保存する（「今」「未実装」も当時の記述）。今回、判定は変えず、designへの参照を追加した。以後の実装状況・細部は「今の姿（design）」の文書を読む。記録・一度きりの手順などに今の設計の記載が無ければ「designに無い」と示す。
- **後継の辿り方**: 置き換え済みのADRはADRの索引（`docs/adr/INDEX.md`、無ければ`sh scripts/adr-index.sh`で作る）と`superseded_by`で後継を辿り、`amended_by`があれば変更した決定を辿る。表のADR-0040はADR-0049へ、ADR-0045はADR-0073へ進む。本文を凍結した大きなADRの一部は小さなamendsで直す（ADR-t598-1決定5。現在の適用範囲は[ADR-t1091-1](../adr/2026-09-30-t1091-1-amend-or-replace-by-number-of-decisions.md)）。上書きされた決定があることだけを理由に旧ADRを丸ごと置き換えない。

## ADRごとの表

### ADR-0001 Rustでruntimeを実装する

| 決定 | 現在 | 上書き | 根拠 | 今の姿（design） |
| --- | --- | --- | --- | --- |
| 箇条1 runtimeとsupervisorをRustで実装する | 有効 | — | crateはRust | [overview](../design/overview.md) |
| 箇条1 単一の`cmux-taskq`バイナリとして配布する | 上書き | ADR-0015（対応表のcrate / バイナリ行） | バイナリ名は`dagq` | [plugin-integration](../design/plugin-integration.md) |
| 箇条2 domain / application / infrastructureをcrateまたはmoduleで分ける | 有効 | — | ADR-0013が1 crateのmodule境界に具体化した | [overview](../design/overview.md) |

### ADR-0002 cmuxを最初のworkspace backendにする

| 決定 | 現在 | 上書き | 根拠 | 今の姿（design） |
| --- | --- | --- | --- | --- |
| 箇条1 cmuxを必須のworkspace backendにする | 有効 | — | | [overview](../design/overview.md) |
| 箇条1 プロダクト名を`cmux-taskq`とする | 上書き | ADR-0015 | 名前は`dagq` | [overview](../design/overview.md) |
| 箇条2 domain / applicationはcmuxのAPIを直接参照せずportを介す | 有効 | — | `WorkspaceBackend`（overview） | [overview](../design/overview.md) |

### ADR-0003 supervisorがagentとworkspaceのライフサイクルを所有する

| 決定 | 現在 | 上書き | 根拠 | 今の姿（design） |
| --- | --- | --- | --- | --- |
| 箇条1 キューごとに一つのsupervisorを起動する | 上書き | ADR-0007 箇条2 | 同じqueueに複数のsupervisorが居てもよく、claimが直列化される | [supervise](../design/supervisor-lifecycle/supervise.md) |
| 箇条1 supervisorがworkspace作成・監視・完了検証・workspace削除を行う | 有効 | — | 閉じる時点はADR-0027 決定1でreviewの後になったが、所有者は変わらない | [supervise](../design/supervisor-lifecycle/supervise.md) |
| 箇条2 agentはworkspaceを削除せず、結果をreceiptで通知する | 有効 | — | AGENTS.mdのworker | [receipt-and-session-exit](../design/supervisor-lifecycle/receipt-and-session-exit.md) |

### ADR-0004 ClaudeとCodexをagent providerとして抽象化する

| 決定 | 現在 | 上書き | 根拠 | 今の姿（design） |
| --- | --- | --- | --- | --- |
| 箇条1 applicationはsessionの契約だけを使い、CLI引数などはadapterに閉じ込める | 有効 | — | `AgentProvider`（overview） | [overview](../design/overview.md)・[provider-lifecycle](../design/provider-lifecycle.md) |
| 箇条2 TaskRunにrequested / actual providerを記録する | 有効 | — | [domain-model](../design/domain-model.md)、[provider-lifecycle](../design/provider-lifecycle.md) | [domain-model](../design/domain-model.md)・[provider-lifecycle](../design/provider-lifecycle.md) |

棚卸し時点では全決定が有効。

### ADR-0005 runtimeをバイナリ、agent integrationをpluginとして配布する

| 決定 | 現在 | 上書き | 根拠 | 今の姿（design） |
| --- | --- | --- | --- | --- |
| 箇条1 runtimeを`cmux-taskq`バイナリとして配布する | 上書き | ADR-0015 | バイナリ名は`dagq`。配布経路はADR-0030がcrates.ioを足した（追加で矛盾しない） | [plugin-integration](../design/plugin-integration.md) |
| 箇条1 Claude Code / Codexのpluginがskill・hookからバイナリを呼ぶ | 有効 | — | 今あるのは`plugins/claude-dagq`だけで、Codexのpluginは未着手（決定は変わっていない） | [plugin-integration](../design/plugin-integration.md) |
| 箇条2 共通repoから各ecosystem向けのmanifestとpackageを出す | 有効 | — | | [plugin-integration](../design/plugin-integration.md) |

### ADR-0006 repositoryごとに1つのqueueをユーザーのデータディレクトリに置き、cwdから解決する

| 決定 | 現在 | 上書き | 根拠 | 今の姿（design） |
| --- | --- | --- | --- | --- |
| 箇条1 `$XDG_DATA_HOME/cmux-taskq/<hash>/queue.db`に置く、hashの規則、`repository`ファイル | 上書き | ADR-0015（データディレクトリ行） | ディレクトリ名は`dagq`。hashと`repository`ファイルは有効 | [persistence](../design/persistence.md) |
| 箇条2 run dir・worktree・logを`runs/<run-id>/`に置く | 有効 | — | ADR-0017が読むたびに解決する規則を足した | [persistence](../design/persistence.md) |
| 箇条3 cwdの`git rev-parse --git-common-dir`で解決、`--db`はoverride、`--repo`は任意 | 有効 | — | | [persistence](../design/persistence.md) |
| 箇条4 `init`で束縛し、以後の全コマンドがopen直後に検査する | 上書き | ADR-0020 決定1 | `rebind`だけは検査を通らず束縛を付け替える | [rebind](../design/supervisor-lifecycle/rebind.md) |
| 箇条5 `locate`、launcherは`CMUX_TASKQ_DB`のときだけ`--db` | 上書き | ADR-0015（環境変数行） | `DAGQ_DB`。`locate`は有効 | [persistence](../design/persistence.md)・[plugin-integration](../design/plugin-integration.md) |

### ADR-0007 leaseをrun単位にし、依存が解けたtaskを上限付きで並列に実行する

| 決定 | 現在 | 上書き | 根拠 | 今の姿（design） |
| --- | --- | --- | --- | --- |
| 箇条1 `run_leases`でrun単位のlease、supervisorごとのtoken | 有効 | — | migration 0005の記述は記録 | [persistence](../design/persistence.md) |
| 箇条2 claimとlease作成を同じトランザクションで行う、複数supervisorでも直列 | 有効 | — | | [persistence](../design/persistence.md) |
| 箇条3 `supervise --parallel N`の常駐ループ、`--once`、signal | 有効（一部上書き） | ADR-0027 決定1 | 「`awaiting_integration`になったrunのleaseを解放する」は、reviewの後までleaseとslotを持つに変わった | [supervise](../design/supervisor-lifecycle/supervise.md)・[review](../design/supervisor-lifecycle/review.md) |
| 箇条4 1 runのruntime error（exit要求のtimeoutを含む）はそのrunだけをabandonしleaseを削除する | 上書き | ADR-0019 決定2 | `exit_request_timed_out`ではleaseを手放さない。ほかのruntime errorのabandonは有効 | [abandon](../design/supervisor-lifecycle/abandon.md)・[receipt-and-session-exit](../design/supervisor-lifecycle/receipt-and-session-exit.md) |
| 箇条5 provisioningの失敗でclaimを止めdrainして非0で終わる | 有効 | — | | [supervise](../design/supervisor-lifecycle/supervise.md) |
| 箇条6 `doctor` / `recover`はrunごとに判定する | 有効 | — | ADR-0041 決定3でwrapperの死んだrunの`recover`はsupervisorが自動で行うようになったが、判定の単位は変わらない | [doctor](../design/supervisor-lifecycle/doctor.md)・[recover](../design/supervisor-lifecycle/recover.md) |

### ADR-0008 merge queueが最新mainへrebase・再検証し、1 task = 1 commitにsquashして着地させる

| 決定 | 現在 | 上書き | 根拠 | 今の姿（design） |
| --- | --- | --- | --- | --- |
| 箇条1 着地は`integrate ID` / `--next`。承認制でSVがレビュー後に呼び、承認なしの自動着地とpushはruntimeが行わない | 上書き | ADR-0040 決定2（ADR-0023 決定2）、ADR-0019 決定3 | review jobのpassでsupervisorが着地させ、`integrate`がpushする。`integrate ID` / `--next`の入口は有効 | [review](../design/supervisor-lifecycle/review.md)・[integrate](../design/supervisor-lifecycle/integrate.md) |
| 箇条2 統合slotは1つ、`one_integrating_run_per_queue`、staleなら`recover`で`awaiting_integration`へ | 有効 | — | | [integrate](../design/supervisor-lifecycle/integrate.md) |
| 箇条3 着地の手順（receipt検査 → rebase → 検証 → `commit-tree` → ref → main → 後始末） | 有効（一部上書き） | ADR-0015（branch `taskq/` → `dagq/`、ref `refs/taskq/` → `refs/dagq/`） | ADR-0017 決定3のrepair、ADR-0029 決定4のscope検査が足された（追加）。rebase後の検証はADR-0040 決定1と整合 | [integrate](../design/supervisor-lifecycle/integrate.md) |
| 箇条4 mainの進め方（`merge --ff-only`か`update-ref`） | 有効 | — | | [integrate](../design/supervisor-lifecycle/integrate.md) |
| 箇条5 commit messageのtrailer `Taskq-Task` / `Taskq-Run` | 上書き | ADR-0015（trailer行） | `Dagq-Task` / `Dagq-Run`。title・summaryの段落は有効 | [integrate](../design/supervisor-lifecycle/integrate.md) |
| 箇条6 衝突と再検証の失敗は`needs_session`。SVが`claude --resume`で開き直し、`integrate ID`で再開する | 上書き | ADR-0019 決定1 | `needs_session`にすることは有効。resumeはsupervisorが自動で行う | [needs-session](../design/supervisor-lifecycle/needs-session.md) |
| 箇条7 `failed` receiptはrunの終了、再試行や取り消しは手動 | 有効（一部上書き） | ADR-0041 決定3（ADR-0024 決定3） | runを`failed`にすることは有効。再試行・resume・人への相談はtriage jobのverdictで決まる | [triage](../design/supervisor-lifecycle/triage.md) |
| 箇条8 mainを進める前のエラーは`integration_error`で元に戻す | 有効 | — | | [integrate-errors](../design/supervisor-lifecycle/integrate-errors.md) |

### ADR-0009 goalとして表現し、workerのpromptに流す

goal 1で実装済みのため、最初の棚卸しで`status: accepted`にした（`accepted_on: 2026-09-25`）。上書きされた決定は下の表から後継ADRとdesignへ辿る。

| 決定 | 現在 | 上書き | 根拠 | 今の姿（design） |
| --- | --- | --- | --- | --- |
| 箇条1 `Goal`エンティティ、`Task.goal_id`はnullable | 有効 | — | | [domain-model](../design/domain-model.md) |
| 箇条2 goalは状態機械を持たない、closeは1回のイベント、verdictの拒否条件 | 上書き | ADR-0041 決定5（ADR-0024 決定5） | draft状態を持つ。close時の拒否条件はADR-0041 決定8が`submitted`を含めて引き継ぐ | [domain-model](../design/domain-model.md)・[goal-review](../design/supervisor-lifecycle/goal-review.md) |
| 箇条3 goalにverification_commandsを持たせない | 有効 | — | | [domain-model](../design/domain-model.md) |
| 箇条4 `goal edit`と`goal_updated` | 有効 | — | | [domain-model](../design/domain-model.md) |
| 箇条5 `set-goal`は`draft` / `ready`だけ | 上書き（未実装） | ADR-0041 決定9 | 「`ready`のtaskは編集しない」の規則がgoalの所属にも及ぶと読む（ADR-0029 決定2の`paths`と同じ扱い）。決定9が挙げる中身の一覧にgoalの所属は無いので、方針に未決の点が残るならplannerが人と確かめる（下の候補）。実装は今も[domain-model](../design/domain-model.md)のとおりdraft / ready | [domain-model](../design/domain-model.md) |
| 箇条6 依存はgoalをまたいでよい | 有効 | — | ADR-0038がgoalへの依存を足した（追加） | [domain-model](../design/domain-model.md) |
| 箇条7 promptにgoal・依存元・`in_progress`の兄弟を載せる | 有効 | — | ADR-0038 決定5が依存先goalの成果を足した（追加） | [prompt](../design/supervisor-lifecycle/prompt.md) |
| 箇条8 `Task.context` | 有効 | — | | [domain-model](../design/domain-model.md) |
| 箇条9 receiptの`follow_ups`は形だけ確認し、SVが`show`で見て登録を判断する | 上書き | ADR-0019 決定4、ADR-0041 決定16 | `integrate`がdraftに登録し、runtimeが立てるplannerが採否を決める | [integrate](../design/supervisor-lifecycle/integrate.md)・[draft-planners](../design/supervisor-lifecycle/draft-planners.md) |
| 箇条10 `candidates`はID順のまま | 上書き | ADR-0040 決定4（ADR-0023 決定4） | 効く優先度 → goalのrank → 解放数 → ID | [domain-model](../design/domain-model.md) |
| 箇条11 schema v6、表とイベント | 記録 | — | イベント名は有効。schemaの番号は記録 | [persistence](../design/persistence.md) |
| 箇条12 pluginの`taskq` skillが「課題を聞く → goal → task」を標準手順にする | 上書き | ADR-0015（skill行）、ADR-0041 決定1 | skillは`dagq`、手順の主体はplanner | [plugin-integration](../design/plugin-integration.md)・[plan-planners](../design/supervisor-lifecycle/plan-planners.md) |
| 箇条13 着手の順と最初のgoalの4 task | 記録 | — | 完了 | designに無い（一度きりの記録・制約、または文書運用の規則） |
| 箇条14 journalテンプレートの節名を019で変える | 失効 | ADR-0036 決定1 | journalは削除された | designに無い（一度きりの記録・制約、または文書運用の規則） |

### ADR-0010 役割名の統一とlaunchd常駐、up / down

| 決定 | 現在 | 上書き | 根拠 | 今の姿（design） |
| --- | --- | --- | --- | --- |
| 1 役割名 supervisor / maintainer / worker、SVとoperatorはmaintainer | 上書き | ADR-0041 決定1（ADR-0024 決定1） | 役割は5つでmaintainerは退役。`supervise`・`supervisors`表を変えないことは有効 | [roles](../design/supervisor-lifecycle/roles.md) |
| 2 cold startは`up`、supervisorはLaunchAgent、`up`がmaintainer workspaceを作る、`CMUX_TASKQ_ROLE` / `QUEUE` | 上書き | ADR-0011 決定1（socket password前提）、ADR-0041 決定6（`up`はsupervisorとinboxだけ）、ADR-0015（環境変数名）、ADR-0026 決定2（`--env`） | launchd常駐と`up`の1コマンドは有効 | [up-down](../design/supervisor-lifecycle/up-down.md) |
| 3 `down`（bootout・drain、`--wait`、`--force`）、maintainer workspaceは閉じない | 有効（読み替え） | — | `down`はinboxとplannerを閉じない（AGENTS.md） | [up-down](../design/supervisor-lifecycle/up-down.md) |
| 4 `up`はPIDの死んだ`supervisors`登録を消す（`up`だけの例外） | 有効 | — | ADR-0045 決定16も生きているが黙った登録はpruneしないとしており整合 | [up-down](../design/supervisor-lifecycle/up-down.md) |
| 5 workspace名 `taskq <repo> maintainer` / `taskq <repo> <task-id> <run-id>` | 上書き | ADR-0011 決定3、ADR-0015、ADR-0018 決定1、ADR-0021 決定1、ADR-0028 決定1 | 今は`[<repo>]<role>` | [naming](../design/supervisor-lifecycle/naming.md) |
| 6 supervisorのlogを起動ごとに`<queue dir>/logs/supervisor-<started_at>.log`へ | 失効 | ADRなし（task 194、proposedの[ADR-0033](../adr/0033-one-tracing-pipeline-with-local-json-lines-and-optional-otlp.md)の実装） | 今は`logs/<process>-<YYYYMMDDTHHMMSSZ>-<pid>.jsonl`（[supervisor-lifecycle](../design/supervisor-lifecycle.md)）。queue dirの`logs/`に置くことと`locate`のlog dirは有効。下の「ADRの外で変わった実装」 | [logs](../design/supervisor-lifecycle/logs.md) |
| 7 maintainerの初期promptはruntime生成、CLIの使い方はskill `taskq-maintain`、AGENTS.mdはrepository固有の注意だけ | 上書き | ADR-0041 決定1、ADR-0015（skill名）、ADR-0016 決定8、ADR-0022 | 初期promptは`inbox_prompt` / `planner_prompt`。「使い方はskill、AGENTS.mdは固有の注意」は有効 | [session-prompts](../design/supervisor-lifecycle/session-prompts.md)・[plugin-integration](../design/plugin-integration.md) |
| 末尾 taskへの分割（T2・T3） | 記録 | — | 完了 | designに無い（一度きりの記録・制約、または文書運用の規則） |

### ADR-0011 cmuxのsocket passwordとup --in-cmux

| 決定 | 現在 | 上書き | 根拠 | 今の姿（design） |
| --- | --- | --- | --- | --- |
| 1 launchd modeはsocket passwordを前提にし、`up`が`CMUX_*`を除いた`cmux ping`でpreflightする | 有効 | — | AGENTS.mdの起動と停止 | [up-down](../design/supervisor-lifecycle/up-down.md) |
| 2 `up --in-cmux`のfallback、`down`はSIGINTでdrainしworkspaceを閉じる、modeを登録に記録する。maintainer workspaceの作成はlaunchdと同じ | 有効（一部上書き） | ADR-0041 決定6、ADR-0015（`cmux-taskq supervise`） | maintainer workspaceは作らない。自動再起動が無いことは有効 | [up-down](../design/supervisor-lifecycle/up-down.md) |
| 3 supervisorのworkspace名 `taskq <repo> supervisor` | 上書き | ADR-0015、ADR-0021 決定1、ADR-0028 決定1 | `[<repo>]supervisor` | [naming](../design/supervisor-lifecycle/naming.md) |

### ADR-0012

置き換え済み（→ [ADR-0039](../adr/0039-adopt-stale-lease-of-live-wrapper-and-renew-own-stale-lease.md)、2026-09-25）。棚卸しの間に着地したtask 259が、段落2の自動`recover`（ADR-0041 決定3）を含む生きている決定を引き継いで丸ごと置き換えた。今の姿は[cleanup-and-recovery](../design/supervisor-lifecycle/cleanup-and-recovery.md)・[recover](../design/supervisor-lifecycle/recover.md)。

### ADR-0013 レイヤーと「型＋関数」

| 決定 | 現在 | 上書き | 根拠 | 今の姿（design） |
| --- | --- | --- | --- | --- |
| 1〜8 レイヤー、型＋関数、集約、VO、作成と復元、エラー、時刻・ID、トリレンマ | 有効 | — | overviewのレイヤー | [overview](../design/overview.md)・[domain-model](../design/domain-model.md) |
| 適用: モジュール境界 | 有効 | — | moduleは後から増えた（`domain::proposal`・`stall`など、追加） | [overview](../design/overview.md) |
| 適用: 状態遷移はdomainに置く | 有効 | — | | [domain-model](../design/domain-model.md) |
| 適用: 外部公開API（schema v8、CLIのJSON・エラー文・exit code、`plugins/`）を変えない | 失効 | ADR-0014 決定1（→ ADR-0045）、ADR-0019、ADR-0022 決定1、ADR-0026 決定1 ほか | リファクタリングのgoalの間の制約。以後のADRがmigrationを足し、CLIとpluginも変わった | designに無い（一度きりの記録・制約、または文書運用の規則） |
| 適用: 進め方（1 task 1観点） | 記録 | — | goalは完了 | designに無い（一度きりの記録・制約、または文書運用の規則） |
| 棚卸し | 記録 | — | 着手時点のスナップショット（ADR自身がそう書く） | designに無い（一度きりの記録・制約、または文書運用の規則） |

### ADR-0014

置き換え済み（→ ADR-0045 → ADR-0073）。今の姿は[install](../design/supervisor-lifecycle/install.md)・[handoff](../design/supervisor-lifecycle/handoff.md)・[up-down](../design/supervisor-lifecycle/up-down.md)。

### ADR-0015 cmux-taskqをdagqに改名する

| 決定 | 現在 | 上書き | 根拠 | 今の姿（design） |
| --- | --- | --- | --- | --- |
| 1 対応表（crate、repository、archive、plugin、marketplace、launcher、環境変数、データディレクトリ、launchd label、branch、ref、trailer） | 有効 | — | | [plugin-integration](../design/plugin-integration.md)・[persistence](../design/persistence.md)・[naming](../design/supervisor-lifecycle/naming.md)・[integrate](../design/supervisor-lifecycle/integrate.md)。改名の全項目の対応表はdesignに無い |
| 1 対応表のcmux workspace名の行 | 上書き | ADR-0018 決定1、ADR-0021 決定1、ADR-0028 決定1 | | [naming](../design/supervisor-lifecycle/naming.md) |
| 1 対応表のskillの行（`dagq-maintain`） | 上書き | ADR-0016 決定8、ADR-0022、ADR-0041 決定1 | 今のskillは`dagq` / `dagq-inbox` / `dagq-planner` / `dagq-recover` | [plugin-integration](../design/plugin-integration.md) |
| 1 対応表のversionの行（0.2.0） | 記録 | — | versionの付け方はADR-0045 決定1 | [build-identifier](../design/supervisor-lifecycle/build-identifier.md) |
| 2 文言の置き換え | 有効 | — | | [overview](../design/overview.md)・[plugin-integration](../design/plugin-integration.md) |
| 3 schemaとAPPLICATION_IDを変えない、versionを0.2.0に | 記録 | — | 一度きり | designに無い（一度きりの記録・制約、または文書運用の規則） |
| 4 互換shimを作らない | 有効 | — | | designに無い（一度きりの記録・制約、または文書運用の規則） |
| 5 `docs/journal/`と既存ADRは凍結、旧名はADR-0015だけ | 失効 | ADR-0036 決定1（journal削除）、ADR-0042（ADRの置き換え規則） | 当時の根拠はADR-0042 決定6。今のappend-onlyの規則はADR-t598-1 決定9 | designに無い（一度きりの記録・制約、または文書運用の規則） |
| Consequences 1 切り替え手順の4（DBを直接触る例外） | 失効 | ADR-0020 決定7 | 例外は無くなった | [rebind](../design/supervisor-lifecycle/rebind.md) |

### ADR-0016 status / watch / doctorの通知経路と圧縮出力、起き直しhook

| 決定 | 現在 | 上書き | 根拠 | 今の姿（design） |
| --- | --- | --- | --- | --- |
| 原則 maintainerは使い捨て、runtimeは起き直しに要る情報を上限のある大きさで返す | 有効（読み替え） | ADR-0041 決定1 | 対象はinboxとplanner | [notification-route](../design/supervisor-lifecycle/notification-route.md)・[session-prompts](../design/supervisor-lifecycle/session-prompts.md) |
| 1 情報源は`status` / `watch` / `doctor`、`watch --after <cursor>` | 有効 | — | `--role`はADR-0022 決定1が足した | [events-watch](../design/supervisor-lifecycle/events-watch.md)・[doctor](../design/supervisor-lifecycle/doctor.md) |
| 2 run_eventsのkind名は公開契約、attentionの判定はdomain、attentionの一覧 | 有効（一部上書き） | ADR-0040 決定2、ADR-0041 決定3・17 | `awaiting_integration`はreview job、`failed`はtriage jobに回り、attentionにならない。一覧はADR-0022・0025・0041が足した | [domain-model](../design/domain-model.md)・[status](../design/supervisor-lifecycle/status.md) |
| 3 runtimeはmaintainerのterminalに打ち込まない | 上書き | ADR-0041 決定1・12・13 | maintainerは無い。plannerにはrevise・answerを送る | [plan-planners](../design/supervisor-lifecycle/plan-planners.md)・[session-send](../design/supervisor-lifecycle/session-send.md) |
| 4 attentionのたびにmaintainer workspaceへ`cmux notify` | 上書き | ADR-0022 決定5 | `ask_opened`のときだけinbox宛て | [cmux-notify](../design/supervisor-lifecycle/cmux-notify.md) |
| 5 `integrate`は自動で呼ばれない、着地の承認はユーザーに残す | 上書き | ADR-0022 決定3、ADR-0040 決定2 | review jobのpassでsupervisorが着地させる | [review](../design/supervisor-lifecycle/review.md)・[integrate](../design/supervisor-lifecycle/integrate.md) |
| 6 既定は圧縮、`--full`でopt-in | 有効 | — | | [status](../design/supervisor-lifecycle/status.md)・[review-command](../design/supervisor-lifecycle/review-command.md) |
| 7 `review ID`が`review.md`を書く | 有効 | — | review jobの入力（ADR-0040 決定2） | [review-command](../design/supervisor-lifecycle/review-command.md) |
| 8 SessionStart hookは`DAGQ_ROLE=maintainer`のときだけ`status`、`dagq-maintain`の分割、`maintainer_prompt`の縮約 | 上書き | ADR-0041 決定1・6 | hookは`status --role <role>`（inbox / planner）。skillはinbox / planner / recoverに分かれた | [plugin-integration](../design/plugin-integration.md)・[session-prompts](../design/supervisor-lifecycle/session-prompts.md) |

### ADR-0017 runのpathをqueueディレクトリから解決する

| 決定 | 現在 | 上書き | 根拠 | 今の姿（design） |
| --- | --- | --- | --- | --- |
| 1 queue配下のpathはrun IDとqueueディレクトリから解決する | 有効 | — | | [persistence](../design/persistence.md) |
| 2 列と書き込みは残し、schemaは変えない | 有効 | — | | [persistence](../design/persistence.md) |
| 3 `integrate`が`git worktree repair`する | 有効 | — | | [integrate](../design/supervisor-lifecycle/integrate.md) |
| 4 移動の手順 | 有効 | — | | [rebind](../design/supervisor-lifecycle/rebind.md) |

ADR-0053へ置き換え済み（下の組Bの記録）。

### ADR-0018 runのworkspace名はtaskのtitle

| 決定 | 現在 | 上書き | 根拠 | 今の姿（design） |
| --- | --- | --- | --- | --- |
| 1 `[<repo>]dagq#<task-id> <task title>` | 上書き | ADR-0028 決定1 | `[<repo>]worker#<task-id> - <task title>` | [naming](../design/supervisor-lifecycle/naming.md) |
| 2 run IDを`--description "run <run-id>"`に置く | 上書き | ADR-0026 決定3 | `dagq role=… queue=… run=… task=…` | [naming](../design/supervisor-lifecycle/naming.md) |
| 3 maintainer / supervisor / resumeの名前は変えない | 上書き | ADR-0021 決定1・2、ADR-0028 決定1・2 | | [naming](../design/supervisor-lifecycle/naming.md) |
| 4 `WorkspaceBackend::create`がtaskを受け取り、名前はadapterの純粋関数 | 有効 | — | | [naming](../design/supervisor-lifecycle/naming.md)・[overview](../design/overview.md) |

### ADR-0019 maintainerの定型作業をruntimeに移す

| 決定 | 現在 | 上書き | 根拠 | 今の姿（design） |
| --- | --- | --- | --- | --- |
| 原則 判断を含まない手順はruntime、ADR-0016の決定3・5を維持する | 上書き | ADR-0040 決定2 | ADR-0016 決定5（自動では着地しない）は維持されていない。「判断を含まない手順はruntime」は有効 | [review](../design/supervisor-lifecycle/review.md)・[integrate](../design/supervisor-lifecycle/integrate.md) |
| 1 `needs_session`のsupervisorによるresume、`integration_approved`、3回まで | 有効（一部上書き） | ADR-0040 決定2、ADR-0027 決定3、ADR-0041 決定1・3 | 「未承認のrunは`awaiting_integration`に戻しwatchで承認を待つ」はreview jobに変わった。3回で解消しないrunは`resume session`のattentionではなく`failed`にして`decide`のask（task 100、overview） | [needs-session](../design/supervisor-lifecycle/needs-session.md) |
| 2 `exit_request_timed_out`でleaseを手放さない | 有効（一部上書き） | ADRなし（task 104） | 「attention（`send /exit`）」は`stuck_exit`のaskになった（[supervisor-lifecycle](../design/supervisor-lifecycle.md)）。下の「ADRの外で変わった実装」 | [receipt-and-session-exit](../design/supervisor-lifecycle/receipt-and-session-exit.md)・[background-recovery-job](../design/supervisor-lifecycle/background-recovery-job.md) |
| 3 `integrate`が着地後にpushする、`push_failed`はattention | 有効 | — | 再試行は人（inboxが知らせる） | [integrate](../design/supervisor-lifecycle/integrate.md) |
| 4 `integrate`が`follow_ups`をdraftに登録する、`ready`にするかcancelするかは人の判断 | 有効（一部上書き） | ADR-0041 決定16・8 | 登録の形はADR-0041 決定16がそのまま引き継ぐ。採否はruntimeが立てるplannerが決め、`ready`にするのはplan reviewだけ | [integrate](../design/supervisor-lifecycle/integrate.md)・[draft-planners](../design/supervisor-lifecycle/draft-planners.md) |
| 5 taskの要求evidence、`evidence_missing`で`needs_session` | 有効 | — | | [validation](../design/supervisor-lifecycle/validation.md) |
| 6 prompt待ちを検知し`prompt_waiting`を記録、応答は人かmaintainer | 有効（一部上書き） | ADR-0041 決定17 | `answer_prompt`のaskでinboxが人に渡す（task 100） | [prompt-waiting](../design/supervisor-lifecycle/prompt-waiting.md) |

### ADR-0020 rebind

| 決定 | 現在 | 上書き | 根拠 | 今の姿（design） |
| --- | --- | --- | --- | --- |
| 1〜7 `rebind`、出力、記録、worktreeのrepair、走行中の拒否、手順、DBの直接操作の例外が無くなる | 有効 | — | | [rebind](../design/supervisor-lifecycle/rebind.md) |

ADR-0053へ置き換え済み（下の組Bの記録）。

### ADR-0021 maintainer / supervisor / resumeのworkspace名

| 決定 | 現在 | 上書き | 根拠 | 今の姿（design） |
| --- | --- | --- | --- | --- |
| 1 `[<repo>]dagq maintainer` / `[<repo>]dagq supervisor` | 上書き | ADR-0028 決定1、ADR-0041 決定1 | | [naming](../design/supervisor-lifecycle/naming.md) |
| 2 resume workspaceの名前 `[<repo>]dagq resume <run ID>` | 上書き | ADR-0028 決定2 | | [naming](../design/supervisor-lifecycle/naming.md) |
| 3 旧名を探す互換の検索は持たない | 有効 | — | ADR-0028 決定4も同じ（識別はUUIDなので互換が要らない） | [naming](../design/supervisor-lifecycle/naming.md) |
| 前提 `up`はtitleの完全一致で探す | 上書き | ADR-0026 決定1 | | [naming](../design/supervisor-lifecycle/naming.md) |
| 切替手順 | 失効 | — | 一度きり | designに無い（一度きりの記録・制約、または文書運用の規則） |

生きている決定は3だけで、ADR-0028 決定4が同じことを言う。下の「deprecated候補」の境界例。

### ADR-0022 ask / answer、inboxとplanner、疑義のあるときの着地、notify

| 決定 | 現在 | 上書き | 根拠 | 今の姿（design） |
| --- | --- | --- | --- | --- |
| 原則 runtimeはどのsessionにも打ち込まない、例外はworkerへのanswer | 上書き | ADR-0041 決定12・13 | plannerにもrevise・answerを送る。worker宛てにはresume・revise・rebaseの依頼（ADR-0019、ADR-0027）も送る | [session-send](../design/supervisor-lifecycle/session-send.md)・[plan-planners](../design/supervisor-lifecycle/plan-planners.md) |
| 1 `asks`表、`ask` / `answer` / `asks`、一意性、`ask_opened` / `ask_answered` | 有効 | — | | [ask](../design/supervisor-lifecycle/ask.md) |
| 1 `kind`は4つ | 上書き | ADR-0041 決定4・11・13、ADR-0043 決定3 | `blocked`・`approve_plan`・`planner_question`・`stalled`が足された | [ask](../design/supervisor-lifecycle/ask.md) |
| 1 `watch --role maintainer`、`ask_answered`はmaintainer向け | 上書き | ADR-0041 決定6 | `--role maintainer`は無い。`ask_answered`はinbox | [events-watch](../design/supervisor-lifecycle/events-watch.md)・[status](../design/supervisor-lifecycle/status.md) |
| 2 workerは`worker_question`で聞いて止まり、answerはruntimeが送る | 有効 | — | | [worker-question-answer](../design/supervisor-lifecycle/worker-question-answer.md) |
| 3 着地は疑義のあるときだけ人に聞く。maintainerが`integrate`を呼ぶ | 上書き | ADR-0040 決定2 | 疑義の観点は有効。判断はreview job、concernの`approve_landing`のanswerはsupervisorが適用する | [review](../design/supervisor-lifecycle/review.md) |
| 4 `up`がinboxとplannerを開く、各役割の定義 | 上書き | ADR-0041 決定1・6 | `up`はinboxだけ（とsupervisor）。inboxの定義は有効 | [roles](../design/supervisor-lifecycle/roles.md)・[up-down](../design/supervisor-lifecycle/up-down.md)・[plan-planners](../design/supervisor-lifecycle/plan-planners.md) |
| 5 `cmux notify`は`ask_opened`のときだけinbox宛て | 有効 | — | ADR-0043 決定3が参照する | [cmux-notify](../design/supervisor-lifecycle/cmux-notify.md) |

### ADR-0023・ADR-0024

置き換え済み（0023 → ADR-0040 → ADR-0049、0024 → ADR-0041 → ADR-0044 → ADR-0047）。0023の今の姿は[validation](../design/supervisor-lifecycle/validation.md)・[review](../design/supervisor-lifecycle/review.md)・[run-environment](../design/supervisor-lifecycle/run-environment.md)・[dependency-diagram](../design/supervisor-lifecycle/dependency-diagram.md)・[stats](../design/supervisor-lifecycle/stats.md)、0024は[roles](../design/supervisor-lifecycle/roles.md)・[triage](../design/supervisor-lifecycle/triage.md)・[observer](../design/supervisor-lifecycle/observer.md)・[plan-planners](../design/supervisor-lifecycle/plan-planners.md)。

### ADR-0025 leaseの無い未完了runを`recover run`のattentionにする

| 決定 | 現在 | 上書き | 根拠 | 今の姿（design） |
| --- | --- | --- | --- | --- |
| 1 leaseの無い未完了runを`recover run`のattentionにする | 有効 | — | | [status](../design/supervisor-lifecycle/status.md)・[recover](../design/supervisor-lifecycle/recover.md) |
| 2 attentionは`lease_released: true`の`runtime_error`だけ | 有効 | — | | [status](../design/supervisor-lifecycle/status.md)・[abandon](../design/supervisor-lifecycle/abandon.md) |
| 3 通知は足さない、maintainerは`watch`で受ける | 有効（一部上書き） | ADR-0041 決定17 | attentionはinboxが受ける | [events-watch](../design/supervisor-lifecycle/events-watch.md) |
| 4 maintainerが`doctor`を見て`recover`する | 上書き | ADR-0041 決定17 | inboxが人に知らせ、人の指示で`dagq-recover`の手順を行う | [doctor](../design/supervisor-lifecycle/doctor.md)・[recover](../design/supervisor-lifecycle/recover.md) |

### ADR-0026 workspaceをUUID・`--env`・workspace groupで扱う

| 決定 | 現在 | 上書き | 根拠 | 今の姿（design） |
| --- | --- | --- | --- | --- |
| 1 識別の正はqueue DB、`session_workspaces`の行（`maintainer` / `supervisor`、後で`planner` / `inbox`） | 有効（一部上書き） | ADR-0041 決定1・6 | `maintainer`と`planner`の行は`up`が忘れる。plannerはオンデマンドで開く | [naming](../design/supervisor-lifecycle/naming.md) |
| 2 roleとqueueを`--env`で渡す、`SessionRole`は`maintainer`を含む5値 | 有効（一部上書き） | ADR-0041 決定1・2 | `maintainer`は無く、jobの`observer` / `reviewer`がある | [roles](../design/supervisor-lifecycle/roles.md)・[naming](../design/supervisor-lifecycle/naming.md) |
| 3 descriptionは機械可読の1行 | 有効 | — | | [naming](../design/supervisor-lifecycle/naming.md) |
| 4 queueごとのworkspace group | 有効 | — | | [naming](../design/supervisor-lifecycle/naming.md) |
| 5 queue hash | 有効 | — | | [naming](../design/supervisor-lifecycle/naming.md) |
| 6 titleの文字列は変えない | 失効 | ADR-0028 決定1 | | [naming](../design/supervisor-lifecycle/naming.md) |
| 切替手順 | 失効 | — | 一度きり | designに無い（一度きりの記録・制約、または文書運用の規則） |

### ADR-0027 workerのsessionをreviewの後まで残し、revise、merge-treeの事前判定

| 決定 | 現在 | 上書き | 根拠 | 今の姿（design） |
| --- | --- | --- | --- | --- |
| 1 sessionを閉じる時点をreviewの後へ | 有効 | — | | [review](../design/supervisor-lifecycle/review.md) |
| 2 verdictを`pass` / `revise` / `concern`、reviseは2回まで | 有効 | — | | [review](../design/supervisor-lifecycle/review.md) |
| 3 自動resumeのsessionも同じ扱い、`integration_approved`のrunはreviewを待たない | 有効（読み替え） | — | 「maintainerか人が`integrate`を呼び済み」は人が呼んだ場合として読む | [review](../design/supervisor-lifecycle/review.md)・[needs-session](../design/supervisor-lifecycle/needs-session.md) |
| 4 passのとき`git merge-tree`で事前判定する | 有効 | — | | [review](../design/supervisor-lifecycle/review.md) |

棚卸し時点では全決定が有効。今の着地の経路はdesignのreview・integrateを読む。

### ADR-0028 titleを`[<repo>]<role>`にする

| 決定 | 現在 | 上書き | 根拠 | 今の姿（design） |
| --- | --- | --- | --- | --- |
| 1 supervisor / worker / inboxのtitle | 有効 | — | | [naming](../design/supervisor-lifecycle/naming.md) |
| 1 maintainerのtitle | 上書き | ADR-0041 決定1 | 役割が無い | [roles](../design/supervisor-lifecycle/roles.md) |
| 1 plannerのtitle `[<repo>]planner` | 上書き | ADR-0041 決定6 | `[<repo>]planner#<planner-id>`（overview） | [naming](../design/supervisor-lifecycle/naming.md) |
| 2 resumeのworkspaceもworkerと同じtitle、description `run <run-id> resume` | 有効（読み替え） | — | 開くのはruntime（自動resume）。「当面はmaintainerが開き」は失効 | [naming](../design/supervisor-lifecycle/naming.md)・[needs-session](../design/supervisor-lifecycle/needs-session.md) |
| 3 `DAGQ_ROLE`の値、`MAINTAINER_ROLE`などの定数 | 上書き | ADR-0041 決定1・2 | `maintainer`は無い | [roles](../design/supervisor-lifecycle/roles.md) |
| 4 旧名との互換は持たない | 有効 | — | | [naming](../design/supervisor-lifecycle/naming.md) |

### ADR-0029 taskが変更してよいパスを宣言する

| 決定 | 現在 | 上書き | 根拠 | 今の姿（design） |
| --- | --- | --- | --- | --- |
| 1 `add --paths`とglobの規則 | 有効 | — | | [domain-model](../design/domain-model.md) |
| 2 draft / readyのtaskの`--paths`を`set-paths`で置き換える | 上書き（未実装） | ADR-0041 決定9 | 中身（`paths`を含む）を編集できるのは`draft`と`submitted`だけ。readyのtaskは`submitted`に戻して直す（決定14）。AGENTS.mdとdagq skillは今も「draft / readyのうちは`set-paths`」と書く | [domain-model](../design/domain-model.md) |
| 3 validatingのscope検査と`scope_violation` | 有効 | — | ADR-0040 決定1が参照する | [validation](../design/supervisor-lifecycle/validation.md) |
| 4 integrateのscope検査 | 有効 | — | | [integrate](../design/supervisor-lifecycle/integrate.md) |
| 5 scope違反のresume、必要なら`failed`で書かせ人が`--paths`を広げて再登録 | 有効（読み替え） | — | 再登録はplannerがproposalで行う（ADR-0041 決定1・8） | [validation](../design/supervisor-lifecycle/validation.md) |
| 6 変更の種類ごとの`--verify`と`--paths`の組み合わせ | 有効 | — | AGENTS.mdのテストの制約 | [validation](../design/supervisor-lifecycle/validation.md)（scopeの関門）。変更の種類ごとの検証コマンドの組み合わせはdesignに無い（AGENTS.md） |

### ADR-0030 crates.ioへのTrusted Publishing

| 決定 | 現在 | 上書き | 根拠 | 今の姿（design） |
| --- | --- | --- | --- | --- |
| 1〜5 追加の経路、tag pushでpublish、Trusted Publishing、第三者actionの例外、導入手順 | 有効 | — | `release` skill | [plugin-integration](../design/plugin-integration.md)（配布経路）。Trusted Publishing・第三者actionの例外・導入手順はdesignに無い（`release` skill） |

棚卸し時点では全決定が有効。

### ADR-0031 inbox / plannerの色・pill・ピン、閉じる前のunpin

| 決定 | 現在 | 上書き | 根拠 | 今の姿（design） |
| --- | --- | --- | --- | --- |
| 1〜4 `up`がinboxに色・pill・ピンを当て、毎回当て直す | 有効 | — | | [up-down](../design/supervisor-lifecycle/up-down.md) |
| 1〜4 `up`がplannerに色・pill・ピンを当てる | 上書き | ADR-0041 決定6 | `up`はplannerを開かない。plannerの色とpillは`plan`が当て、ピンは付けない（[supervisor-lifecycle](../design/supervisor-lifecycle.md)、task 277） | [plan-planners](../design/supervisor-lifecycle/plan-planners.md) |
| 5 失敗はwarning | 有効 | — | | [up-down](../design/supervisor-lifecycle/up-down.md)・[plan-planners](../design/supervisor-lifecycle/plan-planners.md) |
| 6 閉じる前にunpin | 有効 | — | | [naming](../design/supervisor-lifecycle/naming.md) |

## 統合ADRの組（過去の記録）

組A〜Cは2026-09-26に置き換え済み。後継だけを記録する。

| 旧組 | 後継ADR |
| --- | --- |
| A | [ADR-0052](../adr/0052-rust-single-binary-and-plugin-with-cmux-first.md) |
| B | [ADR-0053](../adr/0053-queue-in-data-dir-run-paths-from-queue-and-rebind.md) |
| C | [ADR-0054](../adr/0054-run-lease-ownership-parallel-supervisors-and-recover.md) |

組D〜Jは、ADR-t598-1決定4に従い統合ADRを作らないことにした（2026-09-26、ask 117のadopt）。goal 23のtask 367〜373は取り消された。上書きされた決定を含む旧ADRは棚卸しのために丸ごと置き換えず、読む人は上の表から後継のADRとdesignの文書へ辿る。

## deprecated候補

棚卸しでは、後継が無く丸ごと無効なADRは0001〜0034には無かった。生きている決定や上書きした後継があることは、統合や廃止の理由にはしない。

境界例は**ADR-0021**。決定1・2と前提はADR-0028・0026に上書きされ、生きている決定3（互換の検索を持たない）はADR-0028決定4と同じ内容。後継のある決定を含むため、自動的に`deprecated`にしない。現状は表と[Naming](../design/supervisor-lifecycle/naming.md)を入口に残す。廃止の要否はplannerが人と決める候補であり、この文書ではstatusを変更しない。人の判断を要する互換方針そのものを変える場合だけ、小さな`ADR-t<ID>-<N>`で対象決定をamendsする候補にする。

## ADRの外で変わった実装

最初の棚卸しで見つけた差分を、ADR-t598-1決定3〜5に従って扱う。ファイル名・書式・閾値・CLIなどの細部はdesignを今の姿に直す候補であり、それだけでADRを足さない。人の判断が要る方針・境界・不変条件の変更が未記録なら、小さな`ADR-t<ID>-<N>`（amends）のtaskの候補にする。以下は候補の記録だけで、新しいtaskは登録しない。

| 棚卸しで見つけた差分 | 今の参照先と扱い |
| --- | --- |
| supervisorなどのlog（ADR-0010決定6、task 194） | [Logs](../design/supervisor-lifecycle/logs.md)はJSON Linesのファイル名・書式を記載済み。細部の差分が残ればこのdesignを直す候補にする。ADR-0033がproposedであることだけを理由にacceptや新ADRを求めない。ログの収集・外部送信など人の判断が要る方針を変えるなら、その範囲だけをplannerが人と決め、小さなamendsの候補にする。 |
| exit要求のtimeout（旧ADR-0019決定2、task 104） | 後継ADR-0047決定25・40、[Receipt and session exit](../design/supervisor-lifecycle/receipt-and-session-exit.md)・[Background recovery job](../design/supervisor-lifecycle/background-recovery-job.md)を読む。復旧jobの後にaskへ渡す方針は記録済み。生きているrunの復旧job自体が失敗した場合も[ADR-t609-1](../adr/2026-09-27-t609-1-failed-live-recovery-job-opens-the-alert-ask.md)が決定40をamendsしている。残る実装・記述の差分はdesignに実装状況を書く候補で、新しい決定の候補ではない。 |
| resumeの上限とprompt待ち（旧ADR-0019決定1・6、task 100） | 後継ADR-0047決定24・29・40、[Needs session](../design/supervisor-lifecycle/needs-session.md)・[Prompt waiting](../design/supervisor-lifecycle/prompt-waiting.md)・[Triage](../design/supervisor-lifecycle/triage.md)を読む。復旧job・人へのaskの境界は後継ADRが持つ。回数・askの形などの細部の差分はdesignを直す候補とし、旧記述を統合ADRへ書き直さない。 |

別の判断候補は、ADR-0009箇条5のgoal所属の編集範囲。「readyのtaskは編集しない」がgoal所属に及ぶかという当時の疑問は、[Domain model](../design/domain-model.md)と後継ADR-0047決定9で現在の扱いを確かめる。既存の決定で解けるならdesignの記述を合わせるだけにし、未決の境界を変える必要が残る場合だけ、人と決めて対象決定を小さなADRでamendsする候補にする。

## 進め方

- 今の姿の確認は表のdesignを入口にし、理由が要るときに後継ADRと`amended_by`を辿る。designに無い項目は、現在も必要な設計の記述か、一度きりの記録かを分け、前者だけをdesignへ補う候補にする。
- 実装の細部の差分はdesignを直す。人の判断が要る変更だけをplannerが人と決め、小さなADRの候補にする。大きなADRは凍結し、一部の変更は対象決定をamendsし、同時にdesignを直す。amendsか丸ごとの置き換えかはADR-t1091-1に従い決定の数と変える範囲で決める。
- 候補をtaskにするか、棚卸しの範囲をさらに組み直すかはplannerが人と決める（ADR-t598-1決定11）。この文書はtaskの登録・採否・進捗を管理しない。
- この見直しの完了条件は、判定を保った対応表から後継ADRとdesign（またはdesignに無いこと）を引け、未着手の統合ADR計画が残らず、索引とfrontmatterが一致すること。旧ADRをすべて置き換えることや、上の候補をすべて実施することは条件にしない。

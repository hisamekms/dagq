---
id: design-supervisor-lifecycle-worker-model
type: design
title: "Worker model"
status: current
created: 2026-09-27
updated: 2026-09-28
last_verified: 2026-09-28
scope: runtime
related:
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-run-environment
  - design-supervisor-lifecycle-stats
  - design-supervisor-lifecycle-kpi
  - design-supervisor-lifecycle-plan-review
  - adr-0079
  - design-supervisor-lifecycle-actor-model
---

# Worker model

workerのsessionのmodelとeffortの選び方と記録（[ADR-0079](../../adr/0079-record-task-weight-predictions-and-trial-model-effort-selection.md)の決定3・4、task 576）。予測の記録は[plan review](plan-review.md)の6、読み方は[stats](stats.md#workerのmodelと試しの群)と[kpi](kpi.md#kpiと層)の`--by group`。段上げ（決定5）はまだ無い。worker以外のアクター（決定7の(b)(c)）は[Actor model](actor-model.md)。

## 既定: Opus 5.5・effort mediumを明示して渡す

- runtimeはworkerの起動・resumeのたびに、modelとeffortをproviderに明示して渡す（`AgentProvider::select_model`。Claude Codeでは`--model <model> --effort <effort>`をoptionの末尾、`--`の前に足す）。Claude Codeの既定やユーザー設定に依らない。
- 既定は`claude-opus-5-5`・`medium`（`domain::worker_model::WorkerSession::default`）で、試しが無効なら全runがこれで起動する（今までの挙動と同じ値）。
- 決めるのはclaimで、runのsession wrapper（`application::session`）は起動の直前にそのrunの最初の`run_claimed`の`model` / `effort`を読んで渡す（`WorkerSession::of_run`）。resumeも同じrunの`run_claimed`を読むので、claimと同じ値で開き直す。記録の無いrun（task 576より前にclaimされたもの）は既定で起動する。
- reviseは生きているsessionへの差し戻しなので起動の引数は無く、sessionはclaimの値のまま続く。

## 記録

sessionを開くeventのpayloadに`model`・`effort`・`group`（`control` / `treatment`、試しの外ならnull）を持たせる（`WorkerSession::fields`）。

| event | いつ |
|---|---|
| `run_claimed` | claim（retryで新しいrunをclaimしたときも）。試しの対象の判定で百分位を出したtaskは`trial_percentile`も持つ |
| `resume_started` | `needs_session`のresume。値はそのrunの`run_claimed`のもの |
| `revise_requested` | reviewの`revise`の差し戻し。値はそのrunの`run_claimed`のもの |

## 限定の試し（`[worker.trial]`）

`dagq.toml`の`[worker.trial]`で有効にしたときだけ働く。既定は無効で、有効にするのは人の判断（書式は[Run environment](run-environment.md)）。この repositoryの`dagq.toml`には置いていない。

```toml
[worker.trial]
enabled = true   # 既定 false
window = 60      # 既定 60（domain::prediction::PREDICTION_WINDOW）
```

- **読む時点**: supervisorがclaimのpassごとに読む（`Verifier::worker_trial`）ので、有効・無効の切り替えは再起動なしに次のclaimから効く。読めないときはwarnをlogに出して試しの外でclaimする（壊れた`dagq.toml`はprovisioningがerrorにする）。
- **選ぶ場所**: claimのtransactionの中（`infrastructure::sqlite::claim_task`）で、queueの`task_weight_predicted`と`run_claimed`を読み、純粋関数`domain::worker_model::choose`が決める。交互の順番をtransactionの中で決めるので、並行するclaimが同じ番を取らない。
- **群の固定**: 前のrunの`run_claimed`で群を持ったtaskは、同じ群のmodel / effortで起動する（resume・revise・retryで変えない。予測が後で変わっても変えない）。試しを無効に戻すと、群を持ったtaskも既定で起動する。
- **対象**: 群を持たないtaskで、次の全部を満たすもの。
  - そのtaskの最後の予測の`nature`が`mechanical`
  - そのtask自身を除く、他のtaskの最後の予測を新しい順に`window`件並べた中で、`expected_output_tokens`の百分位（`domain::prediction::percentile`。同じ値は半分に数え、小数1桁）が33.3以下（`LOWER_THIRD`）
  - 他のtaskの予測が`window`件ある（足りない間は誰も対象にしない）
- **割り当て**: 対象になったtaskは、最後に群を持ったtask（各taskの最初の群付きの`run_claimed`の順で最後）と反対の群になり、最初の1件は`control`。`control`は`claude-opus-5-5`・`medium`、`treatment`は`claude-sonnet-5`・`medium`（`WorkerSession::of_group`）。
- **対象外**: 予測の無いtask（`ready --bypass-review`・予測の失敗）、`mechanical`でないtask、下位3分の1より上のtaskは既定のまま（群はnull）。`rework_probability`は選択に使わない。

## 判定

群ごとの比較は`stats`の`trial_groups`（[stats](stats.md#workerのmodelと試しの群)）と、`kpi --by group`（`model` / `effort` / `nature`も。runの最初の`run_claimed`の値で層別する。[kpi](kpi.md#kpiと層)）のrunの層で読む。判定（1群45件前後、taskに由来する手戻りの率の差（treatment − control）が+5ポイント以内か、途中で止める目安）は人とplannerが行い、runtimeは自動で止めたり全面適用したりしない（ADR-0079の決定6）。止めるのは`enabled = false`に戻すこと。

## テスト

- `domain::worker_model`の単体テスト（既定、`of_run`、交互の割り当て、群の固定、対象外）
- `infrastructure::run_env`の`parses_the_worker_trial_table`
- `tests/it/worker_model.rs`: 既定の起動と記録、`[worker.trial]`を有効にした`supervise`での交互の割り当てとSonnetでの起動、claimのtransactionでの選択、`stats`の`trial_groups`
- `tests/it/runtime_session.rs`の`claude_stop_hook_settings_publish_the_idle_marker`（`--model` / `--effort`の位置）、`runtime_triage.rs`と`runtime_review.rs`（`resume_started`と`revise_requested`の記録、resumeの起動の値）

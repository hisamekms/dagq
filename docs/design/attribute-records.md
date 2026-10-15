---
id: design-attribute-records
type: design
title: taskとgoalとfindingの属性の記録（stats・kpi・reportが読む属性をEventStoreから畳む）
status: current
created: 2026-10-11
scope: runtime
related:
  - design-measurement
  - design-architecture
  - adr-t1662-1
  - adr-t1662-2
---

# taskとgoalとfindingの属性の記録

`stats`・`kpi`・`report`が状態の表から読むtaskとgoalの属性を、計測の層がEventStoreだけから作れるよう、状態の変更と同じtransactionで新しい値をeventに書く（[計測](measurement.md)の「SSOTとビュー」、ADR-t1662-2）。
畳むのは`domain::ledger::attributes::fold`で、eventと窓の終わりだけを読み、StateStoreを読まない。
`application::GraphInput::folded`が畳んだ値から`graph_input`と同じ形（tasksとcandidates）を作る。
runtimeの制御はこの畳みを使わず、読み手を移すのは段3（[計測](measurement.md)の「段と後続のtask」）。

## 項目ごとの元

後のeventが前の値に勝つ。
表の「作成」はtaskなら`task_created`、goalなら`goal_created`。
「基準」は記録の無い値を補う`attributes_baseline`（下の「基準と照合」）で、taskの基準はそのtaskの、goalの基準はそのgoalの今の値の全体を持ち、それより前の値を置き換える。

| 項目 | 変更の元のevent | 基準 |
| --- | --- | --- |
| taskの題・change・実行の種類 | 作成、`task_edited`の新しい値 | taskの基準 |
| taskの状態 | 作成、`task_status_changed`、claimの`run_claimed`（`in_progress`になる） | taskの基準 |
| taskのgoal | 作成、`task_goal_changed`（set-goal・follow-upの判定とcorrectionの答えの移動は同じ書き込みを通る） | taskの基準 |
| taskのownの優先度と設定した者 | 作成、`task_priority_changed` | taskの基準 |
| taskの基の優先度・出所・設定した者 | 記録しない（畳む関数がownとgoalの値から状態の読み手と同じ規則、`domain::base_priority`・`PriorityBy::effective`で求める） | 記録しない（taskとgoalの基準の値から同じ規則で求める） |
| draftの出どころ | `draft_origin_recorded`（`draft_origins`に書く全ての経路が`draft_planners::insert_draft_origin`を通る） | taskの基準 |
| task間の依存 | `dependency_added`・`dependency_removed` | taskの基準 |
| goalへの依存 | `goal_dependency_added`・`goal_dependency_removed`（依存先のverdictはgoalの行） | taskの基準 |
| goalの題・優先度・設定した者・tags | 作成、`goal_updated` | goalの基準 |
| goalの状態 | 作成、`goal_status_changed`、`goal_reopened`（`open`になる） | goalの基準 |
| goalのverdictと閉じたこと | `goal_closed`、`goal_reopened`（消す） | goalの基準 |
| 未完了のrun | `run_claimed`、`run_unfinished_changed`（runの状態の保存は全て`runtime_store::save_run`を通り、記録はdomainの`run::unfinished_change`が決める） | taskの基準 |
| candidates | 記録しない（畳む関数`Attributes::candidates`がtaskの状態・goalの状態・前のtaskの状態・依存先のgoalのverdict・未完了のrunから、storeのready queryと同じ規則で求める） | 記録しない（基準を含む畳んだ値から同じ規則で求める） |

未完了の状態は`RunStatus::is_unfinished`で、storeのready queryと同じ集合。

## findingの属性

`report`が`findings`の表から読むfindingの属性（`findings_open`と一覧の並びと欄）も同じ形で記録する。
不変条件: findingの行を変える経路はどれも、同じtransactionで書く変更のevent（`finding_recorded`・`finding_updated`・`finding_status_changed`）に変更の後の属性の全体を載せ、何も変えない記録は何も書かない。
入口は`infrastructure::findings::finding_event`（経路の一覧と載せる値はそのdoc commentと`FindingAttributes`）。
畳むのは`domain::ledger::findings::fold`で、未決着の一覧は`findings`の既定の一覧と同じ順になり、`Report::new`はどちらからも同じ件数と行を作る。

| 項目 | 変更の元のevent | 基準 |
| --- | --- | --- |
| findingの属性の全体（`FindingAttributes`） | 変更の3種のeventの後のものが勝つ | `finding_baseline` |

## 基準と照合

- 記録より前のbinaryが作ったtaskとgoalと、記録より古いbinary（互換のmigrationの後も動く、claimのときに写したrunnerなど）が記録を書かずに変えたものは、eventが状態の値を持たない。
  これを`SqliteQueue::record_attributes_baseline`が照合で直す: `domain::ledger::attributes::KINDS`のeventを畳んだ値と状態の表の値（読み手と同じ読み方）を比べ、食い違うtaskとgoalだけに状態の値を`attributes_baseline`として書く。
- 比べるのは書き込みのlockの外で読んでから、食い違いがあればwriteのtransactionの中でもう一度比べて書く。
  書いた後は畳みと状態が一致するので冪等で、今のbinaryだけが書いたqueueには書かない。
- 照合するのはsupervisorの観測と分析の周回で、プロセスの最初の周回と、その後`RECONCILE_INTERVAL_SECS`ごと（`supervise::live_alerts::AttributesBaseline`）。
  書けなければwarnにして次の間隔でやり直す。
- 1回の照合はtaskとgoalの基準を書いた後に、組を閉じるqueueの印（対象の無い`attributes_baseline`）を1つ書く。
  畳む関数は組の印が窓にあるときだけ組の基準を取り、組の一部だけを窓に入れない。
  最初の印より前に終わる窓は`Folded::BeforeBaseline`として区別し、後の組の印より前に終わる窓はその組の前の値を返す。
- findingは`SqliteQueue::record_findings_baseline`が同じ周回で同じように照合し、決着したものを含む全てのfindingのうち食い違うものに`finding_baseline`を書いて、組を閉じるqueueの印を1つ書く。
  窓の契約（組を丸ごと取るか取らないか、最初の印より前は`BeforeBaseline`）はtaskとgoalの基準と同じ。
- migrationにしない: 互換のmigrationは`run_events`への挿入とそれを書くtriggerを許さず（`schema::compatibility_violations`）、非互換にすると使用中のsupervisorとrunがある間`migrate`が止まるため。

## 落とし穴

- 古いbinaryの書き込みから次の照合までの間は、畳みと状態が食い違う（古いrunnerが終えたrunは、その間は未完了のまま畳まれる）。
- 古いbinaryの`task_priority_changed`はownの値を持たないので、照合までの間、畳む関数は新しい基の優先度と出所からownを読み、設定した者は不明（人のもの）にする。
- 開き直せるのは`achieved`で閉じたgoalだけ（ADR-t1504-2決定9）で、`goal_reopened`はverdictを消して`open`にする。

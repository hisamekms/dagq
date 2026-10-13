---
id: design-supervisor-lifecycle-commands
type: design
title: 計測のコマンド（stats・kpi・mark・marks・report・timeline）をまたぐ約束
status: current
created: 2026-10-13
scope: runtime
related:
  - design-supervisor-lifecycle
  - design-measurement
  - design-authorization
  - adr-0040
  - adr-0051
---

# 計測のコマンド

記録から値を導いて返すコマンド（`stats`・`kpi`・`mark` / `marks`・`report`・`timeline`）をまたぐ約束と、各コマンドのコードの入口の地図。
各コマンドの引数と出力の欄は`--help`と定義のそばのdoc comment、記録のモデルと台帳は[計測](../measurement.md)、決めた理由はリンクしたADRが持つ。

## 入口

| コマンド | コードの入口 |
| --- | --- |
| `stats` | `domain::stats`のmodule doc、`application::stats::stats`、`compose::OneShot::stats_of` |
| `kpi` | `domain::kpi`のmodule doc、`application::kpi::kpi`、`compose::OneShot::kpi_of` |
| `mark` / `marks` | `domain::marks`のmodule doc、`application::marks` |
| `report` | `domain::kpi::report`、`application::report`、supervisorの日次は`application::supervise::report` |
| `timeline` | `domain::timeline`、`compose::observation::timeline` |

## コマンドをまたぐ約束

- 値は記録（run_events）から読むたびに同じ規則で導き、集計やKPIの表を持たない（[ADR-0040](../../adr/0040-verify-once-review-run-env-graph-stats-and-task-priority-in-claim-order.md)決定5、[ADR-0051](../../adr/0051-kpi-time-series-report-and-push.md)）。
  台帳から作る形は[計測](../measurement.md#共通の読み取り関数)が持つ。
- 規則は1か所に書く。
  `kpi`と`report`は期間ごとの窓で`stats`と同じdomainの関数を呼び、observerの入力・plan reviewの衝突の多いファイル・完了見込みも同じ関数を直接呼ぶ。
- 同じ記録・同じ今の時刻・同じ観測からは同じ出力になる。
- 出力はstdoutのJSONで、既存の欄は変えず足すだけにする。
  observer・レポート・plan reviewやスループットの見直しのpromptが欄を名前で読むため。
- 記録の無い古いeventは書き換えず、null・`unknown`・`unlabeled`として数え、0と区別する。
- `stats`・`kpi`・`marks`・`timeline`は読むだけのコマンドで、queueを読み取り専用で開き、observerとheadlessのjobも打てる。
- `report`はqueueを読み取り専用で開くがファイルを書き、`mark`はeventを書くので、どちらもobserverとheadlessのjobには許さない（[Authorization](../authorization.md)）。
- cursor（event ID・unix秒・RFC 3339の時刻）の読み方はどのコマンドでも同じで、`domain::stats::Cursor`のdoc commentが持つ。
- goalで絞ると、taskを持たないevent（observerの観察、taskの無いask、plannerのturnなど）の多くは0か数えないので、絞った値と絞らない値をその種類の欄で比べない。
- 日と週の期間は、hostのlocal timezoneの今のoffsetで全ての期間を切るので、夏時間の切り替えをまたぐ期間は1時間ずれる。
- 印（`mark`・supervisorの起動と停止・`[run.env]`の変化・claimの属性から導く印）はrunの状態を変えず、attentionにならず、`events`の既定の絞り込みと`watch`にも出ず、KPIの期間の区切りと前後の比較にだけ使う。
- KPIの記帳のevent（`domain::kpi::BOOKKEEPING_KINDS`）は、それだけではobserverを起こさない。
- KPIの目標の判定は`kpi`・レポート・push・observerが同じ設定（`dagq.toml`の`[kpi]`にhost.tomlを重ねたもの、`infrastructure::kpi_config`）から同じ関数で読む。
- runのareaは保存せず、`stats`と`kpi`が読むたびに着地したcommitの差分を`dagq.toml`の`[areas]`に通して求めるので、`[areas]`を直せば過去のrunも分類し直され、`[areas]`が壊れていれば両方とも止まる。
- dagqのソースでないrepositoryのqueueでは、cargo専用の計測をどのコマンドも出さない（[Source repository](source-repository.md#cargo専用の計測)）。

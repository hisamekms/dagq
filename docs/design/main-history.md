---
id: design-main-history
type: design
title: mainの履歴の記録
status: current
created: 2026-10-09
scope: runtime
related:
  - design-measurement
  - design-supervisor-lifecycle-stats
  - design-supervisor-lifecycle-supervise
  - adr-t1662-1
  - adr-t1662-2
---

# mainの履歴の記録

`conflict_hotspots`の着地件数と改名・削除、areaの変えたfile（[stats](supervisor-lifecycle/stats.md)）を統計がGitを読まずに作れるよう、supervisorの周回がmainのfirst-parentの履歴を`main_*`のeventとしてEventStore（[計測](measurement.md)の「SSOTとビュー」）に記録する。
何を書くかは`application::main_log::records`が決め、周回からは観測と分析の`supervise::main_log`が呼ぶ。
runtimeの制御はこの記録を読まない。

- **3つの時刻**: commitの時刻（committerの時刻）、観測したhead（最後に正常に読んだhead）、到達時刻（Gitを正常に読み、観測したheadまでが記録済みと確かめた時刻。`main_observed`の`at`、畳んだ`recorded_through`）。
  到達時刻はheadが動かなくても進む。
- **Gitを読む周回**: 周回は毎passでGitを起動しない。
  着地先のbranchの解決の入力のファイルの印（[着地先のbranch](supervisor-lifecycle/landing-branch.md)の「supervisorが確かめる頻度と条件」と同じ`Repository::landing_branch_stamp`。mainへのcommitで変わる）をGitなしで読み、印が変わったとき・到達点の間隔が来たとき・最初の周回だけ、queueとGitを読む。
  印を読めないrepositoryと読み取りの失敗の間は、前に読んでから1分（`MAIN_RETRY_SECS`）経つまで読まない。
- **記録**: Gitを読む周回はheadを読み、前回の到達点から動いていれば間のfirst-parentのcommitを変えたpath（改名と削除を含む）とともに古い順に`main_commits_recorded`へ書き、続けて`main_observed`を書く。
  dagqの着地も人の直接のpushも入る。
  commitは上限まで1つのeventにまとめ、1つのcommitは分けない。
- **到達点の間隔と猶予**: headが動かない間は`MAIN_OBSERVED_INTERVAL_SECS`（10分）ごとに`main_observed`だけを書く。
  猶予`MAIN_RECORD_GRACE_SECS`はその3倍。
- **読めないとき**: commitも到達点も書かず、読めなくなったときに`main_read_failed`、読めるようになったときに`main_read_recovered`を理由つきで書き、前回の到達点から追いつく。
- **取り込み**: `main_observed`の無い最初の周回は、衝突の有無に関わらず全eventの最古の時刻の1秒前からのcommitと、今のheadの全pathを上限の件数ずつに分けた基準（`main_paths_recorded`）を書く。
  基準は全部分が揃って有効になり、途中で止まれば次の周回が取り直し、畳む関数は同じshaを1度だけ数える。
- **force push**: 到達点のcommitがmainのfirst-parentの線から外れれば`main_rewritten`を書き、その線の上のmerge baseから（無ければ最初から）commitと基準を記録し直す。
  畳む関数は後の記録を正にする。
- **畳む関数**: `stats::main_log::fold_main_log`がeventだけからMainHistory・commitごとの変えたfile・到達時刻・Gitの読み取りの状態を返す。
  窓の終わりが到達時刻から猶予より後か、窓が取り込みより前に始まれば、`MainLog::history_for`は理由（Gitの読み取りの失敗中・supervisorの停止・到達点が古い）つきの「記録が無い」を返す。
  statsとkpiの読み手はまだGitを読む。

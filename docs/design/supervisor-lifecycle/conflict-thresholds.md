---
id: design-supervisor-lifecycle-conflict-thresholds
type: design
title: "Conflict thresholds"
status: current
created: 2026-09-26
updated: 2026-10-04
last_verified: 2026-10-04
scope: runtime
related:
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-claim-defer
  - adr-0080
  - adr-t774-1
  - adr-t775-1
---

# Conflict thresholds

`dagq.toml`の`[conflicts]`（goal 31）が、`stats`の`conflict_hotspot`のalert（[`stats`](stats.md#stats)の`conflict_hotspots`）の閾値を持つ。読み込みは`[stall]`と同じ`src/infrastructure/run_env.rs`（`parse_config`と、fileを読む`load_conflict_config`）で、型と既定値は`src/domain/stats/conflicts.rs`の`ConflictConfig`。書式は`[stall]`と同じ（正の整数、`_`の桁区切りと`#`以降のcommentを許し、未知のkey・0以下・重複はエラー）。

| 設定名 | 既定値 | 意味 |
| --- | --- | --- |
| `hotspot_conflicts` | 3 | alertにするファイルの、window内の衝突の最少回数 |
| `hotspot_ratio_percent` | 20 | alertにするファイルの、そのファイルを変えた着地の数に対する衝突の割合の最小（%） |
| `defer_max_secs` | 3600 | alertのファイルで進行中のrunと重なるtaskのclaimを控える上限の秒数（[claimを控える（衝突の多いファイル）](claim-defer.md)、ADR-0080） |

- `stats`はmain checkoutの`dagq.toml`の`[conflicts]`（出力の`conflict_hotspots.config.source`が`file`）、無ければ既定値（`default`）で判定する。supervisorは起動時に同じものを読み（読めなければwarnを出して既定値にし、起動は止めない）、plan reviewのpromptの衝突の多いファイルの`alert`をその値で判定する。同じalertのファイルでclaimを控え、`defer_max_secs`をその上限にする。値は下の[読み直し](#読み直し)でpassごとに読み直すので、変えるのに`down --wait` → `up`は要らない。
- 着地の後のlanding recheck（[Landing recheck](landing-recheck.md)）が見つけた衝突（`landing_recheck_failed`）は`conflict_hotspots`に数えない。数は`stats`の`landing_rechecks`に出る。

## 読み直し

supervisorはloopのpassごとに、`[run.env]`の変更の印（`run_env_changed`）に続けてclaimの前に、main checkoutの`dagq.toml`の`[conflicts]`を読み直す（`Supervisor::reread_conflicts`、`src/application/supervise/claim_defer.rs`。読むのは`Ports::conflicts_file`で、`src/compose.rs`が`SuperviseOptions.load_conflicts`（既定は`load_conflict_config`。起動時の読みも同じものを使い、testは起動時と読み直しで同じエラーを返すものに替える）を渡す。ADR-0080、ADR-t774-1、ADR-t775-1）。`SuperviseOptions.conflicts`で値を与えたとき（test）は`conflicts_file`が`None`で、読み直さない。

- **変わったとき**: 3つの値のどれかが使っている値と違えば、その組を候補として保持する。続くpassでも同じ3つの値を読んだときだけ、新しい値（`source: file`）に替え、cacheしたhotspot（[claimを控える](claim-defer.md#判定の入力)の10分のcache）を捨てる。次のclaimの判定から、新しい閾値のhotspot・新しい`defer_max_secs`（進行中の控えにも効き、数え始めは最初の`claim_deferred`のまま）・plan reviewのpromptの`alert`がその値を使う。queue event `conflicts_config_changed`（`from`・`to`（どちらも`hotspot_conflicts`・`hotspot_ratio_percent`・`defer_max_secs`）・`source`・`supervisor`）を1回記録し、supervisorのlogにinfoで出す。queueの最新の`conflicts_config_changed`の`to`が新しい値と同じなら（同じqueueの別のsupervisorが記録した）記録しない（`domain::stats::conflicts::conflicts_change`）。候補を保持するだけのpassでは、使用中の値・hotspotのcache・eventを変えない。使用中の値に戻れば候補を捨て、別の値なら候補を入れ替えてそこから2回を数える。起動時は即時に適用する
- **起動時の値**: 最初の読み直しが起動時に`dagq.toml`から読んだ値（`source: file`）と同じ値を読んだとき1回だけ、その値をqueueの最新の`conflicts_config_changed`の`to`と比べ、違えば`from`にその`to`、`to`に起動時の値、`source: start`で1回記録し、logにinfoで出す（`Supervisor::record_conflicts_at_start`、`domain::stats::conflicts::conflicts_at_start`）。これで`dagq.toml`の値で起動したsupervisorの使っている値を最新の記録の`to`が表し、上の重複除けが正しく働く（XからYを記録した後にXで起動し直してYに変えても記録される）。記録がまだ無いとき、最新の`to`と同じとき、最初の読み直しが起動時と違う値・読めない・無いとき（その間に別のsupervisorが新しい値を記録したかもしれない）、起動時に`dagq.toml`が無い・読めず既定値で起動したときは記録しない（既定値で起動したsupervisorについては最新の`to`が使っている値と違いうる。ADR-t775-1）
- **読めない・不正なとき**: 候補を捨て、使っている値を保ち（起動の後は既定値に戻さない）、warn `[conflicts] of dagq.toml not read: ...; keeping the values in use`を出す。同じエラーのwarnは、読めるようになるかエラーが変わるまで繰り返さない。起動時の読みのエラーは起動時にwarn（`...; using the defaults`）に出し、最後にwarnしたエラーとして読み直しに渡すので（`read_conflicts_at_start`と`LoopSettings.conflicts_error`）、最初の読み直しが同じエラーならもう出さない（ADR-t775-1）。直して前と同じ値に戻したときは変更ではないので記録しない。なお`dagq.toml`の書式は1つのparserが全体を検査するので、不正な`[conflicts]`の間は着地先のbranch（[Landing branch](landing-branch.md)）も解決できず、claimと着地、headlessのreviewの起動も止まる
- **`dagq.toml`が無いとき**: checkoutの書き換えの途中でありうるので、候補を捨て、使っている値を保つ（`[run.env]`の印と同じ）。欠落やエラーを挟んだ読みは連続とは数えない
- **`[conflicts]`の表やkeyを消したとき**: 消したkeyは既定値として読み（`source: file`）、同じ値を2回続けて読んだときに既定値への変更として適用・記録する。空のファイルやparseできる書きかけも同じ扱いなので、1 passだけの一時的な値は適用しない。ただし2 pass以上同じ書きかけが残れば適用されるため、書き込みは別のファイルからrenameで置き換えるのが確実。この確認待ちは変更の反映を1 pass遅らせる（[ADR-t774-1](../../adr/2026-10-04-t774-1-confirm-conflicts-config-on-consecutive-passes.md)）

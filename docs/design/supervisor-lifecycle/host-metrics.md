---
id: design-supervisor-lifecycle-host-metrics
type: design
title: "hostの負荷の連続の記録"
status: current
created: 2026-09-28
updated: 2026-10-02 # task 1371: disk free
last_verified: 2026-10-02 # task 1371
scope: runtime
related:
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-stats
  - design-supervisor-lifecycle-supervise
  - design-supervisor-lifecycle-kpi
  - adr-0049
  - design-supervisor-lifecycle-disk-space
  - design-supervisor-lifecycle-report
  - design-supervisor-lifecycle-throughput-review
---

# hostの負荷の連続の記録

task 516。supervisorが動いているあいだ、hostの負荷（load average・プロセスの種類ごとのCPUとメモリ・メモリ・swap・pageoutと、task 1371で足したrunのworktreeのファイルシステムの空き）を決まった間隔で取り、queueのディレクトリ（`dagq locate`の`db`のあるディレクトリ）の`host/metrics-YYYYMMDD.csv`に日ごとに書く。eventにはしない（連続の値はqueueの記録ではなく、run_eventsから再導出する`stats`の外の入力。[ADR-0049](../../adr/0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)の決定5がADR-0040の決定5から引き継いだ「新しい表は持たない」）。区間ごとのloadの平均・最大はtask 197がrunのeventに載せる（[`stats`](stats.md#版と負荷と検証コマンド)）ので、ここは連続の記録だけを持つ。

## 記録

- **間隔と保持**: `supervise --host-metrics-interval <秒>`（既定30、0で記録しない）と`--host-metrics-retention-days <日>`（既定30。今日を含めた直近の日数で、0なら消さない）。`dagq.toml`では設定しない。`up`は既定値のsupervisorを立てる
- **取り方**: supervisorのloopが毎pass（drain中・引き継ぎ待ちも）間隔が過ぎたかを見て、過ぎていればjobのthreadで1回取り、ファイルに1行足す。jobは同時に1つで、遅いツールは次の取得を遅らせるだけで積み重ならない。引き継ぎ（exec）は走っているjobの終わりを待ち、引き継ぎを求められた後は新しいjobを始めない（遅い取得がexecを先延ばしにしない）。loopの終わりにも走っているjobを待つ
- **失敗**: 取れなかった値は空のセルにする。ツールの失敗・ファイルに書けない・jobのpanicはログ（warn。成功するまで1回だけ）に出すだけで、claimも着地も止めない
- **ファイル**: hostのlocalの日付のファイルに追記し、新しいファイルには先頭にheaderを書く。同じqueueの別のsupervisorが間隔の半分より近くに書いた行があれば書かない（2本のsupervisorが同じqueueに居ても行が倍にならない。lockは取らないので、同時に新しい日のファイルを開くとheaderか行が重なることがあり、読み手はunix秒の無い行を飛ばす）。書いた後、保持の日数より前の日付の`metrics-YYYYMMDD.csv`を消す（それ以外の名前のファイルは残す）
- **実装**: 列・パース・保持・要約は`domain::host_metrics`、ツールの呼び出しとファイルは`infrastructure::host_metrics`（`sample` / `record` / `read` / `summary`）、supervisorのpassは`application::supervise::host_metrics`

## 列

| 列 | 中身 |
| --- | --- |
| `time` | `unix`のhostのlocal時刻（`YYYY-MM-DDTHH:MM:SS+HH:MM`） |
| `unix` | 取った時刻（unix秒） |
| `load1` `load5` `load15` | getloadavg(3)の1・5・15分のload average |
| `cpu_total` | `ps -A -o pcpu=,rss=,comm=`の`%cpu`の合計（1コアを100とする%） |
| `cpu_<kind>` `rss_<kind>_mb` | プロセスの種類ごとの`%cpu`の合計と常駐メモリ（MB）。kindは`cargo`・`rustc`・`claude`・`dagq`・`other` |
| `mem_total_mb` | 物理メモリ |
| `mem_used_mb` | macOSはwired + active + compressorが占めるページ、Linuxは`MemTotal - MemAvailable` |
| `mem_compressed_mb` | macOSのcompressorが占めるページ（Linuxは空） |
| `swap_total_mb` `swap_used_mb` | macOSは`sysctl -n vm.swapusage`、Linuxは`/proc/meminfo`の`SwapTotal` / `SwapFree` |
| `pageouts` | 起動からの累計。macOSは`vm_stat`の`Pageouts`、Linuxは`/proc/vmstat`の`pswpout` |
| `disk_free_bytes` `disk_total_bytes` `disk_free_pct` | runのworktreeを置くファイルシステムの空き（一般のprocessが使える量）・全体（bytes。丸めない）と、空きの割合（%、小数2桁）。下の[ディスクの空き](#ディスクの空き) |

プロセスの種類はコマンド（`ps -o comm`のpathか名前）で分ける: 名前が`dagq`（`dagq.previous`なども）は`dagq`、名前が`claude`かClaude Codeの`<...>/claude/versions/<version>`は`claude`、`rustc`・`clippy-driver`・`sccache`は`rustc`、`cargo`・`cargo-*`と`target/`の下から走るもの（build scriptとtest binary）は`cargo`、それ以外は`other`。macOSの`%cpu`は減衰する平均なので瞬間の値ではない。

macOSでは`vm_stat`・`sysctl -n hw.memsize`・`sysctl -n vm.swapusage`・`ps -A -o pcpu=,rss=,comm=`を、Linuxでは`/proc/meminfo`・`/proc/vmstat`・`ps -A -o pcpu=,rss=,args=`を使う（`/proc/meminfo`があればLinuxの読み方にする）。Linuxの`comm`は15バイトに切った名前でpathを持たないので、`args`の最初の語（起動したpath）で種類を分ける。各ツールは10秒で打ち切り、UTF-8でないバイトは置き換えて読む。

## ディスクの空き

task 1371（goal 86）。2026-10-01〜02の夜に空き容量が数時間おきに尽き、`queue_hold`（`cost`）でclaimと着地が止まったが、空きの推移の記録が無く、詰まったこと（`queue_hold`のask）しか分からなかった。そこで上の行ごとに空きを記録する。

- **対象**: queueの`runs/`（runのworktreeとそのビルド成果物を置く場所。[空き容量を確かめる](disk-space.md)の判定と同じ）のファイルシステム。`runs/`が無ければqueueのディレクトリ（DBのあるディレクトリ）。queueのディレクトリとworktreeは同じ`runs/`の上にあるので1つで足りる
- **取り方**: `statvfs(3)`の`f_bavail × f_frsize`を空き、`f_blocks × f_frsize`を全体とする（`infrastructure::adapters::disk_space`）。読めなければ3列とも空のセル。全体が0なら割合は空
- **間隔と形**: 他の列と同じ行で、`--host-metrics-interval`（既定30秒）ごと。eventにはしない（上の「eventにはしない」と同じ理由。空きの控えと掃除の記録（`claim_held`・`landing_held`・`auto_repaired`の`disk_cleanup`）は今までどおりeventで、ここは連続の値だけ）
- **列が増えた日**: 列を足す前のバイナリが始めた日のファイルに追記するときは、行の前にこのバイナリのheaderを1度だけ書く（`domain::host_metrics::needs_header`。ファイルの1行目が今のheaderと違うときだけファイルの全体を読み、その中の最後のheaderが今のheaderと違えば書く。1行目が今のheaderなら全体は読まない）。読み手（`parse_file`）は各行をその上にある最後のheaderで読むので、入れ替えた日の前半の行は古い列で、後半の行は新しい列で読める
- **テスト**: `SuperviseOptions::host_metrics`の`HostMetricsSettings::disk`が空きの読み手を差し替える

## 読み口

- [`stats`](stats.md#hostの負荷)の`host`が窓の要約（列ごとの最小・平均・中央値・最大・p90と、CPU秒の`cpu_secs`）を出す。`disk_free_bytes`・`disk_free_pct`の`min`と`median`が窓の空きの最小値と中央値
- [`kpi`](kpi.md#期間の健全性)の各期間の`health.disk`（`domain::host_metrics::DiskFree`）が、その期間の空きの最小値と中央値（bytesと%）を出す。日次・週次のレポートとスループットの見直しの入力もこれを読む
- **CPU秒**（`cpu_secs`、goal 72、task 992）: 各行の`cpu_total`（と`cpu_<kind>`） ÷ 100 × その行が代表する秒の合計。行は前の行からの秒を代表し、窓の最初の行は典型の間隔（窓の中の行の最も短い間隔、つまり記録した間隔。1行なら既定の30秒。窓の始まりより前は数えない）を代表する。代表する秒は典型の間隔の2倍（`max_gap_secs`）を上限にし、記録の途切れを次の行の負荷で埋めない。`{covered_secs, max_gap_secs, total, by_kind}`で、`cpu_total`のある行が無ければnull。`kpi`の`cpu_per_landing`がこれを着地数で割る
- `infrastructure::host_metrics::summary(dir, from, until)`（`domain::host_metrics::summarize`）が任意の区間の要約を返す。[`kpi`](kpi.md#hostの負荷)はこれを各期間・`--since` / `--until`の窓・`--compare`の前後の窓ごとに読み、`host`として並べる（task 872。日次・週次のレポートのJSONにも載る）。`host`は参考の値で目標の判定・breach・push・observerのfindingには使わない。そこから`cpu_per_landing`と`load_per_core`をKPIにする（[`kpi`](kpi.md)、task 992）
- 人はCSVをそのまま読める（1日30秒ごとで約2,900行）

## 外部のsamplerを止める手順

これまでhostの負荷は外部のsampler（`~/.local/share/dagq-hostmetrics/sample.sh`が30秒ごとに`metrics.csv`へ書く）が記録していた。この記録が入ったバイナリに入れ替わった後は次の手順で止めてよい。

1. `dagq status`でsupervisorの`binary_version`がこの変更を含むことを確かめる（`--auto-update`のsupervisorは着地の後に自分で入れ替わる）
2. `ls "$(dirname "$(dagq locate | jq -r .db)")/host/"`に今日の`metrics-YYYYMMDD.csv`があり、`tail -n 3`で30秒ごとに行が増えていることを確かめる
3. `dagq stats`の`host.samples`が0でないことを確かめる
4. 外部のsamplerを、それを起動した方法（launchdのagent・cmuxのworkspaceのshellなど）で止める。pidで止め、`pkill`のように名前で選ばない
5. 過去の`metrics.csv`は比較のために残してよい（runtimeは読まない。列が違う）

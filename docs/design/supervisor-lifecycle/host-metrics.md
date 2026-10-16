---
id: design-supervisor-lifecycle-host-metrics
type: design
title: "hostの負荷の連続の記録"
status: current
created: 2026-09-28
scope: runtime
related:
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-commands
  - design-supervisor-lifecycle-supervise
  - adr-0049
  - adr-t1662-2
  - design-measurement
  - design-persistence
  - design-supervisor-lifecycle-disk-space
  - design-supervisor-lifecycle-throughput-review
---

# hostの負荷の連続の記録

supervisorが動いているあいだ、hostの負荷（load average・プロセスの種類ごとのCPUとメモリ・メモリ・swap・pageoutと、runのworktreeのファイルシステムの空き）を決まった間隔で取り、1回のsampleをqueue.dbのNodeSampleStore（`node_samples`）と、queueのディレクトリ（`dagq locate`の`db`のあるディレクトリ）の`host/metrics-YYYYMMDD.csv`の両方に書く。
NodeSampleStoreが恒久の記録で、CSVは同じsampleから書く人が読む写し（[ADR-t1662-2](../../adr/2026-10-04-t1662-2-measurement-stores-ssot-and-views.md)の決定5・7、[計測](../measurement.md#ssotとビュー)）。
eventにはしない（連続の値でrun_eventsを膨らませない）。
区間ごとのloadの平均・最大はrunのeventに載せる（[`stats`](commands.md)）ので、ここは連続の記録だけを持つ。

## 記録

- **間隔と保持**: `supervise`のflagで決める（flagと既定値は`supervise --help`、`dagq.toml`と`up`との関係は`DEFAULT_INTERVAL_SECS`のdoc comment）。
- **取り方**: supervisorのloopが毎pass（drain中・引き継ぎ待ちも）間隔が過ぎたかを見て、過ぎていればjobのthreadで1回取り、ストアとファイルに1行ずつ書く。
  jobは同時に1つで、遅いツールは次の取得を遅らせるだけで積み重ならない。
  引き継ぎ（exec）は走っているjobの終わりを待ち、引き継ぎを求められた後は新しいjobを始めない（遅い取得がexecを先延ばしにしない）。
  loopの終わりにも走っているjobを待つ
- **失敗**: 取れなかった値は空のセル（ストアではnull）にする。
  ツールの失敗・ストアやファイルに書けない・jobのpanicはログ（warn。成功するまで1回だけ）に出すだけで、claimも着地も止めない。
  ストアとファイルは互いに独立で、片方に書けなくても他方には書く
- **ファイル**: hostのlocalの日付のファイルに追記し、新しいファイルには先頭にheaderを書く。
  書くかどうかはストアの判断に従う（下の「ストア」）。
  ストアを開けない・書けないときだけ、ファイルの最後の行が`host_metrics::apart_secs`より近ければ書かない（2本のsupervisorが同じqueueに居ても行が倍にならない）。
  lockは取らないので、同時に新しい日のファイルを開くとheaderか行が重なることがあり、読み手はunix秒の無い行を飛ばす。
  書いた後、保持の日数より前の日付の`metrics-YYYYMMDD.csv`を消す（それ以外の名前のファイルは残す）
- **ストア**: nodeと取った秒ごとに1行で、行はsampleが入るファイルのlocalの日を持つ。
  sampleを残すかはストアの1回の書き込みが決め（同じnodeで同じlocalの日の行が`apart_secs`より近くにあれば残さない）、ファイルはその判断に従うので、2本のsupervisorのjobが前後して交ざっても、sampleが取った順に届かなくても、両方に書けたsampleは同じになる。
  日は時刻の範囲でなく行ごとに持つので、時計が前の日に戻った区間のsampleも、localの日をまたぐ2本のsupervisorのsampleも、ファイルと同じく残る。
  行の順のidは、保持で消した後も使い直さない（台帳の到達点と係のカーソルが戻らない）。
  値はファイルのセルが読み戻す精度で持つので、両方に書けた期間の同じ窓のストアの要約とファイルの要約は同じになる。
  ストアに書けなかった間にファイルだけに入った行は、次のprocessの取り込みでストアに入る（ファイルに書けなかった行はファイルに戻らない）。
  行の中身はmigrationと`domain::host_metrics::NodeSample`のdoc commentが持つ
- **保持**: ストアの行は、ファイルと同じ日数と数え方で、行の持つlocalの日が保持の期間より前なら取った時刻によらず消すので、時計の切り替えがあってもファイルと同じ行が残る。
  台帳のnodeの行も、台帳を作る係が同じ日数で1日に1度sampleを落とす（[計測](../measurement.md#台帳を作る係)）。
- **CSVの取り込み**: processの最初のjob（失敗したら次のjobでも）が、そのsampleを書く前に、保持の期間にある`metrics-YYYYMMDD.csv`の行をファイルの読み手と同じ読み方でストアに足す。
  取り込みはファイルの各行を1回だけ足す（間隔によらず全ての行を足し、同じ秒の行があれば足さない）ので、何度走っても行は増えず、取り込み済みのファイルに後から足された行（日をまたいで遅れて終わったjob、前の日に戻った時計、ストアに書けなかったsupervisor）も次のprocessの取り込みで足される。
  取り込みをsampleより先にするので、ファイルの直前の行がsampleを締め出すときはストアでも締め出す。
  保持の期間より古い日は既にファイルが無く、hostの値の無い期間のまま
- **実装**: 入口はsampleをストアとファイルの両方に書くjobとpassの`application::supervise::host_metrics`で、列・パース・保持・要約・ツールの呼び出し・ストアはそこから辿る。

## 列

列の意味は`domain::host_metrics::COLUMNS`とOSごとの読み手のdoc commentが持つ。

プロセスの種類はコマンド（`ps -o comm`のpathか名前）で分ける（`domain::host_metrics::process_kind`）。
次に、main checkoutの`dagq.toml`が宣言したprofileを宣言の順に当て、最初に当たったprofileが道具のprocessをbuildの道具（`cargo`の列）かcompiler（`rustc`の列）に分ける（`ToolchainProfile::process_kind`、各profileの印はその実装のdoc comment。[Toolchain profiles](../toolchain-profiles.md)）。
列の名前は、ストアの列とファイルのheaderの名前を変えずに読み続けるために固定のままで、`ToolProcess`のdoc commentが理由を持つ。
宣言の無いrepositoryには移行の間Rustのprofileが暗黙に当たり（ADR-t2268-1決定9）、その印（`domain::toolchain::rust`）で分ける。
`dagq.toml`は取るたびに読み、読めないか知らないprofileを名指すときはどのprofileにも当てない（`run_env::toolchains_in_use`）。
macOSの`%cpu`は減衰する平均なので瞬間の値ではない。

ツールの読み方はOSごとの読み手（`domain::host_metrics`）のdoc comment、読み手の選び方は`infrastructure::host_metrics::sample`のdoc commentが持つ。

## ディスクの空き

`queue_hold`（`cost`）でclaimと着地が止まるときの空きの推移を読めるように、上の行ごとに空きを記録する。

- **対象**: queueの`runs/`（runのworktreeとそのビルド成果物を置く場所。[空き容量を確かめる](disk-space.md)の判定と同じ）のファイルシステム。`runs/`が無ければqueueのディレクトリ（DBのあるディレクトリ）。queueのディレクトリとworktreeは同じ`runs/`の上にあるので1つで足りる
- **取り方**: `statvfs(3)`の`f_bavail × f_frsize`を空き、`f_blocks × f_frsize`を全体とする（`infrastructure::adapters::disk_space`）。
  読めなければ3列とも空のセル
- **間隔と形**: 他の列と同じ行で、同じ間隔ごとに取る。
  eventにはしない（冒頭と同じ理由で、空きの控えと掃除の記録（`claim_held`・`landing_held`・`auto_repaired`の`disk_cleanup`）はeventのまま、ここは連続の値だけ）。
- **列が増えた日**: 列を足す前のバイナリが始めた日のファイルに追記するときは、行の前にこのバイナリのheaderを1度だけ書く（`domain::host_metrics::needs_header`）。
  読み手は各行をその上にある最後のheaderで読むので、入れ替えた日の前半の行は古い列で、後半の行は新しい列で読める。

## 読み口

- [`stats`](commands.md)の`host`が窓の列ごとの要約とCPU秒を出し、要約の値は`domain::host_metrics`のdoc commentが持つ。
- [`kpi`](commands.md)の各期間の`health.disk`（`domain::host_metrics::DiskFree`）が、その期間の空きの最小値と中央値（bytesと%）を出す。
  日次・週次のレポートとスループットの見直しの入力もこれを読む
- **CPU秒**（`cpu_secs`）: 各行の`cpu_total`（と`cpu_<kind>`）をcoreの数に直し、その行が代表する秒を掛けた合計で、代表する秒の決め方と欄は`domain::host_metrics::cpu_secs`のdoc commentが持つ。
  代表する秒には上限があり、記録の途切れを次の行の負荷で埋めない。
  `kpi`の`cpu_per_landing`がこれを着地数で割る。
- `infrastructure::host_metrics::summary(dir, from, until)`（`domain::host_metrics::summarize`）が任意の区間の要約を返す。
  [`kpi`](commands.md)はこれを各期間と比べる窓ごとに読み、`host`として並べる（日次・週次のレポートのJSONにも載る）。
  `host`は参考の値で目標の判定・breach・push・observerのfindingには使わない。
  そこから`cpu_per_landing`と`load_per_core`をKPIにする（[`kpi`](commands.md)）。
- 今の`stats`・`kpi`・レポートはCSVを読む。
  NodeSampleStoreのsampleは台帳を作る係がnodeの行に畳み、共通の読み取り関数がその行だけから同じ要約を作る。
  今のCSVの要約との並走の比較は`stats --ledger`（[計測](../measurement.md#並走の比較と照合)）。
  読み手の切り替えは[計測](../measurement.md)の段の後続が行う
- 人はCSVをそのまま読める。

## 外部のsamplerを止める手順

外部のsampler（`~/.local/share/dagq-hostmetrics/sample.sh`が`metrics.csv`へ書く）でhostの負荷を記録しているhostは、この記録が入ったバイナリに入れ替わった後に次の手順で止めてよい。

1. `dagq status`でsupervisorの`binary_version`がこの変更を含むことを確かめる（`--auto-update`のsupervisorは着地の後に自分で入れ替わる）
2. `ls "$(dirname "$(dagq locate | jq -r .db)")/host/"`に今日の`metrics-YYYYMMDD.csv`があり、`tail -n 3`で間隔ごとに行が増えていることを確かめる。
3. `dagq stats`の`host.samples`が0でないことを確かめる
4. 外部のsamplerを、それを起動した方法（launchdのagent・cmuxのworkspaceのshellなど）で止める。pidで止め、`pkill`のように名前で選ばない
5. 過去の`metrics.csv`は比較のために残してよい（runtimeは読まない。列が違う）

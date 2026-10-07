---
id: design-supervisor-lifecycle-logs
type: design
title: "Logs"
status: current
created: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
  - adr-0033
---

# Logs

runtimeの進行と診断のメッセージは`tracing`の1系統で出す（[ADR-0033](../../adr/0033-one-tracing-pipeline-with-local-json-lines-and-optional-otlp.md)の決定1・2・7。ADRはproposedのまま、実装はtask 194）。applicationは`tracing`のマクロ（`info!` / `warn!` / `error!`）を直接使い、人が読む1行の文言をmessageに、`run_id`・`task_id`・`ask_id`・`op`（`integrate` / `push` / `follow_up` / `cleanup`）・`error`・`reason`などをfieldに持たせる。`integrate::land_integrating`は`integrate` span（`run_id`・`task_id`）の中で走る。portを挟まないのは、`tracing`がI/Oを持たないfacadeで、出口（subscriber）は起動部分が選び、testは`tracing`の`Dispatch`を差し替えて捕まえられるため（ADR-0013の方針1の「portは外部I/Oのため」に照らして、`NoteLog` portと`SupervisorLog`は廃止した）。supervisorがheartbeat・検証・着地のthreadを起こすときは`supervise::spawn_traced`で起こし、起こした側のsubscriberを引き継ぐ。

subscriberは`infrastructure::telemetry::Telemetry`で、`main`がコマンドごとに組み立てて`install`する（`set_global_default`とpanic hook）。`supervise`・`integrate`・`observe`・session wrapper（`session`・`planner-session`）・自動更新のjob（`auto-update`）と`up`・`down`・`rebind`（queueのDBがあるときだけ。無いqueueに`logs/`を作ると、queueを移す先やinitの先にdirが残るため）は`<queue dir>/logs/<process>-<YYYYMMDDTHHMMSSZ>-<pid>.jsonl`（`supervise`は`--log-dir DIR`があればDIR、無ければqueueの`logs/`。どちらもdirは作る）に1行1レコードのJSON Linesを追記し、ほかのコマンドはstderrだけに出す。レコードは`timestamp`（ISO 8601のUTC、ミリ秒）、`level`、`target`（moduleのpath）、`message`、`fields`（eventのfieldに、入っているspanのfieldを重ねたもの。同名はeventが勝つ）、`spans`（外側からの`name`とfield）を持つ。messageやfieldの改行はJSONの`\n`になるので、cmuxのstderrの末尾を含んでも1行は1レコードのまま。1レコードは1回の`write`で追記する。fileの先頭は`target: dagq::telemetry`の起動レコード（`process`・`pid`・`version`）で、panicは`dagq::telemetry::panic`のレコード（`thread`・`location`）になり、どちらもfileだけに書く（panicのstderrは既定のhookが従来どおり出す）。stderrには各eventのmessageだけを1行で出し、文言は従来のstderrと`supervisor-*.log`のものと同じ（新しく足したのは`observe`の`observer (<mode>) started` / `finished: <outcome>`の2行だけ）。`up`・`down`・`rebind`は終わるときに`dagq::telemetry::command`のレコードをfileだけに残す（ADR-0033の決定2。task 255）: 成功ならINFOの`dagq <command> finished: <outcome>`で、fieldは`command`・`outcome`（`up`はsupervisorの`outcome`）・`inbox`（`up`のinboxの`outcome`）・`report`（stdoutに出す結果のJSONを文字列にしたもの）、use caseが失敗を返したとき（`rebind`の拒否を含む）はWARNの`dagq <command> failed: <error>`で`command`・`error`（queueを開く・repositoryを調べる・互換のmigrationを当てるなど、use caseの前で失敗したときは下の`dagq::telemetry::exit`のレコードだけになる）。stderrとstdoutの出力は変えない。`rebind`は従来どおり`logs/rebind.jsonl`にも追記する（[Rebind](rebind.md)）。コマンドが失敗して終わるときは、stderrの`{"error":…}`に加えて`dagq::telemetry::exit`のレコードをfileに残す。失敗や保留はWARN（heartbeatの失敗とsupervisorの異常終了はERROR）、進行はINFOで、runに関わるeventは`run_id`をfieldに持つ（`jq 'select(.fields.run_id == "<run-id>")'`で1 runを追える）。fileが開けなければその旨をstderrに1行出してstderrだけで続け（以前の`--log-dir`はdirが作れないと`supervise`を起動失敗にしていたが、ADR-0033の決定どおり止めない）、書けなかったレコードは捨てる（processは止めない）。file名の時刻はprocessの起動時刻で、`supervisors.started_at`とは一致しない（pidで対応が付く）。以前のバイナリが書いた`supervisor-<started_at>-<pid>.log`（`[unix time] message`のテキスト）はruntimeは読まず、下の保持期間で消える。

保持期間は日数で決め、既定は14日（`telemetry::LOG_RETENTION`。設定では変えない）。fileを開く各process（`Telemetry::open`）が、開く前にそのdir（`--log-dir`を含む）を掃除し、runtimeが書く名前のfile（`<process>-<YYYYMMDDTHHMMSSZ>-<pid>.jsonl`、以前のバイナリの`supervisor-<unix時刻>-<pid>.log`、自動更新のjobの`update-<unix時刻>-<commit>.{log,build.log,e2e.log,json}`と`install`のe2eの関門の`install-<unix時刻>.e2e.log`（[Auto-update](auto-update.md)、[`install`](install.md)））のうち、最後の書き込み（mtime）から14日を過ぎたものを消す。名前にpidがあってそのprocessが生きていれば消さない（書き込みの少ない長寿命のsupervisorのfileを、開いたまま消さないため）。件数ではなく日数にしたのは、fileの数がrunとsessionの数に比例して日によって大きく変わり、件数の上限では忙しい日に障害調査に要る直近のfileまで消えうるため。1つのfileを大きさで分けるrotationはしない（fileはprocessごとで、1 processの寿命の分しか伸びない）。消したときは新しいfileの起動レコードの次に`dagq::telemetry`の`removed N log files last written more than 14 days ago`（`removed`・`retention_days`）をfileだけに残す。消せなかったfileは次の機会に回し、processは止めない。名前の形が違うfile（`launchd.log`、`rebind.jsonl`、時刻の部分が上の形でないfile、dir、symlink）は消さない。`supervise --log-dir DIR`ではDIRが掃除の対象になる。

`up`が作るagentは`--log-dir <queue dir>/logs`で起動し、launchdが拾うstdout / stderrは同じdirの`launchd.log`に溜まる（起動ごとのファイルはruntimeが分け、`launchd.log`は分けない。`launchd.log`は保持期間の掃除の対象外で、ローテーションもしない）。`locate`は`log_dir`、`label`、`launch_agent`（plistのpath。存在しなくても出す）を返す。

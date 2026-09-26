---
id: design-supervisor-lifecycle-observer
type: design
title: "Observer"
status: current
created: 2026-09-26
updated: 2026-09-27
last_verified: 2026-09-27
scope: runtime
related:
  - design-supervisor-lifecycle
  - adr-0044
  - adr-0051
  - design-supervisor-lifecycle-kpi
  - design-supervisor-lifecycle-finding-planners
  - design-domain-model
  - adr-0070
---

# Observer

[ADR-0044](../../adr/0044-findings-proposals-from-findings-and-quiet-observer.md)の決定4・18・23（task 292）。observerはnoteとdraft goalを書かず、findingを記録・更新し、findingに紐づけた`blocked`のaskを上げる（`--because`は`scope` / `discard` / `recovery_failed`から選ぶ）。前回から自分以外のeventが無ければagentを起動しないこと、MCPを読まないこと、`observe --history`はtask 294が実装した。同じADRの決定19〜22のうち、既定の間隔を3時間にすること、proposalを求める印からplannerを立てること、`events --full` / `timeline`はgoal 31の他のtaskが持つ。dagqが回っているかを観察して継続的改善の材料を残すjobで、個々の詰まりは解消しない。cmux workspaceを持たず、`AgentProvider::headless_command`に`assign_session_id`と`without_mcp`を足して起動する（Claudeでは`claude -p --allowedTools 'Bash(dagq:*)' --session-id <id> --strict-mcp-config -- <prompt>`。`--mcp-config`を渡さない`--strict-mcp-config`で、user・project・plugin・claude.aiのどのMCP serverも読み込まない）。実装は`src/observer.rs`。

- **`dagq observe [--since CURSOR] [--daily] [--dry-run] [--timeout SECS] [--claude PATH]`**: 1回のobservation。
  0. **変化が無ければ起動しない**: `--since`も`--dry-run`も無いとき、同じmodeの直前の（skippedでない）`observe_finished`が`succeeded`で、その入力が読んだ最後のevent（payloadの`cursor`。agentが走っている間に他が書いたeventも読んでいないので数える）より後にobserver自身のもの以外のeventが1件も無ければ、入力も集めず、dirも`observe_started`も作らず、`observe_finished`（`outcome: skipped`、`reason`、`previous_event_id`（直前のobservationの`observe_finished`）、`since`と`cursor`（どちらも読むはずだったcursor）、`cursor_saved: false`、件数0と空のid、`duration_secs: 0`、`dir: null`）だけを記録して返す。observer自身のものとして数えないのは、`observe_started` / `observe_finished`、KPIの記帳のevent（`domain::kpi::BOOKKEEPING_KINDS`: supervisorのcandidatesの標本の`candidates_sampled`（[`supervise`](supervise.md)の5）とレポートの`report_written`（[レポート](report.md)）と、pushの`kpi_push_sent` / `kpi_push_failed` / `kpi_push_abandoned`（[push](push.md)。失敗はattentionでinboxに届く）。目標割れの`kpi_breach_started` / `kpi_breach_resolved`は数える。ADR-0051の決定24。記帳のeventを足すtaskはこの一覧に足す。記帳のeventだけではobserverを起こさない）、payloadの`by`が`observer`の`finding_recorded` / `finding_updated` / `finding_status_changed`、`asked_by`が`observer`の`ask_opened`、payloadの`kind`が`observer`の`session_opened` / `session_closed` / `session_turns`（`SqliteQueue::events_besides`）。直前が`failed` / `error`なら（何も観察できていないので）起動する。skippedの`observe_finished`もtimerの期日の起点になる。
  1. 入力を集める: `stats --since <cursor>`（`--since`が無ければ前回のobserveが保存した`<queue dir>/observer/cursor`、それも無ければ`stats`の既定の直近50件。`--daily`は24時間前より前の最後のevent id）、`open` / `proposed`のfinding（`findings`）、直近20件のnote（`notes`）、openなask（`asks --open`）、`graph`の`candidates`と`critical`、KPIの`kpi`と改善のproposalの`improvements`（下の「KPIの目標割れと改善の上限」）。入力のJSONは`observer::observer_input`が組み立て、キーは`stats`・`kpi`・`findings`・`improvements`・`notes`・`open_asks`・`graph`。
  2. promptを作る。役割は「dagqが回っているかを観察し、うまくいっていないことを`finding record`でfindingに記録し（根拠はrun_eventsのidを`--evidence`で渡し、本文に埋めない。同じ種類・対象・subjectは既存のfindingの更新になるので、持っていない根拠か読みの変化があるときだけ記録し直す）、remedyが要るfindingには`--propose`で理由を付け、もう起きていない問題は`finding resolve`し、今人の判断が要るものは`ask --kind blocked --finding ID`でinboxに上げる（openなaskのあるfindingには上げない）。note、goal、taskは書かず、個々の詰まりは解消しない。run / task / goalの状態は変えない」。promptは止まったsessionの閾値の読み方も持つ（ADR-0043の決定6、ADR-0044の決定21、task 408）: statsの`stall_thresholds`は`[stall]`の設定ごとの1項目で、`outcomes`の`answered_wait`が多ければ閾値が早すぎ、`preempted`が多ければ遅すぎの疑い、`resolved_by_nudge` / `resolved_by_enter` / `resolved_by_resend`は検知が効いている印、`pending`は未決で、`by_threshold_secs`で値の変更の前後を比べる。`running_alerts`の`idle_without_receipt`で`nudged: false`かつ`asked: false`のまま閾値を超えたものはsupervisorの検知の漏れの疑い。見直しが要ればkind `threshold`・subjectに設定名のfindingを記録し、繰り返すなら`--propose`を付け、閾値そのものは変えない。promptはKPIの読み方も持つ（下の「KPIの目標割れと改善の上限」）。queueのコマンドは`dagq --db <db>`の形で渡す。`--daily`は24時間の傾向を見る別の文面にする。`--dry-run`はpromptを`{dry_run, mode, since, cursor, prompt}`で返し、何も起動・記録しない。
  3. `<queue dir>/observer/<started_at>/`（同じ秒に既にあれば`-1`以降の接尾辞）を作り、`prompt.md`と`input.json`を書き、`observe_started`（`mode`: `hourly` / `daily`、`since`、`dir`）をtaskの無いrun_eventsに記録する。
  4. そのdirをcwdに、env `DAGQ_ROLE=observer`、`DAGQ_QUEUE=<db>`、PATHの先頭に`dagq`のdirを置いてagentを起動し、stdout / stderrを`output.log`に書く。`--timeout`（既定1800秒）を過ぎたらkillする。
  5. 終了後、`observe_started`より後にobserverが書いた`finding_recorded`と`finding_updated`（payloadの`by: observer`）のfindingとobserverのaskを集め、`observe_finished`（`mode`、`outcome`: `succeeded` / `failed`（非0終了） / `error`（起動できない・timeout）、`exit_code`、`error`、`since`、`cursor`（`stats`の`next_cursor`）、`cursor_saved`、`findings_recorded`、`findings_updated`、`asks`（それぞれ件数）、`recorded_finding_ids`、`updated_finding_ids`、`ask_ids`、`duration_secs`、`dir`）を記録して返す。`succeeded`のhourlyのときだけ`cursor`を`<queue dir>/observer/cursor`に保存する（一時ファイルからrename）。失敗したobservationのwindowは次のobservationが読み直す。dailyはcursorを動かさない。
- **`dagq observe --history [--limit N]`**: 過去のobservationを新しい順に`{observations: [...]}`で返す（既定20件。`observe_finished`ごとに1件で、読むだけなのでqueueを読み取り専用で開き、observerのenvからも打てる）。各件は`event_id`（`observe_finished`）、`mode`、`outcome`、`skipped`、`started_at`（同じ`dir`の`observe_started`の時刻。skippedは`finished_at`と同じ）、`finished_at`、`duration_secs`、入力の範囲の`input: {since, through}`（`since`より後から`through`（`stats`の`next_cursor`）までのeventを読んだ）、`cursor_saved`、書いたfindingの`findings: {recorded, updated, recorded_ids, updated_ids}`、askの`asks: {count, ids}`、`exit_code`、`error`、`dir`を持つ。idを記録する前のバイナリの`observe_finished`はidが`null`で件数だけになる。
- **権限**: observerのenvからのCLIは許可一覧で判定する（`main.rs`の`observer_access`、一覧は[domain-model](../domain-model.md#current-operations)）。読み取り（`findings`と`notes`、変更の印の`marks`、KPIの`kpi`、完了見込みの`forecast`を含む）、`finding record` / `finding resolve`、`ask --kind blocked`だけが通り、`note`、`mark`、`goal add`（`--draft`を含む）、`add`、`finding dismiss`、`ready` / `integrate` / `recover` / `goal ready` / `answer` / `observe`（`--history`は読み取りとして通る） / `supervise`などは`{"error":"observer may not change queue state"}`で拒否される。
- **timer**: `supervise --observe-interval SECS`（既定3600、`--once`のときは既定0。0でobserverを起動しない。dailyも含む）と`--observe-daily BOOL`（既定true）。ループの各passで、走っているobserverが無ければ、dailyが有効で最後のdailyの`observe_started` / `observe_finished`から24時間経っていればdailyを、そうでなく最後のhourlyから`--observe-interval`秒経っていればhourlyを、`<runner> --db <db> observe --claude <claude> [--daily]`の子プロセスで起動する（cwdはcheckout、`DAGQ_ROLE`は外す）。一度も記録の無いmodeは期日が来ている。期日はqueueのrun_eventsで判定するので、別のsupervisorや手の`observe`も数える。加えて同じプロセスが同じmodeを起動してから間隔が経つまでは再起動しない（記録を書く前に落ちたobserverを毎passで起動しないため）。同時に走るobserverは1つで、run slotを使わない。子プロセスの終了はlogに1行残し、記録は`observe_finished`が持つ。launchd modeでもin-cmux modeでも`supervise`の既定値で同じに動く（`up`はこのflagを渡さない）。
- **出力の扱い**: findingは人とplannerが`findings`で影響の大きい順に読み、手当てしないと決めたものを`finding dismiss ID --reason`にする。proposalを求める印の付いたfindingには、supervisorがruntimeのplannerを立ててproposalを作らせる（[Finding planners (supervisor)](finding-planners.md)）。findingに紐づけた`blocked`のaskには、runtimeが`propose`と`dismiss`のoptionを足し、人のその答えをfindingに適用する。`blocked`のaskは`status --role inbox`に`ask_opened`として出る。導入前にobserverが書いたnoteとdraft goalは記録として残り、draft goalは人が開いたplannerで扱う。

## KPIの目標割れと改善の上限

[ADR-0051](../../adr/0051-kpi-time-series-report-and-push.md)の決定24〜26（task 433）。observerはKPIの目標割れの継続を種類`kpi`のfindingにし、そこから作る改善のproposalは同時の数に上限がある。

- **入力の`kpi`**（`OneShot::observer_kpi`、純粋関数は`domain::kpi::observe::observer_input`）: [`kpi`](kpi.md)を直近7日（`--period day --last 7`）と直近4週（`--period week --last 4`）で`dagq kpi`と同じ設定（main checkoutの`dagq.toml`の`[kpi]`にhost.tomlを重ねたもの）で計算し、次を載せる。数字は`kpi`の出力をそのまま引き、observerは作らない。読めなければ`{"error": ...}`にして他の入力で観察を続ける。
  - `config`: `min_samples`・`breach_periods`・`breach_weeks`。
  - `targets`: 日と週の各目標の`period`（`day` / `week`）・`kpi`・`stratum`・`stat`・`min`・`max`・`state`（`ok` / `missed` / `breach` / `not_judged`）・`streak`・`breach_since`と、期間ごとの`values`（`period`・`value`・`n`・`met`・`reason`）。
  - `breaches`: `state`が`breach`の目標ごとに、findingの`finding_kind`（`kpi`）と`subject`（`<KPI>/<層>`。例: `phase.work/kind=runtime`）、根拠の`evidence_event_id`（同じ期間・KPI・層の閉じていない`kpi_breach_started`のevent ID（`SqliteQueue::kpi_breach_events_open`）。supervisorがまだ記録していなければnull）、最後に判定した期間の`value`と`latest_period`、`streak`・`breach_since`、目標割れが始まった期間からの印の`marks`（`period`・`label`・`kind`・`at`）。
  - `trend`: `day` / `week`の期間ごとに`label`・`partial`・`runs`・その期間の印の`marks`と、前の期間より悪化した（判定済みで`verdict: worsened`）KPIの`worsened`（`kpi`・`stratum`・`previous`・`delta`・`ratio`。層は`all`と`kind=*`だけ）。
- **入力の`improvements`**（`OneShot::improvements_of`）: 動いている改善の数`running`、上限`limit`、`reached`、上限に達しているときにplannerを待つfindingの`waiting`（`finding_id`と`reason: improvement_limit`）。`dagq findings`も同じものを`improvements`として返す。
- **promptの読み方**: `breaches`の各項を、種類`kpi`・対象`queue`・その`subject`・根拠`evidence_event_id`で`finding record --kind kpi --queue --subject '<subject>' --evidence <id>`にし、summaryとdetailに値・目標・続いた期間・印をそのまま写す。根拠が無い（まだ記録されていない）目標割れは次のobservationに回す。日と週の同じKPIと層、続いている目標割れは`subject`が同じなので1件のfindingにまとまり（ADR-0044の決定18）、新しいeventがあるときだけ記録し直す（決定21）。`missed`と`not_judged`はfindingにしない。`kpi`のfindingの目標が`ok`に戻ったら、戻った期間を理由に`finding resolve`する。影響と続いた期間から改善が要ると読めば`--propose`を付け、目標割れを`blocked`のaskにはしない。`trend`は印と並べて読み、入力に無い数字を計算しない。
- **改善の上限**（[Finding planners](finding-planners.md)の3と11）: runtimeのplannerがfindingに紐づけて出したproposalのうち終わっていないもの（人が開いたplannerのproposalは数えない）と、`open`のfindingのために立ったruntimeのplannerの数が、`dagq.toml`の`[kpi]`の`max_improvement_proposals`（既定2。host.tomlでは変えない）に達しているあいだ、supervisorは印の付いたfindingに新しいplannerを立てない。findingは`open`のまま待ち、1つ終われば印の古い順に立つ。
- **優先度**: 改善のproposalのtaskは`normal`以下。plan reviewがpassのときに、findingに紐づいたproposalの`high`以上のtaskを`lower_priority`で`normal`に下げる（[Finding planners](finding-planners.md)の6）。
- **test**: `domain::kpi::observe`のunit test（目標割れのsubject・根拠・印、根拠の無い目標割れ）、`observer`のunit test（入力のキーとpromptの読み方）、`tests/it/runtime_observer.rs`の`observe_reads_the_kpis_and_the_improvements_and_keeps_one_kpi_finding_per_subject`（`dagq.toml`の目標と上限が入力に載り、同じsubjectの`kpi`のfindingが1件にまとまる）。

## 完了見込みの誤差（予定）

[ADR-0070](../../adr/0070-forecast-snapshots-and-scoring.md)の決定5（task 473）。まだ実装していない。observerは`kpi`が出す完了見込みの答え合わせのKPI（[stats](stats.md)の「完了見込み」）の目標の判定を入力に読み、目標割れ（`breach`）なら種類`kpi`ではなく種類`forecast`・対象`queue`・`subject`に指標と層（例: `p50_bias/kind=runtime`）のfindingを記録するか更新する。数字は作らず判定を引く。計算の改善が要ればproposalを求める印を付け、runtimeが立てるplannerが計算の改善をproposalにする（改善のproposalの上限と優先度はADR-0051の決定25・26のまま）。`blocked`のaskにはせず、人には上げない。見込みのsnapshotのeventは記帳のeventとしてobserverを起こさない。`dagq forecast`は読み取りのコマンドとして許す（task 474で実装）。日次レポートには見込みの誤差の欄を足す。

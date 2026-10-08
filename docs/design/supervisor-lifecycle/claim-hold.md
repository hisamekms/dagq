---
id: design-supervisor-lifecycle-claim-hold
type: design
title: "claimを控える（load average）"
status: current
created: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle-task-hold
  - adr-t1479-1
  - adr-t1591-1
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-supervise
  - design-supervisor-lifecycle-status
  - design-supervisor-lifecycle-stats
  - design-supervisor-lifecycle-observer
  - design-supervisor-lifecycle-backend-call-failures
  - design-supervisor-lifecycle-disk-space
  - design-supervisor-lifecycle-queue-hold
  - design-supervisor-lifecycle-run-environment
---

# claimを控える（load average）

負荷の高い時間帯にrunを増やすと、cmuxの時間切れ（`backend_call_failed`）とrunの起動の遅れが増える（goal 17、task 327）。supervisorは新しいrunをclaimする前に「claimを控えるか」を1つの判定で決め、控えている間は新しいclaimをしない。走っているrun（sessionの監視・validation・review・resume・triage・着地）には触れない。

## 判定

`domain::claim_hold::ClaimHold::judge`が、判定の入力（`HoldInputs`）から最初に当たった理由（`HoldReason`）を返す。理由は次の順に判定する。

- `authentication` / `usage_limit`: queueで1件の認証か利用上限の`queue_hold`のaskがopen（`HoldInputs.queue_hold`）。`value`はaskの`affected`の数、`threshold`は0で、`claim_held`に`ask_id`が付く。この控えはclaimに加えてheadless jobの起動も止める（[認証と利用上限のaskの待ちとanswer](queue-hold.md)、task 437）
- `disk_space`: queueのdirectoryの空き（`value`、bytes）がclaimに要る空き（`threshold`、直近のrunのビルドの最大値 × `[disk] claim_factor`）を下回る。空きが読めないか閾値が無ければ控えない。掃除とinboxへの知らせと、着地の検証の控え（`landing_held` / `landing_resumed`）は[空き容量を確かめる](disk-space.md)（task 377）
- `load_average`: hostの1分のload average（`getloadavg`）が`supervise --max-load`（既定はhostの論理CPU数の2倍）を超えている。等しいときは控えない。load averageが読めないときは控えない

task 437は`HoldReason`に`authentication` / `usage_limit`、`HoldInputs`に`queue_hold`を足し、同じ判定・同じイベント・同じ`status` / `stats`の出し方を使う。task 377は理由`disk_space`を足し、着地の検証の控えにも同じ判定と同じ形の記録（`HoldKinds`の`LANDINGS`、`transition_of` / `holds_of`）を使う。task 463の衝突の多いファイルの控えはqueue全体ではなく1つのtaskを飛ばすもので、taskのevent（`claim_deferred` / `claim_deferral_ended`）で記録し、`status`の`claim_deferrals`と`stats`の`claim_deferrals`に同じ形で出す（[claimを控える（衝突の多いファイル）](claim-defer.md)）。

`--max-load`の既定の決め方: 与えなければ、hostの論理CPU数（`std::thread::available_parallelism`、`infrastructure::clock::logical_cores`）の2倍（`domain::claim_hold::DEFAULT_LOAD_PER_CORE`）にする。論理CPU数が読めないときは16.0（`FALLBACK_MAX_LOAD`）。CLIの`supervise`が`domain::claim_hold::resolve_max_load`で起動時に1回解決し、解決した値が`claim_held`の`threshold`（`status`の`claim_hold`・`stats`の`claim_holds.held`）に出る。係数2の根拠: この repository の queue の host は8コアで、2026-09-26の`stats --full`の`backend_failures.by_load_band`（load帯ごとの`backend_call_failed`）は`0-4`が1件、`8-16`が1件、`16-32`が56件、`32-64`が257件、`64+`が116件だった。cmuxの時間切れはloadがコア数の2倍（16）を超えたところから出始める。以前の既定は16.0の固定で、コア数の違うhostでは控え方が合わなかったので、コア数に比例させた（task 623。8コアのhostでは16のまま）。明示の`--max-load N`はそのまま使い、`--max-load 0`（0以下）で控えを無効にする。libraryの`SuperviseOptions::new`の既定は無効（`max_load: None`）で、CLIの`supervise`だけが既定を解決して渡す。`up`も`--max-load N`（0以下で無効）を受け、与えたとき（0を含む）だけ起動するsupervisorの引数に`--max-load N`を足す。与えなければ足さず、supervisorが自分のhostで既定を解決する（0未満は`0`として渡す。[up / down](up-down.md)）。

## 判定する場所

`fill_slots`の中で、放置されたrunのadopt・answerの適用・parkしたrunのresume・triage・sweepの後、`[run.env]`のprogramが無いときの打ち切り（ADR-0049の決定9）の次、claimのloopの前に判定する（`Supervisor::hold_claims`）。控えるときはそのpassのclaimをしない。空きslotの有無に関わらず、claimするpassごとに1回判定する。drainやhandoffの途中（claimしないpass）では判定しない。

`--once`のsupervisorは、runが無く控えているpassで終わる（claimできるtaskが無いのと同じ扱い）。

## claimの間隔

loadの保留が有効なsupervisorは、新しいclaimの間を空ける（[ADR-t1479-1](../../adr/2026-10-04-t1479-1-space-new-claims-while-the-load-hold-is-on.md)、task 1479）。1分のload averageはclaimしたばかりのrunのbuildをまだ含まないので、loadの低い瞬間に空いたslotを続けて埋めると、数分後に`--max-load`を大きく越える（finding 36）。

- **判定**: `fill_slots`のclaimのloopで、claimできる候補があるとき、claimの前に`domain::claim_spacing`で判定する。間隔が効いている（`in_effect`: `--max-load`が有効で、`claim_spacing`が0より大きい）なら、queueの最新の`run_claimed`（どのsupervisorのものでも。`latest_event_of`）の時刻 + `claim_spacing`秒（`next_claim_ms`）より前はclaimせずにloopを抜ける。claimした直後のloopの次の周も同じ判定で抜けるので、1つのpassで新しくclaimするのは1本になる。間隔が過ぎた後のpassは、claimの前の`hold_claims`でloadを判定し直し、`--max-load`を越えていれば控える（`claim_held`）。queueの記録から測るので、supervisorの起動し直し・execの引き継ぎ・同じqueueの別のsupervisorのclaimで数え直しにならない。
- **対象**: 新しいclaimだけ。parkしたrunのresume・待ちからの戻り・着地・triage・headless jobは間隔を見ない。
- **間を空けないとき**: `--max-load 0`（0以下）と、libraryの`SuperviseOptions::new`の既定（`max_load: None`）と、`claim_spacing = 0`。今までどおり1つのpassで空いたslotを埋める。e2eはすべての`supervise`に`--max-load 0`を渡すので、間隔は効かない。
- **設定**: `dagq.toml`の`[supervisor] claim_spacing`（0以上の整数の秒、既定180。`domain::claim_spacing::DEFAULT_CLAIM_SPACING_SECS`）。`parallel`と同じく起動時に読み、各passで読み直す（`ClaimState::reread_limits`、[Run environment](run-environment.md)の`[supervisor]`）。CLIのflagは無い。この repositoryの`dagq.toml`には置かない（既定が効き、`claim_spacing`を知らない固定バイナリは置くと`dagq.toml`を読めなくなる）。
- **既定180秒の根拠**: 固定バイナリの読み取り専用のeventsとqueueのdirの`host/metrics-*.csv`で、2026-10-01〜02の`run_claimed`を「10秒以内に続いたclaimの組」で分けると、1本だけのclaim（91組）はclaim時のloadの中央値9.8・その後6分のload1の最大の中央値17.7（16を越えたのは48組）、2本続けたclaim（10組）はclaim時7.35・6分の最大の中央値26.9（16を越えたのは9組）。2026-09-30 00:57:32にtask 1165・1050・1058をload 5.8で1秒以内に、02:08:12に1166・1167・1110を2秒以内にclaimし、load1は01:10までに48に達した。claimの後2分でcargoとrustcのCPUが300〜600%に上がり、load1は2分後にはもう上がっている（10-01 16:43:12の2本は2分後に59、10-02 14:51:18の2本はload 15.3でclaimして2分後に33）。claimから約2分でload1に出るので、それに1分の余裕を足した。
- **記録**: 間隔のための待ちは`claim_held` / `claim_resumed`にしない（observerと`stats`の`claim_holds`の控えの数え方に混ぜない）。待ち始めたときにlogにinfo（`claim_spaced`）を出す。間隔が効いているsupervisorの`run_claimed`のpayloadには、`claim_spacing`（秒）と`claim_spacing_wait_secs`（このprocessが間隔のためにそのclaimを待った秒。待たなければ0）が付く（`domain::measure::ClaimSpacing`）。待ちは、claimできる候補と空きslotがあるのに間隔で抜けた最初のpassから、claimしたpassまで。控え・空きslotが無い・候補が無いpassを挟むとそこで終わる。間隔の効いていないclaimには2つの欄が無い。
- **登録と`status`**: supervisorは登録に`claim_spacing`と出どころ（`claim_spacing_source`）を`parallel`と一緒に書き、`--max-load`（無効ならNULL）を起動時に書く（migration 0063）。`status`のsupervisorの項目の`claim_spacing`がそれと次にclaimできる時刻を出す（[`status`](status.md)）。
- `--once`のsupervisorは、runが無く次のclaimが間隔を待つpassで終わる（控えと同じ扱い）。

## 着地待ちが空けた軽い枠

reviewとe2eを終えて着地の順番を待つだけのrun（着地待ち）が空けた枠では、repositoryが軽いと決めたchangeのtaskだけをclaimする（[ADR-t1591-1](../../adr/2026-10-04-t1591-1-landing-queue-leaves-room-for-light-changes.md)、task 1591）。

- **着地待ち**: `AwaitingSlot`のslotが、e2eが要らないか終わった後に、他のrunの着地中（`integrating`のrunがある）でそのpassの着地を始められなかったとき、`Slot::landing_turn`の印を付ける（見るたびに付け直す）。`SlotTable::landing_queue()`はその数。`used_slots()`は今までどおり着地待ちを含む。
- **判定**: `fill_slots`のclaimのloopの各周で、`domain::light_slots::claim_room`が`used_slots()`・着地待ちの数・`parallel`・戻り待ちの数・`light_changes`が空でないかから`Any`（`used_slots()`が`parallel`未満。今までの空き）・`LightOnly`・`None`を返す。`LightOnly`は、`light_changes`が空でなく、戻り待ちのrunが無く、`used_slots()`から着地待ち（`parallel`件まで。`outside_the_slots`）を引いた数が`parallel`未満のとき。`LightOnly`の周は、claimの順（`claimable`）から`LightChanges::admits`に当たるtask（changeが`light_changes`の1つで、`--paths`を宣言している）だけを残し、同じ順でclaimする。interrupt・urgentでも軽くないtaskは残さない。
- **関門**: claimの控え（`hold_claims`のloadとディスク）はloopの前に判定するので、控えているpassは軽い枠でもclaimしない。claimの間隔（上の「claimの間隔」）は軽い枠の周にも同じに効き（`domain::light_slots::gated`）、軽い枠のclaimも`run_claimed`を記録するので次の間隔を始める。
- **戻る規則**: parkしたrunのresume（着地に失敗して`needs_session`に戻ったrunを含む）と待ちからの戻り・triage・adoptは今までどおり`used_slots()`が`parallel`未満のときだけ行い、軽い枠を使わない。
- **記録**: 軽い枠のclaimの`run_claimed`のpayloadに`light_room: true`が付く（`domain::measure::ClaimAttributes`。それ以外のclaimには無い）。`slots`は今までどおりclaimの前の`used_slots()`（着地待ちを含む）。着地待ちに出入りするeventは無い。
- **設定**: `dagq.toml`の`[supervisor] light_changes`（[Run environment](run-environment.md)の`[supervisor]`）。各passで読み直し、変わったら`supervisor_config_changed`の`from` / `to`の`light_changes`に記録する。flagは無く、全てのflagを与えたsupervisorも読む。
- **見え方**: `status`のsupervisorの`slots.landing_queue`とrunの`progress.slot: landing_queue`（[`status`](status.md)）。`stats`の`idle_slots`の空きと`kpi`の`slot_usage`は軽い枠を数えない（着地待ちを含めて数える今のまま）。
- test: `src/domain/light_slots.rs`のunit test（`light_changes`の検査・`admits`・`claim_room`・戻り待ちとresumeが通常の枠を待つこと・`gated`がloadの控えと間隔を軽い枠にも当てること）、`tests/it/runtime_light_slots.rs`（`light_changes = ["docs"]`・`parallel` 1で、着地待ちのrunが枠を埋めたとき`--paths`を宣言したdocsのtaskはclaimされ、interruptのfeatureと`--paths`の無いdocsのtaskはclaimされない。`a_parked_run_waits_for_a_normal_slot_while_the_landing_queue_fills_it`は、着地待ちが枠を埋めるあいだ`needs_session`のrunをresumeせずに軽いtaskだけをclaimし、枠が空くとinterruptのtaskのclaimより先にresumeすること）。

## 記録

判定がqueueの前回の記録と変わったときだけ、queueイベント（task・goal・runを持たない）を1件書く（`domain::claim_hold::transition`、supervisorの側は`PassEnv::record_hold`）。前回はqueueの最新の`claim_held` / `claim_resumed`で、最新の`claim_held`は、書いたsupervisorがこのsupervisor自身か、今動いている（登録があり、heartbeatがstaleでない）間だけ控えが続いているとみなす。loadはhostのものなので、同じqueueに2つのsupervisorが居ても交互に書き直さない。控えていたsupervisorが止まった（`down --wait`の後の`up`など）か死んだ後に、loadが高いまま起動したsupervisorは自分のtokenで`claim_held`を書き直すので、`status`と`stats`は控えを出し続ける。loadが下がっていれば、残った`claim_held`を`claim_resumed`で終える。

- `claim_held`: 控え始めたとき、または別の理由で控え直したとき。payloadは`reason`、`value`（判定した値。loadなら1分のload average）、`threshold`（`--max-load`）、`message`、`supervisor`、認証か利用上限の控えなら`ask_id`
- `claim_resumed`: 控えが終わったとき。payloadは終わった控えの`reason`と`supervisor`

supervisorのlogにも`claim_held`はwarn、`claim_resumed`はinfoで出る。

## `status`と`stats`

- `status`: 最新の`claim_held` / `claim_resumed`が`claim_held`なら、その`supervisor`の登録の項目に`claim_hold`（`claim_held`のpayloadと`since`（記録の時刻））を付ける（[`status`](status.md)）。着地の検証の控えは同じ形の`landing_hold`
- `stats`: `claim_holds`に、windowの中で始まった控えの`count`と`secs`（合計秒）、理由ごとの`by_reason: {<reason>: {count, secs}}`、今の控え`held`（`{reason, supervisor, since, value, threshold}`、無ければnull）を出す。控えは次の`claim_held` / `claim_resumed`か、同じsupervisorの`supervisor_stopped`で終わり、まだ終わっていない控えとwindowの後に終わった控えはwindowの終わりまでを数える。`--goal`では件数を数えない（taskを持たないため）が、`held`は出す。着地の検証の控えは同じ形の`landing_holds`。控えている間に空きslotがあれば、`idle_slots`の代わりにalert `claim_held`（`value`は空きslotの数）を出すので、控えによる空きと依存の詰まりによる空きを区別できる（[`stats`](stats.md)）
- observerは`stats --since <cursor>`を入力に読むので、`claim_holds`とalert `claim_held`もそのまま載る

## taskのhold（予定・未実装）

taskを1つずつ待たせるhold（[ADR-t1879-1](../../adr/2026-10-07-t1879-1-hold-a-task-at-the-turn-boundary-and-release-it-explicitly.md)）は、この控えと別の層で、まだ実装していない。
holdの開いたreadyのtaskはclaimの候補の列から外れ、claimの確定の中でも確かめ直すので、holdの前に集めた候補もclaimしない。
queue全体の控えではないので`claim_held` / `claim_resumed`を書かず、`stats`の`claim_holds`にも数えない。
詳細は[taskのhold](task-hold.md)が持つ。

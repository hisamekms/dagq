---
id: design-supervisor-lifecycle-claim-hold
type: design
title: "claimを控える（load average）"
status: current
created: 2026-09-26
scope: runtime
related:
  - adr-t1479-1
  - adr-t1591-1
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-supervise
  - design-supervisor-lifecycle-commands
  - design-supervisor-lifecycle-observer
  - design-supervisor-lifecycle-backend-call-failures
  - design-supervisor-lifecycle-disk-space
  - design-supervisor-lifecycle-queue-hold
  - design-supervisor-lifecycle-run-environment
  - adr-t1487-1
---

# claimを控える（load average）

負荷の高い時間帯にrunを増やすと、cmuxの時間切れ（`backend_call_failed`）とrunの起動の遅れが増える。
supervisorは新しいrunをclaimする前に「claimを控えるか」を1つの判定で決め、控えている間は新しいclaimをしない。
走っているrun（sessionの監視・validation・review・resume・triage・着地）には触れない。

## 判定

`domain::claim_hold::ClaimHold::judge`が、判定の入力から最初に当たった理由を返す。
理由は次の順に判定する。

- `authentication` / `usage_limit`: queueで1件の認証か利用上限の`queue_hold`のaskがopen（`HoldInputs.queue_hold`）。
  この控えはclaimに加えてheadless jobの起動も止める（[認証と利用上限のaskの待ちとanswer](queue-hold.md)）
- `disk_space`: queueのdirectoryの空きがclaimに要る空きを下回る。
  空きが読めないか閾値が無ければ控えない。
  要る空きの決め方・掃除とinboxへの知らせと、着地の検証の控え（`landing_held` / `landing_resumed`）は[空き容量を確かめる](disk-space.md)
- `load_average`: hostの1分のload average（`getloadavg`）が`supervise --max-load`を超えている。
  等しいときは控えない。
  load averageが読めないときは控えない

衝突の多いファイルの控えはqueue全体ではなく1つのtaskを飛ばすもので、taskのevent（`claim_deferred` / `claim_deferral_ended`）で記録する（[claimを控える（衝突の多いファイル）](claim-defer.md)）。

`--max-load`の既定はCLIの`supervise`が`domain::claim_hold::resolve_max_load`で起動時に1回解決し、解決した値が`claim_held`の`threshold`に出る。
libraryの`SuperviseOptions::new`の既定は無効で、CLIの`supervise`だけが既定を解決して渡す。
`up`が`--max-load`をsupervisorに渡す規則は[up / down](up-down.md)。

## 判定する場所

fill passの中で、放置されたrunのadopt・answerの適用・parkしたrunのresume・triage・sweepの後、`[run.env]`のprogramが無いときの打ち切り（ADR-0049の決定9）の次、claimのloopの前に判定する（`Supervisor::hold_claims`）。
控えるときはそのpassのclaimをしない。
空きslotの有無に関わらず、claimするpassごとに1回判定する。
drainやhandoffの途中（claimしないpass）では判定しない。

`--once`のsupervisorは、runが無く控えているpassで終わる（claimできるtaskが無いのと同じ扱い）。

## claimの間隔

loadの保留が有効なsupervisorは、新しいclaimの間を空ける（[ADR-t1479-1](../../adr/2026-10-04-t1479-1-space-new-claims-while-the-load-hold-is-on.md)）。
1分のload averageはclaimしたばかりのrunのbuildをまだ含まないので、loadの低い瞬間に空いたslotを続けて埋めると、数分後に`--max-load`を大きく越える。

- **判定**: fill passのclaimのloopで、claimできる候補があるとき、claimの前に`domain::claim_spacing`で判定する。
  間隔が効いている（`--max-load`が有効で、`claim_spacing`が0より大きい）なら、queueの最新の`run_claimed`（どのsupervisorのものでも）の時刻 + `claim_spacing`秒より前はclaimせずにloopを抜ける。
  claimした直後のloopの次の周も同じ判定で抜けるので、1つのpassで新しくclaimするのは1本になる。
  間隔が過ぎた後のpassは、claimの前の控えの判定でloadを判定し直し、`--max-load`を越えていれば控える（`claim_held`）。
  queueの記録から測るので、supervisorの起動し直し・execの引き継ぎ・同じqueueの別のsupervisorのclaimで数え直しにならない。
- **対象**: 新しいclaimだけ。parkしたrunのresume・待ちからの戻り・着地・triage・headless jobは間隔を見ない。
- **間を空けないとき**: `--max-load 0`（0以下）と、libraryの`SuperviseOptions::new`の既定（loadの上限が無い）と、`claim_spacing = 0`。
  このときは1つのpassで空いたslotを埋める。
  e2eはすべての`supervise`に`--max-load 0`を渡すので、間隔は効かない。
- **設定**: `dagq.toml`の`[supervisor] claim_spacing`（書式・既定・読み直しは[Run environment](run-environment.md)の`[supervisor]`）。
- **記録**: 間隔のための待ちは`claim_held` / `claim_resumed`にしない（observerと`stats`の控えの数え方に混ぜない）。
  待ち始めたときにlogにinfo（`claim_spaced`）を出す。
  `run_claimed`に載る間隔と待った時間は`domain::measure::ClaimAttributes`のdoc commentが持つ。
  待ちの区間（どのpassで始まりどのpassで終わるか）は`spaced_since`のcommentが持つ。
- **登録と`status`**: supervisorは登録に間隔とその出どころを`parallel`と一緒に書き、`--max-load`（無効ならNULL）を起動時に書く。
  `status`のsupervisorの項目がそれと次にclaimできる時刻を出す（[`status`](commands.md)）。
- `--once`のsupervisorは、runが無く次のclaimが間隔を待つpassで終わる（控えと同じ扱い）。

## 着地待ちが空けた軽い枠

reviewとe2eを終えて着地の順番を待つだけのrun（着地待ち）が空けた枠では、repositoryが軽いと決めたchangeのtaskだけをclaimする（[ADR-t1591-1](../../adr/2026-10-04-t1591-1-landing-queue-leaves-room-for-light-changes.md)）。

- **着地待ち**: 着地slotを待つ段階のslotが、e2eが要らないか終わった後に、他のrunの着地中（`integrating`のrunがある）でそのpassの着地を始められなかったとき、`Slot::landing_turn`の印を付ける（見るたびに付け直す）。
  `used_slots()`は着地待ちを含む。
- **判定**: fill passのclaimのloopの各周で、`domain::light_slots::claim_room`が、使っているslotの数・着地待ちの数・`parallel`・戻り待ちの数・`light_changes`が空でないかから、どのtaskもclaimできる（使っているslotの数が`parallel`未満）・軽いtaskだけをclaimできる・claimしない、のどれかを返す。
  軽いtaskだけをclaimできるのは、`light_changes`が空でなく、戻り待ちのrunが無く、`used_slots()`から着地待ち（`parallel`件まで）を引いた数が`parallel`未満のとき。
  軽いtaskだけの周は、claimの順から軽いtask（changeが`light_changes`の1つで、`--paths`を宣言している）だけを残し、同じ順でclaimする。
  interrupt・urgentでも軽くないtaskは残さない。
- **関門**: claimの控え（`hold_claims`のloadとディスク）はloopの前に判定するので、控えているpassは軽い枠でもclaimしない。
  claimの間隔（上の「claimの間隔」）は軽い枠の周にも同じに効き（`domain::light_slots::gated`）、軽い枠のclaimも`run_claimed`を記録するので次の間隔を始める。
- **戻る規則**: parkしたrunのresume（着地に失敗して`needs_session`に戻ったrunを含む）と待ちからの戻り・triage・adoptは`used_slots()`が`parallel`未満のときだけ行い、軽い枠を使わない。
- **記録**: 軽い枠のclaimは`run_claimed`にそう印を付ける（`domain::measure::ClaimAttributes`）。
  軽い枠の周で取れない候補は`light_room`の`claim_deferred`で控え、agingは軽いtaskだけに当てる（[実行区分の上限とaging](claim-defer.md#実行区分の上限とaging)）。
  着地待ちに出入りするeventは無い。
- **設定**: `dagq.toml`の`[supervisor] light_changes`（読み直しと記録は[Run environment](run-environment.md)の`[supervisor]`）。
  flagは無く、全てのflagを与えたsupervisorも読む。
- **見え方**: `status`のsupervisorとrunの項目に着地待ちが出る（[`status`](commands.md)）。
  `stats`の空きslotと`kpi`のslotの使用率は軽い枠を数えず、着地待ちを含めて数える。

## 調査中の計画の上限

Spikeがclaimされたgoal（goalの無いSpikeはそれ自体）は、未完了のSpikeか`open`のSpike起点の依頼が残るあいだ調査中の計画で、`[supervisor] max_investigations`がその同時の数を絞る（[ADR-t1487-1](../../adr/2026-10-04-t1487-1-spike-is-an-execution-class-with-a-judged-result-and-a-durable-replanning-request.md)決定7、設定は[Run environment](run-environment.md)の`[supervisor]`）。
数え方・猶予・判定の順・控えの理由は`domain::investigation`のdoc commentが持つ。
控えの欄は`domain::claim_order`のdoc commentが持つ。
コードから読めない約束は次のとおり。

- 控えるのは新しい調査を始めるSpikeのclaimだけで、submitは拒まず、plannerの枠も使わない（待つのはreadyのSpikeで、plannerは結果を待たずに終わる）。
- Spikeの上限（`max_spikes`）が先に効き、両方が効くあいだの控えはSpikeの上限の理由で、Spikeの上限が外れたら調査中の計画の上限の理由に替わる。
- どちらの上限で控えても、空いた枠は実装のtaskが取る。
- claimのトランザクションがqueueの今の姿で数え直すので、別のsupervisorのclaimや順の外のtaskでも上限を超えない。
- 控えは`claim_deferred`として記録し、`candidates`と`graph`は記録から同じ候補を除くので、表示と実際のclaimが一致する。

## 記録

判定がqueueの前回の記録と変わったときだけ、queueイベント（task・goal・runを持たない）を1件書く（`domain::claim_hold::transition`）。
前回はqueueの最新の`claim_held` / `claim_resumed`で、最新の`claim_held`は、書いたsupervisorがこのsupervisor自身か、今動いている（登録があり、heartbeatがstaleでない）間だけ控えが続いているとみなす。
loadはhostのものなので、同じqueueに2つのsupervisorが居ても交互に書き直さない。
控えていたsupervisorが止まった（`down --wait`の後の`up`など）か死んだ後に、loadが高いまま起動したsupervisorは自分のtokenで`claim_held`を書き直すので、`status`と`stats`は控えを出し続ける。
loadが下がっていれば、残った`claim_held`を`claim_resumed`で終える。

- `claim_held`: 控え始めたとき、または別の理由で控え直したとき
- `claim_resumed`: 控えが終わったとき

2つのeventはclaimの前の待ちの区間も兼ねる（[計測](../measurement.md)の「claimの前」）。
区間の行は`claim_held`を書いたsupervisorに属し、下の`stats`と同じ規則（`domain::claim_hold::hold_spans`）で終わり、終わりを書いたsupervisorを残す。
supervisorのslotが全部埋まった区間は、そのsupervisorだけが書く`slots_full_started` / `slots_full_ended`が持つ。

supervisorのlogにも`claim_held`はwarn、`claim_resumed`はinfoで出る。

## `status`と`stats`

- `status`: 最新の`claim_held` / `claim_resumed`が`claim_held`なら、そのsupervisorの登録の項目に控えを付ける（[`status`](commands.md)）。
  着地の検証の控えも同じ形で付ける
- `stats`: windowの中で始まった控えと今の控えを出す（数え方と終わり方は`domain::claim_hold::claim_holds`）。
  着地の検証の控えも同じ形で出す。
  控えている間に空きslotがあれば、空きslotのalertの代わりにalert `claim_held`を出すので、控えによる空きと依存の詰まりによる空きを区別できる（[`stats`](commands.md)）
- observerは`stats`を入力に読むので、控えとalert `claim_held`もそのまま載る

## taskのhold

taskを1つずつ待たせるhold（[ADR-t1879-1](../../adr/2026-10-07-t1879-1-hold-a-task-at-the-turn-boundary-and-release-it-explicitly.md)、`dagq hold` / `dagq release`）は、この控えと別の層で重なる。
holdの開いたtaskは、fill passの1つの列（resume・復旧job・claim）に入る前に外れ、外したことをtaskの`hold_deferred`にholdと工程（`claim` / `resume` / `recover`）ごとに1回残す（`supervise::task_hold`）。
readyのtaskはready queryがholdを除くので、`candidates`にも出ない。
列を作った後にかかったholdも、claim・resume・triageの開始がそれぞれのトランザクションの中でholdを読み直すので、その開始は起きない。
queue全体の控えではないので`claim_held` / `claim_resumed`を書かず、`stats`の控えにも数えない。
走っているrunのturnにholdが効くこと、`status`とattentionへの表示、解除の続け方の選択は予定で、[taskのhold](../../plans/task-hold.md)が持つ。

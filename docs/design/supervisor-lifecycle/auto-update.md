---
id: design-supervisor-lifecycle-auto-update
type: design
title: "Auto-update"
status: current
created: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-handoff
  - design-supervisor-lifecycle-install
  - design-supervisor-lifecycle-up-down
  - adr-0045
  - adr-0073
  - adr-t632-1
  - adr-t963-1
  - adr-t2125-1
  - adr-t1165-1
  - adr-t1233-2
  - adr-t1582-1
  - adr-t1632-1
  - design-persistence
---

# Auto-update

[ADR-0045](../../adr/0045-build-identifier-explicit-migrate-schema-compat-handoff-and-auto-update.md)の決定13・17。`up --auto-update`のsupervisorは、runtimeを変えるcommitがmainに着地するたびに、queueのdirの下でそのcommitをビルドし、同じcheckoutで全部のe2eを流して通ったときだけ（[ADR-t963-1](../../adr/2026-09-29-t963-1-e2e-required-by-diff-and-run-in-full-before-auto-update.md)決定1）、[`install`](install.md#install)と同じ手順で固定バイナリを置き換えて自分を[引き継ぎ](handoff.md#handoff)で入れ替える。走っているsessionは止まらない。supervisor側は`src/application/supervise/update.rs`、ビルドから見張りまでのjobは`src/application/update.rs`（`run`）、CLIは隠しコマンド`dagq auto-update`、配線は`src/compose/host.rs`の`OneShot::auto_update`。

- **有効にする**: `up --auto-update`は、起動するsupervisorの登録の`auto_update`を1にし（launchdの再起動でも同じ引数なので残る）、reuseや引き継ぎで残すliveな登録には`set_auto_update`で1を書く。
  `--auto-update`の無い`up`は同じ登録の`auto_update`を0にする。
  execの引き継ぎは登録の行を変えないので、設定は残る。
  設定ファイルではなく`up`のフラグにした理由はADR-0045のAlternativesが持つ。
- **きっかけ**（`auto_update_pass`）: supervisorはループの各passで、前に見てから見直しの間隔が経っていて、自分の登録の`auto_update`が1なら、mainを見る。
  停止要求の後、claimを止めた後、引き継ぎの要求を受けた後は見ない。
  1. まず`update_failed`のaskの答え（下記）を適用する。
  2. 自分が起動したjobが生きているか、jobが書いた最新の段（askの答えの段は飛ばす。`JOB_STEPS`）でそのjobのpidが生きていれば、何もしない（同時に1つ。execの後の新しいプロセスもDBの行で判定する）。
     execの前に起動したjobはexec後のプロセスの子のままで他に回収する者がいないので、そのpidを回収してから生死を見る（`ProcessControl::reap`）。
     その行のjobが死んでいれば（kill、再起動）、どこまで進んだか分からないので自分ではやり直さず、中断の段の`update_failed`を書いて`update_failed`のaskを開く。
  3. mainのheadと、比べる元（`base`）を決める。
     `base`は最新の`update_started`のcommit、無ければ自分のbuild識別子が名乗るcommit、それも無ければ（リリースのビルド）このプロセスが最初に見たmain。
     headが`base`と同じなら何もしない。
  4. `base`からheadまでの差分がruntimeのpath（`RUNTIME_PATHS`）を変えていれば、headのjobを起動する。
     変えていなければそのheadを覚えて見直さず、何もビルドしないことをheadごとに1回logに出す（`log_nothing_built`）。
     `base`をGitが知らない（別のcloneのビルド）ときは、headを1回ビルドして、それが次の`base`になる。
     `retry`の答えが最新の`update_started`より新しければ、差分を見ずにheadをビルドする。
  - supervisorの着地でも、人が手で打った`integrate`でも、mainが進めば同じように拾う。
    ビルド中に次の着地があっても積まず、jobが終わった後の最初の見直しで、そのときのheadを1回ビルドする。
- **job**（隠しコマンド`dagq auto-update`。引数は`--help`が持つ）: supervisorが自分のbinary（`current_exe`）で起動する。
  setsidで新しいsessionとprocess groupに入れる（supervisorのexecにも、登録済みのin-cmuxのsupervisorのworkspaceが閉じられることにも巻き込まれないため）。
  結果・stderr・ビルドの出力・tracingのlogは`<queue dir>/logs/`の下に置き、古いものは消える（[Logs](commands.md#log)）。
  起動したら、supervisorが`update_started`を書く。
  1. **ビルド**: `<queue dir>/update/checkout`をheadに合わせ、Rustのprofileのbuildの出力（`<queue dir>/update/`の下のdirとそれを指すenv。`UpdatePaths::under`）でreleaseのビルドを走らせる。
     人のmainのcheckoutと`target/`は使わない。
     成功すれば`update_built`。
     - **`update/target`の排他**: jobはcheckoutの前にqueueの`update/lock`を待って取り（`update::hold_target`）、jobが終わるまで持つ（e2eの関門は同じtargetを使い、入れ替えるbinaryもそこにある）。
       release更新のjobと`install --release`も同じlockを取るので、queueの中で`update/target`を使うのは1つだけ。
       持つ間は空き容量の掃除が`update/target`を消さず、掃除が消す間はjobが待つ（[空き容量を確かめる](disk-space.md)の「queueのdirのビルドのキャッシュ」）。
       lockを取れなければ（errorのとき）buildの段の`update_failed`。
     - **e2eの関門**（ADR-t963-1決定1、`e2e_gate::run`）: 続けて同じcheckoutで、ビルドと同じ`CARGO_TARGET_DIR`のまま全部のe2eを流す。
       envはjobのprocessのenvを引き継がず、許す名前（資格情報を除く）だけにmain checkoutの`dagq.toml`の`[run.env]`（[Run environment](run-environment.md)）と1回ごとの`TMPDIR`を重ね、logの冒頭に渡した名前だけを書く。
       許す名前・資格情報を除く規則・足りないときの見え方と戻し方は、runのe2eと同じで`infrastructure::e2e_gate`のmodule docと`PASSED_ENV`が持つ。
       流す前に、hostで1本ずつ流すためのlock（[ADR-t1233-2](../../adr/2026-10-02-t1233-2-e2e-runs-on-the-host-after-review-passes.md)決定4、`install::e2e_lock_path`）を取り、流し直しが終わるまで持つ。
       runtimeがreviewのpassの後に流すrunのe2e（[着地の前のe2e](integrate.md#着地の前のe2e)）と`install`の関門も同じlockを取るので、先に取ったものが流し、後のものは空くまで待つ。
       出力はbuild logに並ぶe2eのlogで、上限（`install::E2E_TIMEOUT`）を超えたらprocess groupを止めて落ちた扱いにする（`stop_group`）。
       落ちたtestはlogの`test <name> ... FAILED`の行から読む。
       hostの道具の有無で外すe2eは無く、全部のe2eで判定する（[ADR-t2159-1](../../adr/2026-10-09-t2159-1-dagq-does-not-use-cmux-and-the-person-opens-the-inbox.md)決定7）。
     - **期間限定で外したケース**（[ADR-t1582-1](../../adr/2026-10-04-t1582-1-temporarily-leave-broker-and-cmux-only-e2e-cases-out.md)）: 今は関門の「全部のe2e」から外したケースは無い。
       外したケースは登録に無いので`update_e2e_passed`には出ない。
       一覧と規則は[testの制約](../../development/testing.md#e2e)。
     - **後始末**: 通っても落ちても、そのe2eの`TMPDIR`を使い捨ての場所として片付ける。
       command lineの引数にその`TMPDIR`の中のpathを持つprocess（e2eが起動したsupervisorやstubのagent。時間切れで止めたときに残る）をSIGTERM（待って止まらなければSIGKILL）で止める。
       続けて`TMPDIR`を消す。
       processは`ps`で列挙して探し、cmuxは呼ばない（[ADR-t2159-1](../../adr/2026-10-09-t2159-1-dagq-does-not-use-cmux-and-the-person-opens-the-inbox.md)決定7）。
       - 消せなければ`TMPDIR`を残す。
         次の関門が始まる前に`<queue dir>/update/e2e`の下の残りを同じ手順で片付ける（並んで走る関門のdirは片付けない）。
         e2eが終わった後もそのprocess groupに残ったもの（stubの`sleep`など）はSIGKILLで止める。
         何をしたかは`cleanup`に残す（欄の意味は`e2e_gate::clean_up`）。
     - **流し直し**（[ADR-t1165-1](../../adr/2026-09-30-t1165-1-e2e-gate-reruns-failed-e2e-once-and-records-quarantined-failures.md)決定1、`e2e_gate::run`）: e2eが上限の内に落ち、logに落ちたtestとlibtestのまとめの行があれば、落ちたtestだけを同じcheckout・env・targetで名前で絞って1回だけ流し直す。
       まとめの行が無い（途中でexitした）e2eは結果の無いtestを残すので流し直さない。
       流し直しは1回目と別の同じ長さの上限と、自分の`TMPDIR`・後始末・logを持つ。
       流し直しで通ったと出たtestだけを通ったとし、それをflakyとする。
       それ以外（落ちた・結果の出ないtest、上限切れと始められないときは流し直した全部）は流し直しでも落ちたtestとし（`E2eRerun`）、流し直しの上限切れと始められないときは印に依らず関門は落ちる。
     - **印**（決定2・3、`domain::e2e_quarantine`）: 関門が流すcheckoutの`.config/e2e-quarantine.toml`（無ければ印は無い）を読む。
       走っている旧バイナリが読む`dagq.toml`には置かない。
       書式と例は`domain::e2e_quarantine`のmoduleのdoc comment、commitの時点の検査は`scripts/check-e2e-quarantine.sh`（CIが実行し、印を変えるtaskのverifyに付ける）の冒頭のcommentが持つ。
       期限切れの印は関門が効かせないだけなので、検査は警告だけで通す。
       印の付け外しの規則は[testの制約](../../development/testing.md#e2eの印)が持つ。
       流し直しでも落ちたtestが全て効いている印を持てば関門は通り、そのtestは印で通した（quarantined）とする。
       印の無いtestが1つでも流し直しで落ちれば落ちる。
       効かない印は、期限を過ぎた印、印が上限を超えるファイルの全部の印、印の付いたtestが今回を含めて決まった回数続けて流し直しでも落ちた印で、読めないファイルでは印を1つも効かせない（`e2e_quarantine`のdoc comment）。
       続けての失敗は、queueの関門のeventを新しい方から見て数え、runtimeがrunに流すe2eの結果も関門として合わせて数える（ADR-t1233-2決定5、数え方は`e2e_quarantine::failures_in_a_row`のdoc comment）。
       印の有無に依らず数えるので、関門を落とした後もそのtestが流し直しで通るまで落ち続ける。
       判定は`application::e2e_verdict::judge`で、自動更新のjobと`install`と、runtimeがrunに流すe2eが共有する（runのe2eが印を読むcommitと外す印は[着地の前のe2e](integrate.md#着地の前のe2e)）。
       落ちたときの`error`と`update_failed`のaskの問いには、落ちたtest、流し直しでも落ちたtest、flaky・quarantinedのtest、印が効かなかったtestとその理由とファイルの理由、1回目と流し直しのlogのpathが載る。
     - 通れば`update_e2e_passed`を書いて2へ進む。
       関門の判定の欄（ADR-t1165-1決定4）は`update_e2e_passed`とe2eの段の`update_failed`の両方に載り、欄の意味は`e2e_verdict::E2eVerdict`の`fields`のdoc commentが持つ。
       `stats`はこれをtestごとに数え（下の`stats`）、observerは`flaky_test`のfindingにする（[Observer](observer.md)）。
       落ちれば（流し直しでも落ちたtestに効いている印の無いものがある、上限切れ、始められない）何も置き換えず、非互換のmigrationのビルドも`staged`に置かず`approve_update`を開かない。
       e2eの段の`update_failed`に落ちたtestかその理由とlogのpathを書き、`update_failed`のask（`retry` / `skip`）を開く。
       新しいaskのkindは作らない。
       同じcommitでは繰り返さず（`base`が進むので、次にruntimeを変える着地のheadで試す）、`retry`の答えは同じheadをもう一度ビルドしてe2eを流す。
       [Release update](release-update.md)のjobはリリースのバイナリなので関門を持たない。
  2. **非互換のmigration**: 新しいbinaryに非互換のmigrationがあれば、`install`と同じ確認をして`<queue dir>/update/staged/dagq`にcopyし、`approve_update`のask（下記）を開いて`update_awaiting_approval`で終わる。
     何も置き換えない。
  3. **入れ替え**: それ以外は`install::install`（`Source::Binary`、drainはしない）を呼ぶ: 確認、互換のmigration、renameでの差し替え（`.previous`を残す）、このqueueのliveなsupervisorへの引き継ぎの要求と、同じpidとtokenで戻るまでの待ち。
     tokenの登録が消えても、同じpidが引き継ぎを頼んだ後に新しいbuild識別子で登録し直していれば、その新しいtokenで戻ったとする（`lifecycle::successor`。pidが再利用されて残った古い行は後継とみなさない）。
     execしたプロセスが死ねばheartbeatが止まった時点で失敗。
     停止要求でdrainに入ったsupervisorは上限を待たずに失敗にし、要求を取り消す（[Handoff](handoff.md#handoff)）。
     上限の直後にsupervisorがexec直前に要求を受け取れば取り消しが要求を見つけないので、`lifecycle::HANDOFF_GRACE`の猶予のあいだ見直し、新しいbuild識別子で戻れば引き継いだとして4で見張る（[`up` / `down`](up-down.md)の引き継ぎ）。
     引き継ぎは最初の失敗で止まらず全員の結果を待つ。
     全員が失敗すれば`install`がfileを戻して`install::HandoffFailed`を返すので、jobは引き継がせたsupervisorのそれぞれを5で戻す（stageは`install`）。
     一部だけが失敗すれば`install`はfileを残して`install::KeptBinary`を返すので（[ADR-t632-1](../../adr/2026-09-27-t632-1-handoff-restores-only-when-every-supervisor-failed.md)）、jobは引き継いだsupervisorを4で見張り、引き継がなかったsupervisorをその理由で失敗にして4の一部の失敗と同じく扱う（stageは`handoff`）。
  4. **見張り**（決定13）: 3の`install`の結果から4・5と6の失敗の記録までは`settle`（`src/application/update.rs`）で、人の`dagq install`の見張り（[`install`](install.md#install)の7、`install_watched`）と共有する（人の`install`では起動したsupervisorが無く、jobの段の`update_restored`を書かず、`update_failed`は人の`install`のものと分かる形で書く）。
     引き継ぎの後、`install`が引き継がせたsupervisor（空ならjobを起動したsupervisor）のすべてについて、同じtokenの登録が新しいbuild識別子のまま、取り戻したときより後のheartbeatを見張りの上限の内に書くのを待つ。
     tokenの登録が消えても、同じpidが`install`を呼ぶ前の時刻以降に新しいbuild識別子で登録し直していれば（照合は3と同じ`lifecycle::successor`）、その登録を追い、進めば成功とする。
     PIDが死んだ、登録が消えて同じpidの登録も無い、build識別子が違う、heartbeatが進まない、のどれかでそのsupervisorを失敗にし、他のsupervisorは最後まで見張る。
     fileは全員で1つなので、全員が失敗したときだけ、`.previous`が置き換えた前のbuild識別子と同じなら`restore`で戻す（違えば戻さず、その旨をaskに書く）。
     一部だけが失敗したときは、新しいbinaryで動いているsupervisorがあるので戻さず、失敗したsupervisorだけを5で戻し（止まったものは置いたままの新しいbinaryで起動し直し、execに失敗して前のbinaryのまま動くものはそのまま）、`update_failed`のaskで人に判断を渡す（[ADR-t632-1](../../adr/2026-09-27-t632-1-handoff-restores-only-when-every-supervisor-failed.md)。askは前のbinaryのまま動くsupervisorを`down --force`と`up`で揃えるか、`install --rollback`で全員を戻すかを案内する）。
     戻したら、supervisorを戻した（5）後に`update_restored`を書く（書けなくても続ける）。
     `update_restored`はjobの途中の段（`JOB_STEPS`）で、jobが死んで最新の段のまま残れば2のinterruptedになる。
  5. **supervisorを戻す**（`bring_back`）: 3の引き継ぎより前の失敗（差し替えの失敗など、supervisorごとの結果が無いもの）の後はjobを起動したsupervisorだけを、3の全員の引き継ぎの失敗と4の失敗の後は失敗したsupervisorのそれぞれを戻す。
     全員の引き継ぎの失敗で、jobを起動したsupervisorが引き継がせた中に居なければ（`install`が見たときliveでなかった）、それも戻す。
     戻すのは見張りが追った今の登録（同じpidが登録し直した新しいtokenの登録なら、modeとworkspaceは入れ替え前の登録のもの）で行う。
     その登録が前のbuild識別子のままheartbeatしていれば（execが失敗して前のbinaryが続けた）何もしない。
     PIDが生きていてheartbeatしていなければSIGTERM（止まらなければSIGKILL）で止め、止めたか死んでいた登録を起動し直す（`application::update::restart`、source buildのjobもリリースのjobも同じ）。
     launchd modeの登録は`KeepAlive`が戻したbinaryで起動し直すので任せ、登録済みのin-cmuxのsupervisorは記録したworkspaceを閉じず（人が閉じる）、戻したbinaryの`up`でlaunchd modeとして起動する（`--auto-update`はその登録が`auto_update`を持つときだけ付ける）。
     この`up`が失敗したときは`update_failed`のaskと`supervisor_stopped`で人に知らせ、人かinboxが`down --wait`の後に`--in-cmux`を付けない`up`を打つ。
     手で起動した（modeの無い）supervisorは起動し直さない。
  6. 成功すれば`update_installed`を書く。
     失敗すれば、失敗した段と理由、binaryを戻したか残したか、supervisorごとの引き継ぎと戻した結果を載せた`update_failed`を書き、`update_failed`のaskを開く。
     欄の意味は`application::update`の`put_in_place`と`failed`のdoc commentが持つ。
- **経過の記録**（[ADR-0073](../../adr/0073-kind-additions-are-compatible.md)の決定17・19〜22）: 上の各段は、task・goal・runのどれにも属さないqueueの`run_events`（kindの一覧は`domain::UPDATE_EVENT_KINDS`）で、payloadの`commit`が対象のmainのcommit（askの答えには無い）。
  `application::update::record`が書き、supervisorと`status`は`update_events`（新しい順）で読む。
  kindを足してもmigrationは要らない。
  表`binary_updates`は残すが、書きも読みもしない。
  すぐ終わるjobは`update_started`より先に終わりの段を書きうるので、`update_events`はjobの段の順に並べ直して返す（どの段をどのjobのものと見るかは`domain::in_job_order`のdoc comment）。
  - `events`で経過を読める。
    `update_installed`はinbox宛てのattention（人に入れ替えを知らせるだけ）で、roleなしの`watch`は起きるが、inboxの`watch`は単独では起きず、inboxが次に別の件で起きたときにその`events`に古い順に載る（[ADR-t1418-1](../../adr/2026-10-03-t1418-1-quiet-notices-do-not-wake-the-inbox-watch.md)、[Events and watch](commands.md)）。
    `update_failed`と`update_awaiting_approval`は下のaskの`ask_opened`がattentionになるので、それ自体はattentionにしない。
  - `timeline RUN`はrunの最初のeventから最後のevent（進行中なら今）までの`update_*`を、runのeventの間にidの順で並べる（引き継ぎがsupervisorの間を説明する）。
    gapはrun自身のeventだけから求める。
  - `stats`の`updates`は`asks`と同じ窓の`update_*`を数え、欄と数え方の意味は`domain::stats::updates`のdoc commentが持つ。
    pluginだけのjobの`update_installed`はバイナリを入れ替えていないので、バイナリの入れ替えとは分けて数える。
    人の`dagq install`の失敗（[`install`](install.md#install)の7）はjobのものと分けて数える。
    関門のtestごとの続けての失敗は関門の印と同じ規則（`e2e_quarantine::failures_in_a_row`）で数える。
- **inboxに知らせる**: 失敗と非互換のビルドは、task・runの無い専用のkindのask（`update_failed` / `approve_update`）で知らせる（askは`ask_opened`を書くのでinboxの`watch`が起きる）。
  同じkindの開いたaskは古いビルドのものなので、新しいaskを開く前に`superseded`と答えて閉じる（`open_update_ask`）。
  ただし人の`dagq install`の失敗のask（[`install`](install.md#install)の7）とjobのaskは互いに閉じ合わず、それぞれ同じ側の古いものだけを閉じる。
  jobの入れ替えが`update_installed`を書いた後、入れたcommitが失敗のcommitを含むjobの開いた`update_failed`のaskはruntimeが答えて閉じる（`update::close_settled_failures`）。
  commitの分からない失敗（リリースのjob・人の`install`）と含まない・答えのあるaskは閉じず、リリースと人の`install`の入れ替えは何も閉じない。
  閉じる失敗は警告のlogだけで、入れ替えは成功のまま。
  - `update_failed`（options `retry` / `skip`）: 何が失敗し、binaryとsupervisorがどうなったかとlogを問う。
    答えはsupervisorが適用する。
    `answer`が答えをruntimeが適用するものと記録し、`status`を適用中にするのは、自動更新を持つliveな登録が居るときだけで、判定は`SupervisorRegistration::applies_updates`の1つ（リリースのjobの失敗の`update_failed`は[Release update](release-update.md)の`applies_releases`で決める）。
    居なければ人がinboxで読む答えとして残る。
    `retry`は`update_retry`を書いて次の見直しでheadをビルドし直し、`skip`は`update_answered`を書いて次のruntimeの着地を待つ。
    どちらもaskを閉じる。
    リリースのjobの失敗が開いた`update_failed`の答えはリリースのpassが適用するので、このpassは飛ばす（`application::update::failed_release`）。
    リリースのjobの段は`base_commit`と`retry_requested`も数えない。
    人の`dagq install`の失敗が開いた`update_failed`（`update::step_install`）の答えはどのpassも適用せず、自動更新を持つliveな登録が居ても`answer`はruntimeが適用するものと記録しないので、inboxが読んで閉じる（`retry`は人が`install`を打ち直すことで、ビルドを起こさない。[`install`](install.md#install)の7）。
  - `approve_update`（options `install` / `skip`）: 非互換のmigrationを含むビルドのversionとmigrationを示す。
    drainはsupervisor自身を止めるので、runtimeは適用しない。
    `install`なら人がinboxから問いに書かれた`dagq install`のコマンドを打つ（[`install`](install.md#install)の6）。
    drainが起動し直すsupervisorへの値の引き継ぎは`install_breaking`のdoc commentが持つ。
    どちらもinboxが閉じる。
- **待ちの見直し**: 引き継いだsupervisorは、execした新しいprocessの最初のpassで、`wait_for_build`のtaskの待ち（[claimを控える](claim-defer.md#依存先の着地を含むbuildを待つtask)、ADR-t1632-1）を自分の新しいbuild識別子で判定し直し、依存先の着地を含めばclaimする（含むかの答えはprocessの中だけでcacheするので、引き継ぎの後は空から読む）。
  `update_installed`そのものは待ちを変えない: 待ちを決めるのは固定バイナリのfileではなくsupervisorの走っているbuildで、引き継いだsupervisorのbuildはそのfileと同じになる。
- **queue serviceとqueueのbroker**: 3の引き継ぎはどちらも止めず、引き継いだsupervisorの最初の見張りが前のbuildのものを1回だけ入れ替える（[Queue service](../queue-service.md#起動と停止adr-t1233-4決定12)・[Queue broker](../queue-broker.md#起動と停止)）。
- **status**: `status`は打った`dagq`のbuild識別子と、自動更新（liveな登録のどれかが有効か、最新の段）と、登録ごとの設定と今のbuild識別子を出す。
  `state`の値の意味と、人の`dagq install`の`update_failed`を数えないことは`application::update::status`のdoc commentが持つ。
- **test**: 判断（askのkind・attention・`stats`の`updates`・印の読み取りと関門の判定・`status`の段）はunit test、関門の実行と後始末、jobの各分岐、supervisorのきっかけと実プロセスの入れ替えはintegration test、作業中のrunの入れ替えと着地はe2eが確かめる。

## dagqのソースのrepositoryだけ

リリースのバイナリ（`X.Y.Z`）で動く外部のprojectの更新は、source buildではなくcrates.ioのリリースの検知とaskで行う（[Release update](release-update.md)、ADR-t618-1）。
jobの確認から見張りまでは同じ`put_in_place`（`src/application/update.rs`）を通る。

`up --auto-update`のsource buildは、queueのrepositoryがdagqのソースのときだけ動く（[ADR-t614-1](../../adr/2026-09-27-t614-1-dagq-source-only-features-by-one-check.md)）。
ソースでないrepositoryでは`up --auto-update`がerrorで止まり（`lifecycle::auto_update_refused`）、自動更新の設定を持つsupervisorもbuildに進まない。
判定と対象の一覧は[Source repository](source-repository.md)にある。
登録の`auto_update`が真のsupervisorは、見る回ごとに判定し、ソースでなければ`update_failed`のaskの答えの適用もjobの起動もせずに戻り、ソースでなくなった最初の回だけ警告をlogに出す。
続けてそのときのmainのheadを読み、何もビルドしないことをheadごとに1回出す。
source buildのbuildの出力のdirとenvは`[toolchains]`の宣言に依らずRustのprofileから取り、source buildを動かすかはこの判定のまま決める（[ADR-t2268-1](../../adr/2026-10-12-t2268-1-toolchain-knowledge-behind-declared-profiles.md)決定2）。

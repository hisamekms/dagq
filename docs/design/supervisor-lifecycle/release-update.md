---
id: design-supervisor-lifecycle-release-update
type: design
title: "Release update"
status: current
created: 2026-09-27
updated: 2026-09-30
last_verified: 2026-09-30
scope: runtime
related:
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-install
  - design-supervisor-lifecycle-auto-update
  - design-supervisor-lifecycle-handoff
  - design-supervisor-lifecycle-push
  - design-plugin-integration
  - adr-t618-1
  - adr-t618-2
---

# Release update

外部のprojectで`cargo install`したdagqを、crates.ioの新しいリリースへ、走っているrunを止めずに入れ替える仕組み（[ADR-t618-1](../../adr/2026-09-27-t618-1-release-update-by-ask-from-crates-io.md)、pluginとの同期は[ADR-t618-2](../../adr/2026-09-27-t618-2-plugin-follows-the-release-update.md)）。dagqのソースのrepositoryの`-dev`のビルドは[Auto-update](auto-update.md)が受け持ち、この仕組みは動かない。

## 対象

- supervisorの`binary_version`（自分のbuild識別子）がpre-releaseもbuild metadataも無い`X.Y.Z`のときだけ動く。`X.Y.Z-dev+…`（`+unknown`・`.dirty`を含む）では調べない。
- ADR-t614-1の「dagqのソースか」の判定は使わない。
- 判定は`domain::release_update::is_release_build`（`-`も`+`も含まず、数字3つの`X.Y.Z`）。supervisorが自分のbuild識別子として使うのは`crate::VERSION`で、testは`SuperviseOptions::release_current`で差し替える。

## 設定（host.tomlの`[update]`）

- 置き場所と優先は[push](push.md)の`[push]`と同じ: `<queue dir>/host.toml`の`[update]`が表ごと優先し、無ければ`$XDG_CONFIG_HOME/dagq/host.toml`（無ければ`~/.config/dagq/host.toml`）。`dagq.toml`からは読まない。
- `release = "ask" | "auto" | "off"`。既定は`"ask"`。`"auto"`はaskを開かずに入れ替える（非互換のmigrationは除く）。`"off"`は調べない。
- `check_interval_secs`（既定86400）は検知の間隔。調べるかどうかは`release`だけが決め、間隔で切る手段は置かない。
- 形の違う値（`release`が3つの文字列のどれでもない、`check_interval_secs`が正の整数でない、知らないkey、`KEY = value`の形でない行、2つ目の`[update]`）は、その値だけを既定として扱い、表のほかの値は効く。`[push]`と違ってerrorにしないので、書き損じで検知が止まることはない。読めないファイル（権限など）も警告にして次のファイルを読む。
- `doctor`の`release_update`に、効いている`release`・`check_interval_secs`、表を読んだファイル（`source`、無ければnull）、既定にした値の`warnings`を出す（`infrastructure::release_update::load_host_update`）。
- supervisorは設定を検知の見直し（下の`RELEASE_LOOK`ごと）のたびに読み、登録にもflagにも持たない。execの引き継ぎや`up`の打ち直しで設定は変わらない。

## 検知

- 読むのはsparse indexの`https://index.crates.io/da/gq/dagq`（4文字以上のcrateの`<先頭2文字>/<次の2文字>/<name>`）。1行が1 versionのJSONで、`vers`と`yanked`を読む。`yanked: true`とpre-release（`-`を含む）を除いた最大のversion（SemVerの順）が最新。
- runtimeにHTTPのclientは足さず、`curl -fsS --max-time 10 -D - [-H 'If-None-Match: <etag>'] <url>`を子プロセスで打つ（macOSに標準である）。`-D -`でheaderをbodyの前にstdoutへ出させ、最後のheaderの塊（`1xx`とproxyの`200 Connection established`の後には次の塊が続くので飛ばす）のstatusと`ETag`を読む。`304`なら前の結果、`2xx`ならbody、それ以外のstatusとcurlの失敗（exitが0でない、見つからない）は読めなかったものとする。前回の`ETag`は最新の`release_checked`の`etag`から取り、`304`なら同じ`release_checked`の`latest`を使い、`not_modified: true`を書く。呼び出しはport（`application::release_update::ReleaseIndex`、hostでは`infrastructure::release_update::CurlIndex`）の後ろにあり、testは`SuperviseOptions::release_index`でstubの`curl`を渡す。
- supervisorは`RELEASE_LOOK`（60秒）ごとと起動の後の最初のpassに、hostの設定を読み、`off`でなければjob threadで調べるかを決める（claimも着地も止めない。loopは待たないが、終わる前とexecの前には待つ）。調べるのは、queueの最新の`release_checked` / `release_check_failed`（どのsupervisorのものでも）が無いか、その`checked_at`（書いたsupervisorの時計のunix秒）から`check_interval_secs`経ったとき。同じqueueの複数のsupervisorはこれで重ねて調べない。加えて、起動の後の最初のpassでは、最新の`release_checked`の`current`が自分のbuild識別子と違えば（入れ替わった直後）間隔の内でも調べる。
- 読んだ行のうちJSONでない行と`vers`の無い行は飛ばし、`vers`を持つ行が1つも無ければ形が違う（`release_check_failed`）。build metadataは順序に使わず、yankedとpre-releaseを除いて1つも残らなければ`latest`はnull。
- 同じjob threadで、supervisorに`--plugin-dir`が無ければinstallしたpluginのversionも読む（下の「pluginのversion」）。読めなければnullで、検知は失敗にしない。
- 結果はqueueのevent: `release_checked`（`latest`・`current`・`plugin`（読めたpluginのversion。`--plugin-dir`のsupervisor、installされていない、読めないときはnull）・`checked_at`・`etag`・`not_modified`・`supervisor`）、`release_check_failed`（`error`・`current`・`checked_at`・`supervisor`）。どちらもattentionにしない。
- `status`の`release_update`に`mode`（hostの設定）・`latest`と`current`（最新の`release_checked`の）・`checked_at`（最新の検知の`checked_at`）・`state`を出し、`state`が`check_failed`なら`error`も出す。`state`は`off`（`release = "off"`）・`not_release`（liveなsupervisorの`binary_version`がどれもリリースでない。liveなsupervisorが居なければ`status`を打ったバイナリで決める）・`unchecked`・`check_failed`（最新の検知が失敗）・`update_available`（`latest > current`）・`up_to_date`。

## ask

supervisor側は`src/application/supervise/release.rs`の`release_update_pass`で、決め方は`application::release_update::next_action`。supervisorはループの各passで、前に見てから`--update-interval`（既定30秒。testは0）経っていれば、自分のbuild識別子がリリースで、hostの設定の`release`が`off`でないときだけ、次を順に行う（停止要求の後とclaimを止めた後は見ない）。`-dev`のビルドのsupervisorは`approve_release`の答えもリリースのjobの`update_failed`の答えも適用しない。

1. **答えを適用する**: 答えられて閉じられていない`approve_release`（`install` / `skip`）と、リリースのjobの失敗が開いた`update_failed`（`retry` / `skip`。どのjobの失敗かは、askのidを`ask_id`に持つ`update_failed`のeventの`release`で見分ける。source buildの失敗の答えは[Auto-update](auto-update.md)のpassが適用し、互いに相手のものを飛ばす）を、`update_answered`（`install` / `skip`）か`update_retry`（`retry`）として`ask_id`・`answer`・`source: "release"`・`release`・`plugin_only`・`supervisor`を書いてからaskを閉じる。optionsにない答えは人が読むものとして残す。
   - `plugin_only`（答えがpluginだけについてか）は、askを開いたときに記録した目的で決め、答えを適用するsupervisorのbuildでは決めない（build識別子の違うsupervisorが同じqueueを共有しても分類が変わらない）。目的は、askを開くときに`open_update_ask`の`details`で`ask_opened`のpayloadに`plugin_only`（true / false）として書く: バイナリの`approve_release`は`false`、pluginだけの`approve_release`は`true`、リリースのjobの失敗の`update_failed`は`stage: plugin`（バイナリのjobが入れ替えの後にpluginの更新で失敗したものと、pluginだけのjobの失敗）なら`true`でそれ以外は`false`、途中で死んだjobの`update_failed`はそのjobの`plugin_only`。答えの適用では`ask_opened_payload`でそれを読む（`release_update::answer_plugin_only`）。`ask_opened`に目的の無いask（この記録より前に開いたもの）は、`update_failed`ならその失敗の`update_failed`のevent（`stage: plugin`か`plugin_only`。`update::failure_plugin_only`）で、それも言わなければ`release`が自分のbuild識別子と同じか（バイナリはもうその版なので、pluginについての答え）で決める。
2. **jobは1つ**: 自分が起動したjobが生きている、またはjobの最新の段（[Auto-update](auto-update.md)と同じ`latest_job_step`）が生きているjobのものなら何もしない（source buildのjobとも同時に1つ）。リリースのjobの段だけの最新（`latest_job_step_of`。後にsource buildのjobが走っていても見つける）が途中の段（`update_started` / `update_built` / `update_restored`）でそのpidが死んでいれば、`update_failed`（`stage: interrupted`・`interrupted: true`・`after`・`release`。pluginだけのjobは`stage: plugin`・`plugin_only: true`で、問いもバイナリに触っていないことを書く）を書いて`update_failed`のaskを開く。source buildのpassも同じく自分の段だけを見る。
3. **次の一手**（`next_action`。`updates`は新しい順）:
   - `update_retry`か`install`の`update_answered`のうち、`plugin_only: true`でなく、その後にその版（かより新しい版）の`update_started`もその頼みの`update_dropped`も無く、版が今のbuild識別子より新しいもの（待っている頼み）があれば、その中で最も新しい版のjobを起動する（答えとretryは1回だけjobを起動する。同じpassに別の版の答えが書かれても頼みは消えない）。pluginだけの頼みは、今のbuildがその版より古くてもバイナリのjobを起動しない。
   - それ以外は、最新の`release_checked`の`latest`が今のbuild識別子より新しく、その版の`update_started`も（`plugin_only: true`でない）`update_answered`も無いとき（jobが試した版は、失敗しても`update_failed`のaskが受け持つので改めて聞かない。`skip`した版も同じ）: `release = "auto"`ならaskを開かずにjobを起動し、`"ask"`なら同じ版の閉じられていない`approve_release`が無ければaskを開く。
4. **pluginだけの一手**（`next_plugin_action`、ADR-t618-2決定4）: 3が何もしないとき、supervisorに`--plugin-dir`が無く、最新の`release_checked`の`latest`が今のbuild識別子と同じ（バイナリが最新のリリース）ときだけ見る。marketplaceは最新のリリースのpluginを配るので、バイナリが最新でないうちはpluginだけを上げない（pluginがバイナリより新しくなりうる）。
   - その版についての`update_retry`か`install`の`update_answered`のうち、`plugin_only: false`でなく（`plugin_only`を書かなかった古いruntimeの答えは今までどおり数える）、その後にその版（かより新しい版）の`update_started`もその頼みの`update_dropped`も無いものがあれば、pluginだけのjob（`--plugin-only`）を起動する（読めたpluginのversionは問わない）。バイナリについての頼みは、今のbuildがもうその版でもpluginだけのjobを起動しない。
   - それ以外は、`release_checked`の`plugin`（その`current`が今のbuild識別子のものだけ。execの後の最初の検知が読み直す）がその版より古く、その版についてpluginを試した・決めた記録（`plugin_only: true`の`update_started` / `update_answered`、`plugin`を持つ`update_installed`、`stage: plugin`の`update_failed`）が無いとき: `"auto"`ならaskを開かずにpluginだけのjobを起動し、`"ask"`なら同じ版の閉じられていない`approve_release`が無ければpluginについてのaskを開く。バイナリのjobは入れ替えの後にpluginも上げるので、その`update_installed`（`plugin`を持つ）で同じ版をもう聞かない（jobの途中で読んだ古い`plugin`でaskが開かないため）。
5. **実行しないpluginだけの頼みを残す**（`release_update::dropped_requests`）: 3と4の前に、`plugin_only: true`の`update_retry`か`install`の`update_answered`のうち、その後にその版（かより新しい版）の`update_started`もその頼みの`update_dropped`も無く、その版より新しいリリースが今のbuild識別子か最新の`release_checked`の`latest`であるものは、4が起動しない（pluginだけのjobはバイナリが最新のリリースのときだけ動く）ので、黙って捨てずに`update_dropped`（`source: "release"`・`release`・`ask_id`・`answer`・`request`（`update_answered`か`update_retry`）・`plugin_only: true`・`reason: "newer_release"`（`DROPPED_NEWER_RELEASE`）・`newer`（より新しい版のうち新しい方）・`current`・`supervisor`）を1回書く。新しいリリースのバイナリのjobは入れ替えの後にpluginもその版に上げるので、古い版のpluginだけの更新は要らない。`update_dropped`は`UPDATE_EVENT_KINDS`に入るqueueのeventで、jobの段ではない（`latest_job_step`と`latest_job_step_of`は答えの2つと同じく飛ばす）。`status`の`auto_update`は最新がこれなら`state: dropped`を出す。attentionにしない。

- kindは`approve_release`（`AskKind::ApproveRelease`。task・runを持たず、`subject`にversionを持つ。pluginだけのaskも同じkindで、`subject`はバイナリの今の版）、options`install` / `skip`（`APPROVE_RELEASE_OPTIONS`）、`reason_category`は`scope`、`asked_by`は`supervisor`。`open_update_ask`で開くので、開いている古い版の`approve_release`は`superseded`と答えて閉じてから開く。問いは`dagq <version> is released on crates.io (https://crates.io/crates/dagq/<version>); this queue's supervisor runs <current>. Answer `install` …`で、`cargo install`の置き場、互換のmigrationを適用して引き継ぐこと、`.previous`が残ること、非互換なら`approve_update`で改めて聞くこと、`skip`で次のリリースまで聞かないことを書く。pluginだけのaskの問いは`The claude-dagq plugin installed in Claude Code is <plugin>, older than dagq <version> that this queue's supervisor runs. Answer `install` …`で、2つのコマンド、バイナリに触らないこと、開いているinboxとplannerのsessionは開き直すと新しいpluginを読むこと、`skip`で次のリリースまで聞かないことを書く。
- `answer`の`runtime_delivers`（とstatusの`applying the answer`）: `approve_release`は、liveで（pidが生きていてheartbeatが`HEARTBEAT_TIMEOUT_SECS`以内）、`binary_version`がリリースで、queueの`host.toml`（無ければhost全体の`host.toml`）の`release`が`off`でない登録が居るとき（`SupervisorRegistration::applies_releases`）。`update_failed`は、askのidを`ask_id`に持つ`update_failed`のeventが`source: "release"`ならこの登録（`applies_releases`）、そうでなければ`auto_update`の登録（`applies_updates`）が適用するものとする（`answer`はSQLで、statusは`application::update::failed_release`で見分ける）。hostの設定は`SqliteQueue::release_updates_on`（`infrastructure::release_update::releases_on`）が読む。居なければ人がinboxで読んで`dagq install --release <version>`を打つ。
- `update_answered`と`update_retry`と`update_dropped`はattentionにしない（Auto-updateと同じ）。`approve_release`の`ask_opened`がinboxのattentionになる。

## 入れ替えのjob

`dagq install --release [<version>]`（人の入口。[`install`](install.md)の「リリースから」）と、supervisorが起動する隠しコマンドのjob`dagq --db <db> release-update --release <version> --token <supervisor> --to <supervisorのbinary> --log <cargo log> --claude … --codex … [--cmux …] [--plugin-dir …] [--cargo …] [--plugin-only]`（`application::update::run_release`、配線は`src/compose.rs`の`OneShot::release_update`）。jobはsource buildの`auto-update`のjobと同じくsetsidで起動し、stdoutは`<queue dir>/logs/release-<unix時刻>-<version>.json`、stderrは同じ名前の`.log`、cargoの出力は`.build.log`に書く。起動したら、supervisorが`update_started`（`pid`・`source: "release"`・`release`・`supervisor`・`version`（今のbuild識別子）・logのpath、pluginだけのjobは`plugin_only: true`）を書く。jobのcwdはsupervisorのrepository root。jobの段はどれも`update_*`で、payloadに`commit`の代わりに`source: "release"`と`release`を持つ（`application::update::step_release`）。source buildの自動更新の見直し（`base_commit`・`retry_requested`）はこれらの段を数えない。

1. **入れる**（`install::release_binary`）: 置き換える先（supervisorの`current_exe`、`install`では`--to`）の`--version`がすでにその版なら（hostの別のqueueの答えで入れ替わった）、cargoを打たずにそのファイルを元にする（`install`は置き換えず`.previous`もそのままで、引き継ぎだけを行う）。そうでなければ`cargo install --locked dagq@<version> --root <queue dir>/update/release --target-dir <queue dir>/update/target`（`ReleaseInstaller` port、hostでは`infrastructure::binaries::CargoInstaller`。testはstubの`cargo`を`supervise --update-cargo` / `install --cargo`（隠しflag）で渡す）で入れた`<queue dir>/update/release/bin/dagq`を元にする。`cargo`が見つからない・失敗した・binaryを残さなかったら`update_failed`（`stage: build`、`error`、cargoのlogの`log`）で、何も置き換えない。問いには`retry`（直してからもう1回）、`skip`（その版を飛ばす）と、手で入れ替える方法（`dagq install --release <version>`か、別の方法で入れたbinaryの`dagq install --from <binary>`）を書く。
2. **非互換のmigration**: 元の`migrate --check`に非互換があれば、`<queue dir>/update/staged/dagq`にcopyして`approve_update`のaskを開き、`update_awaiting_approval`で終わる（[Auto-update](auto-update.md)の2と同じ。`mode = "auto"`でも同じ）。問いは`release <version> (installed as <build>), and it brings breaking migration(s) …`で、書くコマンドは`dagq --db <db> install --from <queue dir>/update/staged/dagq --to <binary> --allow-breaking --cmux … --claude … [--plugin-dir …]`。drainが起動し直すsupervisorは止めた登録のflagを引き継ぎ、リリースのsupervisorは`auto_update`を持たないので`--auto-update`は付かない（ソースでないrepositoryでは`up --auto-update`がerrorになる。ADR-t614-1）。起動し直したsupervisorはhost.tomlの設定をそのまま読むので、この仕組みは打ち直しなしで続く。
3. **確認・差し替え・引き継ぎ・見張り・戻し**: [Auto-update](auto-update.md)の3〜6と同じ（`install::install`を`Source::Binary`で呼ぶ。source buildのjobと共通の`put_in_place`）。成功すれば`update_installed`（`version`・`previous_version`・`migrated`・`supervisors`・`source: "release"`・`release`）。失敗は`update_failed`（`stage`・`error`・`restored`など、source buildと同じ欄）。見張りの失敗で止まったin-cmuxのsupervisorを起動し直す`up --in-cmux`は、その登録が`auto_update`を持つときだけ`--auto-update`を付ける（`compose::restarter`。source buildのjobも同じ）。
4. **plugin**（ADR-t618-2決定1〜3）: 3が見張りまで成功し`update_installed`を書く前に、supervisorに`--plugin-dir`が無ければ、supervisorの`--claude`で`claude plugin marketplace update dagq`と`claude plugin update claude-dagq@dagq`を順に打つ（`lifecycle::PLUGIN_UPDATE_ARGUMENTS`。1つ目が失敗すれば2つ目は打たない。1つにつき300秒まで）。`update_installed`の`plugin`は`{"updated": true, "version": <更新の後に読んだversion、読めなければnull>, "commands": [{"command", "output", "succeeded"}]}`。失敗すれば`plugin`は`{"updated": false, "commands": …}`で、`update_installed`を書いた後に`update_failed`（`stage: plugin`・`plugin`・`version`、打ったコマンドと出力はerrorと`plugin.commands`）を書いて`update_failed`のaskを開く。バイナリは戻さない。問いは`The update of the claude-dagq plugin of Claude Code to release <version> failed: …`で、バイナリはそのまま動くこと、`retry`で次のpassにsupervisorが2つのコマンドを打ち直すこと（上の「ask」の4のとおりpluginだけのjob）、`skip`でpluginをそのままにすること、手で打つコマンドと開き直しを書く。`--plugin-dir`があれば打たず、`update_installed`の`plugin`に`"skipped: plugin-dir"`（`update::PLUGIN_SKIPPED`）を書く。3が失敗して戻したとき（build・check・install・watch）と非互換のmigrationで止まったときはpluginに触らない。source buildのjobの`update_installed`は`plugin`を持たない。
5. `update_installed`はinboxのattention（`report the update`）になる（Auto-updateと同じ）。pluginを更新できたときは`message`に、inboxとplannerのsessionは起動したときのpluginのままなので開き直すと新しいpluginが効くこと（headless jobとこれから開くsessionはもう新しいpluginを読む）を書く。`watch`とattentionの1件（`health::compact_event`）は`message`を`reason`に、`release`・`plugin`も写す。runtimeは常駐のsessionを閉じない。
6. **pluginだけのjob**（`--plugin-only`、ADR-t618-2決定4）: cargoも確認も差し替えも引き継ぎもせず、4のコマンドだけを打つ。成功は`update_installed`（`plugin_only: true`・`version`（その版）・`plugin`・`message`）、失敗は`update_failed`（`stage: plugin`・`plugin_only: true`）と4と同じask。バイナリを入れ替えていないので、`stats`の`updates`は成功を`installed`に入れず`plugin_installed`（版を古い順）と`by_kind`の`update_installed_plugin_only`で数え、`status`の`auto_update`は最新がこの`update_installed`なら`state: plugin_installed`を出す（[Auto-update](auto-update.md)の経過の記録とstatus）。supervisorに`--plugin-dir`があれば（起動されないはずだが）`plugin`は`"skipped: plugin-dir"`。

## pluginのversion

- `claude plugin list --json`（ADR-t617-2決定4の[installしたpluginの確認](../plugin-integration.md#installしたpluginの確認)と同じ出力と読み取り`adapters::plugin_entries`）の、`id`の`@`の前が`claude-dagq`のentryのうち、`enabled: true`のもの（無ければ最初のもの）の`version`。installされていないか`version`が無ければnull、出力が読めなければerror。`adapters::plugin_version`。
- portは`application::InstalledPlugin`（`version`と、2つのコマンドを打つ`update`）。hostでは`adapters::ClaudePlugin`（`executable`は`--claude`、`cwd`はsupervisorのmain checkout、jobではそのcwd）。supervisorは`ReleasePort::plugin`（`--plugin-dir`があればNone）、jobは`run_release`の引数で受ける。
- 比べるのは`X.Y.Z`どうしで、`-dev`などのversionは比べられないので「古くない」とする（askを開かない）。

## test

- `src/application/release_update.rs`のunit testが`next_plugin_action`（pluginだけが古ければ聞き`auto`なら起動する、開いたask・古くない・読めない・バイナリが最新でない・`off`・`-dev`では何もしない、バイナリのjobの`plugin`を持つ`update_installed`・`stage: plugin`の失敗・pluginだけのjobと答えで繰り返さない、`plugin`の無い`update_installed`では聞く、その版の`install`と`retry`はpluginだけのjobを1回起動する）と`next_action`（新しい版を1回聞く、開いたaskの版は聞かない、新しい版は古いaskに代わる、`auto`は聞かずに起動する、`-dev`と`off`は何もしない、`install`の答えと`retry`で起動し、同じpassの別の版の答えで頼みが消えず、起動した版と`skip`した版は繰り返さない、source buildの段を数えない）と`resolve_version`を確かめる。`answer_plugin_only`（記録した目的がbuildに優先し、無ければ`release`とbuildの一致）、pluginだけの頼みがバイナリのjobを起動せずバイナリの版を決めないこと・バイナリの頼みがpluginだけのjobを起動しないこと、`dropped_requests`（より新しい版があるpluginだけの頼みを1回だけ`newer`つきで落とし、バイナリの頼み・最新の版の頼み・jobが続いた頼み・`plugin_only`の無い古い答え・`skip`・`-dev`は落とさない）も確かめる。`src/application/update.rs`のunit testは`update_dropped`が`state: dropped`でjobの段でないことと、`failure_plugin_only`を確かめる。
- `src/infrastructure/asks.rs`のunit testが`approve_release`と`update_failed`の答えの`runtime_delivers`とattentionを、リリースの登録・`-dev`の登録・`release = "off"`で確かめ、`open_update_ask`の`details`が`ask_opened`に書かれ`ask_opened_payload`で読めることを確かめる。
- `tests/it/runtime_release.rs`がin-processのsupervisor（stubの`curl`と、失敗するstubの`cargo`）で、新しいリリースで`approve_release`が1件開き、新しい版で古いaskが`superseded`で閉じること、`install`の答えでjobが起動し、`cargo install`の引数と`update_failed`（`stage: build`）、`retry`でもう1回、`skip`でもう起動も質問もしないこと、`skip`した版では開かず次の版で開くこと、`release = "auto"`はaskなしで1回だけ起動すること、`-dev`のビルドのsupervisorは`approve_release`を適用しないこと、途中で死んだリリースのjobは後にsource buildの段があっても`stage: interrupted`で知らせ、自分ではやり直さないことを確かめる。
- `tests/it/lifecycle_install.rs`がfakeのbinaryと`ReleaseInstaller`で、jobの確認・差し替え・引き継ぎ・見張りの後の`update_installed`（`source: release`）、cargoの失敗の`update_failed`（置き換えない）、非互換のmigrationの`approve_update`（コマンドに`up`と`--auto-update`が無い）、置き換える先がすでにその版ならcargoを打たず置き換えもしないことを確かめる。
- `tests/it/lifecycle_install.rs`がfakeの`InstalledPlugin`で、入れ替えの成功の後に2つのコマンドを打ち、`update_installed`の`plugin`と開き直しの`message`（`compact_event`の`reason`）を書くこと、`--plugin-dir`（None）では打たず`skipped: plugin-dir`を書くこと、pluginの更新の失敗は`update_failed`（`stage: plugin`）で`restore`しないこと、見張りの失敗でバイナリを戻したときはpluginに触らないこと、pluginだけのjobはcargoも差し替えもせず`plugin_only`の`update_installed`か`update_failed`を書くことを確かめる。
- `tests/it/runtime_release.rs`がstubの`claude`で、バイナリが最新でpluginだけ古いと`release_checked`の`plugin`にversionを書き、pluginについての`approve_release`が1件開き、`install`の答えで`plugin_only`のjobが起動してsupervisorの`--claude`で2つのコマンドを打ち（cargoは打たない）、その後は聞きも起動もしないこと、`--plugin-dir`のsupervisorはpluginを読まず聞かないことを確かめる。また、pluginだけの`approve_release`の答えは古いbuildのsupervisorが適用しても`plugin_only: true`でバイナリのjobを起動しないこと、バイナリの`approve_release`の答えはその版のsupervisorが適用しても`plugin_only: false`でpluginだけのjobを起動しないこと、目的の記録の無いaskは`release`とbuildの一致で決まること、新しいリリースの後に適用されたpluginだけの`install`は起動されず`update_dropped`（`reason: newer_release`・`newer`）が1回だけ残り、新しい版を聞くことを確かめる。
- `tests/it/installed_plugin.rs`が`plugin_version`と、stubの`claude`で`ClaudePlugin`のversionの読み取り・2つのコマンドの順とcwd・失敗で止まることを確かめる。
- `tests/it/cli_install_release.rs`が`install --release`（[`install`](install.md)の「リリースから」）を確かめる。

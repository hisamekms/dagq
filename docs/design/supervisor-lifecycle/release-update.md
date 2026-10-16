---
id: design-supervisor-lifecycle-release-update
type: design
title: "Release update"
status: current
created: 2026-09-27
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

外部のprojectで`cargo install`したdagqを、crates.ioの新しいリリースへ、走っているrunを止めずに入れ替える仕組み（[ADR-t618-1](../../adr/2026-09-27-t618-1-release-update-by-ask-from-crates-io.md)、pluginとの同期は[ADR-t618-2](../../adr/2026-09-27-t618-2-plugin-follows-the-release-update.md)）。
dagqのソースのrepositoryの`-dev`のビルドは[Auto-update](auto-update.md)が受け持ち、この仕組みは動かない。

## 対象

- supervisorの`binary_version`（自分のbuild識別子）がpre-releaseもbuild metadataも無い`X.Y.Z`のときだけ動く。
  `-dev`のビルド（`+unknown`・`.dirty`を含む）では調べない。
- ADR-t614-1の「dagqのソースか」の判定は使わない。
- 判定は`domain::release_update::is_release_build`。

## 設定（host.tomlの`[update]`）

- 置き場所と優先は[push](push.md)の`[push]`と同じで、`dagq.toml`からは読まない。
  値と既定、形の違う値と読めないファイルの扱い、supervisorが設定を読む時期は`infrastructure::release_update`のmodule docと`load_host_update`のdoc commentが持つ。
- `"auto"`はaskを開かずに入れ替える（非互換のmigrationは除く）。
- 効いている値・読んだファイル・既定にした値の警告は`doctor`の`release_update`に出る。

## 検知

- 読むのはcrates.ioのsparse indexのdagqのファイルで、yankedとpre-releaseを除いた最大のversion（SemVerの順）が最新。
  読み方は`domain::release_update`のdoc commentが持つ。
- `curl`で読む（打ち方と応答の読み方は`CurlIndex`と`parse_response`のdoc comment）。
  前回の`ETag`で問い、`304`なら前の結果を使う。
  呼び出しはport（`application::release_update::ReleaseIndex`、hostでは`infrastructure::release_update::CurlIndex`）の後ろにある。
- supervisorは起動の後の最初のpassと一定の間隔ごとに、hostの設定を読み、`off`でなければjob threadで調べるかを決める（claimも着地も止めない。loopは待たないが、終わる前とexecの前には待つ）。
  調べるのは、queueの最新の検知（どのsupervisorのものでも）が無いか、間隔を過ぎたときで、同じqueueの複数のsupervisorはこれで重ねて調べない。
  起動の後の最初のpassでは、最新の検知が別のbuildのものなら（入れ替わった直後）間隔の内でも調べる。
- 同じjob threadで、supervisorがpluginをdirectoryから読み込んでいなければinstallしたpluginのversionも読む（下の「pluginのversion」）。
  読めなければnullで、検知は失敗にしない。
- 結果はqueueのevent `release_checked` / `release_check_failed`で、どちらもattentionにしない。
- `status`の`release_update`が設定と最新の検知と`state`を出す（`domain::release_update::status`）。

## ask

supervisor側は`src/application/supervise/release.rs`の`release_update_pass`で、決め方は`application::release_update::next_action`。
supervisorは前に見てから一定の間隔が経ったpassで、自分のbuild識別子がリリースで、hostの設定の`release`が`off`でないときだけ、次を順に行う（停止要求の後とclaimを止めた後は見ない）。
`-dev`のビルドのsupervisorは`approve_release`の答えもリリースのjobの`update_failed`の答えも適用しない。

1. **答えを適用する**: 答えられて閉じられていない`approve_release`と、リリースのjobの失敗が開いた`update_failed`を、`update_answered`か`update_retry`として書いてからaskを閉じる。
   source buildの失敗の答えは[Auto-update](auto-update.md)のpassが適用し、互いに相手のものを飛ばす。
   optionsにない答えは人が読むものとして残す。
   - 答えがpluginだけについてか（`plugin_only`）は、askを開いたときに記録した目的で決め、答えを適用するsupervisorのbuildでは決めない（build識別子の違うsupervisorが同じqueueを共有しても分類が変わらない）。
     目的はaskを開くときに`ask_opened`に書く。
     目的の無いask（記録を始める前に開いたもの）は、その失敗の`update_failed`が言えばそれで、言わなければ`release`が自分のbuild識別子と同じか（バイナリはもうその版なので、pluginについての答え）で決める（`release_update::answer_plugin_only`）。
2. **jobは1つ**: jobの最新の段が生きているjobのものなら何もしない（source buildのjobとも同時に1つ）。
   リリースのjobの段だけの最新が途中の段でそのpidが死んでいれば、`update_failed`を書いて`update_failed`のaskを開く。
   pluginだけのjobの問いはバイナリに触っていないことも書く。
   source buildのpassも同じく自分の段だけを見る。
3. **次の一手**（`next_action`）: jobの続いていないバイナリの頼み（`install`の答えか`retry`）のうち今のbuild識別子より新しい版があれば、その中で最も新しい版のjobを起動する（頼みがjobを起動する回数は`next_action`のdoc comment）。
   pluginだけの頼みは、今のbuildがその版より古くてもバイナリのjobを起動しない。
   それ以外は、最新の検知の`latest`が今のbuildより新しく、その版のjobもバイナリについての答えも無いとき（jobが試した版は、失敗しても`update_failed`のaskが受け持つので改めて聞かない。`skip`した版も同じ）、`auto`ならaskを開かずにjobを起動し、`ask`なら同じ版の閉じられていないaskが無ければaskを開く。
4. **pluginだけの一手**（`next_plugin_action`、ADR-t618-2決定4）: 3が何もしないとき、supervisorがpluginをdirectoryから読み込んでおらず、最新の検知の`latest`が今のbuildと同じ（バイナリが最新のリリース）ときだけ見る。
   marketplaceは最新のリリースのpluginを配るので、バイナリが最新でないうちはpluginだけを上げない（pluginがバイナリより新しくなりうる）。
   - その版についてのjobの続いていない頼みがバイナリについてのものでなければ（目的を書かなかった古いruntimeの答えも数える）、pluginだけのjobを起動する（読めたpluginのversionは問わない）。
     バイナリについての頼みは、今のbuildがもうその版でもpluginだけのjobを起動しない。
   - それ以外は、今のbuildの検知が読んだpluginがその版より古く、その版についてpluginを試した・決めた記録が無いとき、`auto`ならpluginだけのjobを起動し、`ask`なら同じ版の閉じられていないaskが無ければpluginについてのaskを開く。
     バイナリのjobは入れ替えの後にpluginも上げるので、その`update_installed`で同じ版をもう聞かない（jobの途中で読んだ古い`plugin`でaskが開かないため）。
5. **実行しない頼みを残す**（`release_update::dropped_requests`）: 3と4の前に、このbuildが実行しないjobの続いていない頼みを、黙って捨てずに`update_dropped`として1回書く。
   attentionにせず、jobの段でもない。
   - pluginだけの頼みで、その版より新しいリリースが今のbuildか最新の`latest`であるものは、4が起動しないので落とす。
     新しいリリースのバイナリのjobは入れ替えの後にpluginもその版に上げるので、古い版のpluginだけの更新は要らない。
   - 目的を明記したバイナリの頼みは、今のbuildがその版か新しい版なら落とす。
     `latest`だけが新しくてもバイナリの頼みは落とさない。
     目的の無い古いruntimeの頼みの扱いは変えない。
   - dropが頼みを消費するかとその帰結は`dropped_requests`のdoc commentが持つ。
   - バイナリの頼みをpluginの頼みに変えない。
     引き継ぎの失敗などでバイナリだけ上がりpluginが古ければ、4の独立した検知が`auto`のjobかpluginの`approve_release`とその答えで上げる（既にpluginを試した失敗はそのpluginのretryで扱う）。
     バイナリの失敗・中断のretryの問いも、古いbuildでのみinstallし、同じ版以降ではdropを記録しpluginは別に扱うと説明する。

- kindは`approve_release`（`AskKind::ApproveRelease`）で、task・runを持たず`subject`にversionを持ち、pluginだけのaskも同じkindで`subject`はバイナリの今の版。
  開いている古い版の`approve_release`は`superseded`と答えて閉じてから開く。
  バイナリの問いは、`cargo install`の置き場、互換のmigrationを適用して引き継ぐこと、`.previous`が残ること、非互換なら`approve_update`で改めて聞くこと、`skip`で次のリリースまで聞かないことを書く。
  pluginだけの問いは、2つのコマンド、バイナリに触らないこと、開いているinboxとplannerのsessionは開き直すと新しいpluginを読むこと、`skip`で次のリリースまで聞かないことを書く。
- `answer`の答えの適用先: `approve_release`と、リリースのjobの`update_failed`は、リリースを入れるliveな登録が居るとき（`SupervisorRegistration::applies_releases`）runtimeが適用するものとし、それ以外の`update_failed`は`auto_update`の登録が適用する。
  居なければ人がinboxで読んで`dagq install --release <version>`を打つ。
- `update_answered`と`update_retry`と`update_dropped`はattentionにしない（Auto-updateと同じ）。
  `approve_release`の`ask_opened`がinboxのattentionになる。

## 入れ替えのjob

人の入口は`dagq install --release`（[`install`](install.md)の「リリースから」）で、supervisorは隠しコマンドの`release-update`をjobとして起動する（`application::update::run_release`、配線は`src/compose/host.rs`の`OneShot::release_update`）。
jobはsource buildの`auto-update`のjobと同じくsetsidで起動し、出力はqueueの`logs/`に書く。
起動したらsupervisorが`update_started`を書く。
jobの段はどれも`update_*`で、`commit`の代わりにリリースの版を持つ（`application::update::step_release`）。
source buildの自動更新の見直しはこれらの段を数えない。

1. **入れる**（`install::release_binary`）: 置き換える先がすでにその版なら（hostの別のqueueの答えで入れ替わった）、cargoを打たずにそのファイルを元にする（`install`は置き換えず`.previous`もそのままで、引き継ぎだけを行う）。
   そうでなければqueueの`update/`の下に`cargo install --locked`したものを元にする（`ReleaseInstaller` port）。
   cargoが見つからない・失敗した・binaryを残さなかったら`update_failed`で、何も置き換えない。
   問いには`retry`、`skip`と、手で入れ替える方法を書く。
   cargoの間はqueueの更新のlockを持つ（`update::hold_target`、[空き容量](disk-space.md)）。
2. **非互換のmigration**: [Auto-update](auto-update.md)の2と同じく、stageして`approve_update`のaskを開いて終わる（`auto`でも同じ）。
   書くコマンドに`up`の`--auto-update`は付かない（リリースのsupervisorは`auto_update`を持たず、ソースでないrepositoryでは`up --auto-update`がerrorになる。ADR-t614-1）。
   起動し直したsupervisorはhost.tomlの設定をそのまま読むので、この仕組みは打ち直しなしで続く。
3. **確認・差し替え・引き継ぎ・見張り・戻し**: [Auto-update](auto-update.md)の3〜6と同じ（source buildのjobと共通の`put_in_place`）。
4. **plugin**（ADR-t618-2決定1〜3）: 3が見張りまで成功し`update_installed`を書く前に、supervisorがpluginをdirectoryから読み込んでいなければ、supervisorの`claude`でmarketplaceとpluginを更新する2つのコマンドを順に打つ（`lifecycle::PLUGIN_UPDATE_ARGUMENTS`。1つ目が失敗すれば2つ目は打たない）。
   失敗すれば`update_installed`を書いた後に`update_failed`を書いて`update_failed`のaskを開き、バイナリは戻さない。
   問いは、バイナリはそのまま動くこと、`retry`で次のpassにsupervisorが2つのコマンドを打ち直すこと（上の「ask」の4のとおりpluginだけのjob）、`skip`でpluginをそのままにすること、手で打つコマンドと開き直しを書く。
   pluginをdirectoryから読み込むsupervisorでは打たない（`update::PLUGIN_SKIPPED`）。
   3が失敗して戻したときと非互換のmigrationで止まったときはpluginに触らない。
5. `update_installed`はinboxのattentionになる（Auto-updateと同じ）。
   pluginを更新できたときは、inboxとplannerのsessionは起動したときのpluginのままなので開き直すと新しいpluginが効くこと（headless jobとこれから開くsessionはもう新しいpluginを読む）を添える。
   runtimeは常駐のsessionを閉じない。
6. **pluginだけのjob**（ADR-t618-2決定4）: cargoも確認も差し替えも引き継ぎもせず、4のコマンドだけを打ち、失敗は4と同じask。
   バイナリを入れ替えていないので、`stats`と`status`はバイナリの入れ替えと分けて数える（[Auto-update](auto-update.md)の経過の記録とstatus）。

## pluginのversion

- [installしたpluginの確認](../plugin-integration.md#installしたpluginの確認)と同じ出力と読み取りで、dagqのpluginのentryの`version`を読む（`adapters::plugin_version`）。
- portは`application::InstalledPlugin`、hostでは`adapters::ClaudePlugin`（cwdはsupervisorのmain checkout、jobではそのcwd）。
- 比べるのは`X.Y.Z`どうしで、比べられないversionの扱いは`domain::release_update::is_newer`のdoc commentが持つ。

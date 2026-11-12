---
id: design-supervisor-lifecycle-up-down
type: design
title: "`up` / `down`"
status: current
created: 2026-09-26
scope: runtime
related:
  - adr-t1181-1
  - adr-t1582-1
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-integrate
  - design-supervisor-lifecycle-auto-update
  - design-supervisor-lifecycle-source-repository
  - adr-t614-1
  - adr-t617-2
  - design-plugin-integration
  - adr-0013
  - adr-0011
  - adr-t1433-4
  - adr-0014
  - adr-0045
  - adr-0031
  - adr-t632-1
  - design-provider-lifecycle
  - design-persistence
  - adr-t1228-2
  - adr-t2159-1
---

# `up` / `down`

`dagq up`はqueueのruntimeをcold startし、`dagq down`はsupervisorを止める。
入口はapplication層の`src/application/lifecycle.rs`で、外への口と組み立ての境界も同moduleのdoc commentが持つ（[ADR-0013](../../adr/0013-layered-architecture-and-type-function-style.md)）。
冪等で、続けて起動すれば生きている同じbuildのsupervisorを再利用する。
外部との境界はtrait越しに呼び、判定はfakeで、実launchdでの起動・状態・停止はe2eで確かめる（条件は`tests/e2e.rs`の冒頭のdoc comment）。

queueの読み書きは呼び出し元のprincipalでqueue serviceの制御のstepとして送り、DBを開かず、届かなければそのerrorで失敗する。
停止前の記録だけは失敗しても警告して止める（呼び出し元は最初のstepで通っている。[hostのCLI](../queue-service-host-cli.md#制御側のコマンド)）。

## 起動の入口と境界

起動前の検査と結果の読み方は`lifecycle::up`のdoc commentを入口に読む。
検査を通らなければsupervisorを起動せず、理由と対処を返す。
信頼を代行するためにClaude Codeのconfigを書き換えない。
main checkoutを基準にする理由と解決の境界は[Run environment](run-environment.md#main-checkoutの決め方)、信頼の境界は[Provider lifecycle](../provider-lifecycle.md#trust-prompt)にある。
pluginの検査を省く条件は[Plugin integration](../plugin-integration.md#installしたpluginの確認)と`UpEnvironment`のdoc commentにある（[ADR-t617-2](../../adr/2026-09-27-t617-2-installed-plugin-by-default-plugin-dir-for-development.md)決定4）。
着地先の解決は[landing branch](integrate.md#landing-branch)、環境のプログラムの検査は[Run environment](run-environment.md#runenvが名指すプログラムの検査)を参照する。
自動更新はqueueのrepositoryがdagqのソースのときだけ使える（[ADR-t614-1](../../adr/2026-09-27-t614-1-dagq-source-only-features-by-one-check.md)、[Source repository](source-repository.md)）。

登録のpruneの判断は`domain::supervisor_prune::pid_taken_over`、生きているsupervisor自身のsweepとの違いは同moduleのdoc commentにある。
起動はrunのleaseを直さない。
runの復旧は`doctor` / `recover`の責務である。
別labelのagentを扱う入口は`lifecycle::remove_stray_agents_for_up`で、別のqueueのagentには触れない。

起動と停止の依存はservice、broker、supervisorの順で、停止は逆になる。
待たずに停止するときの責任の引き渡しは`lifecycle::down`のdoc commentにある。
brokerはhostのprocessであり、podmanでは動かさない（[ADR-t2114-1](../../adr/2026-10-08-t2114-1-queue-broker-is-a-host-process-with-three-destinations.md)、[Queue broker](../queue-broker.md#起動と停止)）。
`up`のprocess自身はmigrateしない。
serviceによるmigrationと入れ替えを拒む境界は[Queue service](../queue-service.md#起動と停止adr-t1233-4決定12)、具体的な拒否と起動結果は`application::queue_service::ensure_replaced`のdoc commentを参照する。

## supervisorの再利用と入れ替え

再利用と入れ替えの判断の入口は`lifecycle::outdated`で、buildの比較の意味は[Build identifier](build-identifier.md#build-identifier)にある。
全員が引き継げるかの判断は`lifecycle::takes_handoff`、launchdの定義が引き継ぎを許す条件は`lifecycle::agent_takes_handoff`にある。
自動更新と引き継ぎではplistが再読込されず、ProcessTypeの無いagentの制限が残る。
対処の入口は[運用の「ProcessTypeの無いLaunchAgent」](../../development/operations.md#processtypeの無いlaunchagent)にある。
その判定の入口は`lifecycle::agent_lacks_process_type`である。

引き継ぎの待ちと失敗の判断は`lifecycle::hand_off`、部分的な失敗の出力とその後は`lifecycle::PartialHandoff`にある（[Handoff](handoff.md#handoff)）。
走っているrunのsessionとleaseはそのままで、検証と着地の区切りを待つため、待ちが検証1回分に及ぶことがある。
1つの失敗で他の引き継ぎの待ちを打ち切らない（[ADR-t632-1](../../adr/2026-09-27-t632-1-handoff-restores-only-when-every-supervisor-failed.md)）。
引き継げないときのdrainの入口は`lifecycle::replace_supervisors`、待たない指定の拒否は`lifecycle::no_wait_refusal`にある。
drainはrunの長さを待つため上限を置かず、待たない指定だけは待ちを制限する。
拒否の判断はsignalやagentの取り外しより前であり、runが残るため拒まれても停止は頼んでいない。
drain中も答えの済んだ着地の判断は適用され、承認された着地は自分のrunが残る間だけ進む（[ADR-t3330-1](../../adr/2026-10-25-t3330-1-stopping-drain-applies-landing-answers-and-lands-only-while-its-own-runs-keep-it-going.md)）。
入れ替えの約束の根拠は[ADR-0014](../../adr/0014-up-replaces-a-supervisor-of-another-binary-version.md)と[ADR-0045](../../adr/0045-build-identifier-explicit-migrate-schema-compat-handoff-and-auto-update.md)にある。

生きているがheartbeatの古いsupervisorは起動側では入れ替えず、隣に新しいsupervisorが立つことがある。
止める判断は人が行い、生きている別のsupervisorを巻き込まないことが必要である（[ADR-0073](../../adr/0073-kind-additions-are-compatible.md)決定16、[ADR-t3134-1](../../adr/2026-10-23-t3134-1-stop-a-hung-supervisor-beside-a-live-one-without-down.md)）。
具体的な対処は`lifecycle::replace_supervisors`のdoc commentとstatusのattentionにある。
二重のsupervisorの警告の判定は`health::duplicate_supervisors`にあり、引き継ぎの元と同じpidでの後継は1つとして数える。

## hostでの起動

LaunchAgentの中身は`lifecycle::LaunchAgentSpec::xml`、渡す起動引数は同moduleの`supervise_arguments`を入口に読む。
modeの記録と寿命は`runtime_store::coordination::set_supervisor_mode`のdoc commentにあり、起動した登録の取り違えを防ぐ判断は`lifecycle::started_registration`にある。
自動更新の指定を残る登録にそろえる入口は`lifecycle::set_auto_update`（[Auto-update](auto-update.md#auto-update)）、並列数などの出どころは[人の答えを待つrun](waiting.md)と[claimを控える](claim-hold.md)にある。
plistのpathは`infrastructure::location::launch_agent_path`、queue hashの意味は`QueueLocation::hash`のdoc commentを参照する。
bootstrapと停止の待ちは`infrastructure::launchd::Launchctl`、起動後の登録待ちは`lifecycle::wait_for_registration`にある。
登録が時間内に現れなくてもagentは残るため、再起動を繰り返すときはログと停止の判断が必要になる。
macOSの外では人かhostのservice managerがsupervisorをforegroundで動かす（[ADR-t1433-4](../../adr/2026-10-03-t1433-4-supervisor-resides-without-cmux.md)決定4）。

in-cmuxでの新しい起動は何にも触る前に拒む。
残る登録と引数の互換の扱いは`lifecycle`のmodule docと`main::RetiredCmux`にある（ADR-t1433-4決定3）。
workspaceを閉じるのは人で、dagqはcmuxを呼ばない（[ADR-t2159-1](../../adr/2026-10-09-t2159-1-dagq-does-not-use-cmux-and-the-person-opens-the-inbox.md)決定3・6）。
`up`はinboxもplannerも開かない。
plannerはruntimeが立てる（[`plan` / `planners`](plan-planners.md#plan--planners)、[ADR-t1394-1](../../adr/2026-10-03-t1394-1-abolish-person-planners-and-route-planning-through-inbox-requests.md)）。

## `dagq inbox`

inboxは人が自分のterminalで開き、前面で操作する（ADR-t2159-1決定2）。
起動の入口は`lifecycle::inbox`で、同じinboxの中から別のinboxを開かない。
providerの選択と拒否は`lifecycle::inbox_route`、inboxの設定だけを読む境界は`run_env::load_inbox_role`にある（[ADR-t1181-1](../../adr/2026-10-24-t1181-1-codex-inbox-provider-selection-gaps-unavailability-and-records.md)）。
使えないCodexの代わりにClaudeを起動せず、askを開かず、人のfileを読むだけで書かない（[ADR-t3331-1](../../adr/2026-10-25-t3331-1-codex-inbox-writes-no-person-settings-and-refuses-to-open-when-codex-would.md)）。
開いた後にproviderが使えなくなったら人が開き直す。
runtimeはinboxを起動しないため、providerの控えもinboxを止めない。
Claudeの起動を人が禁じたときの境界は`lifecycle::claude_forbidden`にある。
providerごとの起動は[Provider lifecycle](../provider-lifecycle.md)のinboxの節を参照する。
環境が`DAGQ_CONTROL_URL`だけの入口は`compose::control_inbox`（判断は`lifecycle::control_inbox`）である。
そのときは`dagq inbox`がhostの手順（queueの解決・DB・launchd・supervisorの確かめ）を行わず、中継として残り、sessionの終わりでinboxのtokenを失効させる。

起動の記録とguardrailの判定は`inbox_guardrail::judge`、Claudeに渡すsettingsは`adapters::ClaudeCode::inbox_command`のdoc commentにある（[ADR-t1228-2](../../adr/2026-10-02-t1228-2-deny-raw-cmux-to-inbox-and-planner-as-a-guardrail.md)）。
guardrailが無いときの開き直しはpluginのdagq-recoverのreference/up-down.md「Open the inbox again」が正本である。
settingsは起動の引数なので、人がterminalでagentを打ち直したsessionや、この入口を通さないinboxには効かず見えない。
Codexのhookはtrustされなければ動かず、起動の記録だけでは効いているか分からない。
denyはtoolの呼び出しの綴りの照合なので、絶対path・scriptの中・別のshellからの呼び出しを止めるenforcementではない（[Security](../security.md)）。

## 停止の入口と落とし穴

どのmodeで起動したsupervisorも停止の入口は`lifecycle::down`で、inboxのterminalには触れない。
LaunchAgentは必ず外し、定義も消す。
残すと次のloginで起動してしまうためである。
PIDが他のprocessに渡った登録を消しても、そのPIDにsignalは送らない。
bootoutはagentのprocessにsignalを届けるので、同じPIDへ2回目を送るとdrain中のsupervisorが即死する。
送信済みの判定とPIDが分からない場合の扱いは`lifecycle::signalled_by_bootout`にある。
停止の待ちと強制終了後の登録の後始末は`lifecycle::down`のdoc commentを参照する。
強制終了後のrunの復旧は`doctor` / `recover`が扱う。

## CIの見張り（ADR-t1920-1）

[ADR-t1920-1](../../adr/2026-10-06-t1920-1-supervisor-watches-main-ci-keeps-known-failures-and-files-fixes-through-findings.md)と[CI watch](ci-watch.md)を参照する。
CIを読めない環境でsupervisorを起動しないための検査は`infrastructure::ci_watch::preflight`のdoc commentを入口に読む。

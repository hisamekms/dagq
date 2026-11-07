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
  - adr-t2719-1
  - design-persistence
---

# Auto-update

[ADR-0045](../../adr/0045-build-identifier-explicit-migrate-schema-compat-handoff-and-auto-update.md)の決定13・17。
`up --auto-update`のsupervisorは、runtimeを変えるcommitがmainに着地すると、queue専用のcheckoutでビルドし、同じcheckoutで全部のe2eを通してから固定バイナリを置き換え、[引き継ぎ](handoff.md#handoff)で入れ替わる。
走っているsessionは止まらない。
全体の入口は`src/application/update.rs`のmodule docと`run`、supervisor側は`src/application/supervise/host_ops/update.rs`、CLIの配線は`src/compose/host.rs`の`OneShot::auto_update`。

## 有効にする

自動更新は設定ファイルでなく`up --auto-update`で頼む（理由はADR-0045のAlternatives）。
フラグの無い`up`は同じ登録の自動更新を無効にし、execの引き継ぎとlaunchdの再起動では設定が残る。
登録への反映は`set_auto_update`が入口になる。

## きっかけ

mainをいつ見てどの差分をビルドするかは`HostOpsState::auto_update`、比べる元（`base`）の意味は`update::base_commit`が持つ。
停止・claimの停止・引き継ぎの要求の後は新しい更新を始めない。
自動更新のjobは同時に1つで、exec後も記録から生死を判定し、中断は勝手にやり直さずaskで判断を渡す（`look_at_job`）。
mainが進めばsupervisorの着地も人の`integrate`も同じように拾う。
ビルド中の着地は積まず、jobの後の見直しでそのときのheadを拾う。

## job

隠しコマンド`dagq auto-update`の引数は`--help`、起動とlogの配置は`HostOpsState::start_update_job`が持つ。
jobはsupervisorのexecや終了に巻き込まれない独立したsessionで動く。
logの保持は[Logs](commands.md#log)を参照する。

### queueへの経路

jobはDBを開かず、起動したsupervisorのprincipalでqueue serviceの制御側のユースケースへ読みと記録を送る（[hostのCLI](../queue-service-host-cli.md#制御側のコマンド)）。
service.installの認可には登録のtokenのfileが必要で、tokenの無いjobは断られる。
checkout・ビルド・cargo installの子へ資格情報を渡さない（`update_job_command`と`without_credential`）。
execの引き継ぎはtokenも継ぐが、登録が終われば失効し、jobは別のprincipalに替えずerrorで終わる。
そのため起動元を戻す際は失敗の記録とaskを先に済ませる（`update::Recovery`、ADR-t3102-1決定2）。

### ビルド

checkoutとRustのprofileによる出力先、ビルドの記録は`update::run`、pathの意味は`UpdatePaths`が持つ。
人のmain checkoutとtargetを使わない。
更新のtargetはjobが終わるまで排他で持ち、release更新・`install --release`・空き容量の掃除とも共有しない（`update::hold_target`、[queueのdirのビルドのキャッシュ](claim-hold.md)）。

### 着地の検証との順番

自動更新のbuildとe2eの関門は着地の検証と番を交代する（[ADR-t2719-1](../../adr/2026-10-18-t2719-1-auto-update-gate-and-landing-checks-take-turns.md)）。
jobが待ち始めたら新しい検証が追い越さず、走行中の検証の終わりまでしか待たない。
入れ替え・引き継ぎ・見張りはこの排他の外にある。
lockの順序と待つときの通知は`application::heavy_turn::HeavyTurn`、待つ相手の判断は`domain::heavy_turn`が持つ。

### e2eの関門

固定バイナリを置き換える前に、同じcheckoutとtargetで全部のe2eを流す（[ADR-t963-1](../../adr/2026-09-29-t963-1-e2e-required-by-diff-and-run-in-full-before-auto-update.md)決定1）。
環境の許可・資格情報の除外・不足時の見え方と戻し方、上限と後始末は`infrastructure::e2e_gate`のmodule docが持つ。
関門はhostで1本ずつ流し、runの[着地の前のe2e](integrate.md#着地の前のe2e)と人のinstallも同じlockを使う（[ADR-t1233-2](../../adr/2026-10-02-t1233-2-e2e-runs-on-the-host-after-review-passes.md)決定4）。
道具の有無で外すe2eは無く、対象の規則は[testの制約](../../development/testing.md#e2e)にある（[ADR-t2159-1](../../adr/2026-10-09-t2159-1-dagq-does-not-use-cmux-and-the-person-opens-the-inbox.md)決定7、[期間限定の除外の判断](../../adr/2026-10-04-t1582-1-temporarily-leave-broker-and-cmux-only-e2e-cases-out.md)）。
落ちたtestだけを名前で1回流し直す条件と結果は`e2e_gate::run`が持つ（[ADR-t1165-1](../../adr/2026-09-30-t1165-1-e2e-gate-reruns-failed-e2e-once-and-records-quarantined-failures.md)）。

#### 印

印は関門が流すcheckoutのものを使い、走っている旧バイナリの設定には置かない。
書式と効く条件は`domain::e2e_quarantine`のmodule doc、付け外しとcommitの検査は[testの制約](../../development/testing.md#e2eの印)を参照する。
期限切れの印は関門で効かず、commitの検査は警告だけで通す。
関門の判定は`application::e2e_verdict::judge`で、自動更新・人のinstall・runのe2eが共有し、続けての失敗も合わせて数える。
印の有無に依らず失敗を数えるため、関門を落とした後もそのtestが流し直しで通るまで落ち続ける。
関門の記録の欄の意味は`E2eVerdict::fields`、失敗時の問いは`update::e2e_gate`が入口になる。
関門が落ちれば何も置き換えず、非互換のmigrationのビルドもstageせず、承認のaskも開かない。
リリースのバイナリを扱う[Release update](release-update.md)のjobはこの関門を持たない。

### 非互換のmigration

非互換のmigrationを含むビルドは何も置き換えず、人の承認に渡す（`update::stage`と`awaiting_approval`）。
互換性の確認を判定するのは前のbuildの固定バイナリなので、migrationの確認の出力と終了の状態はbuildをまたぐ約束になる（[install](install.md#install)）。

### 入れ替えと見張り

入れ替えは`install::install`、後継の照合と引き継ぎの待ちは`lifecycle::hand_off`が入口になる。
drainはせず、互換のmigrationは入れ替えの口のserviceが適用する（[install](install.md#install)）。
引き継ぎ後の見張り・戻し・失敗の記録は`update::settle`で、人のinstallも共有する。
見張りの成功は新しいbuildでheartbeatが進んだことで判定する（`update::watch`）。
固定バイナリは全員で1つなので、全員が失敗し、戻すfileが置き換え前のbuildと一致するときだけ戻す。
一部が新しいbinaryで動いていれば戻さず、失敗したsupervisorだけを戻してaskで人へ渡す（[ADR-t632-1](../../adr/2026-09-27-t632-1-handoff-restores-only-when-every-supervisor-failed.md)）。

### supervisorを戻す

戻す相手と登録の読み方は`update::Recovery`、戻す必要の判断は`bringing_back`、modeごとの起動し直しは`restart`が持つ。
登録済みのin-cmuxのsupervisorはjobが起動し直さず、workspaceも閉じない。
jobには停止後にupを打つprincipalが無いので、askに載るコマンドを人かinboxが停止の完了後に打つ（`restart_by_update`）。
人のinstallは呼び出し元のprincipalでupを打つ。

## 経過の記録

更新はtask・goal・runに属さないqueueのeventで記録し、kindの追加にmigrationは要らない（[ADR-0073](../../adr/0073-kind-additions-are-compatible.md)決定17・19〜22）。
旧い表binary_updatesは残すが読み書きしない。
記録は`update::record`、supervisorとstatus向けの読取りは`SqliteQueue::update_events`、jobの段の並べ方は`domain::in_job_order`が入口になる。

成功の通知は人に知らせるだけで、roleなしのwatchは起きるがinboxのwatchを単独では起こさず、次に別件で起きたeventsに古い順に載る（[ADR-t1418-1](../../adr/2026-10-03-t1418-1-quiet-notices-do-not-wake-the-inbox-watch.md)、[Events and watch](commands.md)）。
失敗と承認待ちのevent自身はattentionにせず、askの通知がinboxを起こす。
runの間に更新を並べるtimelineは`domain::timeline::merged`、askと同じ窓で更新と関門のtestを数えるstatsは`domain::stats::updates`が入口になる。
関門のflakyなtestは[Observer](observer.md)も読む。

## inboxに知らせる

失敗と非互換のビルドはtask・runの無い専用のaskで知らせ、古いaskとの置き換えは`SqliteQueue::open_update_ask`が持つ。
jobのaskと人のinstallのaskは互いに閉じ合わない。
入れたcommitで解消した失敗のaskの整理は`update::close_settled_failures`が持ち、整理の失敗は入れ替えの成功を取り消さない。

失敗の答えを適用する入口は`HostOpsState::apply_update_answers`。
自動更新を持つliveな登録が居るときだけruntimeが適用する答えとして扱い、判定は`SupervisorRegistration::applies_updates`が持つ。
居なければ人がinboxで読む答えとして残る。
source buildとリリースの失敗はそれぞれのpassが扱う。
人のinstallの失敗はinboxが読んで閉じ、retryは人がinstallを打ち直す意味で、jobを起こさない。

非互換のmigrationの承認はsupervisor自身を止めるdrainを伴うのでruntimeは適用しない。
承認後の操作は[更新のaskへの対応](../../../plugins/claude-dagq/skills/dagq-recover/reference/update.md#the-update-asks)を参照する。
値の引き継ぎは`install_breaking`のdoc commentが持つ。

## 待ちの見直し

依存先の着地を含むbuildを待つtaskは、固定バイナリのfileでなくsupervisorの走っているbuildで判定する。
引き継ぎ後の最初のpassで新しいbuildから判定し直し、成功のeventそのものは待ちを変えない（[依存先の着地を含むbuildを待つtask](claim-hold.md#依存先の着地を含むbuildを待つtask)、ADR-t1632-1）。

## queue serviceとqueueのbroker

引き継ぎはqueue serviceもbrokerも止めない。
入れ替えの順と登録との境界は[Queue service](../queue-service.md#起動と停止adr-t1233-4決定12)と[Queue broker](../queue-broker.md#起動と停止)を参照する。

## status

statusは結果を作ったprocessのbuild識別子と、登録ごとの設定とbuildを示す。
結果を作るのはqueue serviceなので、打ったdagqでなくserviceのbuild識別子になる（[hostのCLI](../queue-service-host-cli.md#送る範囲)）。
自動更新の状態の意味は`update::status`のdoc commentが持つ。

## test

askのkind・attention・stats・印と関門・statusの判断はunit test、関門の実行・後始末・jobの分岐・supervisorのきっかけとprocessの境界はintegration test、作業中のrunの入れ替えと着地はe2eが確かめる。

## dagqのソースのrepositoryだけ

リリースのバイナリで動く外部のprojectの更新は[Release update](release-update.md)が持ち、確認から見張りまではsource buildと共通の`update::put_in_place`を通る。
`up --auto-update`のsource buildはqueueのrepositoryがdagqのソースのときだけ動き、ソースでなければupはerrorで止まり、自動更新の設定を持つsupervisorもbuildに進まない（[ADR-t614-1](../../adr/2026-09-27-t614-1-dagq-source-only-features-by-one-check.md)）。
判定と対象の一覧は[Source repository](source-repository.md)、見直しの際の動作は`HostOpsState::auto_update`が持つ。
source buildの出力先とenvはtoolchainの宣言に依らずRustのprofileから取り、動かすかはソースの判定で決まる（[ADR-t2268-1](../../adr/2026-10-12-t2268-1-toolchain-knowledge-behind-declared-profiles.md)決定2）。

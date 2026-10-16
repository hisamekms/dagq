---
id: design-supervisor-lifecycle-install
type: design
title: "`install`"
status: current
created: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-up-down
  - adr-0045
  - adr-0073
  - adr-t632-1
  - adr-t963-1
  - adr-t2125-1
  - adr-t1165-1
  - adr-t1582-1
  - adr-t618-1
  - design-persistence
---

# `install`

`dagq install`は固定バイナリを入れ替えて、このqueueのsupervisorを引き継がせ、引き継いだsupervisorを見張る人の入口（ADR-0045の決定11・12・14、[ADR-0073](../../adr/0073-kind-additions-are-compatible.md)の決定13・14）。
flagと既定はCLI（`src/main.rs`）の`--help`が持つ。
use caseは`src/application/install.rs`（置き換えと引き継ぎ）と`src/application/update.rs`の`install_watched`（その後の見張り。下の7）、配線は`src/compose/host.rs`の`OneShot::install`、binaryのビルド・起動・置き換えは`src/infrastructure/binaries.rs`（`Binaries` port）。
queueを開く前に処理するので、queueがこのbinaryより古くても新しくても動く。

1. **元**: 与えたbinaryかcheckout、省略時はcwdのrepositoryのmain checkout（[Run environment](run-environment.md#main-checkoutの決め方)）をビルドしたもの、戻すときは`<to>.previous`、リリースならcrates.ioのリリース（下の「リリースから」）。
   - **e2eの関門**（[ADR-t963-1](../../adr/2026-09-29-t963-1-e2e-required-by-diff-and-run-in-full-before-auto-update.md)決定1）: 元がdagqのソース（[Source repository](source-repository.md)）のcheckoutなら、ビルドの後・2の確認の前に、[自動更新](auto-update.md#auto-update)と同じ関門（`Binaries::e2e`、`src/infrastructure/e2e_gate.rs`）で全部のe2eを流す。
     e2eは人のshellのenvを引き継がないので、`install`がビルドしたbinaryのpathからそのビルドのtargetを取って関門に渡し、二度目のビルドをしない。
     envの絞り方は[着地の前のe2e](landing-e2e.md)の「環境」が持ち、hostで1本ずつ流すlockは自動更新の関門とruntimeがrunに流すe2eと共通（[ADR-t1233-2](../../adr/2026-10-02-t1233-2-e2e-runs-on-the-host-after-review-passes.md)決定4）。
     hostの道具の有無で外すe2eは無い（[ADR-t2159-1](../../adr/2026-10-09-t2159-1-dagq-does-not-use-cmux-and-the-person-opens-the-inbox.md)決定7）。
     後始末、落ちたe2eの1回の流し直し、`.config/e2e-quarantine.toml`の印（[ADR-t1165-1](../../adr/2026-09-30-t1165-1-e2e-gate-reruns-failed-e2e-once-and-records-quarantined-failures.md)）は自動更新と同じで、判定は`e2e_verdict::judge`。
     `install`の関門はeventを書かないので、印の続けての失敗はqueueがあれば自動更新の関門とruntimeがrunに流したe2eのeventから数える。
     落ちれば何も置き換えず、落ちたtestかその理由とlogのpathを載せたerrorで止まり、関門を飛ばす手を案内する。
     関門を飛ばすのは人が明示したときだけ。
     binary file（自動更新の`approve_update`の`staged`のbinaryはjobの関門を通ったもの）・戻し・リリースとdagqのソースでないcheckoutは関門を持たない。
     関門を流すかはCLIが`E2eGate`で決め、結果の`e2e`の形は`E2eGate`と`E2eOutcome::report`のdoc commentが持つ。
   - **期間限定で外したケース**（[ADR-t1582-1](../../adr/2026-10-04-t1582-1-temporarily-leave-broker-and-cmux-only-e2e-cases-out.md)）: 外したケースは登録に無いので結果の`e2e`には出ない。
     終了条件はADRと[testの制約](../../development/testing.md#e2e)が持つ。
   - **queueの`update/target`**: `install`がこれを使うのはリリースの`cargo install`だけで、その間だけqueueの`update/lock`を待って取る（下の「リリースから」）。
     checkoutのbuildとe2eはそのcheckoutの`target/`を使い、このlockを取らない。
2. **確認**（決定12）: 元のbuild識別子を読み、使い捨てのqueueで起動できることを確かめる。
   通らなければ何も置き換えない。
   引き継ぎを受けるliveな登録があるのに、元が引き継ぎより前のbinaryなら、execしたsupervisorが引数で止まるので、何も置き換えずに`down`と`up`を案内するerrorで止まる（そのようなbinaryに戻すときも同じ）。
3. **schema**: queueのDBがあれば元の`migrate --check`を読む。
   非互換のmigrationがあれば、人が許していなければ何も置き換えずにerror、許していればdrain（下記6）。
   元がqueueを開けず（非互換のmigrationの後に戻すとき）、適用するものも無ければ、`backups/`を案内するerror。
   互換のmigrationがあれば元の`migrate`で適用する。
4. **置き換え**（決定11）: 元が`<to>`そのもの（リリースで`<to>`がすでにその版だったとき）なら置き換えず（`.previous`もそのまま）、引き継ぎだけを行い、全員の引き継ぎが失敗しても戻さない。
   それ以外は元を`<to>`と同じdirの一時fileにcopyし、`<to>`をhard linkで残してから一時fileを`<to>`にrenameし、残したものを`<to>.previous`にrenameする。
   動いているプロセスは前のinodeを使い続けるので、上書きのcpで殺されることはない。
   戻すときは同じ手順で`<to>`と`.previous`が入れ替わる。
   未知の`DAGQ_ROLE`を拒まず新しいroleを知らないバイナリに戻すと、新しいバイナリが起動して走り続けるjobが制限を受けない窓ができる（[Authorization](../authorization.md#固定バイナリを戻したときの窓)）。
5. **引き継ぎ**: liveな登録のうち引き継ぎを受けられるものに`<to>`をexecさせ（`lifecycle::hand_off`）、受けられないものは`not_handed_off`（`up`でdrainする）に載せる。
   停止要求でdrainに入ったsupervisorは上限を待たずにその場で失敗にし、要求を取り消す（[Handoff](handoff.md#handoff)）。
   上限の直後に要求を受け取ったsupervisorの扱いは`lifecycle::hand_off`のdoc commentと[`up` / `down`](up-down.md)の引き継ぎが持つ。
   `hand_off`は最初の失敗で止まらず、引き継がせた全員の成功か失敗を待つ（[ADR-t632-1](../../adr/2026-09-27-t632-1-handoff-restores-only-when-every-supervisor-failed.md)）。
   fileは全員で1つなので、**全員が失敗したときだけ**`<to>.previous`を`<to>`に戻し（`restore`）、`install::HandoffFailed`のerrorで止まる。
   **一部だけが失敗したとき**は戻さず、失敗したsupervisorを名指して、`down --force`と`up`（新しいbinaryで起動し直す）か戻し（全員を前のbinaryに戻す）を案内する`install::KeptBinary`のerrorで止まる。
   CLIはこのerrorのときも結果をstdoutに出してから、errorをstderrに出して失敗で終わる。
   結果の形は`install::install`と`lifecycle::Handed::report`が持つ。
   queueが無ければ置き換えだけを行う。
   全員の失敗と一部の失敗は、CLIでは7の`update_failed`の経路を通る。
6. **drain**（決定14）: 人が非互換のmigrationを許したとき、liveな登録があれば`down --wait`で止め（同じqueueのDBを持つ別のlabelのagentも外す。[up / down](up-down.md)の`down`）、元の`migrate`（`backups/`に複製してから適用）、置き換えをして、新しいbinaryの`up`で起動し直す（`--claude`と`--codex`の解決は[Provider executables](../provider-executables.md)）。
   止めた登録がin-cmuxでもlaunchd modeで起動する（[ADR-t1433-4](../../adr/2026-10-03-t1433-4-supervisor-resides-without-cmux.md)）。
   止めた登録がflagで取った並列度・自動更新・待ちの上限・runtimeのplannerの上限は同じflagで引き継ぎ、`dagq.toml`か既定から取った値は起動したsupervisorが解き直すので、drainの後に`up`を打ち直す必要はない。
   値の出どころを記録していない（出どころの列より古いbinaryの）登録の値は、flagで取った値と同じく引き継ぐ。
   起動し直しの引数がすでに持つflagは重ねない。
   drainは7の見張りの対象外: 前のbinaryは非互換のmigrationの後のDBを開けないので戻せず、supervisorの起動し直しは上の`up`が行う（その`up`の失敗はerrorで止まる）。
7. **見張り**（ADR-0073の決定13・14）: 5で引き継がせたsupervisorのそれぞれが、新しいbuild識別子のまま、取り戻したときより後のheartbeatを期限の内に書くのを待ち、確かめ終えるまで`install`は成功を返さない（`update::install_watched`）。
   判定・戻し・起動し直しは自動更新のjobの4・5（[Auto-update](auto-update.md#auto-update)）と同じ`update::settle`を通る。
   全員が失敗すれば`.previous`が置き換えた前のbuild識別子と同じときだけ固定バイナリを戻し、一部だけなら戻さず（ADR-t632-1）、失敗したsupervisorだけをそれぞれ戻す。
   戻すときの元の登録は`install::install`を呼ぶ前に読んだ記録から取り、ビルドと関門の間に登録したsupervisorも引き継ぎの後に読み直して足す（`update::registered_since`）。
   queueが無い・liveなsupervisorが居ない・6のdrainのときは見張らない。
   - **inboxに知らせる**: 見張りの失敗と5の引き継ぎの段の失敗は、人の`install`のものと分かる`update_failed`のqueue eventを書き、`update_failed`のask（新しいkindは作らない）を開く。
     askの`subject`は`install`で、開いたaskの一意の索引が`subject`を含むので、jobのaskと並んで開ける。
     askの`superseded`は同じ側の開いたaskだけを閉じるので、前の手動の`install`のaskは閉じ、自動更新・リリースのjobのaskとは互いに閉じ合わない（`open_update_ask`）。
     問いは手動の`dagq install`であること、段、error、binaryと各supervisorの状態、人が手で打つ次の手（同じ`install`のやり直し、戻し、`down --force`と`up`）と、答えを適用するものが居ないことを書く。
     CLIは結果の代わりにこの`update_failed`のpayloadをstdoutに出し、`update::InstallFailed`のerrorをstderrに出して失敗で終わる。
     5の前の失敗（ビルド・e2e・確認・schema・置き換え）はerrorだけで、inboxには知らせない。
   - **自動更新と取り違えない**（`update::step_install`）: 人の`install`が書いた`update_failed`はjobの段ではない。
     `status`の`auto_update`、`retry`の判定、jobの最新の段はこれを読まず、`update_restored`も書かない。
     `stats`の`updates`での数え方は`domain::stats::updates`が持つ。
     askの答えはsupervisorの自動更新のpassもリリースのpassも適用しない（ビルドを起こさない）ので、inboxが答えを読み、人の言葉で問いに書いた手を打ってaskを閉じる（dagq-recover skillの`reference/update.md`の`update_failed`）。

queue serviceとqueueのbrokerは5の引き継ぎでは止めず、execした新しいbinaryのsupervisorの最初の見張りが前のbuildのものを1回だけ入れ替え、6のdrainの後は`up`が新しいbinaryで起動する（[Queue service](../queue-service.md#起動と停止adr-t1233-4決定12)・[Queue broker](../queue-broker.md#起動と停止)）。

`install`自身は引き継ぎの完了と、その後の見張り（上の7）を待つ。
起動し直さなかったもの（modeの無いもの、起動し直しに失敗したもの）は止まったままなので、inboxの`supervisor_stopped`と問いを見て、人が`up`する（leaseはstaleになり、adoptで引き継がれる）。
自動更新のjobは`install::install`を直接呼び、同じ見張りを自分の段（`commit`つきの`update_*`）として行う（[Auto-update](auto-update.md#auto-update)）。
自動更新が非互換のmigrationのビルドで開く`approve_update`のaskは、stagedのbinaryを元に非互換のmigrationを許した`install`を人が打つ入口になる。

元を与えないbuild（上の1の省略時）は、queueのrepositoryがdagqのソースのときだけ動く（[ADR-t614-1](../../adr/2026-09-27-t614-1-dagq-source-only-features-by-one-check.md)。判定とソースでないrepositoryでのerrorは[Source repository](source-repository.md)）。
元を与えたときと戻しとリリースは判定に関係なく動く。
元を与えないときCLIはcwdのrepositoryのmain checkoutを判定し、ソースでなければ`Source::default_checkout`（`src/application/install.rs`）がbuildの前にerrorで止める。

## リリースから（`--release`）

リリースからの`install`（[ADR-t618-1](../../adr/2026-09-27-t618-1-release-update-by-ask-from-crates-io.md)決定6）は、crates.ioのリリースを元にする人の入口で、supervisorの`approve_release`の答えが起動するjob（[Release update](release-update.md)の「入れ替えのjob」）と同じ置き場と`cargo install`を使う。
supervisorが居ないときや、askを待たずに上げたいときに打つ。
配線は`src/compose/host.rs`の`OneShot::install_release`。

- 版を省くと、yankedとpre-releaseを除いた最新にする（`application::release_update::resolve_version`）。
- `install::release_binary`: `<to>`がすでにその版ならそのfileを元にし（cargoを打たない）、そうでなければqueueのdirの下に`cargo install`したものを元にする（`infrastructure::binaries::CargoInstaller`）。
  cargoが見つからない・失敗したら何も置き換えずにerror。
  cargoの間は、自動更新とrelease更新のjobと同じqueueの`update/lock`を待って取り、cargoが終われば手放す（`update::hold_target`）。
  持つ間は空き容量の掃除が`update/target`を消さず、掃除が消す間は待ってから始める（[空き容量を確かめる](disk-space.md)の「queueのdirのビルドのキャッシュ」）。
- その後は上の2〜6と同じ（`Source::Binary`）。

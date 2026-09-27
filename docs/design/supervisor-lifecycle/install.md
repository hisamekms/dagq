---
id: design-supervisor-lifecycle-install
type: design
title: "`install`"
status: current
created: 2026-09-26
updated: 2026-09-28
last_verified: 2026-09-28
scope: runtime
related:
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-up-down
  - adr-0045
  - adr-t632-1
  - adr-t618-1
  - design-persistence
---

# `install`

`dagq install [--from PATH | --release [VERSION]] [--to PATH] [--rollback] [--allow-breaking] [--handoff-timeout SECS] [--cmux EXE] [--claude EXE] [--codex EXE] [--plugin-dir PATH]`は固定バイナリを入れ替えて、このqueueのsupervisorを引き継がせる人の入口（ADR-0045の決定11・12・14）。use caseは`src/application/install.rs`、binaryのビルド・起動・置き換えは`src/infrastructure/binaries.rs`（`Binaries` port）。queueを開く前に処理するので、queueがこのbinaryより古くても新しくても動く。

1. **元**: `--from`がfileならそのbinary、dirならそのcheckout、省略時はcwdのrepositoryのmain checkout（[Run environment](run-environment.md#main-checkoutの決め方)。無ければ`--from`を求めるerror）を`cargo build --release --locked`でビルドする（`CARGO_TARGET_DIR`があればそこ、無ければ`<checkout>/target`の`release/dagq`）。`--rollback`は`<to>.previous`（無ければerror）。`--release [VERSION]`はcrates.ioのリリース（下の「リリースから」）。
2. **確認**（決定12）: 元の`--version`でbuild識別子を読み、使い捨てのqueue（tmpdirの`--db`）で`init`と`list`が通ることを確かめる。通らなければ何も置き換えない。引き継ぎを受けるliveな登録があるのに、元が`supervise --handoff-token`を知らない（`supervise --handoff-token probe --help`が通らない。引き継ぎより前のbinary）ときも、execしたsupervisorが引数で止まるので、何も置き換えずに`down --wait`と`up`を案内するerrorで止まる（`--rollback`で引き継ぎより前のbinaryに戻すときも同じ）。
3. **schema**: queueのDBがあれば元の`migrate --check`を読む。非互換のmigrationがあれば、`--allow-breaking`が無ければ何も置き換えずにerror、あればdrain（下記6）。元がqueueを開けず（floorより古い。非互換のmigrationの後の`--rollback`）、適用するものも無ければ、`backups/`を案内するerror。互換のmigrationがあれば元の`migrate`で適用する。
4. **置き換え**（決定11）: 元が`<to>`そのもの（`--release`で`<to>`がすでにその版だったとき）なら置き換えず（`.previous`もそのまま）、引き継ぎだけを行い、全員の引き継ぎが失敗しても戻さない。それ以外は 元を`<to>`と同じdirの一時file（`.<name>.install-<pid>`）にcopyして0755にし、`<to>`をhard linkで`.<name>.previous-<pid>`に残してから一時fileを`<to>`にrenameし、残したものを`<to>.previous`にrenameする。動いているプロセスは前のinodeを使い続けるので、上書きのcpで殺されることはない。`--rollback`は同じ手順で`<to>`と`.previous`が入れ替わる。`<to>`の既定はこのbinary自身（`current_exe`）。
5. **引き継ぎ**: liveな登録（PIDが生きていてheartbeatが30秒以内）のうち引き継ぎを受けられるものに`<to>`をexecさせ（`lifecycle::hand_off`、上限`--handoff-timeout`、既定1800秒）、受けられないものは`not_handed_off`（`up`でdrainする）に載せる。上限の時点で戻っていなかったsupervisorの取り消しが要求を見つけなければ（上限の直後に新しいbinaryが登録し直して要求の列を消した）、短い猶予（`lifecycle::HANDOFF_GRACE`）のあいだ見直し、新しいbuild識別子で戻れば成功に数える（task 715。詳細は[`up` / `down`](up-down.md)の引き継ぎ）。`hand_off`は最初の失敗で止まらず、引き継がせた全員の成功か失敗を待つ（[ADR-t632-1](../../adr/2026-09-27-t632-1-handoff-restores-only-when-every-supervisor-failed.md)）。fileは全員で1つなので、**全員が失敗したときだけ**`<to>.previous`を`<to>`に戻し（`restore`）、その旨と全員の失敗の理由を添えたerrorで止まる。**一部だけが失敗したとき**は戻さず、`<N> of the <M> supervisors took the handoff to <version>, so it stays at <to>; …`で始まり、失敗したsupervisorの名前と理由、`down --force`と`up`（新しいbinaryで起動し直す）か`install --rollback`（全員を前のbinaryに戻す）を案内するerror（`install::KeptBinary`）で止まる。CLI（`src/main.rs`）はこのerrorのときも結果をstdoutに出してから、errorのJSONをstderrに出して終了コード1で終わる。結果は`{"outcome":"installed"|"partially_handed_off","target","version","previous","previous_version","migrated","kept","supervisors":[…],"not_handed_off":[…]}`で、`kept`は一部の失敗で新しいbinaryを残したときtrue。`supervisors`はsupervisorごとの結果（`lifecycle::Handed::report`）で、成功なら`{"token"（今のtoken。同じpidが登録し直せば新しいtoken）,"previous_token","pid","mode","workspace_id","version"（前のbuild識別子）}`、失敗なら`{"token","pid","mode","workspace_id","version","error"}`。queueが無ければ置き換えだけを行う。
6. **drain**（`--allow-breaking`、決定14）: liveな登録があれば`down --wait`で止め、元の`migrate`（非互換なので`backups/`に複製してから適用）、置き換え、`<to> --db <db> up [--parallel <止めた登録のparallel>（その出どころが`flag`か、出どころの列より古いbinaryの登録なら）] [--in-cmux（止めた登録がin_cmuxなら）] [--auto-update（止めた登録のどれかがauto_updateなら）] [--max-waiting <止めた登録のmax_waiting>（記録があり、その出どころが`flag`か記録の無いとき。古いbinaryの登録はNoneで渡さない。出どころの列より古いbinaryの登録は既定の4でも渡す）] --cmux … [--claude …] [--codex …] [--plugin-dir …]`で起動し直す（`--codex`は見つかれば実体のpathに解決し、見つからなければ与えた値のまま渡す）。`--auto-update`と`--max-waiting`は、起動し直しの引数（`--cmux`など）がすでに持っていれば重ねない。止めた登録の自動更新と待ちの上限が引き継がれるので、drainの後に`up --auto-update`を打ち直す必要はない。結果は`{"outcome":"installed",…,"migrated","drained","up"}`。

`install`自身は引き継ぎの完了を待ち、全員が失敗すればfileを戻してerrorを返す（一部の失敗ではfileを残す。上の5）。新しいbinaryが起動直後に死んだときは、supervisorは止まったままなので、inboxの`supervisor_stopped`と`install`のerrorを見て、人が戻ったbinaryで`up`する（leaseはstaleになり、adoptで引き継がれる）。決定13の見張り（引き継ぎの後のheartbeatの確認、`.previous`への戻し、止まったsupervisorの起動し直し、inboxへの`update_failed`）は、`up --auto-update`の自動更新のjobがこの`install`を呼んだ後に行う（[Auto-update](auto-update.md#auto-update)）。自動更新が非互換のmigrationのビルドで開く`approve_update`のaskは、`install --from <queue dir>/update/staged/dagq --allow-breaking`を人が打つ入口になる。

`--from`なしのbuild（上の1の省略時）は、queueのrepositoryがdagqのソースのときだけ動く（[ADR-t614-1](../../adr/2026-09-27-t614-1-dagq-source-only-features-by-one-check.md)。判定とソースでないrepositoryでのerrorは[Source repository](source-repository.md)）。`--from`付きと`--rollback`と`--release`は判定に関係なく動く。`--from`なしのときCLI（`src/main.rs`）はcwdのrepositoryのmain checkoutを判定し、ソースでなければ`Source::default_checkout`（`src/application/install.rs`）がbuildの前に`install without --from builds dagq from the repository's sources, and <checkout> is not dagq's source (...); update dagq with `cargo install dagq`, or pass --from with a built binary or a checkout of dagq`のerrorで止める。

## リリースから（`--release`）

`--release [VERSION]`（[ADR-t618-1](../../adr/2026-09-27-t618-1-release-update-by-ask-from-crates-io.md)決定6、`--from`・`--rollback`とは併用できない）は、crates.ioのリリースを元にする人の入口で、supervisorの`approve_release`の答えが起動するjob（[Release update](release-update.md)の「入れ替えのjob」）と同じ置き場と`cargo install`を使う。supervisorが居ないときや、askを待たずに上げたいときに打つ。配線は`src/compose.rs`の`OneShot::install_release`。

- VERSIONは`X.Y.Z`だけを受ける（それ以外はcargoを打つ前にerror）。省略すると（`--release`だけ）sparse indexを`curl`で読み、yankedとpre-releaseを除いた最新にする（`application::release_update::resolve_version`）。
- `install::release_binary`: `<to>`の`--version`がすでにVERSIONならそのfileを元にし（cargoを打たない）、そうでなければ`cargo install --locked dagq@VERSION --root <queue dir>/update/release --target-dir <queue dir>/update/target`（`infrastructure::binaries::CargoInstaller`、出力は`<queue dir>/logs/install-release-VERSION.log`）の`<root>/bin/dagq`を元にする。cargoが見つからない・失敗したら何も置き換えずに`install release VERSION: …`のerror。隠しflagの`--cargo`はtestがstubを渡す。
- その後は上の2〜6と同じ（`Source::Binary`）。非互換のmigrationは`--allow-breaking`が無ければerror、あればdrain。結果は`install`の結果に`release`（VERSION）と`log`を足したもの。
- test: `tests/it/cli_install_release.rs`がstubの`cargo`で、失敗で置き換えないこと、成功で`--root`と`--target-dir`の引数と置き換え（`.previous`）、リリースでないVERSIONの拒否を確かめる。

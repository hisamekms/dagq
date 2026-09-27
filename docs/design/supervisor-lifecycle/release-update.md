---
id: design-supervisor-lifecycle-release-update
type: design
title: "Release update"
status: draft
created: 2026-09-27
updated: 2026-09-27
last_verified: 2026-09-27
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

**実装状況**: 決定済み・未実装。以下は実装で決める細部を含む予定の姿で、実装のtaskがこの文書を今の姿に直してstatusを`current`にする。今は、人が`cargo install --locked dagq`と`claude plugin update claude-dagq@dagq`を打ってから`dagq up`を打つと、`up`がbuild識別子の違いでsupervisorを引き継がせる（[`up`](up-down.md)、ADR-0073決定15）。

## 対象

- supervisorの`binary_version`（自分のbuild識別子）がpre-releaseもbuild metadataも無い`X.Y.Z`のときだけ動く。`X.Y.Z-dev+…`（`+unknown`・`.dirty`を含む）では調べない。
- ADR-t614-1の「dagqのソースか」の判定は使わない。

## 設定（host.tomlの`[update]`）

- 置き場所と優先は[push](push.md)の`[push]`と同じ: `<queue dir>/host.toml`の`[update]`が表ごと優先し、無ければ`$XDG_CONFIG_HOME/dagq/host.toml`（無ければ`~/.config/dagq/host.toml`）。`dagq.toml`からは読まない。
- `release = "ask" | "auto" | "off"`。既定は`"ask"`。`"auto"`はaskを開かずに入れ替える（非互換のmigrationは除く）。`"off"`は調べない。
- `check_interval_secs`（既定86400）は検知の間隔。調べるかどうかは`release`だけが決め、間隔で切る手段は置かない。
- 形の違う値は既定として扱い、`doctor`が警告する。
- supervisorは設定をpassごとに読み、登録にもflagにも持たない。execの引き継ぎや`up`の打ち直しで設定は変わらない。

## 検知

- 読むのはsparse indexの`https://index.crates.io/da/gq/dagq`（4文字以上のcrateの`<先頭2文字>/<次の2文字>/<name>`）。1行が1 versionのJSONで、`vers`と`yanked`を読む。`yanked: true`とpre-release（`-`を含む）を除いた最大のversion（SemVerの順）が最新。
- runtimeにHTTPのclientは足さず、`curl -fsS --max-time 10`を子プロセスで打つ（macOSに標準である）。前回の`ETag`を`If-None-Match`で送り、304なら前の結果を使う。
- supervisorは起動の後の最初のpassと、前の検知から`check_interval_secs`経ったpassで調べる。同じqueueの複数のsupervisorは、最新の`release_checked` / `release_check_failed`の時刻を見て、間隔の内なら調べない。
- `update_failed`のask（`retry` / `skip`）の答えは、このjobの失敗についても上の`approve_release`と同じsupervisorが適用する（source buildの`auto_update`の登録を要する今の判定`SupervisorRegistration::applies_updates`を、リリースで動き設定が`off`でないsupervisorにも広げる）。`retry`は失敗した版のjobをもう1回起動し、`skip`はその版を飛ばす。
- 結果はqueueのevent: `release_checked`（`latest`・`current`・`plugin`（読めたpluginのversion、無ければnull））、`release_check_failed`（`error`）。どちらもattentionにしない。`status`の`release_update`に`mode`・`latest`・`checked_at`・`state`を出す。

## ask

- 開く条件: `latest > current`（か、ADR-t618-2決定4のpluginだけが古い）、`mode = "ask"`、その版が`skip`されていない、同じ版の開いたaskが無い。
- kindは`approve_release`（task・runを持たない。`AskKind::ApproveRelease`）、options`install` / `skip`、`reason_category`は`scope`、`asked_by`は`supervisor`。新しい版のaskを開く前に、開いている古い版の`approve_release`を`superseded`と答えて閉じる（`open_update_ask`と同じ）。
- 答えは、liveで、自分のbuild識別子がリリースで、hostの設定の`release`が`off`でないsupervisorが適用する（`answer`は`runtime_delivers: true`）。居なければ人がinboxで読んで`dagq install --release <version>`を打つ。
  - `install`: `update_answered`（`version`・`answer`）を書き、下のjobを起動する。
  - `skip`: `update_answered`を書き、その版では開かない（次の版で開く）。

## 入れ替えのjob

`dagq install --release [<version>]`と、supervisorが起動する同じ手順のjob（source buildの`auto-update`のjobと同じくsetsidで起動し、`update_*`のeventで経過を残す）。

1. **入れる**: `cargo install --locked dagq@<version> --root <queue dir>/update/release --target-dir <queue dir>/update/target`。できるのは`<queue dir>/update/release/bin/dagq`。置き換える先（supervisorの`current_exe`、`install`では`--to`）が既にその版を名乗っていれば（hostの別のqueueの答えで入れ替わった）、cargoを打たずにそのファイルを使う。`cargo`が見つからない・失敗したら`update_failed`（`stage: build`、cargoのlogのpath）。
2. **非互換のmigration**: `migrate --check`に非互換があれば、`<queue dir>/update/staged/dagq`にcopyして`approve_update`のaskを開く（[Auto-update](auto-update.md)の2と同じ。`mode = "auto"`でも同じ）。問いに書くコマンドは`dagq --db <db> install --from <queue dir>/update/staged/dagq --to <binary> --allow-breaking --cmux … --claude …`で、source buildの場合と違い、drainの後に`up --auto-update`を打ち直させない（ソースでないrepositoryでは`up --auto-update`がerrorになる。ADR-t614-1）。`install`が起動し直したsupervisorはhost.tomlの設定をそのまま読むので、この仕組みは打ち直しなしで続く。
3. **確認・差し替え・引き継ぎ・見張り・戻し**: [Auto-update](auto-update.md)の3〜6と同じ（`install::install`を`Source::Binary`で呼ぶ）。成功すれば`update_installed`（`version`・`previous_version`・`source: "release"`）。
4. **plugin**（ADR-t618-2）: 3が成功し、supervisorに`--plugin-dir`が無ければ、supervisorの`--claude`で`claude plugin marketplace update dagq`と`claude plugin update claude-dagq@dagq`を打つ。失敗は`update_failed`（`stage: plugin`、打ったコマンドと出力）で、バイナリは戻さない。`--plugin-dir`があれば打たず、`update_installed`の`plugin`に`"skipped: plugin-dir"`を書く。
5. `update_installed`のattention（`report the update`）に、inboxとplannerを開き直すと新しいpluginが効くことを書く。

pluginのversionの読み方（`claude plugin list`の出力か、Claude Codeのinstall済みのpluginの記録）は、ADR-t617-2決定4のpluginの確認と同じ実装のtaskが決め、ここに書く。

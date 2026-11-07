---
id: design-plugin-integration
type: design
title: Claude Code and Codex plugin integration
status: current
created: 2026-09-21
scope: distribution
related:
  - adr-t655-1
  - adr-0005
  - adr-0006
  - adr-0010
  - adr-0016
  - adr-0018
  - adr-0019
  - adr-0021
  - adr-0026
  - adr-0028
  - adr-0030
  - adr-0047
  - adr-0048
  - adr-t617-1
  - adr-t617-2
  - adr-t906-1
  - adr-t1394-1
---

# Claude Code and Codex plugin integration

runtimeとpluginを分離する。
pluginはskill、hook、provider設定を配布し、SQLiteとsupervisorは`dagq`バイナリが担当する。

```text
dagq repository
  ├── runtime binary
  ├── .claude-plugin/marketplace.json  (Claude Code の marketplace としての自己申告)
  ├── plugins/claude-dagq
  └── plugins/codex-dagq (未着手)
```

Claude Code pluginは`.claude-plugin/plugin.json`と`skills/`を持ち、Codex pluginは`.codex-plugin/plugin.json`とskills/hooks/scriptsを持つ。
共通の手順はshared skillから生成または同期する。
pluginのskillからPATH上の`dagq`を呼び出し、見つからなければインストール方法を案内する。

platformごとのbinary release、checksum、version compatibilityはruntime側で管理する（[配布](#配布)）。
pluginはruntimeのDB schemaを直接操作しない。

## 配布

binary releaseとchecksumはruntime側の責務なので（[ADR-0005](../adr/0005-binary-and-plugin-distribution.md)）、releaseはGitHub Actionsの`.github/workflows/release.yml`が`v*`のtag pushで作る。
targetは`aarch64-apple-darwin`だけ（他のplatformは未対応）。

### tagとversionの一致規則

tagは`v<version>`で、versionが一致しなければbuildの前に止まる。
比較の対象とtagを渡したときの規則は`scripts/check-plugin-version.sh`の冒頭のコメント、実行の入口は`release.yml`。

marketplaceのentryの互換と検査の出力も同scriptのコメントが持つ（[ADR-t617-1](../adr/2026-09-27-t617-1-plugin-marketplace-pinned-to-release-tag.md)決定3・4）。
リリース済みmigrationの不変を守る検査の入口は`scripts/check-migration-numbers.sh`（[Persistence](persistence.md)）。

### artifact

Releaseには、バイナリのarchiveとそのchecksumの`SHA256SUMS`の2つのファイルを添付する（名前と中身は`release.yml`）。

### checksumの検証とインストール

利用者のarchiveとcrates.ioからのインストール、リリースの手順は[配布の手順](../development/plugin-distribution.md)にある。
checksumの検証が通らないarchiveは展開しない。

### crates.io（ADR-0030）

crates.ioは追加の経路で、GitHub Releaseのartifactは変わらない（[ADR-0030](../adr/0030-publish-to-crates-io-on-tag-push-with-trusted-publishing.md)）。
publishの順序・再実行時のskip・Trusted Publishingの認証は`release.yml`のコメントとstepが入口。
許す第三者actionはcrates.io公式の認証actionだけで、長期tokenのsecretは持たない。

## Claude Code plugin (`plugins/claude-dagq`)

Claude Codeのplugin形式（`.claude-plugin/plugin.json`、`skills/<name>/SKILL.md`、`hooks/hooks.json`、`bin/`）に従う。
hookは`SessionStart`・`Stop`・`SessionEnd`だけで（ADR-0016、ADR-0048の決定6、[ADR-t906-1](../adr/2026-09-28-t906-1-guarantee-the-inbox-watch.md)）、agent・MCPは持たない。

| 知りたいこと | 入口（`plugins/claude-dagq/`からのpath） |
| --- | --- |
| manifestとhookの配線 | `.claude-plugin/plugin.json`と`hooks/hooks.json` |
| バイナリの解決 | `bin/dagq`（下の「launcher」） |
| 起き直し・watchの保証・区間の記録 | `hooks/`（下の各hookの節） |
| 登録と参照 | `skills/dagq/SKILL.md` |
| 人との中継 | `skills/dagq-inbox/SKILL.md` |
| runtimeの計画とdraftの判断 | `skills/dagq-planner/SKILL.md` |
| 人が指示する復旧 | `skills/dagq-recover/SKILL.md` |
| 場面ごとの手順と判断の規則 | 各skillの`reference/` |

各`SKILL.md`は手順だけを書いて小さく収め（大きさの上限は`tests/plugin.rs`が確かめる）、場面ごとの役割の手順と判断の規則は各skillの`reference/`に置いて本文から「必要な時に読む」と指す。
`reference/`はコマンドのflagと出力の欄を列挙せず、`dagq <command> --help`と出力そのものに任せる。
skillは呼ぶたびに読み込まれcompaction後にも読み直されるので、読み込み単位を小さくする。
descriptionはtriggerが重ならないように書き分け、各`SKILL.md`のdescriptionを正とする。

常駐sessionが使うCLIの手順はskillに集め、AGENTS.mdにはrepository固有の注意だけを残す（ADR-0010）。
レビューと着地はsupervisorのreview job（ADR-0027）、失敗runはtriage jobが行い、残る手で行う手順は`dagq-recover`が持ち、inboxが人の指示でそれを実行する（ADR-0024、[overview](overview.md#用語集)の「退役した役割」）。
`up`の手順は`dagq-recover`のsection 5を指す（`up`はplannerを開かず、plannerはruntimeだけが立てる。ADR-t1394-1）。
inbox / plannerの初期promptはruntimeが生成し（[Prompt](supervisor-lifecycle/prompt.md)）、それぞれ`dagq-inbox` / `dagq-planner` skillを指す。

### 起き直しhook（ADR-0016）

inbox（唯一の常駐session）とplanner（proposalごとのオンデマンドのsession）は状態を持たず、起き直しをhookが自動化する。
source・roleごとの出力と失敗時の扱いは`hooks/session-start.sh`の冒頭のコメント、配線は`hooks/hooks.json`が入口。
roleとqueueは起動側が渡す（[Roles](supervisor-lifecycle/roles.md)、[`dagq inbox`](supervisor-lifecycle/up-down.md#dagq-inbox)）。
workerはruntimeのsettingsで起動されpluginを読まず、読んでもroleが異なるので影響しない。

出力には役割を示すテキストの1行が必須で、JSONだけだとClaude Codeはhookの制御JSONと解釈し、未知のキーを捨ててcontextに何も足さない。
`status`の言語の指示も出力するので、初期promptが失われても指示が残る（[Language](supervisor-lifecycle/language.md#promptへの渡し方)）。
inboxはwatchを最初の一手として張る。
watchをshellのループで囲むと、語分割でcursorが空になったループが空回りし、askが届かなくなる。
hookの後の役割の手順は各skillに戻し、watchの結果でintegrateは呼ばない。
Codexの同じ役目は[Provider lifecycle](provider-lifecycle.md#inboxの起動)のinboxの起動を読む。

### watchを張らせるStop hook（ADR-t906-1）

inboxのClaude sessionを起こすのは`watch --role inbox`の終了だけで、supervisorはinboxに打ち込まず（watcherが居ないときは`inbox_nudged`と`[push]`。[inboxへの知らせ](supervisor-lifecycle/ask-push.md#supervisorによるinboxへの知らせ)、[ADR-t1433-5](../adr/2026-10-03-t1433-5-inbox-watch-without-typing-into-the-inbox.md)）、watchの`[push]`の通知はsessionを起こさない。
`/clear`・compaction・再起動の後にwatchを張り直さずにturnを終えると、その後に開いたaskは人に届かない。
Stop hookがそのturnの終わりを止める。

条件と制御JSONの形、失敗時の扱いは`hooks/stop-watch.sh`の冒頭のコメント、配線は`hooks/hooks.json`が入口。
watchが返った後に報告だけしてturnを終えると、そのまま誰も起こさなくなるので、Stop hookは猶予を使わない。
statusの猶予は居ない時間を数えすぎないためのもので、目的が異なる。

### sessionの区間hook（ADR-0048）

runtimeがheadlessで起動しないinbox・plannerのClaude sessionの区間は、pluginのhookが記録する（[ADR-0048](../adr/0048-record-claude-sessions-by-kind-with-open-and-active-time.md)の決定6）。
区間の書き方・推定の終了・同時sessionの制約は[provider-lifecycle](provider-lifecycle.md#claude-sessionの区間)、集計は[stats](supervisor-lifecycle/commands.md)。
runtimeのplanner（全て非対話）の区間はturnから記録する（[ADR-t1394-2](../adr/2026-10-03-t1394-2-runtime-planner-route-interactive-or-headless.md)の決定4）ので、そのturnの`claude -p`がこのhookを走らせても、hookはqueueに書かず報告をwrapperに渡し、hookの開いている区間にheadlessの区間を含めない。

hookの配線は`hooks/hooks.json`、渡す条件と失敗時の扱いは`hooks/session-event.sh`の冒頭のコメントを読む。
区間のkindと開閉の判断は`domain::sessions::hook_kind`と`hook_changes`、取り込みは`infrastructure::sessions::record_hook`が入口。
`/clear`とcompactionで二重に数えず、時間の閾値でsessionを終了させない。
closeはtranscriptが読めなくても成立し、取り込みはsupervisorに任せる。
terminalは問わず、古いbinaryが記録したworkspace IDも読まない。
失敗でsessionを止めず、区間のeventだけを書きrun・proposal・plannerの状態を変えない。
Codexのinboxの記録とtrustの落とし穴は[provider-lifecycle](provider-lifecycle.md#inboxの区間)を読む。

### launcher

skillはすべて`${CLAUDE_PLUGIN_ROOT}/bin/dagq`を呼ぶ。
launcherはバイナリを解決してcwdのまま`dagq <args>`を`exec`するだけで、DBのpathを計算せず、DBも開かない（[ADR-0006](../adr/0006-queue-per-repository.md)）。

解決の順序・queueの指定・`--resolve`とversion不一致時の出力は`bin/dagq`のコメント、報告と更新の案内は[`reference/locate.md`](../../plugins/claude-dagq/skills/dagq/reference/locate.md)を読む。
major.minorが異なっても解決自体は有効なので止めず、CLIの差が不明であることを警告する。
launcherはReleaseのasset名やこのrepositoryの開発に固有の手順を案内しない（[ADR-t617-1](../adr/2026-09-27-t617-1-plugin-marketplace-pinned-to-release-tag.md)）。

### skillの契約

完了はStop hookやreceiptファイルの存在ではなく、showのrunの状態とeventで判定する。
readyにするのはplan reviewだけで、goal closeはplannerの手順ではない。
登録は[`reference/register.md`](../../plugins/claude-dagq/skills/dagq/reference/register.md)、goalの終了は[`reference/goal-close.md`](../../plugins/claude-dagq/skills/dagq/reference/goal-close.md)、follow_upの採否と再検討は[Draft planners](supervisor-lifecycle/draft-planners.md)を読む。

supervisorの起動と停止の手順は[`dagq-recover`のup/down](../../plugins/claude-dagq/skills/dagq-recover/reference/up-down.md)が入口。
upはinboxとplannerを開かず、skillは重複起動を自分で判定しない（[up/down](supervisor-lifecycle/up-down.md)）。
着地はintegrateだけが行い、inboxはaskに答えるだけ（[Review](supervisor-lifecycle/review.md)、[Integrate](supervisor-lifecycle/integrate.md)）。
手でreviewする例外の手順は[`reference/review-by-hand.md`](../../plugins/claude-dagq/skills/dagq-recover/reference/review-by-hand.md)、resumeと失敗後の扱いは[Needs session](supervisor-lifecycle/needs-session.md)、wrapperの停止とlogは[非対話のworker](supervisor-lifecycle/headless-worker.md)を読む。
recoverの拒否はバイナリが判断し、skillはprocessをkillせずdoctorの理由を人に示す。
圧縮形の出力と全文の使い分けは[`reference/inspect.md`](../../plugins/claude-dagq/skills/dagq/reference/inspect.md)と[`doctor.md`](../../plugins/claude-dagq/skills/dagq-recover/reference/doctor.md)が入口。

### marketplaceとinstall

marketplaceの入口はrootの`.claude-plugin/marketplace.json`、利用者のinstallと更新は[配布の手順](../development/plugin-distribution.md#marketplaceからのインストール)を読む。
runtimeバイナリはpluginに同梱しない。

リリースのtagに固定する決定は[ADR-t617-1](../adr/2026-09-27-t617-1-plugin-marketplace-pinned-to-release-tag.md)、install済みpluginと開発用checkoutの境界は[ADR-t617-2](../adr/2026-09-27-t617-2-installed-plugin-by-default-plugin-dir-for-development.md)。
固定の実装が入るまではmainのHEADのpluginを配る。
entryの形と互換は`scripts/check-plugin-version.sh`のコメント、リリースの変更は[release skill](../../.claude/skills/release/SKILL.md)を読む。
Anthropicのdirectoryと公式marketplaceには出さない。
外部projectのバイナリとpluginを同じリリースへ更新する約束と承認は[Release update](supervisor-lifecycle/release-update.md)にある（[ADR-t618-2](../adr/2026-09-27-t618-2-plugin-follows-the-release-update.md)）。

### installしたpluginの確認

[ADR-t617-2](../adr/2026-09-27-t617-2-installed-plugin-by-default-plugin-dir-for-development.md)の決定4。
`--plugin-dir`を付けない`dagq up`と`dagq inbox`は、inboxとruntimeのplannerが読むinstall済みの`claude-dagq`を、何かを開く前に確かめる（[`up` / `down`](supervisor-lifecycle/up-down.md)のpreflight）。
runtimeが打つupは確かめない。
判定と文言、確かめない経路は`lifecycle::require_installed_plugin`と`UpEnvironment::restart`、引き継ぎの環境変数の互換は`UP_RESTART_ENV`のdoc commentを読む。
project scopeを数えるcwdと出力の共有は`adapters::ClaudeCode::installed_plugin`、一覧の判定は`adapters::plugin_state`が入口。

### 読み込みと検証

開発時の実CLIの読み込みと検証は[配布の手順](../development/plugin-distribution.md#開発時の読み込みと検証)を読む。

- `tests/plugin.rs`がmanifest、marketplace manifest、launcher、skillとreferenceの形と汎用性（受け持ちは[文書の規則](../development/documents.md)の「pluginの汎用性」）、hookを実バイナリで確かめる。
  確かめる項目と固有の印の一覧はtestが持つ。
  テストは`XDG_DATA_HOME`を一時dirに向け、開発者の実queueに触れない。
  skillに書いたコマンド列のうち自動テストにしないものは、workerが実バイナリと使い捨てqueueで確かめず、人かinboxに任せる（[workerがhostと実queueでできないこと](../development/operations.md#workerがhostと実queueでできないこと)）。

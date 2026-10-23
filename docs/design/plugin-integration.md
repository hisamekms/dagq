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

Claude Code pluginは`.claude-plugin/plugin.json`と`skills/`を持ち、Codex pluginは`.codex-plugin/plugin.json`とskills/hooks/scriptsを持つ。共通の手順はshared skillから生成または同期する。pluginのskillからPATH上の`dagq`を呼び出し、見つからなければインストール方法を案内する。

platformごとのbinary release、checksum、version compatibilityはruntime側で管理する（[配布](#配布)）。pluginはruntimeのDB schemaを直接操作しない。

## 配布

binary releaseとchecksumはruntime側の責務なので（[ADR-0005](../adr/0005-binary-and-plugin-distribution.md)）、releaseはGitHub Actionsの`.github/workflows/release.yml`が`v*`のtag pushで作る。
targetは`aarch64-apple-darwin`だけ（他のplatformは未対応）。

### tagとversionの一致規則

tagは`v<version>`で、`<version>`は`Cargo.toml`の`[package].version`と完全に一致する（例: version `0.2.0` には tag `v0.2.0`）。workflowはcheckoutの直後、buildより前のstepで`GITHUB_REF_NAME`から先頭の`v`を外したものと`Cargo.toml`の値を比べ、違えばそこでfailする。pluginの`.claude-plugin/plugin.json`のversionもクレートと同じ値にそろえる。

versionの一致とmarketplaceのentryの形は`scripts/check-plugin-version.sh`が検査し（[ADR-t617-1](../adr/2026-09-27-t617-1-plugin-marketplace-pinned-to-release-tag.md)の決定3・4）、一致しないものをファイルと両方の値つきで出してexit 1にする。
CIと`release.yml`（buildより前に、tagを渡して）が実行する。
いつ何を比べるか（常に・リリースのversionのとき・tagを渡したとき）と、受けるentryの2つの形（相対のsourceとgit-subdir）は、scriptの先頭のcommentが持つ。

migrationについては`release.yml`が同じくbuildの前に`scripts/check-migration-numbers.sh --release "$GITHUB_REF_NAME"`を実行し、前のリリースのtagのmigrationが変わっていないことを検査する（[Persistence](persistence.md)のリリース済みのmigration）。

### artifact

Releaseには、バイナリのarchiveとそのchecksumの`SHA256SUMS`の2つのファイルを添付する（名前と中身は`release.yml`）。

### checksumの検証とインストール

同じdirectoryに両方を置いて検証する。

```sh
VERSION=0.2.0
gh release download "v$VERSION" --repo hisamekms/dagq
shasum -a 256 -c SHA256SUMS
tar -xzf "dagq-v$VERSION-aarch64-apple-darwin.tar.gz"
mkdir -p ~/.local/bin
install -m 755 dagq ~/.local/bin/dagq
```

`shasum -a 256 -c SHA256SUMS`が`OK`を出さないarchiveは展開しない。`~/.local/bin`をPATHに入れておくと、pluginのlauncherもsupervisorの起動も同じバイナリを解決する。

### crates.io（ADR-0030）

crates.ioは追加の経路で、GitHub Releaseのartifactは上のとおり変わらない。
release.ymlはReleaseに添付し終えた後、crates.ioに同じversionがあればskipし、無ければ`cargo publish --locked`する。
認証はTrusted Publishingで短期tokenを得て、長期tokenのsecretは持たない。
release.ymlで許す第三者actionはその`rust-lang/crates-io-auth-action`だけ。

packageは`Cargo.toml`の`include`でbuildに要るものに絞り、`cargo publish --dry-run --locked`で確かめる。

利用者は`cargo install --locked dagq`でsourceからbuildする（RustとCコンパイラが要る。対応はmacOS Apple Siliconだけで変わらない）。
入る場所は`~/.cargo/bin/dagq`なので、`~/.local/bin/dagq`と併用せずPATH上の`dagq`を1つにする。
更新は同じ`cargo install --locked dagq`で上書きしてから`dagq up`（version違いのsupervisorを入れ替える）。

リリースは`Cargo.toml`と`plugin.json`のversionを上げてmainに着地し、`v<version>`のtagをpushすると、GitHub Releaseとcrates.ioの両方に出る。
Trusted Publisherは既存のcrateにしか登録できないので、最初の1回はユーザーが手で`cargo publish`し、crates.ioでTrusted Publisherを登録する（手順はADR-0030の決定5）。

## Claude Code plugin (`plugins/claude-dagq`)

Claude Codeのplugin形式（`.claude-plugin/plugin.json`、`skills/<name>/SKILL.md`、`hooks/hooks.json`、`bin/`）に従う。
hookは`SessionStart`・`Stop`・`SessionEnd`だけで（ADR-0016、ADR-0048の決定6、[ADR-t906-1](../adr/2026-09-28-t906-1-guarantee-the-inbox-watch.md)）、agent・MCPは持たない。

```text
plugins/claude-dagq/
  .claude-plugin/plugin.json      name "claude-dagq"、version はクレートと同じ
  bin/dagq                        launcher（POSIX sh。下の「launcher」）
  hooks/hooks.json                SessionStart・Stop・SessionEnd で下の 3 つの script を呼ぶ
  hooks/session-start.sh          inbox / planner の起き直しの出力（下の「起き直しhook」）
  hooks/stop-watch.sh             watch の無い inbox の turn の終わりを止める（下の「watchを張らせるStop hook」）
  hooks/session-event.sh          inbox / planner の session の区間を記録する（下の「sessionの区間hook」）
  skills/dagq/                    バイナリと DB の解決、goal の登録と task への分解、参照コマンド
  skills/dagq-inbox/              inbox のループ: ask と attention を人に知らせ、人の指示があるときだけ dagq-recover の手順を実行する。自分では判断しない。人が頼んだ計画は request add で runtime の planner に移譲する
  skills/dagq-planner/SKILL.md    runtime だけが立てる planner: 依頼の計画、goal / task の登録と submit（ready にはしない）、draft の採否
  skills/dagq-recover/            人が手で行うこと（inbox か人の DAGQ_ROLE の無い terminal から、人の指示で）
  skills/*/reference/             本文から「必要な時に読む」と指す役割の手順・判断の規則と --help への案内
```

各`SKILL.md`は手順だけを書いて小さく収め（大きさの上限は`tests/plugin.rs`が確かめる）、場面ごとの役割の手順と判断の規則は各skillの`reference/`に置いて本文から「必要な時に読む」と指す。
`reference/`はコマンドのflagと出力の欄を列挙せず、`dagq <command> --help`と出力そのものに任せる。
skillは呼ぶたびに読み込まれcompaction後にも読み直されるので、読み込み単位を小さくする。
descriptionはtriggerが重ならないように書き分ける: `dagq`は登録と参照、`dagq-planner`はruntimeが立てるplanner session（`DAGQ_ROLE=planner`）の依頼の計画・登録・draftの判断・goal close、`dagq-inbox`はinbox session（`DAGQ_ROLE=inbox`）のaskとattentionの中継と人が頼んだ計画の依頼（`request add`）、`dagq-recover`は人が手で行うこと（`dagq-inbox`と`dagq-planner`は[ADR-0022](../adr/0022-ask-answer-inbox-planner-and-landing-on-doubt.md)）。

常駐sessionが使うCLIの手順はskillに集め、AGENTS.mdにはrepository固有の注意だけを残す（ADR-0010）。
レビューと着地はsupervisorのreview job（ADR-0027）、失敗runはtriage jobが行い、残る手で行う手順は`dagq-recover`が持ち、inboxが人の指示でそれを実行する（ADR-0024、[overview](overview.md#用語集)の「退役した役割」）。
`up`の手順は`dagq-recover`のsection 5を指す（`up`はplannerを開かず、plannerはruntimeだけが立てる。ADR-t1394-1）。
inbox / plannerの初期promptはruntimeが生成し（[Prompt](supervisor-lifecycle/prompt.md)）、それぞれ`dagq-inbox` / `dagq-planner` skillを指す。

### 起き直しhook（ADR-0016）

inbox（唯一の常駐session）とplanner（proposalごとのオンデマンドのsession）は状態を持たないsessionで、起動・resume・compaction・`/clear`からの起き直しをhookが自動化する。

- `hooks/hooks.json`は`SessionStart`にmatcherの無い1グループを置き、全部のsourceで`${CLAUDE_PLUGIN_ROOT}/hooks/session-start.sh`を呼ぶ（[ADR-t906-1](../adr/2026-09-28-t906-1-guarantee-the-inbox-watch.md)）。
  scriptはhookのstdinの`source`を読み、plannerの起動とresumeでは何も出さない（起動は初期promptが担い、resumeは元のcontextを持つ）。
  inboxは全部のsourceで出す（`/clear`の後だけでなく、起動・resumeでもwatchを張ることを最初の一手にするため）。
  stdinが無い・`source`が読めないときはcompact / clearと同じく出す。
- hookはroleとqueueを環境変数（`DAGQ_ROLE`・`DAGQ_QUEUE`）から得る。
  inboxは人が`dagq inbox`で開き、`up`はinboxを開かない（渡し方は[Roles](supervisor-lifecycle/roles.md)のinboxと[`dagq inbox`](supervisor-lifecycle/up-down.md#dagq-inbox)、[ADR-t2159-1](../adr/2026-10-09-t2159-1-dagq-does-not-use-cmux-and-the-person-opens-the-inbox.md)決定2・3）。
- `session-start.sh`（成功時のstdoutは役割の1行と`status`のJSONで、stderrは混ぜない）は`DAGQ_ROLE`が`inbox` / `planner`のどちらでもなければ何も出力せずexit 0する。
  workerはruntimeの`--settings`で起動されpluginを読まないが、読んだとしてもroleが違うので影響しない。
- その2つなら、先頭に役割とskillの1行を出し、続けて`bin/dagq status --role <role>`をそのまま出す。
  `status`は言語の設定があるときの指示の文面も持つので（[Language](supervisor-lifecycle/language.md#promptへの渡し方)、ADR-t616-2）、compactionと`/clear`で初期promptが失われても指示が残る。
  Claude Codeはこのstdoutをcontextに入れる。
  先頭の1行は必須で、Claude CodeはstdoutがJSONとして読めればhookの制御JSONとして解釈し、未知のキーを捨ててcontextに何も足さない。
  `DAGQ_QUEUE`があり`DAGQ_DB`が無ければ`DAGQ_DB`にしてlauncherに渡すので、cwdがrepositoryの外でもそのsessionのqueueを読む。
  inboxでは役割の1行の前に、最初の一手として`status`のcursorから`watch --role inbox --until-attention`を張ることを命じる1行を出す。
  このwatchはattentionが来たときだけ返るので、sessionはshellのループを書かない（語分割でcursorが空になったループは空回りし、askが届かなくなる）。
- バイナリが見つからない（`DAGQ_BIN`が実行可能でない、PATHに`dagq`が無い）時や`status`が失敗した時も1行だけ理由を出してexit 0し、session開始を止めない。
- inboxはhookの出力を起点に`dagq-inbox`の手順（openなaskを人に見せ、残りのattentionを知らせ、`watch --role inbox`を再開）へ、plannerは`dagq-planner`の手順へ戻る。`watch`の結果で`integrate`は呼ばない。

### watchを張らせるStop hook（ADR-t906-1）

inboxのClaude sessionを起こすのは`watch --role inbox`の終了だけで、supervisorはinboxに打ち込まず（watcherが居ないときは`inbox_nudged`と`[push]`。[inboxへの知らせ](supervisor-lifecycle/ask-push.md#supervisorによるinboxへの知らせ)、[ADR-t1433-5](../adr/2026-10-03-t1433-5-inbox-watch-without-typing-into-the-inbox.md)）、watchの`[push]`の通知はsessionを起こさない。
`/clear`・compaction・再起動の後にwatchを張り直さずにturnを終えると、その後に開いたaskは人に届かない。
Stop hookがそのturnの終わりを止める。

- `hooks/hooks.json`は`Stop`（matcherなし）で`${CLAUDE_PLUGIN_ROOT}/hooks/stop-watch.sh`を呼ぶ。
- `stop-watch.sh`は`DAGQ_ROLE`が`inbox`のときだけ働き、生きている`watch --role inbox`が無ければturnの終わりを止める。
  数え方と出力の形はscriptの冒頭のコメントが持つ。
  reasonは未処理のwatchの出力の扱いとwatchの張り方を書く（文面もscriptが持つ）。
  Claude Codeはturnを続けてreasonをmodelに渡す。
- 猶予（watchが返った後の時間）を使わないのは、watchが返った後にinboxが報告だけしてturnを終えると、猶予の中では止められず、そのまま誰も起こさなくなるため。
  `status`の猶予は居ない時間を数えすぎないためのもの。
- それ以外は何も出さずexit 0で、hookはsessionを止めない（場合の一覧はscriptの冒頭のコメント）。

### sessionの区間hook（ADR-0048）

runtimeがheadlessで起動しないinbox・plannerのClaude sessionの区間は、pluginのhookが記録する（[ADR-0048](../adr/0048-record-claude-sessions-by-kind-with-open-and-active-time.md)の決定6）。
区間の書き方と推定の終了は[provider-lifecycle](provider-lifecycle.md#claude-sessionの区間)、集計は[stats](supervisor-lifecycle/commands.md)。
runtimeのplanner（全て非対話）の区間はturnから記録する（[ADR-t1394-2](../adr/2026-10-03-t1394-2-runtime-planner-route-interactive-or-headless.md)の決定4）ので、そのturnの`claude -p`がこのhookを走らせても、hookはqueueに書かず報告をwrapperに渡し、hookの開いている区間にheadlessの区間を含めない。

- `hooks/hooks.json`は`SessionStart`にmatcherの無い2つ目のグループ（全部のsource）で`${CLAUDE_PLUGIN_ROOT}/hooks/session-event.sh open`を、`SessionEnd`（matcherなし）で`session-event.sh close`を呼ぶ。
  起き直しの`session-start.sh`のグループと出力はそのまま。
- `session-event.sh`は`DAGQ_ROLE`が`inbox` / `planner`で`DAGQ_QUEUE`があるときだけ、hookのstdinをそのまま`bin/dagq session-event open|close`（隠しコマンド）に渡す。
  `DAGQ_DB`が無ければ`DAGQ_QUEUE`を`DAGQ_DB`にする。
  それ以外のsession（workerを含む。workerの区間はruntimeが書く）では何もしない。
- 区間のkindは`domain::sessions`の`hook_kind`が決める（読む環境変数は`SessionHook`のdoc comment）。
- `/clear`とcompactionで二重に数えない: 同じsession_idの`SessionStart`（`resume`・`compact`）は開いている区間を続け、別のsession_idの`SessionStart`は同kindの開いている区間を推定の終了で閉じてから開き（時間の閾値は使わない。次の開始1回が契機）、閉じた区間への2回目の`SessionEnd`は何も書かない。
- closeはtranscriptを読まない: `SessionEnd` と次の `SessionStart` によるcloseは先に `session_closed` をcommitし、残りの稼働時間・tokens・modelの取り込みはsupervisorに任せる。
  読めなくても区間は閉じたまま。
  sessionが動くterminalは問わず、前のbinaryが区間に記録したworkspace IDは読まない。
  同じIDのresume・compactや別kindの区間には影響しない。
  詳細と同時sessionの制約は[provider-lifecycle](provider-lifecycle.md#claude-sessionの区間)。
- 失敗してもsessionを止めない: 何も出力せず（`SessionStart`のstdoutはcontextに入るので）、`dagq`が無い・実行できない、queueが開けない、入力にsession_idが無い、記録に失敗した、のどれでもexit 0する。記録のCLIは区間のeventだけを書き、run・proposal・plannerの状態を変えない。

### launcher

skillはすべて`${CLAUDE_PLUGIN_ROOT}/bin/dagq`を呼ぶ。launcherはバイナリを解決してcwdのまま`dagq <args>`を`exec`するだけで、DBのpathを計算せず、DBも開かない（[ADR-0006](../adr/0006-queue-per-repository.md)）。

- バイナリ: `DAGQ_BIN`、なければPATHの`dagq`。
  どちらもなければ、入れ方か`DAGQ_BIN`の指定を案内するerrorをstderrに出す（CLI本体のエラー形式と同じ）。
  Releaseのasset名やdagqのrepositoryの開発に固有の手順は出さない（[ADR-t617-1](../adr/2026-09-27-t617-1-plugin-marketplace-pinned-to-release-tag.md)）。
- queue: バイナリがcwdのrepositoryから解決し、launcherは`DAGQ_DB`があるときだけそれを渡す（形はlauncherの冒頭のコメント）。
  dirの作成と束縛は`init`が行う。
- `--resolve`: `dagq locate`のJSONに、バイナリと両方のversion（バイナリのものは[build識別子](supervisor-lifecycle/build-identifier.md#build-identifier)）を加えた1つのobjectを返す（errorとwarningのときの手順はpluginの`dagq`の`reference/locate.md`）。
  skillはこれをユーザーへの報告に使う。
- version不一致: pluginとバイナリのversionのmajor.minorが違うとき、stdoutの解決結果はそのまま出したうえでstderrに警告を1行出し、exitは0のまま（解決自体は正しく、CLIの差だけが不明）。
  skillは止まらずユーザーに報告し、古い方の更新（pluginは`claude plugin update claude-dagq@dagq`、バイナリは`cargo install --locked dagq`）を案内する。
  警告の文もこの2つを案内し、ReleaseのURLは出さない。

### skillの契約

- 完了はStop hookやreceiptファイルの存在ではなく、`show`のrunの状態とeventで判定する（見るものは`dagq` skill）。
- 登録の標準手順は「課題を読む（runtimeのplannerは依頼の人の言葉と参照を初期promptで受ける。ADR-t1394-1）→ `goal add`で登録 → taskに分解して`add --goal`で登録 → `lint`と`submit`でplan reviewに出す」（[ADR-0009](../adr/0009-goal-groups-tasks.md)、`ready`にするのはplan reviewだけ: [ADR-0044](../adr/0044-findings-proposals-from-findings-and-quiet-observer.md)の決定8）。
  goalなしを許すのは一発task（1 taskで終わり判断を揃える相手がいないもの）だけ（判断基準は`dagq` skill）。
- 終わったgoalのcloseはplannerの手順ではない（[ADR-0047](../adr/0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)の決定16・43）。
  supervisorのheadlessのgoal reviewのjobが閉じるか、未達をdraftにするか、`approve_goal`のaskで人に聞く（[Goal review](supervisor-lifecycle/goal-review.md)）。
  `dagq` skillの`reference/goal-close.md`がそれを案内し、`goal close`はdraft goalの破棄と人の言葉のときだけに使う。
  receiptの`follow_ups`は`integrate`が着地後に同じgoalのdraft taskとして登録する（ADR-0019の決定4）。
  着地の報告は登録されたtaskを挙げるだけで、draftごとにruntimeが立てるplannerが、採用・不採用・判断できない（`planner_question`のask）のどれかを選ぶ（[ADR-0044](../adr/0044-findings-proposals-from-findings-and-quiet-observer.md)の決定16、[Draft planners](supervisor-lifecycle/draft-planners.md)）。
  再検討の時刻を付けたdraftは、依頼が無くてもその時刻にruntimeのplannerが決め直す（[ADR-t1540-1](../adr/2026-10-05-t1540-1-a-kept-draft-returns-to-runtime-planners-at-its-revisit-time.md)、[Draft planners](supervisor-lifecycle/draft-planners.md#再検討の時刻)）。
- supervisorの起動は、人に頼まれたinbox（か人の`DAGQ_ROLE`の無いterminal）が`dagq-recover` skillのsection 5に従い`"$DAGQ" up`をlauncher経由で呼ぶ（`--plugin-dir`はrepositoryの指示が名指すpathがあるときだけで、`$CLAUDE_PLUGIN_ROOT`は渡さない。ADR-t617-2。[supervisor-lifecycle](supervisor-lifecycle/up-down.md#up--down)、ADR-0044の決定6）。
  `up`がsupervisorを常駐させ、生きているsupervisorがあればそれを使い続けるので、skillは重複起動の判定を自分では行わない。
  `up`はinboxもplannerも開かない。
  inboxは人が`dagq inbox`で開き（[Roles](supervisor-lifecycle/roles.md)のinbox、[ADR-t2159-1](../adr/2026-10-09-t2159-1-dagq-does-not-use-cmux-and-the-person-opens-the-inbox.md)決定2・3）、plannerはruntimeが立てる（ADR-t1394-1）。
  `restart supervisor`（`supervisor_stopped`と、生きているsupervisorが居ないときの`supervisor_stale`）はinboxの`watch`が拾って人に知らせ、人の指示で`up`を叩き直す（死んだ登録をpruneして起動し直す）。
  生きたsupervisorが居るときの次の手は`supervisor_attention`のdoc commentが持つ。
  同じ`restart supervisor`の`supervisor_duplicate`（同じqueueに二重のsupervisorかLaunchAgent）は、`up`だけでは同じbuildのsupervisorをreuseして直らないので、人の指示で`down --wait`の後に`up`を叩く（判定は[supervisor-lifecycle](supervisor-lifecycle/up-down.md#up--down)の3、手順は`dagq-recover`の`reference/up-down.md`）。
  停止は`dagq down`で、`--force`は実行中のrunを捨てるので人の明示の指示が要る。
  supervisorのlogの場所と書式は`dagq-recover` skillの`reference/up-down.md`が案内する（`infrastructure::telemetry`）。
- mainへの着地はruntimeの`integrate ID` / `integrate --next`が行う（rebase → 再検証 → squash、[ADR-0008](../adr/0008-merge-queue-squash-landing.md)）。
  acceptしたrunはsupervisorがsessionを開いたままheadlessでreviewし、passなら自分で着地させ、reviseは生きているsessionに返し、concernなら`approve_landing`のaskを作ってその答えを自分で適用する（[ADR-0027](../adr/0027-keep-worker-session-through-review-revise-verdict-and-merge-tree-precheck.md)、[supervisor-lifecycle](supervisor-lifecycle/review.md#review-supervisor)）。
  inboxは答えるだけ。
  headlessのreviewが失敗したrun（`review by hand`）とreview以前のrun（`review and integrate`）だけは、inboxが人の指示で`dagq-recover`の`reference/review-by-hand.md`に従い、`review ID`の`review.md`をsubagentにレビューさせて結論（`pass` / `concern`と理由）だけ受け取る。
  `pass`なら人の指示で`integrate`を呼び（`watch`の結果やイベントの副作用としては呼ばない）、`concern`なら`approve_landing`のaskを作り、その答えはsupervisorが適用する。
  `needs_session`のrunはsupervisorがresumeし（resumeのwrapperはruntimeが起動して止め、inboxはsessionを作らない）、解消しなければ`failed`にして`decide`のaskにする。
  runのsession wrapperはworkspaceなしのbackgroundのprocessだけで動く（[ADR-t1433-3](../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)）。
  `failed` / `interrupted`のrunにまだ動いているwrapperはtriageの後にsupervisorが止める。
  runのturnは`run log`で読む。
- `recover`はバイナリが拒否条件を判定する。
  skillはプロセスをkillせず、`doctor`が示す止めている理由を人に示す。
- `show`・`goal show`・`doctor`の既定は圧縮形で、skillはそれぞれの説明に全文の形と切り詰めを書き、切り詰めた欄が要る手順では全文の形を使う。

### marketplaceとinstall

repository rootの`.claude-plugin/marketplace.json`がこのrepository自身をmarketplace（名前は`dagq`）にする。
ユーザーの導線は2行。

```sh
claude plugin marketplace add hisamekms/dagq
claude plugin install claude-dagq@dagq
```

`add`はGitHubのrepositoryをcloneし、`install`はそのcloneの`./plugins/claude-dagq`からuser scopeに入れる。
更新は`claude plugin marketplace update dagq`と`claude plugin update claude-dagq@dagq`。
pluginはskillと`SessionStart` / `Stop` / `SessionEnd` hookだけでinstallに`-y`を要する宣言commandはなく、runtimeバイナリは同梱しない（`cargo install --locked dagq`で別に入れる。launcherのエラー文がその手順を持つ）。

**決定済み・未実装（[ADR-t617-1](../adr/2026-09-27-t617-1-plugin-marketplace-pinned-to-release-tag.md)、[ADR-t617-2](../adr/2026-09-27-t617-2-installed-plugin-by-default-plugin-dir-for-development.md)）**: 実装が入るまでは上のとおりmainのHEADのpluginを配る。実装後の姿は次のとおり。

- marketplaceのentryの`source`はgit-subdirの形で、`ref`は最新のリリースのtag。
  entryに`version`は書かない（versionはtagの`plugins/claude-dagq/.claude-plugin/plugin.json`の`X.Y.Z`になり、cacheは`~/.claude/plugins/cache/dagq/claude-dagq/<X.Y.Z>/`）。
  利用者の導線（`marketplace add hisamekms/dagq`・`install claude-dagq@dagq`・`plugin update claude-dagq@dagq`）は変わらない。
- `release.yml`はtagの`Cargo.toml`の`[package].version`と`plugin.json`の`version`がtagの`X.Y.Z`と一致することを検査し、違えばbuildの前に止まる（上の「tagとversionの一致規則」）。
- `-dev`を外してversionを`X.Y.Z`にするリリースの変更で、entryの`ref`をこれから打つ`vX.Y.Z`に書き換え、着地したらすぐにtagをpushする（release skillの手順）。
  `release.yml`はtagの`marketplace.json`のentryがgit-subdirの形で`ref`がそのtagであることも検査する（リリースのversionでは相対のsourceだと落ちるので、リリースの変更でentryを書き換えないとreleaseが止まる）。
- 利用者の更新はバイナリの`cargo install --locked dagq`とpluginの`claude plugin update claude-dagq@dagq`の組。
  launcherのmajor.minorの警告は残す（上の「launcher」）。
- `up`は`--plugin-dir`が無ければ`claude`に何も足さず、installしたpluginを使う。
  skillは`$CLAUDE_PLUGIN_ROOT`を`--plugin-dir`に渡さず、repositoryの指示が指定するpathがあるときだけ付ける（上のsupervisorの起動の項）。
  `--plugin-dir`が無く`claude-dagq`がinstallされて有効なことを確かめられなければ、`up`はsupervisorを起動する前に止めてinstallのコマンドを案内する（ADR-t617-2の決定4。下の「installしたpluginの確認」）。
  dagqのrepositoryは[operations](../development/operations.md)のとおり`--plugin-dir <repository>/plugins/claude-dagq`を付け、`--plugin-dir`のpluginが同じ名前のinstall済みのpluginより優先される。
- Anthropicのdirectoryと公式のmarketplace（`claude-plugins-official`）には出さない。
- 外部のprojectのリリースの更新は、バイナリを入れ替えた後にinstallしたpluginも同じリリースへ上げ、バイナリが最新でpluginだけが古いときも`approve_release`で聞く。
  `--plugin-dir`のsupervisorはpluginに触らない。
  pluginのversionは下の「installしたpluginの確認」と同じ`claude plugin list --json`から読む（[ADR-t618-2](../adr/2026-09-27-t618-2-plugin-follows-the-release-update.md)、[Release update](supervisor-lifecycle/release-update.md)）。

### installしたpluginの確認

[ADR-t617-2](../adr/2026-09-27-t617-2-installed-plugin-by-default-plugin-dir-for-development.md)の決定4。
`--plugin-dir`を付けない`dagq up`と`dagq inbox`は、inboxとruntimeのplannerが読むinstall済みの`claude-dagq`を、何かを開く前に確かめる（[`up` / `down`](supervisor-lifecycle/up-down.md)のpreflight）。
`dagq plan`は何も開かずに拒むので（[ADR-t1394-1](../adr/2026-10-03-t1394-1-abolish-person-planners-and-route-planning-through-inbox-requests.md)、[`plan` / `planners`](supervisor-lifecycle/plan-planners.md)）、確かめない。
`--plugin-dir`を付けたときは確かめない（そのpluginが同じ名前のinstall済みのpluginより優先されるため）。
runtimeが立てるplannerと`install`の引き継ぎは確かめない（supervisorに渡された`--plugin-dir`だけを使う）。
runtimeがsupervisorを起動し直すために打つ`up`（`install --allow-breaking`のdrainの後と、自動更新の戻しで登録済みのin-cmuxのsupervisorをlaunchd modeで起動し直すとき）も確かめない。
runtimeはその`up`の環境に`DAGQ_UP_RESTART`（`lifecycle::UP_RESTART_ENV`）を置く（envにしたのは、知らない古いバイナリが無視できるから）。

- 確かめ方: `claude plugin list --json`を、sessionが開くrepositoryのroot（project scopeのpluginもそこで数える）で実行する。
  entryの取り出しはリリースの更新がpluginのversionを読むのと共有する（[Release update](supervisor-lifecycle/release-update.md#pluginのversion)）。
  判定と文言の入口は`lifecycle::require_installed_plugin`。
- 止まるとき: 有効なinstallでなければ（コマンドの失敗と読めない出力を含む。判定は`adapters::plugin_state`のdoc comment）、非0で止まり、英語のerrorで上の「marketplaceとinstall」の公式のinstallのコマンドを案内する（無効のときだけ`claude plugin enable <id>`の案内が付く）。

### 読み込みと検証

- 検証: `claude plugin validate plugins/claude-dagq`（hookとskillを含む）と`claude plugin validate .claude-plugin/marketplace.json`（`--strict`も通る）、inventory: `claude --plugin-dir plugins/claude-dagq plugin details claude-dagq`。
- 開発中の読み込み: `claude --plugin-dir /path/to/dagq/plugins/claude-dagq`（そのsessionのみ）。supervisorがworkerに渡すのもこの形（`up --plugin-dir`）。
- marketplace経由のinstallは、使い捨ての`HOME` / `CLAUDE_CONFIG_DIR`でローカルpathを`marketplace add`して`install`し、`plugin list`と`plugin details`でskillが載ることを確認する。
- `tests/plugin.rs`がmanifest、marketplace manifest、launcher、skillとreferenceの形と汎用性（受け持ちは[文書の規則](../development/documents.md)の「pluginの汎用性」）、hookを実バイナリで確かめる。
  確かめる項目と固有の印の一覧はtestが持つ。
  テストは`XDG_DATA_HOME`を一時dirに向け、開発者の実queueに触れない。
  skillに書いたコマンド列のうち自動テストにしないものは、workerが実バイナリと使い捨てqueueで確かめず、人かinboxに任せる（[workerがhostと実queueでできないこと](../development/operations.md#workerがhostと実queueでできないこと)）。

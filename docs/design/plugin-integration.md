---
id: design-plugin-integration
type: design
title: Claude Code and Codex plugin integration
status: current
created: 2026-09-21
updated: 2026-10-04 # task 1467: details of the dagq, dagq-planner and dagq-recover SKILL.md moved to reference (after task 655)
last_verified: 2026-10-04 # task 1467
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

> **予定（goal 92）**: inboxのwatchの生存はADR-t906-1を置き換えた[ADR-t1433-5](../adr/2026-10-03-t1433-5-inbox-watch-without-typing-into-the-inbox.md)が決める。SessionStart / Stop hookは引き継ぎ、supervisorのinboxへの打ち込みはやめる。askの`cmux notify`はinboxの`watch --role inbox`が出す（[ADR-t1433-1](../adr/2026-10-03-t1433-1-cmux-is-used-only-by-the-inbox.md)決定2）。後続のtaskが実装するまでの今の姿である。

runtimeとpluginを分離する。pluginはskill、hook、provider設定を配布し、SQLite・supervisor・cmux操作は`dagq`バイナリが担当する。

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

binary releaseとchecksumはruntime側の責務なので（[ADR-0005](../adr/0005-binary-and-plugin-distribution.md)）、releaseはGitHub Actionsの`.github/workflows/release.yml`が作る。trigger は `v*` の tag push、runnerは`macos-14`、targetは`aarch64-apple-darwin`だけ（他のplatformは未対応）。

### tagとversionの一致規則

tagは`v<version>`で、`<version>`は`Cargo.toml`の`[package].version`と完全に一致する（例: version `0.2.0` には tag `v0.2.0`）。workflowはcheckoutの直後、buildより前のstepで`GITHUB_REF_NAME`から先頭の`v`を外したものと`Cargo.toml`の値を比べ、違えばそこでfailする。pluginの`.claude-plugin/plugin.json`のversionもクレートと同じ値にそろえる。

version の一致は`scripts/check-plugin-version.sh`が検査し（[ADR-t617-1](../adr/2026-09-27-t617-1-plugin-marketplace-pinned-to-release-tag.md)の決定3・4）、一致しないものをファイルと両方の値つきで出してexit 1にする。CI（`.github/workflows/ci.yml`）は引数なしで、`release.yml`は上のstepの後、buildより前に`--tag "$GITHUB_REF_NAME"`で実行する。

| 検査 | いつ |
| --- | --- |
| `plugins/claude-dagq/.claude-plugin/plugin.json`の`version`が`Cargo.toml`の`[package].version`と同じ | 常に（mainではどちらも`X.Y.Z-dev`） |
| `.claude-plugin/marketplace.json`のclaude-dagqのentryが下の2つの形のどちらか | 常に |
| entryがgit-subdirの形で`ref`が`vX.Y.Z` | versionがリリースの`X.Y.Z`（`-dev`などの後置が無い）のとき。`-dev`のあいだは前のリリースの`ref`のまま（切り替え前は相対のsource）でよいので比べない |
| entryがgit-subdirの形で`ref`がそのtag | tagを渡したとき（tagのmarketplaceが自分のtagを指す。決定4） |
| `Cargo.toml`と`plugin.json`のversionがtagの`X.Y.Z` | `--tag vX.Y.Z`のとき（`release.yml`） |

tagは`--tag vX.Y.Z`か、`GITHUB_REF_TYPE`が`tag`のときの`GITHUB_REF_NAME`で渡す。marketplaceのentryの形は2つだけを受ける（JSONはpython3で読む）。

- 相対のsource: `"source": "./plugins/claude-dagq"`（決定6の切り替え前の形。他の相対のpathは受けない）。
- git-subdir: `"source": {"source": "git-subdir", "url": "hisamekms/dagq", "path": "plugins/claude-dagq", "ref": "v<X.Y.Z>"}`で、entryに`version`が無い（決定2・3）。`ref`は`v`と3つの数の形。

違うものは、entryの欄（`source.ref`・`source.path`・`source.url`・`version`・相対のsource）と値を名指して出す。`tests/plugin.rs`は、scriptの写しと3つのファイルだけの一時のrepositoryで、`-dev`の相対のsource・リリースのgit-subdir・tagと一致するrefが通り、tagに対する相対のsource・refの食い違い・entryの`version`・path / urlの違い・tagでないrefが名指しでexit 1になることを確かめ、repositoryの`marketplace.json`が2つの形のどちらかで、そのpluginのdirectoryに`plugin.json`があることを確かめる。git-subdirの形の`marketplace.json`は`claude plugin validate --strict`を通る（Claude Code 2.1.283で確認）。

migrationについては`release.yml`が同じくbuildの前に`scripts/check-migration-numbers.sh --release "$GITHUB_REF_NAME"`を実行し、前のリリースのtagのmigrationが変わっていないことを検査する（[Persistence](persistence.md)のリリース済みのmigration）。

### artifact

Releaseには2つのファイルを添付する。

| ファイル | 中身 |
| --- | --- |
| `dagq-v<version>-aarch64-apple-darwin.tar.gz` | `dagq`バイナリ（`cargo build --release --locked --target aarch64-apple-darwin`）、`LICENSE`、`README.md`をアーカイブ直下に平置き |
| `SHA256SUMS` | 上のtar.gzの`shasum -a 256`出力1行 |

release notes は tag からの自動生成（`gh release create --generate-notes`）でよい。

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

crates.ioは追加の経路で、GitHub Releaseのartifactは上のとおり変わらない。release.ymlはReleaseに添付し終えた後、`https://crates.io/api/v1/crates/dagq/<version>`が200（同じversionがある）ならskipし、404なら`cargo publish --locked`する（それ以外のstatusはfail）。認証はTrusted Publishingで、jobの`id-token: write`から`rust-lang/crates-io-auth-action@v1`が短期tokenを得て`CARGO_REGISTRY_TOKEN`に渡す。長期tokenのsecretは持たない。release.ymlで許す第三者actionはこれだけ。

packageは`Cargo.toml`の`include`で`src/`、`migrations/`（`include_str!`で埋め込む）、`Cargo.toml`、`Cargo.lock`、`README.md`、`LICENSE`に絞る。`cargo publish --dry-run --locked`で確かめる。

利用者は`cargo install --locked dagq`でsourceからbuildする（Rust 1.98以上とCコンパイラ。対応はmacOS Apple Siliconだけで変わらない）。入る場所は`~/.cargo/bin/dagq`なので、`~/.local/bin/dagq`と併用せずPATH上の`dagq`を1つにする。更新は同じ`cargo install --locked dagq`で上書きしてから`dagq up`（version違いのsupervisorを入れ替える）。

リリースは`Cargo.toml`と`plugin.json`のversionを上げてmainに着地し、`v<version>`のtagをpushすると、GitHub Releaseとcrates.ioの両方に出る。Trusted Publisherは既存のcrateにしか登録できないので、最初の1回はユーザーが手で`cargo publish`し、crates.ioでrepository `hisamekms/dagq`、workflow `release.yml`、environmentなしを登録する（手順はADR-0030の決定5）。

## Claude Code plugin (`plugins/claude-dagq`)

[plan](../plans/current.md)のステップ8で実装。Claude Code 2.1.278のplugin形式（`.claude-plugin/plugin.json`、`skills/<name>/SKILL.md`、`hooks/hooks.json`、`bin/`）に従う。hookは`SessionStart`・`Stop`・`SessionEnd`だけで（ADR-0016がそれまでの「hookを持たない」を改め、ADR-0048の決定6がsessionの区間の記録を、[ADR-t906-1](../adr/2026-09-28-t906-1-guarantee-the-inbox-watch.md)がinboxのwatchを張らせるStop hookを足した）、agent・MCPは持たない。

```text
plugins/claude-dagq/
  .claude-plugin/plugin.json      name "claude-dagq"、version はクレートと同じ
  bin/dagq                        launcher（POSIX sh）
  hooks/hooks.json                SessionStart（全部）で session-start.sh と session-event.sh open、Stop で stop-watch.sh、SessionEnd で session-event.sh close を呼ぶ
  hooks/session-start.sh          DAGQ_ROLE が inbox / planner の時だけ（planner の startup / resume は除く）、inbox は watch --role inbox --until-attention を最初の一手とする 1 行、役割と skill の 1 行と dagq status --role <role> を stdout に出す
  hooks/stop-watch.sh             DAGQ_ROLE が inbox で watch --role inbox が 1 つも watching でない時だけ、turn の終わりを block して watch --role inbox --until-attention のコマンドを reason で渡す
  hooks/session-event.sh          DAGQ_ROLE が inbox / planner で DAGQ_QUEUE がある時だけ、hook の stdin を dagq session-event open|close に渡して session の区間を記録する（何も出力しない）
  skills/dagq/                    バイナリと DB の解決、goal の登録と task への分解、ready、参照コマンドの要点、結果の読み方
    reference/locate.md             install、version 警告、db_exists false と rebind、言語の設定の場所
    reference/inspect.md            参照コマンドの表と各フィールド（findings・events --full と絞り込み・timeline・observe --history を含む）、list のページング、task / run の状態、graph、goal edit / set-goal
    reference/register.md           goal と task の登録の詳細: goal の欄と draft goal、task の欄（--verify・--depends-on(-goal)・--context・--evidence・--paths・--change）、draft から ready まで（submit・plan review の pass / revise / concern・proposal withdraw・ready --bypass-review・candidates）、登録後の変更（edit・draft・cancel・dependency）、priority の段、planner の確認・revise・依頼の計画の手順
    reference/goal-close.md         goal の close（終わった goal は supervisor の goal review の job が判定して閉じる。ADR-0047。goal close は draft goal の破棄と人の言葉のときだけ）
    reference/observer.md           observer の finding の見方（findings・events・timeline・observe --history、kind: kpi の finding と改善の上限）と行き先（印からの runtime の planner、blocked の ask の propose / dismiss、人の言葉で inbox が記録する依頼（request add --ref finding:N）の planner での submit --finding / finding dismiss）
    reference/kpi.md                dagq kpi（期間・種類・層・比較・目標）、dagq mark / marks と kpi --compare での前後比較、dagq forecast の見込み（p50 / p90 と前提、流入を含まない）と forecast.* の答え合わせの KPI、dagq report と日次のレポートの場所、host.toml の [push] と送るもの・失敗の attention
  skills/dagq-inbox/              inbox のループ: status --role inbox → watch --role inbox --until-attention を background で 1 本（shell ループなし） → open な ask を人に見せて answer → それ以外の attention（回答済みの ask、止まった supervisor、失敗した review / triage / plan review、応答しない planner、runtime の planner が決めきれなかった draft と finding、push の失敗。finding に紐づく blocked の ask の propose（提案にする）/ dismiss と stalled の ask の propose は runtime が適用する）を人に知らせ、人の指示があるときだけ dagq-recover の手順を実行 → 次の watch。自分では判断しない。人が頼んだ計画は request add で依頼として記録して runtime の planner に移譲する
    reference/status.md             status / watch / events / asks / show のフィールド、attention の next の一覧、stalled の ask（促しの後に開く条件、question の中身、wait / intervene / propose の扱いと runtime が閉じる条件）、run の状態一覧
    reference/asks.md               ask の kind ごとの意味と option（approve_landing・approve_plan・decide・stalled・worker_question・planner_question・answer_prompt・stuck_exit・blocked・queue_hold・update_failed / approve_update）、runtime が適用する answer（propose / dismiss）
    reference/requests.md           人が頼んだ計画の移譲（ADR-t1394-1）: request add の打ち方（人の言葉・--note・--ref）、runtime の planner の振る舞い、requests での追い方、結末（report the request's proposal / rephrase or drop the request）の伝え方、依頼についての planner_question、開いている planner への planner request
  skills/dagq-planner/SKILL.md    planner（runtime だけが立てる。人が開く dagq plan は ADR-t1394-1 で廃止）: 依頼の計画と submit か理由付きの request decline、dagq skill による goal / task の登録、lint と submit（ready にはしない）、plan review の revise の修正と再 submit、決めきれないものだけの planner_question（task・finding・依頼）、draft の採用・不採用・ask と finding の submit --finding・finding dismiss・ask、交通整理を plan review に任せること、goal close（人の言葉があるときだけ）
  skills/dagq-recover/            人が手で行うこと（inbox か人の DAGQ_ROLE の無い terminal から、人の指示で）: supervisor が居ないときの doctor と recover、triage by hand、plan review を飛ばす ready --bypass-review と plan review by hand、up / down と固定バイナリの更新、review by hand と integrate、push の失敗、run の session への操作（stuck_exit / answer_prompt / stalled の answer の実行、届かなかった worker への answer）
    reference/doctor.md             doctor --full のフィールド、よくある場合、recover の結果と拒否条件
    reference/triage-by-hand.md     triage by hand（終わった run の復旧 job の失敗: last_error と recovery-* のファイルを読み、人の指示で ready ID か cancel ID）と recover by hand（ADR-t609-1 より前の runtime が記録した、生きている run の復旧 job の失敗: 画面を人と読み section 7 の手順で実行。今の runtime はその alert の ask を開く）
    reference/plan-review-by-hand.md plan review by hand（plan-reviews/<id>/ と proposal show を読み、人の選ぶ ready --bypass-review / submit --proposal での出し直し / cancel）と check the planner
    reference/up-down.md            up / down の出力、別 version の入替、退役した role の workspace、cmux の接続拒否と --in-cmux、in_cmux の down、log
    reference/review-by-hand.md     supervisor の review との関係、review ID → subagent → pass なら人の指示で integrate / concern なら approve_landing の ask、push の失敗、review.md の中身、integrate の再検証、follow_ups、approve_landing の answer の扱い
    reference/session.md            read-screen / send-key / send の使い方、answer_prompt の ask、届かなかった worker の answer、stalled の ask の入口
    reference/stalled.md            stalled の ask の出どころ（receipt の無い idle の促しの後、復旧 job の long_background / idle_process の escalate）、wait（supervisor が適用して close し数え直す）/ propose / intervene の扱い、intervene の手順（画面を読む、background の処理を確かめて session に止めさせる、指示を打つ、/exit で run を止める、ask close）と runtime が閉じる条件
    reference/stuck-exit.md         stuck_exit の ask の answer の実行: 確認画面の読み方、Exit and stop tasks の前の worktree と receipt の確認、/exit
    reference/resume.md             runtime の resume が送るものと終わり方、3 回で解消しないときの decide の ask
```

各`SKILL.md`は手順だけを書き8 KB以下に収め（`tests/plugin.rs`が確かめる）、出力フィールドや状態の一覧は各skillの`reference/`に置いて本文から「必要な時に読む」と指す。skillは呼ぶたびに読み込まれcompaction後にも読み直されるので、読み込み単位を小さくする。descriptionはtriggerが重ならないように書き分ける: `dagq`は登録と参照、`dagq-planner`はruntimeが立てるplanner session（`DAGQ_ROLE=planner`）の依頼の計画・登録・draftの判断・goal close、`dagq-inbox`はinbox session（`DAGQ_ROLE=inbox`）のaskとattentionの中継と人が頼んだ計画の依頼（`request add`）、`dagq-recover`は人が手で行うこと（`recover run` / `triage by hand` / `review by hand` / `review and integrate` / `push main` / `restart supervisor`、回答済みの`stuck_exit` / `answer_prompt` / `stalled`のaskの実行、`up` / `down`）。

ADR-0010は、常駐sessionが使うCLIの手順（起動・監視・レビュー・着地・停止）をskillに集め、AGENTS.mdにはrepository固有の注意だけを残すことを決めた。task 16で`taskq-run`を廃してその内容を常駐session用のskillへ移し、task 65（ADR-0016の(6)(7)(8)）でそれを3本に分け、task 88（[ADR-0022](../adr/0022-ask-answer-inbox-planner-and-landing-on-doubt.md)）で`dagq-inbox`と`dagq-planner`を足した。task 100（ADR-0024の決定1、6とConsequences）で常駐sessionの役割そのものを退役させ（[overview](overview.md#用語集)の「退役した役割」）、その3本のskillを消した: レビューと着地はsupervisorのreview job（ADR-0027）、失敗runはtriage job（task 98）が行い、残る手順（`review_failed`のときの手での`integrate`、supervisorが居ないときの`recover`、`up` / `down` / バイナリ更新、runのsessionへのキー送信）は`dagq-recover`に移し、inboxが人の指示でそれを実行する。`up`の手順は`dagq-recover`のsection 5を指す（`up`はplannerを開かず、plannerはruntimeだけが立てる。ADR-t1394-1）。inbox / plannerの初期promptはruntimeが生成し（[supervisor-lifecycle](supervisor-lifecycle/session-prompts.md#session-prompts)）、それぞれ`dagq-inbox` / `dagq-planner` skillを指す。

### 起き直しhook（ADR-0016）

inbox（唯一の常駐session）とplanner（proposalごとのオンデマンドのsession）は状態を持たないsessionで、起動・resume・compaction・`/clear`からの起き直しをhookが自動化する。

- `hooks/hooks.json`は`SessionStart`にmatcherの無い1グループを置き、`startup` / `resume` / `clear` / `compact`の全部で`${CLAUDE_PLUGIN_ROOT}/hooks/session-start.sh`を呼ぶ（[ADR-t906-1](../adr/2026-09-28-t906-1-guarantee-the-inbox-watch.md)。それまでは`compact|clear`だけだった）。scriptはhookのstdinの`source`を読み、plannerの`startup` / `resume`では何も出さない（`startup`は初期promptが担い、`resume`は元のcontextを持つ）。inboxは全部のsourceで出す（`/clear`の後だけでなく、起動・resumeでもwatchを張ることを最初の一手にするため）。stdinが無い・`source`が読めないときはcompact / clearと同じく出す。
- `DAGQ_ROLE`と`DAGQ_QUEUE`はcommandの前置きではなく、`up`とsupervisor（廃止前は`dagq plan`も）がworkspaceを作るときの`--env`で渡る（[ADR-0026](../adr/0026-identify-workspaces-by-uuid-env-and-queue-group.md)）。workspaceの全shellが継承するので、hookはそのworkspaceで`claude`を打ち直したsessionでもroleを環境変数から得る（`cmux workspace env <id> --json`で確かめられる）。値は`supervisor` / `worker` / `planner` / `inbox`（`observer`とheadlessのjobの`review-job`・`recovery-job`・`plan-review-job`・`goal-review-job`はworkspaceを持たない）。actor idの`DAGQ_ACTOR_ID`（workerは`DAGQ_RUN_ID`・`DAGQ_TASK_ID`も）も同じ`--env`で渡る（[Roles](supervisor-lifecycle/roles.md#actors)）。
- `session-start.sh`（成功時のstdoutは役割の1行と`status`のJSONで、stderrは混ぜない）は`DAGQ_ROLE`が`inbox` / `planner`のどちらでもなければ何も出力せずexit 0する。workerはruntimeの`--settings`で起動されpluginを読まないが、読んだとしてもroleが違うので影響しない。
- その2つなら、先頭に役割の1行（`This session is the dagq <role> (DAGQ_ROLE=<role>); follow the dagq-<role> skill of the dagq plugin. The queue status for this role (dagq status --role <role>); when its language.instruction is set, write for people as it says:`）を出し、続けて`bin/dagq status --role <role>`（supervisor、未完了run、そのrole宛てのattention、openなask、cursor、言語の`language`。inboxにはattentionのすべて、plannerには無い）をそのまま出す。`language.instruction`は言語の設定があるときの指示の文面で（[Language](supervisor-lifecycle/language.md#promptへの渡し方)、ADR-t616-2）、compactionと`/clear`で初期promptが失われても指示が残る。Claude Codeはこのstdoutをcontextに入れる。先頭の1行は必須で、Claude Code（2.1.281で確認）はstdoutがJSONとして読めればhookの制御JSONとして解釈し、未知のキー（`asks`、`attention`、`cursor`など）を捨ててcontextに何も足さない（task 182。それまでは`status`のJSONだけを出していて、`/clear`後のsessionは役割もqueueの状態も知らなかった）。`up`が渡す`DAGQ_QUEUE`があり`DAGQ_DB`が無ければ`DAGQ_DB`にしてlauncherに渡すので、cwdがrepositoryの外でもそのsessionのqueueを読む。inboxでは役割の1行の前に、最初の一手としてwatchを張ることを命じる1行（`First move, before anything else: start the watch of the dagq-inbox skill (reference/watch.md) with run_in_background from cursor <cursor>, the one command "<launcher>" watch --role inbox --until-attention --after <cursor> with no shell loop around it, unless that watch is still running among this session's background tasks; keep one running from then on.`。`<cursor>`は`status`のtop levelの`cursor`）を出す。`--until-attention`（task 943）のwatchはtimeoutを持たずattentionが来たときだけ返るので、sessionはshellのループ（以前の`reference/watch.md`の`sh -c`と`jq`）を書かない（2026-09-28、zshの語分割でcursorが空になったループが空回りし、askが4〜5時間届かなかった）。`--until-attention`を知らない古いバイナリでは引数の誤りで非0になり、空回りせずに報告される。
- バイナリが見つからない（`DAGQ_BIN`が実行可能でない、PATHに`dagq`が無い）時や`status`が失敗した時も1行だけ理由（`dagq status unavailable: …` / `dagq status failed: …`）を出してexit 0し、session開始を止めない。
- inboxはhookの出力を起点に`dagq-inbox`の手順（openなaskを人に見せ、残りのattentionを知らせ、`watch --role inbox`を再開）へ、plannerは`dagq-planner`の手順へ戻る。`watch`の結果で`integrate`は呼ばない。

### watchを張らせるStop hook（ADR-t906-1）

inboxのClaude sessionを起こすのは`watch --role inbox`の終了と、watcherが居ないあいだにsupervisorが打ち込む1行の知らせ（[通知経路](supervisor-lifecycle/notification-route.md#supervisorによるinboxへの知らせadr-t906-1)。`dagq-inbox` skillはそれを受けたら`status --role inbox`を読んでwatchを張ると書く）だけで、`cmux notify`はsessionを起こさない。`/clear`・compaction・再起動の後にwatchを張り直さずにturnを終えると、その後に開いたaskは人に届かない（2026-09-28に約3時間）。Stop hookがそのturnの終わりを止める。

- `hooks/hooks.json`は`Stop`（matcherなし）で`${CLAUDE_PLUGIN_ROOT}/hooks/stop-watch.sh`を呼ぶ。
- `stop-watch.sh`は`DAGQ_ROLE`が`inbox`のときだけ働く。hookのstdinの`stop_hook_active`が`true`（Stop hookのblockで続いたturn）なら何もしない（1回までしか止めず、繰り返さない）。`bin/dagq status --role inbox`の`inbox_watcher.watching`（猶予を含まない、heartbeatが新しく、processが終わっていない`watch --role inbox`の数。SIGKILLで止まったwatchは閾値を待たずに0になる。[`events` / `watch`](supervisor-lifecycle/events-watch.md#inboxのwatcherの記録adr-t906-1)）が0なら1秒おいてもう一度読み（turnの終わりに張ったwatchがまだ記録を書いていない場合）、それでも0なら`{"decision": "block", "reason": "..."}`をstdoutに出す。reasonはwatchが無いことと、返ったばかりのwatchの出力が未処理なら先に処理してその`cursor`から次を張ること、そうでなければ`status`のcursorからの`"<launcher>" watch --role inbox --until-attention --after <cursor>`の1コマンドをshellのループで包まずにbackgroundで張ること、既に走っているwatchなら失敗していないか確かめることを書く。Claude Codeはturnを続けてreasonをmodelに渡す。
- 猶予（watchが返って120秒）を使わないのは、watchが返った後にinboxが報告だけしてturnを終えると、猶予の中では止められず、そのまま誰も起こさなくなるため。`status`の`state`の猶予は居ない時間を数えすぎないためのもの。
- それ以外は何も出さずexit 0: inbox以外のrole（worker・planner・roleなし）、`dagq`が無い・実行できない、`status`が読めない、`inbox_watcher`の無い古いバイナリの`status`。hookはsessionを止めない。

### sessionの区間hook（ADR-0048）

runtimeがheadlessで起動しないinbox・planner（runtimeが立てるものと、廃止前に人が`dagq plan`で開いたもの）のClaude sessionの区間（kind・session_id・開始・終了）は、pluginのhookが記録する（[ADR-0048](../adr/0048-record-claude-sessions-by-kind-with-open-and-active-time.md)の決定6、task 387）。区間の書き方と推定の終了は[provider-lifecycle](provider-lifecycle.md#claude-sessionの区間)、集計は[stats](supervisor-lifecycle/stats.md#claude-session)。非対話のruntimeのplanner（`[roles.runtime_planner] route = "headless"`）の区間はturnから記録する（[ADR-t1394-2](../adr/2026-10-03-t1394-2-runtime-planner-route-interactive-or-headless.md)の決定4、task 1398）ので、そのturnの`claude -p`がこのhookを走らせても、`record_hook`はplannerの行の`route`が`headless`なら何も書かず`skipped: headless_planner`を返し、hookの開いている区間に`route: headless`の区間を含めない。

- `hooks/hooks.json`は`SessionStart`にmatcherの無い2つ目のグループ（`startup` / `resume` / `clear` / `compact`の全部）で`${CLAUDE_PLUGIN_ROOT}/hooks/session-event.sh open`を、`SessionEnd`（matcherなし）で`session-event.sh close`を呼ぶ。起き直しの`session-start.sh`のグループと出力はそのまま。
- `session-event.sh`は`DAGQ_ROLE`が`inbox` / `planner`で`DAGQ_QUEUE`があるときだけ、hookのstdin（`session_id`・`transcript_path`・`cwd`・`source` / `reason`）をそのまま`bin/dagq session-event open|close`（隠しコマンド）に渡す。`DAGQ_DB`が無ければ`DAGQ_QUEUE`を`DAGQ_DB`にする。それ以外のsession（workerを含む。workerの区間はruntimeが書く）では何もしない。
- 区間のkindはworkspaceの`--env`の`DAGQ_SESSION_KIND`（`up`がinboxに`inbox`、supervisorが立てるplannerに`runtime_planner`を置く。廃止前の`dagq plan`は`planner`を置いた）で、無い古いworkspaceは`DAGQ_ROLE`（plannerは`DAGQ_PLANNER_ORIGIN=runtime`なら`runtime_planner`）から決める。workspaceは`CMUX_WORKSPACE_ID`、plannerは`DAGQ_PLANNER_ID`から取る。
- `/clear`とcompactionで二重に数えない: 同じsession_idの`SessionStart`（`resume`・`compact`）は開いている区間を続け、別のsession_idの`SessionStart`は同じworkspaceの開いている区間を`next_span`で閉じてから開き、閉じた区間への2回目の`SessionEnd`は何も書かない。
- closeはtranscriptを読まない（task 655）: `SessionEnd` と次の `SessionStart` によるcloseは先に `session_closed` をcommitし、残りの稼働時間・tokens・modelの取り込みはsupervisorの10分ごととobserver前に任せる。最終 `session_turns` の印で一度だけ取り込み、読めなくても区間は閉じたまま。`CMUX_WORKSPACE_ID` が無い場合は、workspace IDの無い同kindの次の別sessionの開始で前を `inferred` に閉じる（時間の閾値は使わない。次の開始1回が契機）。同じIDのresume・compactや別kind、workspace IDのある区間には影響しない。詳細と同時sessionの制約は[provider-lifecycle](provider-lifecycle.md#claude-sessionの区間)。
- 失敗してもsessionを止めない: 何も出力せず（`SessionStart`のstdoutはcontextに入るので）、`dagq`が無い・実行できない、queueが開けない、入力にsession_idが無い、記録に失敗した、のどれでもexit 0する。記録のCLIは区間のeventだけを書き、run・proposal・plannerの状態を変えない。

### launcher

skillはすべて`${CLAUDE_PLUGIN_ROOT}/bin/dagq`を呼ぶ。launcherはバイナリを解決してcwdのまま`dagq <args>`を`exec`するだけで、DBのpathを計算せず、DBも開かない（[ADR-0006](../adr/0006-queue-per-repository.md)）。

- バイナリ: `DAGQ_BIN`、なければPATHの`dagq`。どちらもなければ`{"error": ...}`をstderrに出し、`cargo install --locked dagq`（`~/.cargo/bin`がPATHにあること）で入れるか、別の場所のバイナリを`DAGQ_BIN`に絶対pathで指定するよう案内する（CLI本体のエラー形式と同じ）。Releaseのasset名やdagqのrepositoryの開発に固有の手順は出さない（[ADR-t617-1](../adr/2026-09-27-t617-1-plugin-marketplace-pinned-to-release-tag.md)）。
- queue: バイナリがcwdのrepositoryから`$XDG_DATA_HOME/dagq/<hash>/queue.db`に解決する。`DAGQ_DB`が設定されているときだけ`--db "$DAGQ_DB"`を前置する。dirの作成と束縛は`init`が行う。
- `--resolve`: `dagq locate`のJSON（`db`、`db_exists`、`queue_dir`、`runs_dir`、`source`、`git_common_dir`）に`binary`、`binary_version`（`dagq --version`の`dagq `の後、build識別子`X.Y.Z`または`X.Y.Z-dev+<commit>[.dirty]`。[supervisor-lifecycle](supervisor-lifecycle/build-identifier.md#build-identifier)）、`plugin_version`（launcherの隣の`.claude-plugin/plugin.json`をsedで読む）、`repo`（`git rev-parse --show-toplevel`、repository外は空文字）を加えた1つのobjectを返す。skillはこれをユーザーへの報告と、cmux workspaceへ渡す絶対pathの取得に使う。`--version` / `--help`はそのままバイナリに渡す。
- version不一致: `plugin_version`と`binary_version`のmajor.minorが違うとき、stdoutの解決結果はそのまま出したうえでstderrに`{"warning": ...}`を1行出し、exitは0のまま（解決自体は正しく、CLIの差だけが不明）。skillは止まらずユーザーに報告し、古い方の更新（pluginは`claude plugin update claude-dagq@dagq`、バイナリは`cargo install --locked dagq`）を案内する。警告の文もこの2つを案内し、ReleaseのURLは出さない。

### skillの契約

- 完了はStop hookやreceiptファイルの存在ではなく、`show`のrun `status`（`awaiting_integration` / `needs_session` / `integrated`）、`result_commit`、`last_error`、`validation_finished`イベントで判定する。
- 登録の標準手順は「課題を読む（runtimeのplannerは依頼の人の言葉と参照を初期promptで受ける。ADR-t1394-1）→ `goal add`で登録 → taskに分解して`add --goal`で登録 → `lint`と`submit`でplan reviewに出す」（[ADR-0009](../adr/0009-goal-groups-tasks.md)、`ready`にするのはplan reviewだけ: [ADR-0044](../adr/0044-findings-proposals-from-findings-and-quiet-observer.md)の決定8）。goalなしを許すのは一発task（typo修正、clippy警告の解消など、1 taskで終わり判断を揃える相手がいないもの）だけで、判断基準は「2つ目のtaskが存在する、または後のtaskがこのtaskの決定（名前・境界・形式）を知る必要があるならgoalを作る」。`goal add`はtitle、description、acceptance（全task着地後にplannerがgoalの達成を判定する基準）、constraints（命名・境界・やらないこと）、doc（repository内の参照文書のパス。workerはworktreeで読むのでcommit済みであること）を集め、`add`は`--goal`と`--context`（goalの記述で足りないときの背景と最初に読むもの）を足す。`goal list` / `goal show`はInspectの要点と`reference/inspect.md`の表にあり、`set-goal`はdraft / readyのtaskだけ、`goal edit`は`goal_updated`イベントに新旧を残しclaim済みのrunには届かない、と書く。
- 終わったgoalのcloseはplannerの手順ではない（[ADR-0047](../adr/0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)の決定16・43）。goalの全taskが`completed`か`canceled`でdraft・submittedが残っていなければ、supervisorのheadlessのgoal reviewのjobが各taskの着地したreceipt（`summary`・evidence・`follow_ups`）をacceptanceに照らし、`achieved`で閉じるか、未達を`goal_gap`のdraftにするか、`approve_goal`のaskで人に聞く（[Goal review](supervisor-lifecycle/goal-review.md)）。`dagq` skillの`reference/goal-close.md`がそれを案内し、`goal close`はdraft goalの破棄（`--verdict abandoned`）と、人の言葉（`goal review by hand`の後か、`request add --ref goal:N`の依頼のplanner、人の`DAGQ_ROLE`の無いterminal）のときだけに使う。`abandoned`はdraft / submitted / readyのtaskをcancelしない（`in_progress`があるときだけ拒否する）ので、先にcancelする。receiptの`follow_ups`は`integrate`が着地後に同じgoalのdraft taskとして登録する（ADR-0019の決定4）。着地の報告は登録されたtaskを挙げるだけで、draftごとにruntimeが立てるplannerが、採用（acceptance・verificationなどを`edit`で補って`submit`しplan reviewに出す）・不採用（`cancel`）・判断できない（`planner_question`のask）のどれかを選ぶ（[ADR-0044](../adr/0044-findings-proposals-from-findings-and-quiet-observer.md)の決定16）。draftを直接`ready`にする者は居ない。plannerの待ちや`planner_question`や`keep_draft`で残ったdraftはgoal reviewを止め、補って`submit`するか`cancel`するかを、人の言葉で記録した依頼（`request add --ref task:N`）のplannerか、人の`DAGQ_ROLE`の無いterminalで決める。
- supervisorの起動は、人に頼まれたinbox（か人の`DAGQ_ROLE`の無いterminal）が`dagq-recover` skillのsection 5に従い`"$DAGQ" up`（必要なら`--parallel N`。`--plugin-dir`はrepositoryの指示が名指すpathがあるときだけで、`$CLAUDE_PLUGIN_ROOT`は渡さない。ADR-t617-2）をlauncher経由で呼ぶ（[supervisor-lifecycle](supervisor-lifecycle/up-down.md#up--down)、ADR-0044の決定6）。`up`がsupervisorを常駐させ、inboxのworkspaceの有無をqueue DBに記録したUUIDで判定し（titleでは探さない）、生きているsupervisorがあれば`reused`を返すので、skillは重複起動の判定もworkspaceの作成も自分では行わず、`cmux workspace create`で`supervise`を起動する手順も持たない。`up`はplannerを開かず、plannerはruntimeが立てる（人が開く`dagq plan`は廃止し、inboxへの計画の依頼の案内を付けて拒む。ADR-t1394-1）。inboxのsession（workspaceの`--env`の`DAGQ_ROLE`）の中から呼ぶとinboxは`skipped`になり、これはerrorではない。`restart supervisor`（`supervisor_stopped` / `supervisor_stale`）はinboxの`watch`が拾って人に知らせ、人の指示で`up`を叩き直す（死んだ登録をpruneして起動し直す）。停止は`dagq down [--wait] [--force]`で、`--force`は実行中のrunを捨てるので人の明示の指示が要る。supervisorのlogは`locate`の`log_dir`にある。`supervise`・`integrate`・`observe`・session wrapperがprocessごとに`<process>-<UTC time>-<pid>.jsonl`（1行1レコードのJSON Lines。`jq`で`fields.run_id`などで絞れる）を書き、launchdのstdout / stderrは`launchd.log`に溜まる。以前のバイナリの`supervisor-<started_at>-<pid>.log`は残り、読まれない（[supervisor-lifecycle](supervisor-lifecycle/logs.md#logs)）。`dagq-recover` skillの`reference/up-down.md`も同じ場所と書式を案内する。
- mainへの着地はruntimeの`integrate ID` / `integrate --next`が行う（rebase → 再検証 → squash、[ADR-0008](../adr/0008-merge-queue-squash-landing.md)）。acceptしたrunはsupervisorがsessionを開いたままheadlessでreviewし、passなら自分で着地させ、reviseは生きているsessionに返し、concernなら`approve_landing`のaskを作ってその答え（`land` / `send_back` / `cancel`）を自分で適用する（[ADR-0027](../adr/0027-keep-worker-session-through-review-revise-verdict-and-merge-tree-precheck.md)、[supervisor-lifecycle](supervisor-lifecycle/review.md#review-supervisor)）。inboxは答えるだけ。headlessのreviewが失敗したrun（`review by hand`）とreview以前のrun（`review and integrate`）だけは、inboxが人の指示で`dagq-recover`の`reference/review-by-hand.md`に従い、`review ID`の`review.md`をsubagentにレビューさせて結論（`pass` / `concern`と理由）だけ受け取り、`pass`なら人の指示で`integrate`を呼び（`watch`の結果やイベントの副作用としては呼ばない）、`concern`なら`ask --kind approve_landing --run <run_id> --option land --option send_back --option cancel`を作り、その答えはsupervisorが適用する。`needs_session`のrunはsupervisorがresumeし（resume workspaceはruntimeが開閉し、sessionは作らない）、3回で解消しなければ`failed`にして`decide`のask（`retry` / `cancel`）にする。runのworkspace名は`[<repo>]worker#<task-id> - <task title>`（[ADR-0028](../adr/0028-workspace-titles-are-repo-and-role.md)。[ADR-0018](../adr/0018-run-workspace-named-after-the-task.md)の書式を上書き）、descriptionは`dagq role=worker queue=<queue hash> run=<run-id> task=<id>`で（[ADR-0026](../adr/0026-identify-workspaces-by-uuid-env-and-queue-group.md)。queueのworkspaceは`[<repo>]`のworkspace groupにまとまる）、`failed` / `interrupted`のrunのworkspaceはtriageの後にsupervisorが閉じる。
- `recover`はバイナリが拒否条件を判定する。skillはプロセスをkillせず、`doctor --full`の`blockers`を人に示す。
- `show`・`goal show`・`doctor`の既定は圧縮形で（長い文字列は300文字で`…`と`truncated: true`、`show`は最新runと直近10件のイベントの要点、`doctor`はrun 1件1行相当）、skillはそれぞれの説明に`--full`と切り詰めを書き、receiptやpayload、`run_dir`、`blockers`が要る手順では`--full`を付ける。

### marketplaceとinstall

repository rootの`.claude-plugin/marketplace.json`がこのrepository自身をmarketplaceにする（marketplace名`dagq`、`owner.name` `hisamekms`、`plugins`は`claude-dagq`の1件で`source`はrepository相対の`./plugins/claude-dagq`）。ユーザーの導線は2行。

```sh
claude plugin marketplace add hisamekms/dagq
claude plugin install claude-dagq@dagq
```

`add`はGitHubのrepositoryをcloneし、`install`はそのcloneの`./plugins/claude-dagq`からuser scopeに入れる。更新は`claude plugin marketplace update dagq`と`claude plugin update claude-dagq@dagq`。pluginはskillと`SessionStart` / `Stop` / `SessionEnd` hookだけでinstallに`-y`を要する宣言commandはなく、runtimeバイナリは同梱しない（`cargo install --locked dagq`で別に入れる。launcherのエラー文がその手順を持つ）。

**決定済み・未実装（[ADR-t617-1](../adr/2026-09-27-t617-1-plugin-marketplace-pinned-to-release-tag.md)、[ADR-t617-2](../adr/2026-09-27-t617-2-installed-plugin-by-default-plugin-dir-for-development.md)）**: 実装が入るまでは上のとおりmainのHEADのpluginを配る。実装後の姿は次のとおり。

- marketplaceのentryの`source`は`{"source": "git-subdir", "url": "hisamekms/dagq", "path": "plugins/claude-dagq", "ref": "v<X.Y.Z>"}`で、`ref`は最新のリリースのtag。entryに`version`は書かない（versionはtagの`plugins/claude-dagq/.claude-plugin/plugin.json`の`X.Y.Z`になり、cacheは`~/.claude/plugins/cache/dagq/claude-dagq/<X.Y.Z>/`）。利用者の導線（`marketplace add hisamekms/dagq`・`install claude-dagq@dagq`・`plugin update claude-dagq@dagq`）は変わらない。
- `release.yml`はtagの`Cargo.toml`の`[package].version`と`plugin.json`の`version`がtagの`X.Y.Z`と一致することを検査し、違えばbuildの前に止まる（実装済み: `scripts/check-plugin-version.sh --tag`。上の「tagとversionの一致規則」）。
- `-dev`を外してversionを`X.Y.Z`にするリリースの変更で、entryの`ref`をこれから打つ`vX.Y.Z`に書き換え、着地したらすぐにtagをpushする（release skillの手順）。`release.yml`はtagの`marketplace.json`のentryがgit-subdirの形で`ref`がそのtagであることも検査する（実装済み。リリースのversionでは相対のsourceだと落ちるので、`v0.4.0`のリリースの変更でentryを書き換えないとreleaseが止まる）。切り替えは次のリリース（`v0.4.0`）から。
- 利用者の更新はバイナリの`cargo install --locked dagq`とpluginの`claude plugin update claude-dagq@dagq`の組。launcherのmajor.minorの警告は残す（launcherのエラーと警告の案内はこの組に揃え済み。上の「launcher」）。
- `up`は`--plugin-dir`が無ければ`claude`に何も足さず、installしたpluginを使う。skillは`$CLAUDE_PLUGIN_ROOT`を`--plugin-dir`に渡さず、repositoryの指示が指定するpathがあるときだけ付ける（skillの変更は実装済み: `dagq-recover` skillのsection 5と`reference/up-down.md`、上のsupervisorの起動の項）。`--plugin-dir`が無く`claude-dagq`がinstallされて有効なことを確かめられなければ、`up`はsessionを開く前に止めてinstallのコマンドを案内する（ADR-t617-2の決定4。実装済み: 下の「installしたpluginの確認」）。dagqのrepositoryはAGENTS.mdのとおり`--plugin-dir <repository>/plugins/claude-dagq`を付け、`--plugin-dir`のpluginが同じ名前のinstall済みのpluginより優先される。
- Anthropicのdirectoryと公式のmarketplace（`claude-plugins-official`）には出さない。
- 外部のprojectのリリースの更新は、バイナリを入れ替えた後にinstallしたpluginも同じリリースへ上げ（supervisorの`--claude`で`claude plugin marketplace update dagq`と`claude plugin update claude-dagq@dagq`）、バイナリが最新でpluginだけが古いときも`approve_release`で聞く。`--plugin-dir`のsupervisorはpluginに触らない。pluginのversionは下の「installしたpluginの確認」と同じ`claude plugin list --json`から読む（[ADR-t618-2](../adr/2026-09-27-t618-2-plugin-follows-the-release-update.md)、[Release update](supervisor-lifecycle/release-update.md)。実装済み）。

### installしたpluginの確認

[ADR-t617-2](../adr/2026-09-27-t617-2-installed-plugin-by-default-plugin-dir-for-development.md)の決定4。`--plugin-dir`を付けない`dagq up`は、inboxのsessionが読むinstall済みの`claude-dagq`を、sessionもsupervisorも開く前に確かめる。`dagq plan`は何も開かずに拒むので（[ADR-t1394-1](../adr/2026-10-03-t1394-1-abolish-person-planners-and-route-planning-through-inbox-requests.md)、[`plan` / `planners`](supervisor-lifecycle/plan-planners.md)）、確かめない。`--plugin-dir`を付けたときは確かめない（そのpluginが同じ名前のinstall済みのpluginより優先されるため）。runtimeが立てるplannerと`install`の引き継ぎは確かめない（supervisorに渡された`--plugin-dir`だけを使う、今までの振る舞いのまま）。runtimeがsupervisorを起動し直すために打つ`up`（`install --allow-breaking`のdrainの後と、自動更新の後のin-cmuxのsupervisor）も確かめない。runtimeは`LocalBinaries::run`でその`up`の環境に`DAGQ_UP_RESTART=1`（`lifecycle::UP_RESTART_ENV`）を置き、`up`はそれを`UpEnvironment::restart`として読む（envにしたのは、知らない古いバイナリが無視できるから）。

- 確かめ方: `up`の`--claude`（なければPATHの`claude`）で`claude plugin list --json`を、sessionが開くrepositoryのroot（`up`のinboxのcwd。project scopeのpluginもそこで数える）で実行する。出力は`[{"id": "<plugin>@<marketplace>", "version": ..., "scope": "user", "enabled": true, "installPath": ...}]`の配列（Claude Code 2.1.283で確認）で、`id`の`@`の前が`claude-dagq`のentryのどれかが`enabled: true`なら有効とする（marketplaceとscopeは問わない）。entryの取り出し（`adapters::plugin_entries`）はリリースの更新がpluginのversionを読む`adapters::plugin_version`と共有する（[Release update](supervisor-lifecycle/release-update.md#pluginのversion)）。portは`AgentProvider::installed_plugin`（結果は`PluginState`の`Enabled` / `Disabled` / `Missing`）、Claude Codeの実装は`ClaudeCode::installed_plugin`と出力の読み取りの`adapters::plugin_state`、判定と文言は`lifecycle::require_installed_plugin`。
- 止まるとき: 未install（`claude-dagq`のentryが無い）、無効（entryがあるがどれも`enabled: false`）、コマンドの失敗（起動できない・非0・時間切れ）、出力を読めない（JSONの配列でない、`id`か`enabled`の無いentry）。どれも非0で止まり、errorは次の形の英語:

```text
<reason> in the Claude Code at <claude>, so the inbox session would start without the dagq skills its prompt names. Install it with `claude plugin marketplace add hisamekms/dagq` and `claude plugin install claude-dagq@dagq`[ (or enable it with `claude plugin enable <id>`)], then run `dagq up` again; to use a plugin checkout instead, pass --plugin-dir
```

  `<reason>`は`the claude-dagq plugin is not installed`・`the claude-dagq plugin is installed but disabled`（このときだけ`claude plugin enable <id>`の案内が付く）・`whether the claude-dagq plugin is installed could not be checked (<error>)`。`up`は末尾に`; the supervisor was not started`を付ける。installのコマンドは`lifecycle::PLUGIN_INSTALL_COMMANDS`（上の「marketplace」の公式の手順）。
- test: `tests/it/installed_plugin.rs`が、fixtureのstubの`claude`（`tests/common/lifecycle.rs`。`plugin list`に`<stub>.plugins`を出し、無ければ失敗する）で、有効ならsupervisorとinboxが開き、上の4つの場合は何も開かずに文言どおり止まり、`--plugin-dir`を付けたときと`DAGQ_UP_RESTART`の`up`では`plugin list`を呼ばないことと、`dagq plan`がどの場合も`plugin list`を呼ばずinstallの案内も出さずに拒むこと（`plan_refuses_without_asking_for_the_installed_plugin`）を確かめる。e2eのstubの`claude`（`tests/e2e.rs`）は`plugin list`に有効な`claude-dagq`を返す。

### 読み込みと検証

- 検証: `claude plugin validate plugins/claude-dagq`（hookとskillを含む。Claude Code 2.1.280で確認）と`claude plugin validate .claude-plugin/marketplace.json`（`--strict`も通る）、inventory: `claude --plugin-dir plugins/claude-dagq plugin details claude-dagq`。
- 開発中の読み込み: `claude --plugin-dir /path/to/dagq/plugins/claude-dagq`（そのsessionのみ）。supervisorがworkerに渡すのもこの形（`up --plugin-dir`）。
- marketplace経由のinstallは、使い捨ての`HOME` / `CLAUDE_CONFIG_DIR`でローカルpathを`marketplace add`して`install`し、`plugin list`と`plugin details`でskillが載ることを確認する（task 29、Claude Code 2.1.278で確認。task 65以降は`plugin details`でskill 5件とhook 1件、task 88以降はskill 7件、task 100以降はskill 4件）。
- `tests/plugin.rs`がmanifest（name、versionの一致）、marketplace manifest（marketplace名、pluginのname、`source`が相対の`./plugins/claude-dagq`かgit-subdirの形でpluginのdirectoryを指すこと）、`check-plugin-version.sh`のmarketplaceのentryの検査、launcherのversion比較（`--version`と`locate`だけ答えるfake binaryを`DAGQ_BIN`にして、major.minorが同じならstderrが空、1 minor違えばstderrに`{"warning": ...}`が出てexit 0。警告は`claude plugin update claude-dagq@dagq`と`cargo install --locked dagq`を含みReleaseのURLを含まない）、skill一覧（`dagq` / `dagq-inbox` / `dagq-planner` / `dagq-recover`）、各`SKILL.md`が8 KB以下であること、`reference/`のファイルと本文からの参照が過不足なく対応すること（他skillの`skills/<name>/reference/`への参照はそのskillで解決する）、skillとreferenceが利用先のrepositoryに固有の印（dagqのrepositoryの検証のコマンド・test binary・`[areas]`の名前・task・goal・askの番号の逸話・日付など。印の一覧はtestが持つ。受け持ちは[文書の規則](../development/documents.md)の「pluginの汎用性」）を含まないこと、`reference/review-by-hand.md`が`review ID`・`integrate`・`approve_landing`のaskの3値・`git push origin main`を持つこと、inbox / recoverのskillが`goal add` / `add` / `goal close`を持たず、inboxがattentionの`next`ごとの行き先と「人の指示があるときだけ」の線引きを、recoverがsection 3〜7を、plannerが登録とgoal closeと`up`の参照を持つこと、frontmatter（先頭行`---`、`name`がdirectory名、`description`）、hook（`hooks.json`の形式、`session-start.sh`がmatcherなしの`SessionStart`、`stop-watch.sh`がmatcherなしの`Stop`、`session-start.sh`がplannerの`startup` / `resume`で何も出さずinboxの`startup` / `resume` / `clear` / `compact`でwatchを最初の一手とする行と`status`を出すこと、`stop-watch.sh`がwatcherの居ないinboxでblockとwatchのコマンドを含むreasonを返し、watcherが居る・`stop_hook_active`・role無し・worker・planner・バイナリ無しで何も出さずexit 0すること、`session-event.sh open`がmatcherなしの`SessionStart`・`session-event.sh close`がmatcherなしの`SessionEnd`、scriptが実行可能、`session-event.sh`がrole無し・worker・`DAGQ_QUEUE`無しで何も記録せず、inboxの開始・compaction・`/clear`（`SessionEnd`の`clear`→新しいsession_idの`SessionStart`）・終了と2回目の終了で区間を1回ずつ開き閉じ、`DAGQ_SESSION_KIND`の無い古いplannerのworkspaceの`/clear`で前の区間を`next_span`で閉じ、`stats`の`sessions.by_kind`に`inbox` / `planner`の件数が出ること、バイナリ無し・実行できない・queueが開けない・session_idの無い入力・未知のeventで出力が空でexit 0、role無し・別role（worker、observer、supervisor）で出力が空、inbox / plannerで出力の先頭が役割とskillの1行でstdout全体はJSONとして読めないこと、inboxで`supervisor_stopped`と`ask_opened`のattentionとopenなaskを持つ`status`、plannerでattentionの無い`status`、`DAGQ_QUEUE`での解決、バイナリ無し・`status`失敗で1行とexit 0）、launcherの解決（`XDG_DATA_HOME`配下、worktreeからの共有、`DAGQ_DB`の優先、`binary_version`と`plugin_version`）・エラー（`cargo install --locked dagq`・`~/.cargo/bin`・`DAGQ_BIN`を含み、ReleaseのURL・tarball名・`SHA256SUMS`・`cargo build`を含まないこと）・`init`・登録・`show`を実バイナリで確認する。テストは`XDG_DATA_HOME`を一時dirに向け、開発者の実queueに触れない。skillに書いたコマンド列のうち自動テストにしないもの（goal系の`goal add` → `add --goal` → `ready` → `goal show` → `goal close`、runtime系の`up` → `status` → `down`）は、skillを変えたtaskのrun sessionが`cargo build --locked`したバイナリと使い捨てrepository・使い捨てqueue（`--db`）で実行し、その実行ログをreceiptのevidenceに残す（task 13、task 16）。

---
id: design-broker
type: design
title: Resource broker
status: current
created: 2026-09-28
scope: runtime
tags:
  - security
  - broker
related:
  - adr-t2113-1
  - adr-t2113-3
  - adr-t840-1
  - adr-t838-1
  - adr-t1582-1
  - adr-t827-1
  - adr-t827-2
  - adr-t827-3
  - adr-t827-4
  - adr-t728-1
  - adr-t728-2
  - design-security
  - design-authorization
  - design-supervisor-lifecycle-run-environment
---

# Resource broker

> **撤去予定（未実装）**: resource brokerは外すと決まった（[ADR-t2113-1](../adr/2026-10-08-t2113-1-remove-the-resource-broker.md)）。
> 撤去の実装はまだで、この文書はコードに残る今の姿を書く。
> resource brokerに機能を足さない。

## 概念

### 目的

resource broker（`dagq-broker`）は、workerのfs・process・git・packageの操作を、runごとのtokenで名指したworktreeの中だけに仲介する。
目的は、workerの操作をtokenとauditでrunに結び、誤りと事故を上限と閉じ込めで止めること。

**隔離ではない。**
workerはhostのプロセスのままで、`preferred`のworkerはbrokerを迂回して組み込みの道具やhostのファイルを直接使える。
`required`で組み込みの道具を拒むのもClaude Codeの設定で、guardrailでありenforcementではない（下の「required」）。
host実行は助言的（[Security](security.md#host実行は助言的advisory)、ADR-t728-1決定6）で、brokerの契約（token・閉じ込め・上限・audit）はそれを変えない。

この文書の「broker」はresource brokerのことで、実行側からqueue serviceへの出口の「broker」とは別物（ADR-t827-4決定6）。
人とinboxとplannerの手順はpluginの`dagq`と`dagq-recover`の`reference/broker.md`が持つ。

### 全体の流れ

```text
host (macOS)                                   Podman machine "dagq" (Linux VM)
┌──────────────────────────────────┐           ┌──────────────────────────────────────┐
│ supervisor (dagq)                │  podman   │ container dagq-broker-<queue hash>   │
│  - broker::ensure (machine/image │──────────▶│  dagq-broker  (non-root, read-only)  │
│    /container/health)            │           │   mounts (same absolute paths):      │
│  - token 発行・更新・失効          │           │    <queue dir>/runs          rw      │
│                                  │           │    <git common dir>          rw      │
│                                  │           │     (config, hooks は ro で重ねる)   │
│ worker (Claude Code, host)       │           │    <queue dir>/broker/key    ro      │
│  └─ dagq-broker-client mcp ──────┼─HTTP─────▶│    <queue dir>/broker/active ro      │
│      (--mcp-config)  127.0.0.1:P │           │    <queue dir>/broker/audit  rw      │
└──────────────────────────────────┘           └──────────────────────────────────────┘
```

1. supervisorはmodeが`disabled`でなければ、dagq専用のPodman machineの中にqueueごとのbrokerのcontainerを用意し、healthを見張る。
2. claimでworkerを起こすとき、brokerが健康で版が一致すれば、runのtokenを発行してtoken fileと有効な印を置き、`mcp.json`をworkerに渡す。
3. workerの`dagq-broker-client mcp`（stdioのMCP server）が道具の呼び出しをloopbackのHTTPでbrokerに送る。
4. brokerはtokenを検証し、capabilityとworkspaceで閉じ込め、backendで実行し、1要求1行のauditを残して答える。
5. runが終わればsupervisorが印とtoken fileを消して失効させ、組み込みの道具の数を記録する。

### 責務と境界

- protocol（`crates/dagq-broker-protocol`）: 要求と応答の型・capability・error・tokenの署名と検証・build識別子で、3者が共有する唯一の契約。
- server（`crates/dagq-broker`）: containerの中でtokenの検証・閉じ込め・上限・auditを強制し、queueもDBも知らない。
- client（`crates/dagq-broker-client`）: workerのMCP serverと人の診断のCLIで、dagqにもserverにも依存しない。
- dagq: machine・image・containerの用意、tokenの発行と失効、workerへの受け渡し、modeの解決で、HTTPの依存を持たない（ADR-t827-1決定2）。
- 制御側の操作（workerの`dagq`のコマンドとreceiptの書き込み）はbrokerの外に残る。

### 不変条件

- tokenを発行するのは信頼する制御側のsupervisorだけで、AI actorにtokenを作るコマンドは無い。
- tokenの値はprompt・envの値・event・log・auditのどこにも出ない（`mcp.json`はtoken fileの場所だけを持つ）。
- 誰の要求かはtokenのclaimsだけで決まり、要求の本体の欄では決まらない。
- 版が食い違えば（clientが無い・clientかserverのbuildがdagqと違う・protocolの版が違う）、tokenを発行せず道具を渡さない（fail closed、ADR-t827-1決定7）。
- 未知の欄・capability・error code・roleは読む側で失敗する（fail closed）。
- auditの書けない要求は実行せず、auditの無い答えを返さない。
- dagqは人の既定のmachineと既定の接続を起動・停止・変更しない。
- modeが`disabled`のqueueでは、supervisor・`up`・`down`はpodmanを探しも呼びもせず、workerの起動は変わらない。

## crateとバイナリ

- 3つのcrateとrootの`dagq`の版は1つで、`scripts/check-plugin-version.sh`が一致を検査する（ADR-t827-1決定1）。
- async runtimeとHTTPのcrateは入れず、serverとclientは`std::net`の上の自前の小さな同期のHTTPを使う（最小のmachineでのbuildを軽く保つため）。
- protocolの型はserializeが欄の宣言の順とkeyの順なので同じ値は同じbyte列になり、claimsの署名はこのbyte列に対して行う。
- dagqの側の入口: 用意は`src/application/broker.rs`と`src/infrastructure/broker_podman.rs`・`broker_queue.rs`、tokenと受け渡しは`src/application/broker_run.rs`と`src/infrastructure/broker_token.rs`、supervisorは`src/application/supervise/broker.rs`、設定と判断は`src/domain/broker.rs`、管理のコマンドは`src/application/broker_admin.rs`。
- coverageの関門は`--workspace`で全crateを数える（ADR-t828-1）。
  cargo-llvm-covは`-p`も`--workspace`も無いとroot packageだけを数える。
- podmanを要るtestは`#[ignore]`で関門とCIに数えず、CIはmacOSだけなので、serverのLinuxの経路とContainerfileはCIではbuildされない。
- `src/application/update.rs`の`RUNTIME_PATHS`は`crates/`と`rust-toolchain.toml`を含む（ADR-t827-1決定4）。

## 配布と版

- `dagq-broker-client`は`dagq`の隣、brokerのimageはdagq専用のmachineの中に置き、どちらも同じbuild識別子で照合する（ADR-t827-1決定5・6）。
- build識別子の規則は`crates/dagq-broker-protocol/src/build_id.rs`の1か所で、3つのcrateの`build.rs`が同じ集合を見るので、どれかの編集で3つそろって`.dirty`になる。
- `install`とauto-updateはdagqとclientを同じbuildから確認して一緒に置き・戻し、別のbuildのclientをdagqの隣に残さない（[`install`](supervisor-lifecycle/install.md#install)・[Auto-update](supervisor-lifecycle/auto-update.md#auto-update)）。
- workerに渡すclientは`application::broker::resolve_client`が選ぶdagqの隣のものだけで、版が違えば渡さない。
- imageの材料はdagqのバイナリに埋め込み（`src/broker_material.rs`・`src/infrastructure/broker_image.rs`）、brokerの起動はcheckoutの作業ファイルを読まない。
  releaseはContainerfileだけを埋めてcrates.ioの同じ版のserverを作り、どちらもmachineからcrates.ioとdocker.ioへのnetworkを要る。
- imageのtagは`application::broker::image_tag`、imageの中のserverのbuild識別子はdagqがbuild-argで渡す（`build_id::given_identifier`）。
- `application::broker::start`はhealthの`build`をdagqのものと比べ、違えば1回だけ作り直し、それでも違えば`version_mismatch`にする。
- auto-updateの後は、有効な印のあるrunのために古いcontainerを動かし続け、その間の新しいclaimにはtokenを出さない（ADR-t827-3決定2）。
- 古いimageとdanglingのimageの掃除は`prune_images`と`prune_dangling`で、`start`を失敗させない。
- releaseの手順は`release.yml`と`.claude/skills/release`、releaseのupdateは[Release update](supervisor-lifecycle/release-update.md)で、imageはregistryに出さない。

## transport

- `127.0.0.1:<port>`のHTTP/1.1とJSONだけで、TLSもproxyも持たない（ADR-t827-2決定1）。
  hostへのpublishは`127.0.0.1`だけで、serverは`--container`のとき以外loopbackでないaddressへのbindを拒む。
- portはqueueごとで、選んだportは`<queue dir>/broker/state.json`に残し、containerを起動し直しても同じportを使う。
- 要求と応答はprotocolの版のheaderを持ち、clientは版の無い・違う応答の本体を解釈しない。
- serverの判定の順は`crates/dagq-broker/src/server.rs`が持つ。
  約束: healthだけがtokenなしで答え、未知のpathもtokenより前には答えない（default deny）。
  pathの字面の閉じ込めは`TokenClaims::confine`、symlinkの解決はbackendが行う。
- 読めない要求と途中で切れた要求は`invalid_request`で答えてauditに残し、1 byteも送らずに切れた接続は要求ではないので残さない。
- 操作の一覧は`dagq_broker_protocol::Operation`で、shellの文字列を受けるopは持たない（要るならADRが別のcapabilityとして決める）。

### error

- error codeとHTTP statusの対応は`dagq_broker_protocol::ErrorCode`。
- messageにtoken・ファイルの中身・envの値・diffを入れない。

## token

- 書式・署名・検証の順は`crates/dagq-broker-protocol/src/token.rs`、claimsの意味は`claims.rs`の`TokenClaims`。
- 鍵は`<queue dir>/broker/key`（`infrastructure::broker_token::ensure_key`）で、替えると全てのtokenが無効になる。
  定期の入れ替えは無く、人がbrokerを止めて鍵を消し、`broker start`で作り直す。
- **偽造できる。**
  hostの同じユーザーのプロセスも`process.exec`も鍵を読めうるので、tokenは誤りと事故を止めてauditでrunを名指すためのもので、境界ではない。
- 発行は`issue_run_token`で、token fileはcontainerにmountしない`<queue dir>/broker/tokens/`に置く（`process.exec`から他のrunのtokenを読めないように）。
- serverは要求ごとに有効な印（`<queue dir>/broker/active/<jti>`）の有無を見て、clientは要求ごとにtoken fileを読み直す。
- 期限の近いtokenの発行し直しと、終わったrunの失効は`broker_sweep`（`src/application/supervise/broker.rs`）。
  runの読み取りが失敗したときは不在と区別し、消さずに次のpassで読み直す。
  supervisorが止まっている間に終わったrunのtokenは、次の起動の最初のpassか期限まで使える。
- `disabled`に戻したqueueでは、podmanを呼ばずに残った全てのtokenと印の無いtoken fileを失効させ、turnの前にも消す（`broker_sweep_disabled`・`broker_before_turn`）。
- runのbrokerのevent（tokenの発行と失効・`broker_unavailable`・`broker_tool_use`）はattentionではない（`src/domain/event_kind.rs`）。

## mountと閉じ込め

- containerのmountは`ContainerSpec::run_args`で、全てhostと同じ絶対パス（ADR-t827-2決定5）。
  gitの共通dirはrwで、その`config`と`hooks`だけを読み取り専用で重ね、containerからhostでコードを走らせる設定を書かせない。
- **落とし穴**: `<common>/config`はcontainerから読めるので、brokerを有効にするrepositoryでは資格情報をrepositoryのconfigに置かない。
- `$HOME`・`~/.ssh`・`~/.gitconfig`・podmanのsocket・queue DB・main checkout・他のqueueとhostのenvはcontainerに入れない。
- fsの閉じ込めの手順は`crates/dagq-broker/src/backends/fs.rs`のmoduleのdoc comment。
  約束: 確かめたものをそのまま開くので確認と開くことの間の競合を作らず、途中のどこかにsymlinkがあれば中を指すものも拒む。
  hard linkは見分けられない（hostのworkerはhostで直接書けるので失うものは無い）。
- 別のrunのworktreeはmountされているが、tokenのworkspaceの外なのでfsのopでは届かない。
- **落とし穴**: fsの書き込みのtmpは、renameの前にbrokerが止まるとworkspaceに残り、掃除されない（`git.add`がstageしないだけ）。

### 既知の制限（Phase 1）

`process.exec`のプロセスはbrokerと同じuidで同じmountを見る（ADR-t827-2決定6）。
許したプログラムによっては、他のrunのworktreeとreceiptの読み書き、鍵を読んでのtokenの偽造、auditの書き換え、gitの他のbranchのrefとobjectsの書き換えができうる。
`exec_allow`の既定は空で、ファイルを読むプログラムを許すのは使い捨てのrepositoryの検証だけにする。
hostのworkerはこれら全てをhostで直接できるので、Phase 1で失うものは無い（ADR-t728-1決定6）。
Podman machineは既定でhostの`$HOME`をVMにmountするので、containerから抜け出せば`~/.ssh`などが見える（ADR-t827-3決定6）。

## git

- 入口は`crates/dagq-broker/src/backends/git.rs`で、gitdirの確かめ方・env・`-c`の設定・各opの約束はそのmoduleのdoc commentと各関数にある（ADR-t827-2決定7）。
- 約束: gitはtokenのworkspaceでだけ走り、workerが`.git`を書き換えてmain checkout・別のrunのworktree・自分で作ったrepositoryを指させても届かない。
- 約束: repositoryのconfigと`.gitattributes`（workerが書ける）が定義するhook・filter・外部diff・transportは全て止める。
- push・fetch・remote・config・checkout・branch・resetのopは無い。
- `git.show`はHEADの履歴にあるcommitだけを見せ、別のrunのbranchのcommitは`workspace_violation`。
- `git.commit`はplumbingで作り、tokenのbranchだけを、読んだ親からだけ動かす（hookは走らない）。
- `git.add`はfsのtmpをstageせず、tmpそのものを名指す要求は黙って成功にせず拒む（workerがstageしたと思い込むため）。
- `process.exec`の`argv[0]`が`git`なら拒むが、これは誤りを止めるもので、`sh -c`・`env`・複製したバイナリからは通る。

## process.exec

- 入口は`crates/dagq-broker/src/backends/process.rs`で、argvの解き方・cwd・env・timeout・出力の上限はそのmoduleのdoc commentにある（ADR-t827-2決定8）。
- 約束: shellを通さず、`argv[0]`はallowlistの名前だけで、brokerが固定の`PATH`で探す。
- 約束: envは空から始め、brokerのプロセスのenvを何も継がない。
- 上限を超えた出力は途中まで返さずprocess groupを止めてerrorにする（ADR-t827-2決定8）。
- **落とし穴**: `setsid`で抜けた子はprocess groupでは止まらず、containerのpidsの上限が最後の歯止めになる。
- **落とし穴**: allowlistは`argv[0]`しか見ないので、interpreter（shell・引数のコマンドを走らせるもの・言語）を入れるとそこから`git`もworkspaceの外も走らせられる。
  serverは一覧（`crates/dagq-broker/src/config.rs`の`INTERPRETERS`）の名前を拒まずwarningを出す（使い捨てのrepositoryの検証が`sh`を使うため、ADR-t827-3決定9）。
  子の読み込みを変えるenvの名前は拒んで起動しない（`LOADER_ENV_PREFIXES`）。
- imageには言語のtoolchainが無く、軽いコマンドだけが走る（ADR-t827-3決定9）。

## package.install

- 入口は`crates/dagq-broker/src/backends/package.rs`と、名前とargvの検査`dagq_broker_protocol::package::check_command`（[ADR-t840-1](../adr/2026-10-05-t840-1-broker-package-backend-runs-only-configured-commands.md)）。
- repositoryが`dagq.toml`の`[broker.package]`に名前ごとに書いたコマンドだけを、名前で走らせる。
  要求はargv・env・stdinを持たない。
- 実行は`process.exec`と同じ経路で、`exec_allow`とは独立している。
- dagqの設定の読み込みとserverの起動は同じ検査をし、dagqは行番号付きのerror、serverは起動しない。
- **落とし穴**: 限るのは`argv`だけで、コマンドが読むworkspaceの中身（workerが書ける）は限らない。
  `npm install`のlifecycle scriptやcargoの`build.rs`・`.cargo/config.toml`からworkerの書いたものが走りうるので、scriptを走らせない形を設定する。
- imageにはtoolchainが無いので、使うにはimageかmountの用意が別に要る。
  resource brokerはnetworkの制限を持たず、外への経路を絞るのはqueueのbroker（[ADR-t2113-1](../adr/2026-10-08-t2113-1-remove-the-resource-broker.md)）。

## containerとPodman machine

- 人が行うのはpodmanをhostに入れることだけで、dagq専用のmachine `dagq`の`init`と`start`は必要になったときにruntimeが行う。
- 用意の手順は`application::broker::start`（machine→image→container→health→imageの掃除）で、各段は冪等。
  `dagq broker start`とsupervisorは同じ`QueueBroker::start`を通る。
- queueごとのlockが同じqueueの`start`・`stop`を、host全体のmachineのlock（`broker_podman::machine_lock_home`）が別のqueue・test・e2eの関門のmachineの起動と停止を直列にする。
  host全体のlockは`start`の間ずっと持つので、buildの途中でmachineが止まらない代わりに、他のqueueは待つ。
- **落とし穴**: machineのlockを`XDG_DATA_HOME`の下に置くと、e2eのfixtureごとに別のlockになり、並ぶe2eが互いのmachineを止める。
- machineの起動の失敗は同じlockの中で1回だけやり直し（`ensure_machine`）、接続が途中で切れたコマンドは接続が答えるのを待ってやり直す（`Reconnecting`、ADR-t1162-1）。
- 人のmachineが動いていれば`machine_busy`にして何もしない。
- machineとcontainerの資源は`MachineSpec`・`ContainerLimits`で`host.toml`から上書きでき、machineの資源は`init`のときにしか効かない（ADR-t827-3決定7）。
- **落とし穴**: podmanは接続が1つも無いhostで最初に作ったmachineの接続を既定にする（`MachineSpec::init_args`）。
- 有効な印のある間は、別のimageや引数の古いcontainerも残す。
- 失敗は`BrokerFailure`（`FailureCode`）で、黙って続けない。
- 権限: `broker start`・`stop`は`service.lifecycle`、`broker status`・`logs`・`audit`は`queue.read`（[Authorization](authorization.md#capability)）。
  `start`と`stop`はmodeに依らず動く。

### machineのgvproxyの後片付け

- podmanのmachineはstartのたびにhostで`gvproxy`を起動し、stopの後にも孤児として残ることがあり、積み上がるとhostのメモリを食う。
- 片付けは`application::broker::clean_gvproxy`（見分けは`is_gvproxy_of`）で、machineが`stopped`と確かめられたときだけhost全体のlockの中で行う。
- 片付けるのは`QueueBroker`の`start`・`stop`だけで、`ensure_machine`・`release_machine`・`connect`はprocessに触れない。
- 片付けの失敗はstopやstartの成否を変えない。
- 孤児が残る原因は実podmanでしか確かめられず、まだ確定していない。

### image

- 入口は`containers/broker/Containerfile`と`src/broker_material.rs`。
- 実行のstageは`git`だけを持つ非rootのimageで（ADR-t827-3決定8・9）、`--userns=keep-id`でmountしたファイルの持ち主と揃える。
- 依存のcompileはcache mount（`CACHE_MOUNTS`）で使い回し、層のcacheは当てにしない（danglingの掃除が消すため）。
  **落とし穴**: cache mountはmachineのdiskの上で増え続け、上限と掃除は無い。

### supervisorの統合

ADR-t827-3の決定2・3。
入口は`src/application/supervise/broker.rs`のmoduleのdoc commentと`Supervisor::broker_pass`。

- modeは起動時に読み（`compose::load_broker_setup`）、起動の後の変更は起動し直すまで効かない。
- brokerの用意はjobのthreadで走り、imageのbuildの間もclaimを待たせない（`required`を除く）。
- healthが続けて失敗すればcontainerを1回起動し直し、直れば`auto_repaired`（ADR-0047の1層目）。
- `broker_unhealthy`はinbox宛てのattentionで、1つの不調で1回知らせる（`domain::broker::attention_stands`）。
- `preferred`ではbrokerの状態でclaimを止めず、使えなければ`broker_unavailable`を残して道具なしで動かす。
  自分の`ensure`が返る前は動いているbrokerを使い（`BrokerControl::running_port`）、負荷で1回答えそこねただけで道具なしにならないようhealthを数回見直す。
- supervisorがbrokerを止めるのは、`down`が頼んだdrainを終えqueueにほかのsupervisorが残らないときだけ（`stop_broker_after_down`）。
  `up`の入れ替えとexecの引き継ぎでは止めない。
- `down`は停止を頼み、drainを見届けた後には自分でも止める（`lifecycle::BrokerLifecycle`）。
- `up`のpreflightはpodmanがPATHで解決できることを確かめる。

## workerの道具（MCP）

- 渡すのは、modeが`preferred`か`required`でbrokerが健康で版が一致し、providerがClaude Codeのworkerのときだけ（Codexには渡さない）。
- 渡し方: `<run dir>/broker/mcp.json`をexecutorが`--mcp-config`で渡し、server単位の許可はsettingsでなく`--allowedTools`で渡す（`application::broker_run`・`AgentProvider::broker_tools`）。
  `disabled`のrunのsettingsと引数は変わらない。
- MCPの道具の一覧・入力のschema・結果とerrorの形・切り詰めは`crates/dagq-broker-client/src/mcp.rs`（`TOOLS`）。
  brokerの拒否は道具のerrorとしてbrokerのerror本体をそのまま返し、clientの失敗とは区別する。
- clientの切り詰めはbrokerの上限の後にかかり、modelに渡す量を限る（`mcp_cut`）。
- `write_receipt`は`required`のときだけ出て、brokerを通さずclientのプロセスがhostでreceiptを書く（brokerが答えなくても失敗のreceiptを書ける）。
- workerのpromptの段落は`prompt::BROKER_TOOLS`（`preferred`）と`prompt::BROKER_REQUIRED`で、tokenの値もtoken fileの場所も書かない。
  promptがclientの全ての道具を名指すことはtestが守る。
- 人の診断のCLIは`crates/dagq-broker-client/src/cli.rs`で、URLはloopbackだけを受ける（tokenをhostの外へ送らない）。

## required

[ADR-t838-1](../adr/2026-10-05-t838-1-required-broker-mode-refuses-built-in-tools-and-holds-claims.md)。
`required`のqueueでは、workerに組み込みのファイルとコマンドの道具を使わせずbrokerの道具だけで作業させ、brokerが使えなければworkerを起こさない。
**guardrailでありenforcementではない**: Claude Codeの設定はhostのプロセスを隔離せず、`status`と`doctor`の`enforcement: advisory`は変えない。

- 印: supervisorはworkerの起動・resume・reopen・非対話のturnの前に、runのdirに`broker_run::REQUIRED_FILE`を置く（他のmodeなら消す）。
  印のpathに何かあれば印とみなし、書けない印で組み込みの道具に戻らない。
  turnの前に印を書けなければ、tokenを失効させてturnを依頼しない。
- executor: 印があり`mcp.json`が無いrunは、agentを起動する前に拒む（`broker_run::worker_broker`）。
- turnの組み立ては`ClaudeCode::turn_command`と`headless_required_settings`（`src/infrastructure/adapters.rs`）。
  - permission modeとallowで、`Bash`は`dagq`で始まるコマンドだけになる。
    Claude Codeのdenyはallowに勝つので、`Bash`そのものはdenyに入れない（入れると`dagq`も打てない）。
  - **落とし穴**: setting sourcesを読まないので、利用者のsettingsの認証・model・pluginに頼るhostでは`required`のturnが失敗しうる。
- claimとresumeの前: `Supervisor::broker_holds_claims`が、今workerに道具を渡せるかとtokenを発行できるか（鍵・dir・repositoryのcommitter、`RunTokens::ready`）を見て、できなければそのpassはresumeもclaimもしない。
  landing・review・triageは続く。
  止めたことはinbox宛てのattention `broker_claims_held`で、`reason`が変われば書き直し、渡せるようになれば閉じる。
- 判断の関数は`domain::broker`。
- claimの後に渡せなかったrun（runごとの失敗）は、claimの`provision`ならrunを`abandon`し、resumeとreopenならworkspaceを開かない。
  runごとの失敗は次のpassの判定を止めない。
- 走っているrunでbrokerが落ちても、supervisorはrunを止めない。
- 実Claudeでの確かめは[手動スモーク](manual-smoke.md#required-の-broker-のスモーク)、実podmanでは`tests/e2e/broker.rs`。

## audit

- 入口は`crates/dagq-broker/src/audit.rs`の`AuditRecord`（欄の意味）と`AuditLog`（ADR-t827-4決定2）。
- `<queue dir>/broker/audit/<YYYY-MM-DD>.jsonl`（UTCの日）に1要求1行で追記し、拒んだ要求とhealthも残す。
- 残さないもの: token・署名・鍵・ファイルの中身・diff・プロセスの入出力・envの名前と値・引数・commit message。
- **落とし穴**: auditは記録で、改ざんへの耐性は持たない（hostのworkerも`process.exec`も書ける）。
  引数のhashは照合用で、推測しやすい引数は総当たりで戻せる。
- 読むのは`dagq broker audit`（`application::broker_admin::audit`、port `AuditFiles`、adapter `infrastructure::broker_audit::AuditDir`）。
  queue DBには取り込まず、DBも開かない。

## 組み込みの道具の数

`preferred`の間、workerがbrokerを通さずに組み込みの道具で行った操作をrunごとに数え、どこがbrokerに移っていないかを見えるようにする（入口は`src/domain/broker_usage.rs`）。

- 数えるのはbrokerの道具を渡したrunだけで、`disabled`と道具なしのrunのsettingsは変わらない。
- workerのsettingsに組み込みの道具ごとの`PreToolUse`のhookを足し（`infrastructure::adapters::with_direct_tool_hooks`）、道具の名前だけをrun dirのlogに追記する。
  hookは必ずexit 0で終わり、道具を止めない。
  道具の入力は残さず、制御側の`Bash`も`Bash`として数える。
- 記録は、runが終わってtokenを失効させるsweepで1回だけ（`domain::broker_usage::records_tool_use`、`Supervisor::broker_record_usage`）。
  hookのlogはworkerが書けるので、linkを辿らず通常のfileだけを上限付きで読み（[非対話のworker](supervisor-lifecycle/headless-worker.md)の「run dirの`turns/`」）、知らない名前の行は数えない。
- 出口はrunのevent `broker_tool_use`と、`show`の最新runの欄（`domain::broker_usage::latest_tool_use`）。

## mode と設定

- repositoryの方針は`dagq.toml`の`[broker]`（`domain::broker::BrokerConfig`、読み手は[Run environment](supervisor-lifecycle/run-environment.md)の`parse_config`）。
  hostの事情は`host.toml`の`[broker]`（`HostBroker`、`infrastructure::broker_config::load_host_broker`）。
- modeは`disabled`（既定）・`preferred`・`required`で、`host.toml`は`disabled`に落とせるだけで上げられない（`resolve_mode`、ADR-t827-4決定4）。
- `dagq.toml`の値はcontainerの`serve`の引数になる（`BrokerConfig::serve_args`）。
  変えればcontainerの引数の指紋が変わり、作り直される。
- この repositoryの`dagq.toml`には`[broker]`を置かない（`[broker]`を知らない固定バイナリが止まるため、本番queueはdisabled）。
  検証は使い捨てのrepositoryで行う。

## status と doctor

- `actors`の`backend: host`・`enforcement: advisory`は変えない（brokerを使うworkerも隔離されていない、ADR-t827-4決定5）。
- `status`の`broker`は記録（eventと`state.json`）を読むだけで、podmanのコマンドもbrokerへの要求も打たない（`domain::broker::health_report`・`application::broker::client_report`）。
- `doctor`の`broker`もpodmanを打たず、machine・image・healthは`dagq broker status`が見る。

## capabilityの関係

- brokerのcapabilityはprotocolの`BrokerCapability`で、queueの操作の`Capability`（[Authorization](authorization.md#capability)）とは別の名前空間。
- `reserved.filesystem_*`・`reserved.network`・`reserved.secret_read`は誰にも与えないまま（ADR-t827-4決定5）。

## 段階

- Phase 1: workerはhostのまま、`preferred`で契約を証明する。
- Phase 2: `required`、組み込みの道具の数、package backend。
- Phase 3以降は無い。
  workerのcontainer化はresource brokerを経ず、runごとのcontainerの中で組み込みの道具をそのまま使う形で行う（[ADR-t2113-1](../adr/2026-10-08-t2113-1-remove-the-resource-broker.md)・[ADR-t2113-3](../adr/2026-10-08-t2113-3-only-workers-resume-and-integrate-verification-run-in-containers.md)）。
  gitはmountの設計で守り（[ADR-t2113-2](../adr/2026-10-08-t2113-2-git-is-guarded-by-the-mount-design.md)）、外への経路はqueueのbrokerだけにする。

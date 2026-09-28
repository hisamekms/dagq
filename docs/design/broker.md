---
id: design-broker
type: design
title: Resource broker
status: draft
created: 2026-09-28
updated: 2026-09-28
last_verified: 2026-09-28
scope: runtime
tags:
  - security
  - broker
related:
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

fs・process・gitを仲介するresource broker（`dagq-broker`）の設計。goal 58（Phase 1）が実装する。**この文書はまだ実装されていない姿（status: draft）を書き、goal 58の各taskが実装に合わせて直し、全体が着地したら`current`にする。** 決定の理由は[ADR-t827-1](../adr/2026-09-28-t827-1-broker-crates-binaries-and-version-alignment.md)（crateとバイナリと配布）、[ADR-t827-2](../adr/2026-09-28-t827-2-broker-transport-run-token-and-workspace-confinement.md)（transport・token・mountと閉じ込め・git）、[ADR-t827-3](../adr/2026-09-28-t827-3-supervisor-runs-the-broker-container-on-a-dedicated-podman-machine.md)（containerとPodman machine）、[ADR-t827-4](../adr/2026-09-28-t827-4-worker-mcp-tools-audit-mode-and-relations.md)（workerの道具・audit・mode・関係）。

名前: この文書の「broker」はresource brokerのこと。goal 38（draft）の「broker」は実行側からqueue serviceへの出口で、別物（ADR-t827-4決定6）。

**隔離ではない。** workerはhostのプロセスのままで、`preferred`のworkerはbrokerを迂回して組み込みの道具やhostのファイルを直接使える。host実行は助言的（[Security](security.md#host実行は助言的advisory)、ADR-t728-1決定6）で、この段はbrokerの契約（token・閉じ込め・上限・audit）を証明するもの。

## 全体の形

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

## crateとバイナリ

rootの`Cargo.toml`のpackageは`dagq`のまま（ADR-t827-1決定1）。workspaceに`crates/`の3つを足す。

| crate | 種類 | 中身 | 依存（増える主なもの） | crates.io |
| --- | --- | --- | --- | --- |
| `dagq-broker-protocol`（`crates/dagq-broker-protocol`） | lib | 要求と応答のDTO、`BrokerCapability`、`ErrorCode`、`TokenClaims`、`sign` / `verify`（HMAC-SHA256）、pathの定数、protocolの版（`PROTOCOL_VERSION = 1`） | `serde`・`serde_json`（今）、`sha2`・`hmac`（`sign` / `verify`を足すtaskで。base64urlは自前の小さな関数） | publishする（最初） |
| `dagq-broker`（`crates/dagq-broker`） | lib + bin `dagq-broker`（`main`は薄い） | HTTPのserver（同期の小さなserver。例: `tiny_http`）、fs・process・gitのbackend、tokenの検証、audit | protocol・HTTPのserver | publishする（最後。releaseのimageの材料） |
| `dagq-broker-client`（`crates/dagq-broker-client`） | lib + bin `dagq-broker-client` | HTTPのclient（例: `ureq`、TLSなし）、subcommand `mcp`（stdioのMCP server。JSON-RPCは`serde_json`で自前）、診断のCLI | protocol・HTTPのclient | publishする（dagqの後） |

- `dagq`は`dagq-broker-protocol`だけに依存し（`version = "=<同じ版>"`と`path`）、HTTPの依存を持たない。brokerのhealthは`dagq-broker-client health --json`を子プロセスで呼んで見る（ADR-t827-1決定2）
- 版は全crateで1つ。rootの`[package] version`は継承にせず文字どおりに書き（`scripts/check-plugin-version.sh`が`[package]`のversionを読むため）、`crates/`の各crateも同じ値を書く。一致は`check-plugin-version.sh`に検査を足して守る
- tokioなどのasync runtimeは入れない（最小のmachineでのbuildを軽く保つ）
- 今（task 828）: protocolの型（下の「protocolの型」）と、serverとclientの骨組み（`dagq-broker --version`・`dagq-broker health`（healthの応答のJSONを出す）、`dagq-broker-client --version`）。未知の引数はexit 2。HTTP・token・backend・MCPは後のtaskが足す。serverとclientの`--version`はまだ`CARGO_PKG_VERSION`で、dagqのbuild識別子（`X.Y.Z-dev+<commit>`）にそろえるのは後のtask。dagqはまだprotocolに依存しない（tokenの発行を足すtaskで依存する）

### protocolの型

`dagq-broker-protocol`の型は全て`#[serde(deny_unknown_fields)]`で、未知の欄・capability・error code・roleを読むと失敗する（fail closed）。serializeは欄の宣言の順で、集合と表は`BTreeSet` / `BTreeMap`でkeyの順なので、同じ値は同じbyte列になる（`encode` / `decode`。claimsの署名はこのbyte列に対して行う）。testは各型の正確なJSONを固定する。

| 型 | wire |
| --- | --- |
| `BrokerCapability` | `"fs.read"`・`"fs.write"`・`"process.exec"`・`"git.read"`・`"git.write"`（この順） |
| `TokenClaims` | 上の「token」の欄の順。`role`は`BrokerRole`（`"worker"`だけ）、`committer`は`Committer {name, email}`、`capabilities`は`BTreeSet<BrokerCapability>`、`task_id`・`iat`・`exp`は整数 |
| `BrokerRequestId` | 文字列そのまま（serverが作るuuid） |
| `ErrorCode` / `BrokerError` / `ErrorBody` | 下の「error」の7つのcodeとHTTP status、`{"error":{"code","message","request_id"}}` |
| `Operation` | 下の表のmethodとpathとcapability、auditの`op`の名前（`fs.list`・`git.status`など） |
| `HealthResponse` | `{"status":"ok","build","protocol"}` |
| `fs::{Read,List,Write,Edit}{Request,Response}` | 下の表の欄。`offset` / `limit`は無ければ省く。`create_dirs` / `replace_all`は既定`false`で常に出す。`List`の`entries`は`{name, kind, size}`で`kind`は`file` / `dir` / `symlink` / `other` |
| `process::{ExecRequest,ExecResponse}` | `env`は`BTreeMap`で空なら省く。`stdin` / `timeout_secs`は無ければ省く。`exit_code`はsignalで終わったとき`null` |
| `git::*` | `status`の応答は`{branch, entries:[{path, status}]}`（`branch`はdetachedで`null`、`status`はporcelainの2文字）。`diff`の要求は`staged`（常に出す）と`paths`（空なら省く）。`log`の`commits`は`{commit, author, time, subject}`。`add`の応答と`status`の要求は`{}` |

### workspaceと検証

```toml
[workspace]
members = [".", "crates/dagq-broker-protocol", "crates/dagq-broker", "crates/dagq-broker-client"]
default-members = [".", "crates/dagq-broker-protocol", "crates/dagq-broker", "crates/dagq-broker-client"]
resolver = "3"
```

- `default-members`でrootの`cargo test --locked`・`cargo clippy --locked --all-targets -- -D warnings`・`cargo nextest run`が全crateを覆う。workerの手元の`cargo test --locked --test it <module>::`と`--lib <module>`は`-p`なしで通る（task 828で確かめた。`--test it`はdagqのtestだけを選び、`--lib`は全crateのunit testからfilterに合うものを選ぶ）。brokerのcrateのtestは`-p <crate>`で絞る
- coverageの関門は`cargo llvm-cov nextest --locked --workspace --fail-under-lines 80`（ADR-t828-1がADR-t827-1決定3をamends）。cargo-llvm-cov（0.9.1）はrootがpackageのworkspaceで`-p`も`--workspace`も無いと`default-members`を見ずroot package（dagq）だけをreportに入れるので、`--workspace`で全crateを80%に数える。登録済みのtaskの旧コマンド（`--workspace`なし）はdagqのcoverageと全crateのtestの成否を見て、brokerのcrateの行は数えない
- 新しいcrateのtestは各crateの`src/`のunit testと`crates/<crate>/tests/`に置く。serverのtestは`CARGO_BIN_EXE_dagq-broker`をhostのプロセスとして`--bind 127.0.0.1:0`と一時のdirで起こし、podmanなしでcoverageに数える。podmanを要るtest（containerの起動・mount・LANで待ち受けないことの確認）は`#[ignore]`で、e2eと同じく関門とCIに数えない
- `scripts/check-test-file-lines.sh`は`tests/`と`crates/*/tests/`の下の`.rs`を見る（task 828）
- CI（`ci.yml`）のcoverageの関門は上の`--workspace`の形で、他のコマンドはそのまま。CIはmacOSだけなので、serverのLinux（musl）の経路とContainerfileはCIではbuildもtestもされない。serverのコードはOSに依らない部分（tokenの検証・閉じ込め・exec・audit）をmacOSのtestで覆い、Linuxだけの部分（`openat2`など）は`cfg`で分けてpodmanを要る`#[ignore]`のtestとスモークで確かめる
- dagqのpodmanを呼ぶ部分（`broker::ensure`）はpodmanのコマンドをportの後ろに置き、判定（machine・image・containerの状態からの次の一手、lock、版の比較）をstubのportでunit testする。実物のpodmanを呼ぶadapterだけが`#[ignore]`のtestになる
- releaseの`cargo build --release --locked --target`は全default membersを作るので、`-p dagq -p dagq-broker-client`に絞る。`cargo publish --locked`は`-p`で4つのcrateを順に打ち、crates.ioに同じ版があるcrateは飛ばす（`release.yml`）。Trusted Publisherは既にあるcrateにしか登録できないので、brokerの3つのcrateの最初のpublishは人がAPI tokenで手で行い、それぞれにTrusted Publisherを登録する（`.claude/skills/release`）
- `install`とauto-updateのbuildは今`cargo build --release --locked -p dagq`（dagqだけ。`src/infrastructure/binaries.rs`）で、clientを一緒にbuildして置くのは下の「配布と版」のtask

### runtimeのパス

`src/application/update.rs`の`RUNTIME_PATHS`に`crates/`を足す（ADR-t827-1決定4）。`crates/`の変更はclientとimageの材料を変えるので、auto-updateの対象になる。imageのbuildのstageのRustの版は`rust-toolchain.toml`から取るので、`rust-toolchain.toml`も足す。

## 配布と版

| もの | 置き場所 | 作る時 | 版の確認 |
| --- | --- | --- | --- |
| `dagq` | `~/.local/bin/dagq`（今までどおり） | `install`・auto-update・releaseのupdate | build識別子 |
| `dagq-broker-client` | `dagq`の隣（`~/.local/bin/dagq-broker-client`） | dagqと同じcheckout・同じ`cargo build --release --locked -p dagq -p dagq-broker-client` | `dagq-broker-client --version`がdagqのbuild識別子と一致 |
| brokerのimage | dagq専用のmachineの中、`localhost/dagq-broker:<build tag>` | brokerの起動のとき、tagが無ければ | tag（build識別子）とhealthの`build` |

- `install`とauto-updateは`<queue dir>/update/staged/`に`dagq`と`dagq-broker-client`を置き、両方を同じ確認（`--version`）とrenameで差し替え、それぞれ`.previous`を残す。先にclient、次にdagqの順で置き、dagqの確認か引き継ぎが失敗したら両方を`.previous`に戻す。`install --rollback`も両方を戻す（`src/application/install.rs`・`src/application/update.rs`・`src/infrastructure/binaries.rs`）
- `HostActorExecutor`が使うclientは`std::env::current_exe()`の隣の`dagq-broker-client`だけで、その`--version`がdagqのbuild識別子と一致するときだけ使う（結果はバイナリのmtimeごとにcacheする）
- imageの材料（ADR-t827-1決定6）:
  - dev build（checkoutからのbuild。`build.rs`が`crates/dagq-broker`を見つけたとき）: `Containerfile`、`crates/dagq-broker-protocol`と`crates/dagq-broker`のsource、workspaceのmanifestをこの2つに絞ったもの、`Cargo.lock`を`build.rs`がtarにして`include_bytes!`で埋める。buildのstageは`--locked`を付けずに作る（`Cargo.lock`は全workspaceのもので、使わないpackageの行が消えるだけで、使うpackageの版はlockのまま）
  - release（crates.ioのdagqのpackage。`crates/`を含まない）: `Containerfile`だけを埋め（dagqのpackageの中に置く）、buildのstageは`cargo install --locked dagq-broker@<version>`で同じ版のserverを作る
  - brokerを起動するdagqは材料を一時のdirに展開して`podman build`する。どちらもmachineからcrates.ioとdocker.ioへのnetworkを要る
- build tagはbuild識別子の`+`を`-`に置き換えたもの（`0.5.0-dev-<commit>`、`.dirty`は`-dirty`）
- 版の食い違い（clientが無い・clientの版が違う・healthの`build`がdagqと違う・`X-Dagq-Broker-Protocol`が違う）では、dagqはtokenを発行せずworkerに道具を渡さない（fail closed、ADR-t827-1決定7）。`preferred`ではrunのevent `broker_unavailable`（`reason: version_mismatch`）を残してworkerは道具なしで動く
- auto-updateの後（healthの`build`がdagqと違う）: 古いcontainerは`active/`に印が残る間、そのrunのために動かし続ける（そのrunのworkerのMCPのprocessは古いclientで、同じ版どうしで話す）。その間の新しいclaimには`version_mismatch`でtokenを出さない。印が無くなったら`broker::ensure`が新しいimageで起動し直す（ADR-t827-3決定2）。古いcontainerが要る間も、そのrunのtokenの期限の前の発行し直しは同じ鍵で続ける
- 古いimageは現在と1つ前のtagだけを残し、`broker::ensure`がそれ以外を`podman image rm`する
- release: crates.ioには`dagq-broker-protocol` → `dagq` → `dagq-broker-client` → `dagq-broker`の順でpublishする（serverはreleaseのimageのbuildの材料。ADR-t827-1決定6・8）。GitHub Releaseにはdagqと同じ形の`dagq-broker-client-v<version>-<target>.tar.gz`を足し、`SHA256SUMS`に含める。imageはregistryに出さない。releaseのupdate（[Release update](supervisor-lifecycle/release-update.md)）は`cargo install --locked dagq@<version>`と`dagq-broker-client@<version>`を同じ`--root`に入れて両方を差し替える

## transport

- `127.0.0.1:<port>`のHTTP/1.1とJSONだけ。hostへのpublishは`-p 127.0.0.1:<port>:<container port>`だけにする。containerの中でbrokerがbindするaddressは、Podmanの転送がloopbackに届けるなら`127.0.0.1`、届けないならcontainerの私的なinterface（LANのinterfaceではない）にし、実装のtaskが測って決める。`dagq-broker`は`--container`のとき以外、loopbackでないaddressへのbindを拒む。`#[ignore]`のtestが、hostのLANのaddressへの接続が拒まれることを確かめる
- portはqueueごと。`host.toml`の`[broker] port`（既定0 = 空いているportを選ぶ）。選んだportは`<queue dir>/broker/state.json`に残し、containerを起動し直すときも同じportを使う
- header: `Authorization: Bearer <token>`、`X-Dagq-Broker-Protocol: 1`。応答にも`X-Dagq-Broker-Protocol`と`X-Dagq-Broker-Build`を付ける
- 要求の本体の上限は8 MiB（超えれば`invalid_request`）

| method と path | capability | 要求 | 応答 |
| --- | --- | --- | --- |
| `GET /v1/health` | なし（tokenを要らない） | — | `{"status":"ok","build":"<build識別子>","protocol":1}` |
| `POST /v1/fs/read` | `fs.read` | `{path, offset?, limit?}`（行） | `{content, lines, truncated}` |
| `POST /v1/fs/list` | `fs.read` | `{path}` | `{entries:[{name, kind, size}]}` |
| `POST /v1/fs/write` | `fs.write` | `{path, content, create_dirs?}` | `{bytes}` |
| `POST /v1/fs/edit` | `fs.write` | `{path, old_string, new_string, replace_all?}` | `{replacements}` |
| `POST /v1/process/exec` | `process.exec` | `{argv, env?, stdin?, timeout_secs?}` | `{exit_code, stdout, stderr, duration_ms}` |
| `POST /v1/git/status` | `git.read` | `{}` | `{branch, entries}` |
| `POST /v1/git/diff` | `git.read` | `{staged?, paths?}` | `{diff, truncated}` |
| `POST /v1/git/log` | `git.read` | `{limit?}`（既定20、上限200） | `{commits}` |
| `POST /v1/git/add` | `git.write` | `{paths}` | `{}` |
| `POST /v1/git/commit` | `git.write` | `{message}` | `{commit}` |

`fs.edit`は組み込みのEditと同じで、`old_string`が1つだけ見つかるとき（`replace_all`なら1つ以上）に置換し、見つからない・複数あるときは`invalid_request`。

### error

本体は`{"error":{"code":"<code>","message":"<人向けの短い文>","request_id":"<uuid>"}}`。messageにtoken・ファイルの中身・envの値を入れない。

| code | HTTP status | 意味 |
| --- | --- | --- |
| `unauthorized` | 401 | tokenが無い・書式が違う・署名が合わない・期限切れ・有効な印が無い・未知のcapability・claimsの欠け |
| `capability_denied` | 403 | tokenがopの要るcapabilityを持たない。execのallowlistの外のプログラム、`git`のexec |
| `workspace_violation` | 403 | workspaceの外、`..`、symlinkの逃げ、worktreeの`.git`、run branchでないHEADでのcommit |
| `timeout` | 504 | execのtimeout（プロセスは止めた） |
| `output_limit` | 413 | execの出力かfs・gitの応答が上限を超えた（execはプロセスを止めた） |
| `backend_error` | 502 | fs・process・gitの失敗（存在しないファイル、gitのerror） |
| `invalid_request` | 400 | 未知のpath・未知の欄・型の誤り・protocolの版の違い・本体の上限超え・Editの不一致 |

## token

- 書式: `dagq1.<base64url(claimsのJSON)>.<base64url(HMAC-SHA256(key, "dagq1." + claims部))>`
- claims（`TokenClaims`、`#[serde(deny_unknown_fields)]`）:

| 欄 | 中身 |
| --- | --- |
| `v` | `1` |
| `jti` | tokenのID（uuid v4） |
| `actor_id` | workerのactor id（[Roles](supervisor-lifecycle/roles.md#actors)の形） |
| `role` | `worker`（Phase 1ではworkerだけに発行） |
| `task_id` / `run_id` | runのtaskとrun |
| `workspace` | runのworktreeの絶対パス（canonical） |
| `branch` | `dagq/<run id>`（commitを許すbranch） |
| `committer` | `{name, email}`。発行のときにrepositoryの`git config user.name` / `user.email`から入れる（containerにはhostの`~/.gitconfig`が無いため） |
| `capabilities` | `["fs.read","fs.write","process.exec","git.read","git.write"]`の部分集合 |
| `iat` / `exp` | 発行と期限（UNIX秒） |

- 鍵: `<queue dir>/broker/key`（32 byteの乱数、mode 0600）。`broker::ensure`が無ければ作る。containerには読み取り専用でmountする。鍵を替えると全てのtokenが無効になる。Phase 1に定期の入れ替えは無く、入れ替えは人がbrokerを止めて鍵を消し、`broker start`で作り直す（生きているrunのtokenは次の更新のtickで新しい鍵で発行し直す）
- 偽造: hostでは同じユーザーのプロセス（workerを含む）が鍵を読めるので、tokenを偽造できる。Phase 1の`process.exec`も鍵をmountから読めうる（下の「既知の制限」）。tokenは誤りと事故を止め、auditでrunを名指すためのもので、境界ではない
- 発行: supervisorがclaimでworkerを起こすとき（`provision`）とresumeのとき、modeが`preferred`でbrokerが健康で版が一致するときだけ。capabilityはdagqの`broker_grants(role)`（Phase 1はworkerだけに上の5つ）から写す。書き先は`<queue dir>/broker/tokens/<run id>`（mode 0600、同じdirの一時ファイルとrenameで置く）。containerにmountしない場所に置き、`process.exec`から他のrunのtokenを読めないようにする
- 有効な印: `<queue dir>/broker/active/<jti>`（中身はrun_id）。発行で作り、失効で消す。brokerは要求ごとに印の有無を見る
- 期限: `exp = iat + 12時間`。supervisorはleaseの更新のtickで、残りが4時間を切った生きているrunのtokenを発行し直す（新しいjtiの印を作ってtoken fileを置き換え、古い印を消す）。clientは要求ごとにtoken fileを読み直す
- 失効: runの終わり（`integrated`・`failed`・`canceled`・leaseの喪失・`recover`）とresumeの発行し直しで印を消し、`<queue dir>/broker/tokens/<run id>`と`<run dir>/broker/`を消す。supervisorは起動時に、終わったrunの印を掃除する。supervisorが止まっている間に終わったrunの印は、次の起動の掃除か期限まで残る（その間そのtokenは使える）
- event: runのevent `broker_token_issued`（`jti`・`capabilities`・`exp`）と`broker_token_revoked`（`jti`・`reason`）。tokenの値は残さない
- 未知のcapabilityを含むtokenは全体を拒む（fail closed）

## mountと閉じ込め

containerのmount（全てhostと同じ絶対パス。ADR-t827-2決定5）:

| host | 権限 | 理由 |
| --- | --- | --- |
| `<queue dir>/runs` | rw | runのworktree（`runs/<run id>/worktree`） |
| repositoryのgitの共通dir（`git rev-parse --path-format=absolute --git-common-dir`） | rw | worktreeの`.git`ファイルが指す`<common>/worktrees/<name>`、objects、refs |
| `<common>/config`・`<common>/hooks` | ro（上のrwに重ねる） | containerからhookや、hostでコードを走らせる設定（`core.hooksPath`・`core.fsmonitor`・`diff.external`・filter）を書かせない（ADR-t827-2決定5） |
| `<queue dir>/broker/key` | ro | tokenの検証 |
| `<queue dir>/broker/active` | ro | 有効な印 |
| `<queue dir>/broker/audit` | rw | audit |

`<common>/config`にremoteのURLの中のtokenや`http.extraHeader`があればcontainerから読めるので、brokerを有効にするrepositoryでは資格情報をrepositoryのconfigに置かない。

mountしないもの: `$HOME`、`~/.ssh`、`~/.aws`、`~/.config`（ghのtokenを含む）、`~/.gitconfig`、cargoのhome、Podman / Dockerのsocket、queue DB（`queue.db`とそのwal）、queue dirのそれ以外（`host.toml`・`update/`など）、main checkoutの作業ファイル、他のqueue。

閉じ込め（fsのop）:

1. 要求の`path`はworkspaceからの相対か、workspaceの中の絶対パス。字句で正規化し、workspaceの根より上に出る`..`は拒む
2. 開くのはworkspaceの根のdir fdからの`openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS)`（Linux）で、確認と開くことの間の競合（親dirをsymlinkに差し替える）を作らない。`openat2`の無いOS（macOSのhostのtest）は要素ごとの`openat(O_NOFOLLOW)`で辿る
3. workspaceの根の`.git`（worktreeのgitdirを指すファイル）とその下は、読みも書きも拒む。hostのfilesystem（APFS）は大文字と小文字を区別しないので、名前の比較は大文字と小文字を区別せずに行う
4. どれに当たっても`workspace_violation`

別のrunのworktreeはmountされているが、tokenのworkspaceの外なのでfsのopでは届かない。

### 既知の制限（Phase 1）

`process.exec`のプロセスはbrokerと同じuidで同じmountを見る（ADR-t827-2決定6）。execを許したプログラムによっては次ができうる:

- 他のrunのworktree・receiptの読み書き
- mountした鍵と`active/`を読んでのtokenの偽造
- auditの書き換えと削除
- gitの共通dirの他のbranchのref・`packed-refs`・objectsの書き換え（`config`と`hooks`は読み取り専用）

`exec_allow`の既定は空で、ファイルを読むプログラムを許すのは使い捨てのrepositoryの検証だけにする。hostのworkerはこれら全てをhostで直接できるので、Phase 1で失うものは無い（ADR-t728-1決定6）。Phase 3以降で、execをbrokerの秘密に届かない別のuidとrunごとのmountで走らせる。

Podman machineは既定でhostの`$HOME`をVMにmountするので、containerから抜け出せば`~/.ssh`などが見える（ADR-t827-3決定6）。

## git

- brokerのgitは次の設定で走らせる: `-c core.hooksPath=/dev/null -c credential.helper= -c core.fsmonitor=false -c protocol.allow=never -c core.sshCommand=false -c core.pager=cat -c safe.directory=<workspace>`、diffとlogは`--no-ext-diff --no-textconv`、env `GIT_TERMINAL_PROMPT=0`・`GIT_CONFIG_NOSYSTEM=1`・`HOME=<一時のdir>`、authorとcommitterはtokenの`committer`
- opは`status`・`diff`・`log`・`add`・`commit`だけ。push・fetch・pull・remote・config・checkout・branch・reset・rebase・tag・credentialは持たない
- `commit`は`git symbolic-ref HEAD`がtokenの`branch`（`refs/heads/dagq/<run id>`）のときだけ行う。違えば`workspace_violation`
- `add`の`paths`はfsと同じ閉じ込めを通す
- `process.exec`の`argv[0]`のbasenameが`git`なら、allowlistに関わらず`capability_denied`。これは誤りを止めるもので、`sh -c`・`env`・複製したバイナリからは通る

## process.exec

- `argv`は配列で、shellを通さない。`argv[0]`はallowlist（`dagq.toml`の`[broker] exec_allow`。既定は空で、execは全て拒む）のbasenameと一致するものだけ。`git`は常に拒む
- cwdはworkspace。envは固定の`PATH=/usr/local/bin:/usr/bin:/bin`・`HOME=<execごとの一時のdir>`（brokerのgitの`HOME`とは別）・`LANG=C.UTF-8`・`TERM=dumb`に、要求の`env`のうち`[broker] exec_env`に名前があるものだけを足す。他の値は捨てる（名前はauditに残さない）
- timeout: 要求の`timeout_secs`（既定`[broker] exec_timeout_secs` = 60、上限`exec_max_timeout_secs` = 300）。超えたらprocess groupにSIGKILLを送り`timeout`。`setsid`で抜けた子はprocess groupでは止まらず、containerのpidsの上限が最後の歯止めになる
- 出力: stdoutとstderrの合計が`[broker] output_limit_bytes`（既定1 MiB）を超えたらprocess groupを止めて`output_limit`
- 走るのはcontainerの中で、containerのmemory・cpu・pidsの上限に入る。imageにはtoolchainが無く、軽いコマンド（`sh`・`ls`・`cat`・`grep`など、allowlistにあるもの）だけ。使い捨てのrepositoryの代表のtaskもそれで済むものにする（ADR-t827-3決定9）
- `sh`をallowlistに入れると、shellからimageの中の`git`も走らせられる（containerに資格情報もremoteへの経路の設定も無いので上流へのpushは通らないが、同じrepositoryの他のrefは書き換えうる）。使い捨てのrepositoryの検証のためだけに使い、既定には入れない

## containerとPodman machine

`broker::ensure(queue)`（supervisor・管理のコマンド・podmanを要るtestが呼ぶ）は次を順に冪等に行う。queueごとのlock `<queue dir>/broker/lock`（flock）で直列にする。

1. **machine**: host全体のlock `$XDG_DATA_HOME/dagq/podman-machine.lock`（無ければ`~/.local/share/dagq/podman-machine.lock`）の中で、`podman machine inspect dagq`が無ければ`podman machine init dagq --cpus <cpus> --memory <MiB> --disk-size <GiB>`（rootless、既定のvolumeのまま）。止まっていれば`podman machine start dagq`。**`podman system connection default`と`machine set-default`は打たない。** 他のmachineが動いていてstartできないときは状態を`machine_busy`にし、人のmachineは止めない
2. **image**: `podman --connection dagq image exists localhost/dagq-broker:<build tag>`が無ければ、埋めた材料を展開して`podman --connection dagq build --build-arg CARGO_BUILD_JOBS=1 -t localhost/dagq-broker:<build tag>`。buildは長いので、supervisorは子プロセスでbackgroundに走らせ（状態`building`、logは`<queue dir>/broker/build.log`）、claimを待たせない。buildの間の`preferred`のclaimは`broker_unavailable`（`reason: building`）で道具なしに進む
3. **container**: `dagq-broker-<queue hash>`が無ければ作る。imageのtagが違うときは`active/`に印が無くなってから作り直す（上の「配布と版」）。`<common>/config`と`<common>/hooks`は`-v ...:ro`で重ねる。起動の形:

   ```sh
   podman --connection dagq run -d --name dagq-broker-<queue hash> \
     --userns=keep-id --read-only --tmpfs /tmp:size=64m \
     --cap-drop=all --security-opt no-new-privileges \
     --memory 512m --cpus 1 --pids-limit 256 \
     -p 127.0.0.1:<port>:8750 \
     -v <queue dir>/runs:<queue dir>/runs:rw \
     -v <git common dir>:<git common dir>:rw \
     -v <queue dir>/broker/key:<queue dir>/broker/key:ro \
     -v <queue dir>/broker/active:<queue dir>/broker/active:ro \
     -v <queue dir>/broker/audit:<queue dir>/broker/audit:rw \
     localhost/dagq-broker:<build tag> --container --listen <addr>:8750 ...
   ```

4. **health**: `dagq-broker-client health --json --url http://127.0.0.1:<port>`が`build`の一致を返すまで最大30秒待つ

- `<queue dir>/broker/state.json`: `port`・`container`・`image`・`build`・`started_at`・`state`（`building` / `running` / `unhealthy` / `machine_busy` / `stopped`）
- supervisorは起動時とclaimの前（modeが`disabled`でないとき）に`ensure`し、tickごと（30秒）にhealthを見る。3回続けて失敗したら、まずcontainerを1回起動し直し（`auto_repaired`、ADR-0047の1層目）、それでも失敗ならqueueのattention `broker_unhealthy`（next: `dagq broker status`）をinbox宛てに出す。`machine_busy`も同じattentionで`reason: machine_busy`
- `down`はdrainの後に`podman --connection dagq stop dagq-broker-<queue hash>`し、host全体のlockの中で、machineに動いているcontainerが無ければ`podman machine stop dagq`（`broker::release_machine`）。podmanを要る`#[ignore]`のtestとスモークも終わりに同じ`release_machine`を呼ぶ
- execの引き継ぎ（installとauto-update）ではcontainerを止めない。新しいsupervisorの`ensure`がbuildの違いを見て作り直す。作り直しの間の要求はclientが接続の失敗として1回だけ再試行する
- 管理のコマンド: `dagq broker status`（状態を変えない）、`dagq broker start`・`dagq broker stop`（Operationの判定を通す。新しいcapability `broker.manage`をuser・inbox・supervisorに与え、[Authorization](authorization.md)と[Security](security.md)の表に足す）、`dagq broker logs`（`podman logs`の末尾）、`dagq broker audit`（下の「audit」）
- `up`のpreflight: modeが`disabled`でなければpodmanの実行ファイルを確かめ、無ければsupervisorを起動しない（`[run.env]`のプログラムの検査と同じ扱い）
- event: queueのevent `broker_started`（`port`・`build`・`image`）・`broker_stopped`・`broker_image_built`（`build`・`duration_ms`）

### 資源（host.tomlで上書き）

| 値 | 既定（起点） | 決め方 |
| --- | --- | --- |
| machineの名前 | `dagq` | 固定。人の既定のmachineと分ける |
| machine CPU | 1 | |
| machine memory | 1024 MiB | imageのbuildが通らなければ512 MiBずつ上げ、通った最小をここに書く |
| machine disk | 10 GiB | buildのimage（rust）と実行のimageが入る最小 |
| container memory / cpus / pids | 512m / 1 / 256 | brokerとexecの軽いコマンドが通る最小 |
| buildの並列度 | `CARGO_BUILD_JOBS=1` | 最小のmachineでメモリを越えないため |

実装のtaskは、最小のmachineでのimageのbuildの所要時間と最大のメモリ、brokerの常駐のメモリを測ってこの表に書く。

### image

`Containerfile`（dagqに埋める材料の1つ）の形:

- buildのstage: `docker.io/library/rust:<rust-toolchain.tomlと同じ版>-alpine`（digestで固定）で`cargo build --release -p dagq-broker`（`CARGO_BUILD_JOBS`はbuild-arg）。muslの静的なバイナリ
- 実行のstage: `docker.io/library/alpine:<版>`（digestで固定）に`git`だけを`apk add --no-cache`し、非rootの`USER`にし（起動は`--userns=keep-id`でhostのユーザーのuidに写し、mountしたファイルの持ち主と揃える。imageの`USER`は`--userns`無しで起動されたときも非rootで動くため）、`ENTRYPOINT ["/usr/local/bin/dagq-broker"]`
- 言語のtoolchainは入れない

## workerの道具（MCP）

- modeが`preferred`で、brokerが健康で版が一致し、providerがClaude Codeのworker（とresume）にだけ渡す。Codexのworkerには渡さず、`broker_unavailable`（`reason: provider`）を残す（CodexのMCPの渡し方は後のtask）
- `<run dir>/broker/mcp.json`:

  ```json
  {"mcpServers":{"dagq-broker":{"command":"<dagqの隣>/dagq-broker-client","args":["mcp"],
    "env":{"DAGQ_BROKER_URL":"http://127.0.0.1:<port>","DAGQ_BROKER_TOKEN_FILE":"<queue dir>/broker/tokens/<run id>"}}}}
  ```

  を`HostActorExecutor`がClaude Codeの`--mcp-config <file>`で渡す。envにtokenの値は入れない
- 道具（`mcp__dagq-broker__<name>`）: `read_file`（`path`・`offset`・`limit`）、`list_dir`（`path`）、`write_file`（`path`・`content`）、`edit_file`（`path`・`old_string`・`new_string`・`replace_all`。組み込みのEditと同じ）、`exec`（`argv`・`env`・`stdin`・`timeout_secs`）、`git_status`、`git_diff`（`staged`・`paths`）、`git_log`（`limit`）、`git_add`（`paths`）、`git_commit`（`message`）。errorはMCPの`isError`とerror codeを返す
- workerのsettingsの`permissions.allow`にserver単位の`mcp__dagq-broker`を足す。`preferred`では組み込みの道具を拒まない
- workerのpromptに、brokerの道具があるときだけ「ファイルの読み書き・置換、許されたコマンド、run branchのgitはbrokerの道具を優先する」段落を足す
- 人の診断のCLI: `dagq-broker-client health`、`fs read|list|write|edit`、`exec`、`git status|diff|log|add|commit`、`token inspect`（token fileのclaimsだけを出し、tokenと署名は出さない）。`--url`と`--token-file`（無ければenv）

## audit

- 置き場所: `<queue dir>/broker/audit/<YYYY-MM-DD>.jsonl`（UTCの日付）。brokerが1要求1行で追記する。30日より古いファイルはbrokerの起動時に消す
- auditは記録で、改ざんへの耐性は持たない（上の「既知の制限」と、hostのworkerが同じファイルを書けること）
- 欄: `ts`・`request_id`・`jti`・`run_id`・`task_id`・`actor_id`・`op`（`fs.read`など）・`capability`・`path`（workspaceからの相対）・`program`（execの`argv[0]`のbasename）・`argc`・`argv_sha256`・`result`（`ok`かerror code）・`exit_code`・`duration_ms`・`bytes_in`・`bytes_out`
- 署名の合わないtokenの要求は`jti`・`run_id`などをnullにし、`result: unauthorized`だけを残す（claimsを信用しない）
- `argv_sha256`は照合用で、推測しやすい引数はhashから総当たりで戻せる
- 残さないもの: token・署名・鍵、ファイルの中身、diff、execのstdout・stderr・stdin、envの名前と値、`argv`の引数、commit message
- 読む: `dagq broker audit [--run ID] [--task ID] [--since T] [--until T] [--limit N]`（状態を変えない。JSONの配列）。queue DBには取り込まない

## mode と設定

`dagq.toml`（repositoryの方針。main checkoutの作業ファイル）:

```toml
[broker]
mode = "preferred"            # disabled（既定）| preferred | required（Phase 2まで起動を拒む）
exec_allow = ["sh", "ls", "cat", "grep"]   # 既定は空
exec_env = []                 # execに通すenvの名前
exec_timeout_secs = 60
exec_max_timeout_secs = 300
output_limit_bytes = 1048576
```

`host.toml`（`<queue dir>/host.toml`か`$XDG_CONFIG_HOME/dagq/host.toml`。hostの事情）:

```toml
[broker]
disable = false               # true ならこの host では mode を disabled に落とす（上げることはできない）
podman = "/opt/homebrew/bin/podman"   # 省略時は PATH
machine_cpus = 1
machine_memory_mib = 1024
machine_disk_gib = 10
container_memory = "512m"
container_cpus = "1"
container_pids = 256
port = 0                      # 0 は空いている port
```

- `[broker]`が無ければ`disabled`。`required`は`ensure_implemented`と同じくerrorにし、supervisorを起動しない（黙って`preferred`に落とさない）
- `dagq.toml`の`[broker]`は[Run environment](supervisor-lifecycle/run-environment.md)の読み手（`parse_run_env`）が表として受け付ける必要がある。`[broker]`を知らない固定バイナリは未知の表で止まるので、この repositoryの`dagq.toml`には置かない（本番queueはdisabled）。検証は使い捨てのrepositoryで行う

## status と doctor

- `actors`（task 738）の`backend: host`・`enforcement: advisory`・`sandboxed: false`は変えない（brokerを使うworkerも隔離されていない。ADR-t827-4決定5）
- 別の欄`broker`: `mode`・`state`・`port`・`build`・`image`・`machine`（`name`・`state`）・`client`（`path`・`build`・`matches`）・`active_tokens`（数）
- `doctor`は`broker`の検査（podmanの有無、machine、image、health、clientの版）を足す

## capabilityの関係

- brokerのcapability（`fs.read`・`fs.write`・`process.exec`・`git.read`・`git.write`）はprotocolの`BrokerCapability`で、queueの操作の`Capability`（[Authorization](authorization.md#capability)）とは別の名前空間
- `reserved.filesystem_read`・`reserved.filesystem_write`・`reserved.network`・`reserved.secret_read`は誰にも与えないまま（ADR-t827-4決定5）
- tokenを発行できるのは信頼する制御側のsupervisorだけ。AI actorにtokenを作るコマンドは無い

## 段階

- Phase 1（goal 58）: この文書。workerはhostのまま、`preferred`で契約を証明する
- Phase 2: `required`の強制（組み込みの道具を拒み、brokerが使えなければclaimしない）
- Phase 3以降: workerのcontainer化（`PodmanActorExecutor`）、runごとのmountでの閉じ込め、containerのworkerのqueueの操作（goal 38か後のgoal）

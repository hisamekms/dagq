---
id: design-broker
type: design
title: Resource broker
status: draft
created: 2026-09-28
updated: 2026-10-05 # task 839
last_verified: 2026-10-05 # task 839
scope: runtime
tags:
  - security
  - broker
related:
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
| `dagq-broker-protocol`（`crates/dagq-broker-protocol`） | lib | 要求と応答のDTO、`BrokerCapability`、`ErrorCode`、`TokenClaims`、`sign` / `verify`（HMAC-SHA256）、pathの定数、protocolの版（`PROTOCOL_VERSION = 1`） | `serde`・`serde_json`・`sha2`（HMACとbase64urlは自前の小さな関数。`hmac` crateは足さない） | publishする（最初） |
| `dagq-broker`（`crates/dagq-broker`） | lib + bin `dagq-broker`（`main`は薄い） | HTTPのserver（`std::net`の上の自前の小さな同期のserver。task 830）、fs・process・gitのbackend、tokenの検証、audit | protocol・`uuid`・`sha2`（HTTPのcrateは足さない） | publishする（最後。releaseのimageの材料） |
| `dagq-broker-client`（`crates/dagq-broker-client`） | lib + bin `dagq-broker-client` | HTTPのclient（`std::net`の上の自前の小さなclient、TLSなし。task 834）、subcommand `mcp`（stdioのMCP server。JSON-RPCは`serde_json`で自前）、診断のCLI | protocol・`serde`（HTTPのcrateは足さない。testだけがserverの`dagq-broker`にdev-dependencyで依存する） | publishする（dagqの後） |

- `dagq`は`dagq-broker-protocol`だけに依存し（`version = "=<同じ版>"`と`path`）、HTTPの依存を持たない。brokerのhealthは`dagq-broker-client health --json`を子プロセスで呼んで見る（ADR-t827-1決定2）
- 版は全crateで1つ。rootの`[package] version`は継承にせず文字どおりに書き（`scripts/check-plugin-version.sh`が`[package]`のversionを読むため）、`crates/`の各crateも同じ値を書く。一致は`check-plugin-version.sh`に検査を足して守る
- tokioなどのasync runtimeは入れない（最小のmachineでのbuildを軽く保つ）
- 今（task 828）: protocolの型（下の「protocolの型」）と、serverとclientの骨組み（`dagq-broker --version`・`dagq-broker health`（healthの応答のJSONを出す）、`dagq-broker-client --version`）。未知の引数はexit 2。HTTP・token・backend・MCPは後のtaskが足す。serverとclientの`--version`はtask 910でdagqのbuild識別子にそろえた（下の「配布と版」の「build識別子」）。
- task 829: protocolに`token`（`sign` / `verify` / `check_active`、`SigningKey`・`BrokerSessionToken`・`TokenError`、`TokenClaims::require` / `confine`）、dagqに`src/infrastructure/broker_token.rs`（`ensure_key`・`issue_run_token`・`broker_grants`）。dagqはprotocolに依存する（`=<同じ版>`と`path`）。claimでの発行・token fileと有効な印の書き込み・失効はhost workerの統合のtaskが足す
- task 836: dagqに`src/application/broker.rs`（machine・image・container・healthの判定と引数。podmanは`Podman` portの後ろ）と`src/infrastructure/broker_podman.rs`（podmanの実行・`flock`・healthのHTTP・imageのbuild contextの用意・`state.json`）、`dagq broker status|start|stop`、`doctor`の`broker`、`containers/broker/Containerfile`（下の「containerとPodman machine」）。supervisorの起動とclaimの前の`ensure`・tickのhealth・attention・`down`での停止・`up`のpreflightはtask 923（下の「supervisorの統合」）
- task 830: serverに`dagq-broker serve`（下の「serve」）。HTTPのserver・loopbackだけのbind・health・tokenの認証・default denyのrouting・構造化のerror・audit。fs・process・gitのbackendは`Backend` traitの後ろの`Unimplemented`（全てのopを`backend_error`の「`<op> is not implemented yet`」で返す）で、各backendのtaskが置き換える
- task 831: serverにfsのbackend（`crates/dagq-broker/src/backends/fs.rs`の`FsBackend`。下の「mountと閉じ込め」の閉じ込め、`fs.read`・`fs.list`・`fs.write`・`fs.edit`、`--fs-limit-bytes`の上限、tmpとrenameのatomicな書き込み）。serverはbackendに渡す前にauditの日のファイルを追記で開けることを確かめ、開けなければopを走らせずに`backend_error`（`the audit could not be written`）で答える。processとgitはまだ`Unimplemented`
- task 832: serverにprocessのbackend（`crates/dagq-broker/src/backends/process.rs`の`ProcessBackend`。下の「process.exec」のargvの実行、workspaceのcwd、envの消毒、timeoutと出力とstdinの上限、process groupの停止）
- task 833: serverにgitのbackend（`crates/dagq-broker/src/backends/git.rs`の`GitBackend`。下の「git」）。`git.status`・`git.diff`・`git.log`・`git.show`・`git.add`・`git.commit`・`git.restore`をtokenのworktreeだけで行う（show・restoreはtask 833の受け入れ条件が求め、2026-09-28に人がreviewの差し戻しで決めた。ADR-t827-2決定7の「status・diff・add・commit・logの類」に入るrun branchの読み書きとして足す）
- task 923: supervisor・`up`・`down`への統合と`[broker]`の設定の読み込み（`src/domain/broker.rs`の`BrokerMode`・`BrokerConfig`・`HostBroker`、`dagq.toml`の`[broker]`は`src/infrastructure/run_env.rs`、`host.toml`の`[broker]`は`src/infrastructure/broker_config.rs`、queueのbrokerを動かす`QueueBroker`は`src/infrastructure/broker_queue.rs`、supervisorの側は`src/application/supervise/broker.rs`）。下の「supervisorの統合」と「mode と設定」
- task 834: clientに`BrokerClient`（`crates/dagq-broker-client/src/client.rs`。fs・process・gitのopごとのtypedな呼び出しで、brokerの拒否は`ClientError::Broker { status, error }`にbrokerの`BrokerError`をそのまま入れて返す）と診断のCLI（`src/cli.rs`。下の「workerの道具（MCP）」の人の診断のCLI）。HTTPは`src/http.rs`の1接続1要求（`Connection: close`、`Content-Length`で読む、応答は64 MiBまで、接続と送信は10秒・応答の待ちは既定330秒）。clientはdagqにもserverにも依存しない（`crates/dagq-broker-client/tests/client.rs`が`cargo tree`で確かめる）。protocolに`BrokerSessionToken::unverified_claims`（鍵なしでclaimsを読む。`token inspect`のためだけで、判定には使わない）を足した。`mcp`はtask 835
- task 843: 配布と版（下の「配布と版」）。`install`とauto-updateがdagqとclientを同じbuildから確認して一緒に置き・戻し、`HostActorExecutor::broker_client`（`application::broker::resolve_client`）が版の一致するclientだけを返し、imageのbuildにdagqのbuild識別子を渡してhealthの`build`を比べ、`status`・`doctor`・`broker status`にclientとimageの版を出す
- task 835: clientにsubcommand `mcp`（`crates/dagq-broker-client/src/mcp.rs`。stdioのMCP server。下の「workerの道具（MCP）」の道具・入力のschema・errorと切り詰め）。testは`crates/dagq-broker-client/tests/mcp.rs`が、testの中で起こしたbrokerに対してバイナリの`mcp`をstdioで動かし、initialize・tools/list・tools/callと各道具の写し・拒否のtool error・切り詰めを確かめる（fixtureは`tests/common/mod.rs`でclientのtestと共有）
- task 915: `git.add`がfsのtmp（`.dagq-broker-<uuid>.tmp`）をpathspecのexcludeで除き、tmpを名指す要求を`invalid_request`にする（下の「git」の`add`）
- task 837: host workerへのtokenとMCPの受け渡し（下の「token」の発行・更新・失効、「workerの道具（MCP）」、「status と doctor」）。`src/application/broker_run.rs`（`RunTokens` port・`mcp.json`の形・`worker_mcp_config`）、`src/infrastructure/broker_token.rs`の`QueueRunTokens`（token file・有効な印・`mcp.json`の書き込みと失効）、supervisorの`broker_grant`と`broker_sweep`（`src/application/supervise/broker.rs`）、`AgentProvider::broker_tools`（Claude Codeの`--mcp-config`と`--allowedTools`）、promptの`BROKER_TOOLS`。testは`tests/it/runtime_broker.rs`（testの中で起こしたbrokerがtokenを受け、失効の後に拒む。`integrated`・`failed`・`interrupted`・`succeeded`のどれで終わったrunもtokenを失う）と、podmanを要るe2eの`tests/e2e/broker.rs`（`#[ignore]`。今は[ADR-t1582-1](../adr/2026-10-04-t1582-1-temporarily-leave-broker-and-cmux-only-e2e-cases-out.md)で本文を残したまま`#[cfg(any())]`で期間限定で登録から外れ、`--ignored`でも関門でも流れない。task 1451がcfgを外して復帰させ、着地の前に実podmanで流す）
- task 925: dagqに`dagq broker logs`と`dagq broker audit`（`src/application/broker_admin.rs`。下の「containerとPodman machine」の管理のコマンドと「audit」）。どちらも`queue.read`で状態を変えない。`FailureCode`に読むだけのコマンドの`machine_missing`・`machine_stopped`・`container_missing`を足した
- task 1089: 古いimageの掃除（下の「配布と版」。`application::broker::prune_images`を`start`の終わりに呼び、結果は`StartReport::images`と`broker_started`の`images`）
- task 1131: dangling（`<none>`）のimageの掃除（下の「配布と版」。`prune_images`の後に`application::broker::prune_dangling`が`podman --connection dagq image prune --force`を1回呼び、結果は`ImagePrune`の`dangling_removed`・`dangling_error`）
- task 1125: `disabled`に戻したqueueのsupervisorが、`preferred`で残った全てのtoken・token file・`mcp.json`をpodmanなしで`mode_disabled`で失効させ、resumeに道具を渡さない（`Ports::broker_leftovers`、`Supervisor::broker_sweep_disabled`、testは`tests/it/runtime_broker.rs`の`a_disabled_supervisor_revokes_the_tokens_an_earlier_mode_left`と`a_disabled_supervisor_resumes_a_run_without_the_tools_left_to_it`）
- task 1141: `disabled`のsweepが印の無いtoken fileのrunも失効させ（`RunTokens::token_files`）、非対話のrunのturnの要求の前にも残ったものを消す（`Supervisor::broker_before_turn`、testは`tests/it/runtime_broker.rs`の`a_disabled_answer_turn_gets_no_unmarked_tools_and_token_file`・`a_disabled_answer_turn_gets_no_unmarked_tools`・`a_disabled_supervisor_removes_an_unmarked_token_file_of_no_run`）
- task 1126: `ensure_machine`が、startの失敗か接続が答えない（`podman --connection dagq info`）dagqのmachineを1回だけstopとstartでやり直し、`MachineOutcome`の`restarted`・`restart_reason`に残す（unit testは`src/application/broker.rs`の`a_start_that_fails_with_eof_is_stopped_and_started_once_more`・`a_started_machine_that_does_not_answer_is_restarted_once`・`a_restart_that_does_not_help_is_machine_failed_after_one_try`）
- task 839: 組み込みの道具の数（下の「組み込みの道具の数」）。`src/domain/broker_usage.rs`（`DIRECT_TOOLS`・`DIRECT_TOOLS_LOG`・`ToolUsage`）、workerのsettingsの`PreToolUse`のhook（`infrastructure::adapters::with_direct_tool_hooks`）、`RunTokens::usage`、sweepの`broker_tool_use`と`show`の`runs[0].broker_tool_use`

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
| `git::*` | `status`の応答は`{branch, entries:[{path, status}]}`（`branch`はdetachedで`null`、`status`はporcelainの2文字）。`diff`の要求は`staged`（常に出す）と`paths`（空なら省く）。`log`の`commits`は`{commit, author, time, subject}`。`show`の要求は`commit`（無ければ省く）と`paths`（空なら省く）、応答は`{commit, show, truncated}`。`restore`の要求は`staged`（常に出す）と`paths`。`add`・`restore`の応答と`status`の要求は`{}` |

### workspaceと検証

```toml
[workspace]
members = [".", "crates/dagq-broker-protocol", "crates/dagq-broker", "crates/dagq-broker-client"]
default-members = [".", "crates/dagq-broker-protocol", "crates/dagq-broker", "crates/dagq-broker-client"]
resolver = "3"
```

- `default-members`でrootの`cargo test --locked`・`cargo clippy --locked --all-targets -- -D warnings`・`cargo nextest run`が全crateを覆う。workerの手元の`cargo test --locked --test it <module>::`と`--lib <module>`は`-p`なしで通る（task 828で確かめた。`--test it`はdagqのtestだけを選び、`--lib`は全crateのunit testからfilterに合うものを選ぶ）。brokerのcrateのtestは`-p <crate>`で絞る
- coverageの関門は`cargo llvm-cov nextest --locked --workspace --fail-under-lines 80`（ADR-t828-1がADR-t827-1決定3をamends）。cargo-llvm-cov（0.9.1）はrootがpackageのworkspaceで`-p`も`--workspace`も無いと`default-members`を見ずroot package（dagq）だけをreportに入れるので、`--workspace`で全crateを80%に数える。登録済みのtaskの旧コマンド（`--workspace`なし）はdagqのcoverageと全crateのtestの成否を見て、brokerのcrateの行は数えない
- 新しいcrateのtestは各crateの`src/`のunit testと`crates/<crate>/tests/`に置く。serverのtestは`CARGO_BIN_EXE_dagq-broker`をhostのプロセスとして`serve --listen 127.0.0.1:0`と一時のdirで起こし（`crates/dagq-broker/tests/serve.rs`）、podmanなしでcoverageに数える。podmanを要るtest（containerの起動・mount・LANで待ち受けないことの確認）は`#[ignore]`で、e2eと同じく関門とCIに数えない
- `scripts/check-test-file-lines.sh`は`tests/`と`crates/*/tests/`の下の`.rs`を見る（task 828）
- CI（`ci.yml`）のcoverageの関門は上の`--workspace`の形で、他のコマンドはそのまま。CIはmacOSだけなので、serverのLinux（musl）の経路とContainerfileはCIではbuildもtestもされない。serverのコードはOSに依らない部分（tokenの検証・閉じ込め・exec・audit）をmacOSのtestで覆い、Linuxだけの部分（`openat2`など）は`cfg`で分けてpodmanを要る`#[ignore]`のtestとスモークで確かめる
- dagqのpodmanを呼ぶ部分（`broker::ensure`）はpodmanのコマンドをportの後ろに置き、判定（machine・image・containerの状態からの次の一手、lock、版の比較）をstubのportでunit testする。実物のpodmanを呼ぶadapterだけが`#[ignore]`のtestになる
- releaseの`cargo build --release --locked --target`は全default membersを作るので、`-p dagq -p dagq-broker-client`に絞る。`cargo publish --locked`は`-p`で4つのcrateを順に打ち、crates.ioに同じ版があるcrateは飛ばす（`release.yml`）。Trusted Publisherは既にあるcrateにしか登録できないので、brokerの3つのcrateの最初のpublishは人がAPI tokenで手で行い、それぞれにTrusted Publisherを登録する（`.claude/skills/release`）
- `install`とauto-updateのbuildは`cargo build --release --locked -p dagq -p dagq-broker-client`（`src/infrastructure/binaries.rs`の`BUILD_ARGS`。serverはimageの中でbuildするので含めない。task 843）

### runtimeのパス

`src/application/update.rs`の`RUNTIME_PATHS`に`crates/`を足す（ADR-t827-1決定4）。`crates/`の変更はclientとimageの材料を変えるので、auto-updateの対象になる。imageのbuildのstageのRustの版は`rust-toolchain.toml`から取るので、`rust-toolchain.toml`も足す。

## 配布と版

| もの | 置き場所 | 作る時 | 版の確認 |
| --- | --- | --- | --- |
| `dagq` | `~/.local/bin/dagq`（今までどおり） | `install`・auto-update・releaseのupdate | build識別子 |
| `dagq-broker-client` | `dagq`の隣（`~/.local/bin/dagq-broker-client`） | dagqと同じcheckout・同じ`cargo build --release --locked -p dagq -p dagq-broker-client` | `dagq-broker-client --version`がdagqのbuild識別子と一致 |
| brokerのimage | dagq専用のmachineの中、`localhost/dagq-broker:<build tag>` | brokerの起動のとき、tagが無ければ | tag（build識別子）とhealthの`build` |

- build識別子（task 910）: `dagq`・`dagq-broker`・`dagq-broker-client`は同じ規則でbuild識別子を埋め、`--version`とserverのhealth・`serve`のlisteningの`build`・`X-Dagq-Broker-Build`はそれを出す。規則は`crates/dagq-broker-protocol/src/build_id.rs`の1か所（`build_identifier`・`is_prerelease`・Gitのcommitとdirtyの判定・rerun-if-changedの対象（repository rootの`src`・`crates`・`migrations`・`build.rs`・`Cargo.toml`・`Cargo.lock`とGitの`HEAD`・index・`packed-refs`・branchのref。3つとも同じ集合を見るので、どれかの編集で3つそろって`.dirty`になる）。`dagq::build_id`はその再export）で、3つのcrateの`build.rs`はprotocolをbuild-dependencyにして`build_id::emit`を呼び、`DAGQ_BUILD_ID`を埋める（dagqはroot`.`、`crates/`のcrateは`../..`をrepository rootとして見る。crateはrootの`crates/<crate名>`に居るときだけそのrootを見て、別の場所に展開されたもの（他のrepositoryにvendorされたなど）は自分のdirを見るので`unknown`になる）。releaseの版（prereleaseでない`X.Y.Z`）では3つとも`X.Y.Z`。rootがGitのworktreeのrootでないとき（crates.ioのsource、`cargo package`の検証のcopy、brokerのimageのbuild）は`X.Y.Z-dev+unknown`。imageの材料には`crates/dagq-broker/build.rs`も入れる（下の「imageの材料」）
- `install`とauto-updateは、置くdagqの隣（buildなら`target/release/`、`--rollback`なら`.previous`）の`dagq-broker-client`を、dagqの確認（`--version`と使い捨てのqueueでの起動）に続けて`--version`がdagqのbuild識別子と同じことで確かめ、dagqがclientと一緒に作ったbuild（checkout・自動更新・release）ではどちらかが通らなければどちらも置き換えない（人が渡したbinaryと`--rollback`では版の違うclientを置かず、置き場のclientを退ける）。置くのは先にclient、次にdagqで、同じrenameでそれぞれ`.previous`を残す。dagqを置けなければclientを戻す。全員の引き継ぎの失敗（`install`）と見張りの失敗（auto-update）ではdagqと一緒にclientを戻す（前のclientが無ければ、または`.previous`が前のbuildでなければ新しいclientを消す）。`install --rollback`も両方を入れ替える。元の隣にclientが無いdagq（clientより前のbuild、clientの無いリリース）を置くときは、置き場のclientを`.previous`へ退け、別のbuildのclientをdagqの隣に残さない。非互換のmigrationのbuildは`<queue dir>/update/staged/`に`dagq`と隣の`dagq-broker-client`を置き、人の`install --from <staged>/dagq`が両方を置く（`src/application/install.rs`の`Client`・`src/application/update.rs`の`stage`と`restore_client`・`src/infrastructure/binaries.rs`の`set_aside`。詳細は[`install`](supervisor-lifecycle/install.md#install)と[Auto-update](supervisor-lifecycle/auto-update.md#auto-update)）
- `HostActorExecutor`が使うclientは`std::env::current_exe()`の隣の`dagq-broker-client`だけで、その`--version`がdagqのbuild識別子と一致するときだけ使う（supervisorは起動時に1回解決して`BrokerPort::client`に持つ。`HostActorExecutor::broker_client`が`application::broker::resolve_client`を呼ぶ。無ければ`client_missing`、版が違うか読めなければ`version_mismatch`の`BrokerFailure`で、clientを渡さない。`--version`は`infrastructure::broker_podman::client_version`が読み、結果はバイナリのmtimeごとにprocessの中でcacheする。workerにMCPを渡す配線は下の「workerの道具（MCP）」）
- imageの材料（ADR-t827-1決定6。task 924）: `build.rs`が材料を集めてustarのtarにし（`src/broker_material.rs`の`collect`と`tar`。`build.rs`は`#[path]`で同じファイルを読む）、`$OUT_DIR/broker-image.tar`に書く。バイナリはそれを`include_bytes!`で埋める（`src/infrastructure/broker_image.rs`の`MATERIAL`）。tarは中身が同じなら同じbytesになる（名前の順、mode 0644、owner 0、時刻0）。buildのstageのRustの版も埋める（`DAGQ_BROKER_RUST_VERSION`: `rust-toolchain.toml`のchannel、無ければpackageの`rust-version`）。どちらの材料かは`DAGQ_BROKER_MATERIAL`（`broker_image::KIND`）
  - dev build（checkoutからのbuild。`build.rs`が`crates/dagq-broker-protocol`と`crates/dagq-broker`の`Cargo.toml`を見つけたとき。`Kind::Source`）: `Containerfile`、`crates/dagq-broker-protocol`と`crates/dagq-broker`の`Cargo.toml`・`build.rs`・`src/`（testsは入れない）、workspaceのmanifestをこの2つに絞ったもの（`narrowed_manifest`）、`Cargo.lock`。buildのstageは`--locked`を付けずに作る（`Cargo.lock`は全workspaceのもので、使わないpackageの行が消えるだけで、使うpackageの版はlockのまま）。`crates/`の2つのcrateの`src/`の隠しファイル（`.`で始まるもの）とsymlinkは入れない。`build.rs`はこれらと`containers/broker/Containerfile`・`rust-toolchain.toml`をrerun-if-changedで見る
  - release（crates.ioのdagqのpackage。`crates/`を含まない。`Kind::Release`）: `Containerfile`だけを埋める（dagqのpackageの`include`に`/containers/broker/Containerfile`を入れる。`rust-toolchain.toml`はinstallする人のtoolchainを決めないようにpackageに入れず、Rustの版は`rust-version`）。`Containerfile`の`# dagq:build-from-source begin`から`end`までの行（`WORKDIR`・`COPY . .`・`cargo build`）を`build.rs`が`RUN cargo install --locked dagq-broker@<version> --root /usr/local/dagq-broker`と`/dagq-broker`へのinstallに置き換える（`release_containerfile`）。同じ版のserverをcrates.ioから作る
  - brokerを起動するdagqは埋めた材料を一時のdir（`<queue dir>/broker/build-context`）に展開して（`EmbeddedSource::stage`。先に空にする。絶対pathと`..`を含むentryは拒む）`podman build`する。checkoutの作業ファイルは読まない（`dagq broker start`の`--source`と、`CARGO_MANIFEST_DIR`・作業dirのmain checkoutからのbuildはやめた）。材料が展開できなければ`image_source_missing`。どちらもmachineからcrates.ioとdocker.ioへのnetworkを要る
  - test: `src/broker_material.rs`（tarの往復とsystemの`tar -tf`、checkoutの材料の中身、`crates/`の無いpackageの形ではContainerfileだけで`cargo install --locked dagq-broker@<version>`になること）、`src/infrastructure/broker_image.rs`（checkoutのファイルを変えた・消した後も、埋めた材料だけでbuildの時点の材料と同じ中身を展開できること）
- build tag（`application::broker::image_tag(build, material)`）: cleanのdev buildとreleaseはbuild識別子の`+`とmetadataの中の`.`を`-`に置き換えたもの（`0.5.0-dev-<commit>`、releaseは`0.5.0`）。`.dirty`のbuildは埋めた材料のtarとRustの版のSHA-256（`broker_image::material_hash`）の先頭12文字（`MATERIAL_HASH_DIGITS`）を足した`0.5.0-dev-<commit>-dirty-<hash>`にし、中身の違うdirtyのbuildが同じtagを共有しない（planner の決定）。このbuildのimageは`broker_image::image()`で、`QueueBroker::image`・`status`・`doctor`の`image`はそれを出す
- imageのbuildには`--build-arg DAGQ_BROKER_IMAGE_BUILD=<dagqのbuild識別子>`を渡す。buildのstageはGitを持たないので、`build_id::emit`はこのenv（`build_id::GIVEN_ENV`）があって同じ版（`X.Y.Z`そのものか`X.Y.Z+<metadata>`）を名指すときだけそれを埋める（`given_identifier`）。これでimageの中のserverの`--version`とhealthの`build`がimageを作ったdagqのbuild識別子になる
- `broker start`（`application::broker::start`）はhealthの`build`をdagqのbuild識別子と比べ、黙って続けない。違えば（このbuildのtagのimageが、識別子を渡す前のdagqで作られていたなど）containerとimageを消して1回だけbuildし直し、それでも違えば`version_mismatch`の`BrokerFailure`にする（unit testは`a_broker_of_another_build_is_built_again_unless_runs_hold_it`。podmanのportのfakeで）。`active/`に印がある（runがtokenを持つ）間は、古いimageのcontainerを残したとき（`kept_stale`）も同じtagのcontainerでも消さず、違ったまま（`build_matches: false`）にする（そのrunのworkerは古いclientで話す）。結果の`build`・`build_matches`・`rebuilt`に出す
- 版の食い違い（clientが無い・clientの版が違う・healthの`build`がdagqと違う・`X-Dagq-Broker-Protocol`が違う）では、dagqはtokenを発行せずworkerに道具を渡さない（fail closed、ADR-t827-1決定7）。`preferred`ではrunのevent `broker_unavailable`（`reason: version_mismatch`）を残してworkerは道具なしで動く
- auto-updateの後（healthの`build`がdagqと違う）: 古いcontainerは`active/`に印が残る間、そのrunのために動かし続ける（そのrunのworkerのMCPのprocessは古いclientで、同じ版どうしで話す）。その間の新しいclaimには`version_mismatch`でtokenを出さない。印が無くなったら`broker::ensure`が新しいimageで起動し直す（ADR-t827-3決定2）。古いcontainerが要る間も、そのrunのtokenの期限の前の発行し直しは同じ鍵で続ける
- 古いimageの掃除（task 1089）: `application::broker::start`が、healthが答えた後に（`build_matches`の判定と作り直しの後、`version_mismatch`で失敗したときはしない）`prune_images`を呼び、dagqのmachineの`localhost/dagq-broker`のimageのうち、今のbuildのtag（`ContainerSpec::image`）と、それ以外で作成時刻（`podman images`の`Created`）が最も新しい1つ（1つ前。tagはbuild識別子と材料のhashで順序を持たないので時刻で決める。同じ時刻ならnameの大きい方）を残し、残りをnameで`podman --connection dagq image rm <name>`（`--force`なし）する。machineのcontainer（`podman --connection dagq ps --all`。`active/`に印があるrunのために残した`kept_stale`のcontainerや、同じmachineの別のqueueのcontainer）が使うimage（nameかid）は1つ前より古くても消さない。`localhost/dagq-broker:`で始まらないnameは読まないので、他のrepositoryのimageは消さず、別の名前も持つimageはそのnameが外れるだけ。人の既定のmachineには触らない（全てのコマンドが`--connection dagq`）。掃除はbrokerの用意を失敗させず、結果は`StartReport::images`（`ImagePrune`: `removed`・`in_use`（古いが使われているので残したもの）・`failed`（`{image, error}`）・`error`（一覧が読めず何も消さなかった理由）と、下のdanglingの掃除の`dangling_removed`・`dangling_error`）に出し、`dagq broker start`の出力と`broker_started`の`images`に残る。modeが`disabled`のqueueではsupervisorが`start`を呼ばないので何もしない。unit testは`start_removes_all_but_the_current_and_the_previous_image`（podmanのportのfakeで）
- danglingのimageの掃除（task 1131）: `containers/broker/Containerfile`は多段（`rust:*-alpine`のbuild stage → `alpine`）で、`podman build`はtagの無いbuild stageのimageを`<none>`（dangling）として残す。build識別子がbuild-argに入るので着地ごとにbuild stageが作り直され、Rustのtoolchainとtargetを含む大きなimageがmachineのdiskにたまる。そこで`start`は`prune_images`の直後（同じ条件: healthが答えbuildの照合の後、`version_mismatch`で失敗したときはしない）に`prune_dangling`で`podman --connection dagq image prune --force`を1回呼ぶ。`-a`/`--all`も`--filter`も付けないので消えるのはtagの無いimageだけで、container（buildahのbuild中のものを含む）が使うimageはpodmanが消さないので、同じmachineの別のqueueのbuildを壊さない。build cache（`podman system prune`や`--all`）には次のbuildを遅くするので触らない。人の既定のmachineには触らない（`--connection dagq`）。結果は`ImagePrune`の`dangling_removed`（pruneが出した消したimageのid）と`dangling_error`（失敗の理由）に出し、失敗は`start`を失敗させない。modeが`disabled`のqueueでは`start`が呼ばれないので何もしない。unit testは`start_prunes_the_dangling_images_once_after_the_old_ones`
- release: crates.ioには`dagq-broker-protocol` → `dagq` → `dagq-broker-client` → `dagq-broker`の順でpublishする（serverはreleaseのimageのbuildの材料。ADR-t827-1決定6・8）。GitHub Releaseにはdagqと同じ形の`dagq-broker-client-v<version>-<target>.tar.gz`を足し、`SHA256SUMS`に含める。imageはregistryに出さない。releaseのupdate（[Release update](supervisor-lifecycle/release-update.md)）は`cargo install --locked dagq@<version>`と`dagq-broker-client@<version>`を同じ`--root`に入れて両方を差し替える（`CargoInstaller`。clientのinstallが失敗すればclientを置かず、`install`が置き場のclientを退けるので、brokerは両方がそろうまで使われない）

## transport

- `127.0.0.1:<port>`のHTTP/1.1とJSONだけ。hostへのpublishは`-p 127.0.0.1:<port>:<container port>`だけにする。containerの中でbrokerがbindするaddressは、Podmanの転送がloopbackに届けるなら`127.0.0.1`、届けないならcontainerの私的なinterface（LANのinterfaceではない）にし、実装のtaskが測って決める。`dagq-broker`は`--container`のとき以外、loopbackでないaddressへのbindを拒む。`#[ignore]`のtestが、hostのLANのaddressへの接続が拒まれることを確かめる
- portはqueueごと。`host.toml`の`[broker] port`（既定0 = 空いているportを選ぶ）。選んだportは`<queue dir>/broker/state.json`に残し、containerを起動し直すときも同じportを使う
- header: `Authorization: Bearer <token>`、`X-Dagq-Broker-Protocol: 1`。応答にも`X-Dagq-Broker-Protocol`と`X-Dagq-Broker-Build`を付ける
- 要求の本体の上限は8 MiB（超えれば`invalid_request`）

### serve

`dagq-broker serve`の引数（設定はcommand lineだけで、supervisorがcontainerの起動の引数に書く）:

| flag | 既定 | 中身 |
| --- | --- | --- |
| `--listen <addr>:<port>` | `127.0.0.1:8750` | 待ち受け。`--container`が無ければloopbackでないaddress（`0.0.0.0`・`::`・LAN）をexit 2で拒む |
| `--container` | なし | containerの中。containerの自分のinterfaceへのbindを許す（hostへのpublishは`127.0.0.1`だけ） |
| `--key <file>` | 必須 | 鍵（32 byte）。起動時に1回読む |
| `--active <dir>` | 必須 | 有効な印 |
| `--audit <dir>` | 必須 | auditの置き場所。無ければ作り、起動時に保持の日数を過ぎた日のファイルを消す |
| `--root <dir>`（複数可） | 必須（1つ以上、絶対パス） | mountしたruns dir。tokenの`workspace`がどのrootの下にも無ければ`workspace_violation` |
| `--exec-timeout-secs` / `--exec-max-timeout-secs` / `--output-limit-bytes` | 60 / 300 / 1048576 | 上限の既定値（backendに渡す） |
| `--fs-limit-bytes` | 4194304 | fsの中身の上限（`fs.read`が返す中身、`fs.write`・`fs.edit`が書く中身、`fs.edit`が読むファイル、`fs.list`の応答）。超えれば`output_limit` |
| `--exec-allow <name>` / `--exec-env <name>`（複数可） | 空 | execのallowlist（backendに渡す）。`--exec-allow`にinterpreterの一覧の名前（basenameで比べる）があれば、起動時にstderrへ名前つきのwarningを1行ずつ出して起動は続ける。`--exec-env`に`LD_`・`DYLD_`で始まる名前（大文字小文字を区別した前方一致）があれば名前つきのerrorで起動しない（exit 2）。どちらもenvの値やtokenを出さない（「process.exec」の注意） |

- bindしたら`{"listening":"<addr>","build":"<build>"}`の1行をstdoutに出す（port 0で選ばれたportをtestと起動側が読む）
- HTTPは`std::net`の上の自前の小さなserver（1接続1要求・1接続1 thread で同時に32接続まで（超えた接続は答えずに閉じる）、本体は`Content-Length`だけで`Transfer-Encoding`は拒む、本体は届いた分だけ伸ばして読む、応答は`Connection: close`、要求の全体を読む期限と書きのtimeoutは30秒、要求の頭は16 KiBまで）。以前の例の`tiny_http`はofflineのbuildで使えず、要るのはloopbackで自分のclientと話すことだけなので足さない
- 判定の順（どれかで拒めば先を見ない）: (1) `GET /v1/health`だけはtokenなしで答える。(2) `Authorization: Bearer <token>`を`verify`（書式・署名・claims・期限）と`check_active`で確かめる（無い・Bearerでない・通らなければ`unauthorized`）。未知のpathもtokenより前には答えない（default deny）。(3) methodとpathを`Operation::route`で引き、無ければ`invalid_request`（`no such operation`。`POST /v1/health`も）。(4) `X-Dagq-Broker-Protocol`が`1`でない・無ければ`invalid_request`。(5) opの要るcapabilityをtokenが持たなければ`capability_denied`。(6) 本体をopの要求の型で読む（未知の欄は`invalid_request`。workerが本体に`run_id`などを書いても未知の欄で拒み、誰の要求かはtokenのclaimsだけで決める）。(7) tokenの`workspace`が`--root`の下か、要求のpath（fsの`path`、gitの`paths`）を`TokenClaims::confine`で字面で閉じ込める（外は`workspace_violation`。symlinkはbackendが解く）。(8) opのbackend（fs・process・git）に渡す
- 読めない要求（HTTPでない・頭が長すぎる・本体の上限超え・chunked）と、途中で切れた・期限を過ぎた要求（`the request is incomplete`）は`invalid_request`で答え（届けば）、auditに残す。1 byteも送らずに切れた接続は要求ではないので、答えずauditにも残さない
- auditの行は応答を送る前に書く。書けなければstderrに要求のIDと理由を出し、応答を`backend_error`（`the audit could not be written`）に替える（auditの無い答えを返さない）。backendに渡す前に、auditの日のファイルを追記で開けることを確かめ、開けなければopを走らせずに同じ`backend_error`で答える（書けないauditのままfsの書き込みなどを行わない。task 831）

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
| `POST /v1/git/show` | `git.read` | `{commit?, paths?}`（既定`HEAD`） | `{commit, show, truncated}` |
| `POST /v1/git/add` | `git.write` | `{paths}` | `{}` |
| `POST /v1/git/commit` | `git.write` | `{message}` | `{commit}` |
| `POST /v1/git/restore` | `git.write` | `{staged?, paths}` | `{}` |

`fs.edit`は組み込みのEditと同じで、`old_string`が1つだけ見つかるとき（`replace_all`なら1つ以上）に置換し、見つからない・複数あるとき（messageに一致の数、中身は入れない）・`old_string`が空・`old_string`と`new_string`が同じときは`invalid_request`。`replacements`は置換した数。

fsのopの細部（task 831）:

- `fs.read`: UTF-8のテキストの行（`\n`を含めて返す）。`offset`は飛ばす行数（既定0）、`limit`は返す行の上限（既定は上限なし）。`truncated`は返した後にまだ中身があるか。返す中身が`--fs-limit-bytes`を超えれば`output_limit`（`offset` / `limit`で絞って読み直す）。UTF-8でない・通常のファイルでない（dir・FIFOなど。FIFOは`O_NONBLOCK`で開くので止まらない）・無いものは`backend_error`
- `fs.list`: 1階層だけ。`.`と`..`を除き、名前の順。symlinkは辿らずに`kind: symlink`で出す。workspaceの根の`.git`も名前と種類だけは出す（中は読めない）
- `fs.write`: 中身が上限を超えれば何も書かずに`output_limit`。同じdirに`.dagq-broker-<uuid>.tmp`を`O_CREAT | O_EXCL | O_NOFOLLOW`で作って書き、`fsync`してから`renameat`で置き換える（途中で失敗すればtmpを消す）。ただし書いてからrenameするまでにbrokerのプロセスが止まる（kill・containerの停止）とtmpはworkspaceに残る。brokerは起動のときにworkspaceを知らないので掃除はせず、下の「git」の`add`がstageしない（hostのworkerが自分のgitで`add -A`する経路は対象の外）。既存のファイルは権限のbit（`0777`の範囲）を引き継ぎ、新しいファイルは`0666`からumaskを引いたもの。`create_dirs`なら無いdirを`mkdirat`で作る（作ったdirもsymlinkを辿らずに開く）。dir・FIFOなど通常のファイルでないものへの書き込みとworkspaceそのものは`backend_error`
- `fs.edit`: 上限を超えるファイルと、置換後に上限を超えるものは`output_limit`。書き込みは`fs.write`と同じatomicな置き換えで、権限を引き継ぐ
- mkdir・remove・statの独立したopは無い（ADR-t827-4の道具の形と上の表に従う。dirは`fs.write`の`create_dirs`で作る）


### error

本体は`{"error":{"code":"<code>","message":"<人向けの短い文>","request_id":"<uuid>"}}`。messageにtoken・ファイルの中身・envの値を入れない。

| code | HTTP status | 意味 |
| --- | --- | --- |
| `unauthorized` | 401 | tokenが無い・書式が違う・署名が合わない・期限切れ・有効な印が無い・未知のcapability・claimsの欠け |
| `capability_denied` | 403 | tokenがopの要るcapabilityを持たない。execのallowlistの外のプログラム、`git`のexec |
| `workspace_violation` | 403 | workspaceの外、`..`、symlinkの逃げ、worktreeの`.git`、run branchでないHEADでのcommit |
| `timeout` | 504 | execのtimeout（プロセスは止めた） |
| `output_limit` | 413 | execの出力かfs・gitの応答、`fs.write`・`fs.edit`が書く中身が上限を超えた（execはプロセスを止めた。fsは何も書かない） |
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
- 期限: `exp = iat + 12時間`。supervisorは各pass（`Supervisor::broker_sweep`）で、残りが4時間（`RENEW_BEFORE_SECS`）を切った生きているrunのtokenを、brokerが使えるときに発行し直す（新しいjtiの印を作ってtoken fileと`mcp.json`を置き換え、古い印を消す。`broker_token_issued`に`renews`（古いjti）、`broker_token_revoked`に`reason: renewed`）。clientは要求ごとにtoken fileを読み直す
- 失効: supervisorは各passのclaimの前（drainと引き継ぎの間も）と止まる直前（`supervise --once`の最後のpassで終わったrunのため）に有効な印を全て見て（`broker_sweep`）、runが終わっていれば（statusが`integrated`・`succeeded`・`failed`・`interrupted`。taskの`cancel`・leaseの喪失・`recover`もrunをこのどれかにする）そのrunの印を全て消し、`<queue dir>/broker/tokens/<run id>`と`<run dir>/broker/`を消して、印ごとに`broker_token_revoked`（`reason`はrunのstatus）を残す。queueが知らないrunの印はtoken fileとともに消し、eventは残さない。`preferred`と`disabled`のどちらも、runの読み取りが失敗したときは不在と区別し、warnを出してそのpassでは飛ばす（task 1142）。そのrunの印・token file・`<run dir>/broker/mcp.json`は消さず、`broker_token_revoked`も残さず、次のpassで読み直す。resumeの発行し直しも前の印を消す。supervisorが止まっている間に終わったrunの印は、次の起動の最初のpassか期限まで残る（その間そのtokenは使える）。modeが`disabled`のsupervisor（`preferred`から戻したqueue）はpodmanを探しも呼びもせず、fileだけを掃除する（task 1125）: 同じ時点に有効な印を全て見て、生きているrunのものも含めてそのrunの印・token file・`<run dir>/broker/`を消し、印ごとに`broker_token_revoked`（`reason: mode_disabled`）を残す（`Supervisor::broker_sweep_disabled`）。起動とresumeの前の`broker_grant`も、`disabled`ではそのrunに残ったものを同じく消す（`mode_disabled`）ので、executorが`mcp.json`を見つけて`--mcp-config`と`--allowedTools mcp__dagq-broker`を渡すことも、promptに`BROKER_TOOLS`の段落が載ることもない。印の無いものも残りうる（途中で失敗した失効、queueの知らないrunの`revoke`など）ので（task 1141）、`disabled`のsweepは有効な印に加えて`<queue dir>/broker/tokens/`のtoken file（印の有無に依らない。`RunTokens::token_files`）の名のrunも同じく失効させ（queueが知らないrunならtoken fileと印を消す）、非対話のrunのturnの要求（`worker_question`の答え・reviewの差し戻し・resumeなど、supervisorが`turns/`に書く全てのturn。`request_turn`）の前にも`broker_grant`と同じくそのrunに残ったものを消す（`Supervisor::broker_before_turn`）。印が無ければ`broker_token_revoked`は残さない。どちらもpodmanを探しも呼びもしない。`preferred`ではturnの前に何もしない（runは発行された道具を持ち続ける）。印の無い`disabled`のqueueでは何も書かず、workerの起動のenv・settings・引数は変わらない
- event: runのevent `broker_token_issued`（`jti`・`capabilities`・`exp`、発行し直しなら`renews`）・`broker_token_revoked`（`jti`・`reason`）・`broker_unavailable`（`reason`・`message`）・`broker_tool_use`（下の「組み込みの道具の数」）。どれもattentionではない。tokenの値は残さない（`tests/it/runtime_broker.rs`が、queueのdir・run dir・queue DB・workspaceのenvにtokenの値が無いことを確かめる）
- 未知のcapabilityを含むtokenは全体を拒む（fail closed）

### 実装（protocolとdagq）

- protocolの`sign(key, claims)`は`encode`のbyte列から上の書式を作る。`verify(key, token, now)`は、書式（`dagq1.`・2つの部分・paddingの無いbase64url。余りのbitが0でない符号も拒む）→署名（claimsを読む前に、定数時間で比べる）→claimsの読み（未知の欄・capability・roleと、英数字と`-`でない`jti`）→`v`→期限（`now >= exp`で期限切れ）の順に見て、どれかで失敗すれば`TokenError`を返す。`TokenError::code()`は`BadKey`・`Malformed`・`BadSignature`・`BadClaims`・`UnsupportedVersion`・`Expired`・`Revoked`を`unauthorized`、`CapabilityDenied`を`capability_denied`、`WorkspaceViolation`を`workspace_violation`にする
- 失効の受け側は`check_active(claims, <active dir>)`: `<active dir>/<jti>`があり中身（前後の空白を除く）がclaimsの`run_id`のときだけ通し、それ以外は`Revoked`
- 要求ごとの判定: `TokenClaims::require(capability)`（無ければ`CapabilityDenied`）と`TokenClaims::confine(path)`（相対はworkspaceから、絶対はworkspaceの下だけ。`..`はどこでも拒み、別のrunのworktreeも外になる。字面の判定だけで、symlinkはserverが解く）
- `SigningKey`（32 byte）と`BrokerSessionToken`の`Debug`は`<redacted>`で、`Display`を持たない。tokenの値は`BrokerSessionToken::expose`（token fileと`Authorization`のためだけ）からしか出ない。`TokenError`の文言は理由だけで、token・署名・鍵を含まない
- dagqの`ensure_key(queue dir)`は`<queue dir>/broker/key`を読み、無ければ`getrandom`の32 byteを同じdirの一時ファイル（mode 0600、`create_new`）に書き、`hard_link`で置く（読み手が半端な鍵を見ず、同時に作った2つのsupervisorは先に置かれた鍵にそろう）。modeが0600でなければ0600に絞り、長さが違えばpathだけを名指すerrorにする（人が消して作り直す）
- dagqの`issue_run_token(key, actor, workspace, committer, now)`はworkerのactor（runとtaskを持つ）だけに発行し、workspaceは絶対パスをcanonicalにし、`jti`はuuid v4、`branch`は`dagq/<run id>`、capabilityは`broker_grants(role)`（workerは5つ、他のroleは空）、`exp = iat + TOKEN_TTL_SECS`（12時間）。返す`IssuedToken`はclaimsとtokenを持ち、eventにはclaimsの`jti`・`capabilities`・`exp`だけを書く

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
2. 開くのは、workspaceを含むmountの根（`--root`。supervisorが渡す信頼する値で、ここだけは普通に開く）のdir fdから、workspaceまでと要求のpathの全ての要素を1つずつ、前の要素のfdからの`openat(O_NOFOLLOW)`（dirは`O_DIRECTORY`も）で辿る。確かめたものをそのまま開くので、確認と開くことの間の競合（親dirをsymlinkに差し替える）を作らない。途中の要素・最後の要素・workspaceそのもの（`runs/<run id>/worktree`をsymlinkに差し替えたもの）のどれがsymlinkでも、workspaceの中を指すものも含めて拒む（`RESOLVE_NO_SYMLINKS`と同じ）。`openat`で開けなかったときは`fstatat(AT_SYMLINK_NOFOLLOW)`でsymlinkかを見て`workspace_violation`と`backend_error`を分ける。この辿り方はLinuxでもmacOSでも同じで、`openat2`は使わない（`..`は字句で拒み済み、magic linkは`/proc`だけ）。hard linkは見分けられない（hostのworkerはworkspaceの外のファイルへのhard linkを作れるが、hostで直接書けるので失うものは無い）
3. workspaceの根の`.git`（worktreeのgitdirを指すファイル）とその下は、読みも書きも拒む（gitのbackendだけが触る）。hostのfilesystem（APFS）は大文字と小文字を区別しないので、名前の比較は大文字と小文字を区別せずに行う（`.GIT`・`.Git`も拒む）。より深い`.git`（`src/.gitignore`などを含む）は普通の名前
4. `~`は展開しない（workspaceの中の`~`という名前）
5. どれに当たっても`workspace_violation`

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

- brokerは`git`の実行ファイルをtokenのworkspaceでだけ走らせる（workerはrepositoryを選べない）。gitを走らせる前に、(1) fsと同じ辿り方でworkspaceを`--root`からsymlinkを辿らずに開き、(2) workspaceの根の`.git`が通常のファイル（worktreeのgitdirを指すファイル）であることを`fstatat(AT_SYMLINK_NOFOLLOW)`で確かめ（symlink・dirは`workspace_violation`、無ければ`backend_error`）、`O_NOFOLLOW`で1回だけ読む。(3) 中身の`gitdir: <絶対パス>`が指すgitdirは、どの`--root`の下にも無い`<common>/worktrees/<name>`で、その`commondir`が`<common>`に解決し、その`gitdir`ファイルがworkspaceの`.git`を指し返すこと（どれか違えば`workspace_violation`）。workerが`.git`を書き換えてmainのcheckout・別のrunのworktreeのgitdir・workspaceの中に自分で作ったrepository（configを握れる）を指させても、opはそこに届かない。(4) 以後の全てのgitのプロセスには解決した`GIT_DIR`（gitdir）・`GIT_COMMON_DIR`・`GIT_WORK_TREE`（workspace）をenvで渡し、`.git`を読み直させない（確かめてから走らせるまでの間の書き換えを効かせない）。共通dirは、containerでも同じ絶対パスでmountされている（ADR-t827-2決定5）
- 環境: envは空にしてから、`PATH`（brokerの`PATH`）・`HOME=<opごとに作って消す一時のdir>`・`LC_ALL=C`・`GIT_CONFIG_NOSYSTEM=1`・`GIT_CONFIG_GLOBAL=/dev/null`・`GIT_TERMINAL_PROMPT=0`・`GIT_OPTIONAL_LOCKS=0`・`GIT_PAGER=cat`・`GIT_EDITOR=false`・`GIT_TRACE2*=0`・`GIT_ALLOW_PROTOCOL=`（空。どのtransportも許さない）・`GIT_NO_LAZY_FETCH=1`・`GIT_NO_REPLACE_OBJECTS=1`と、上の`GIT_DIR`など、authorとcommitter（`GIT_AUTHOR_*`・`GIT_COMMITTER_*`、tokenの`committer`）だけを置く
- 設定（`-c`）: `core.hooksPath=/dev/null`・`credential.helper=`・`core.askPass=`・`core.fsmonitor=false`・`core.untrackedCache=false`・`core.pager=cat`・`core.editor=false`・`core.quotePath=false`・`core.sshCommand=false`・`core.attributesFile=/dev/null`・`core.excludesFile=/dev/null`・`protocol.allow=never`と`protocol.<file|ext|ssh|git|http|https>.allow=never`（repositoryのconfigのprotocolごとの`allow`に上書きさせない）・`commit.gpgSign=false`・`log.showSignature=false`・`diff.ignoreSubmodules=all`・`color.ui=false`・`gc.auto=0`・`maintenance.auto=false`・`safe.directory=<workspace>`。repositoryのconfigが定義するfilterのdriverは、opの前に`git config --get-regexp '^filter\..*\.'`で名前を読み、driverごとに`filter.<driver>.clean=`・`smudge=`・`process=`・`required=false`で消す（`.gitattributes`はworkerが書けるので、addとstatusでclean filterを走らせない）。diffは`--no-ext-diff --no-textconv --no-color`
- opは`status`・`diff`・`log`・`show`・`add`・`commit`・`restore`だけ。push・fetch・pull・remote・config・checkout・branch・reset・rebase・tag・credentialは持たない（`/v1/git/push`などは`invalid_request`の`no such operation`）
- `status`: `branch`は`git symbolic-ref -q --short HEAD`（detachedで`null`）、`entries`は`git status --porcelain=v1 -z --untracked-files=all --ignore-submodules=all --no-renames`の各行
- `diff`: `paths`は下の閉じ込めを通して`:(top,literal)<workspaceからの相対>`のpathspecにする（workspaceそのものは`:(top)`。workerのpathspecのmagicは効かない）。`staged`は`--cached`
- `log`: `limit`は既定20で、200を超える値は200にする。`--no-show-signature`
- `show`: 1つのcommitのmessageとpatch（`git show --no-ext-diff --no-textconv --no-color --no-show-signature --ignore-submodules=all --format=medium <commit> -- <pathspec>`）。`commit`は既定`HEAD`の任意のrevisionで、空・`-`で始まる（optionとして読まれうる）ものは`invalid_request`。`git rev-parse --verify -q <commit>^{commit}`で解き（commitにならなければ`backend_error`）、`git merge-base --is-ancestor <解いたcommit> HEAD`でHEADの履歴にあるものだけを見せる（mainのbaseまでの履歴は見え、別のrunのbranchのcommitは`workspace_violation`）。`paths`はdiffと同じpathspec。応答の`commit`は解いた完全なid。出力の上限はdiffと同じで`truncated`
- `add`: `paths`は1つ以上（空は`invalid_request`）。diffと同じpathspec。workspaceの根の`.git`（大文字小文字を問わない）は`workspace_violation`。symlinkの先は辿らない（gitが拒む）。fs.write・fs.editがcrashで残したtmp（名前が`.dagq-broker-`で始まり`.tmp`で終わり、その間が1文字以上のもの）はstageしない: pathspecの後ろに`:(top,exclude,glob)**/.dagq-broker-?*.tmp`を足し、`.`やdirを渡しても同じ要求の他のファイルだけがstageされる。tmpそのものを名指す（最後の要素がその名前のpathが`paths`に1つでもある）要求は、何もstageせずに`invalid_request`（黙って成功にすると、workerがstageしたと思い込むため）。利用者のrepositoryの`.git/info/exclude`は変えない（task 915）
- `restore`: `paths`は1つ以上（空は`invalid_request`）で、diffと同じpathspec。`staged`が無い・falseなら`git restore --worktree`（worktreeをindexから戻す）、trueなら`git restore --staged --source=HEAD`（indexをHEADから戻す＝unstage。worktreeは変えない）。別のrevision（`--source`）からは戻せない。書き込みはgitが行い、symlinkの先へは書かない（gitが拒んで`backend_error`）。smudge filterは上の設定で消える
- `commit`: `git symbolic-ref -q HEAD`がtokenの`branch`（`refs/heads/dagq/<run id>`）のときだけ行い、違う・detachedなら`workspace_violation`。`git commit`ではなくplumbingで作る: `write-tree`、親（`refs/heads/<branch>`のcommit）のtreeと同じなら`backend_error`（nothing to commit）、`commit-tree --no-gpg-sign <tree> -p <親>`（messageは前後の空白を除いて末尾に改行を付け、stdinで渡す。空は`invalid_request`）、`update-ref -m "commit (dagq-broker)" refs/heads/<branch> <commit> <親>`（古い値つきなので、動かすのはrun branchだけで、読んだ親からだけ）。hookは走らない
- 上限: gitの各プロセスは`--exec-timeout-secs`（既定60秒）で、超えればprocess groupにSIGKILLを送って`timeout`。gitが終わった後もpipeを持ち続ける子孫は同じ期限まで待ってからprocess groupごと止め、期限の後もpipeを読むthreadは待たない（要求を止めない）。stdoutは`--output-limit-bytes`（既定1 MiB）までで、`diff`と`show`は文字の境界で切って`truncated: true`、他は`output_limit`
- errorのmessageは、gitのstderrのうち最初の`fatal:` / `error:`の行（200文字まで）か終了の状態だけで、diffやファイルの中身を入れない。auditは他のopと同じ1要求1行で、commit message・diff・pathspecの外のpathは残さない（`paths`が1つのときだけ`path`）
- `process.exec`の`argv[0]`のbasenameが`git`なら、allowlistに関わらず`capability_denied`。これは誤りを止めるもので、`sh -c`・`env`・複製したバイナリからは通る

## process.exec

- `argv`は配列で、shellを通さない。`argv[0]`はallowlist（`dagq.toml`の`[broker] exec_allow`、serveの`--exec-allow`。既定は空で、execは全て拒む）にあるプログラムの名前だけで、`/`を含むpath（`/bin/ls`・`./x`）はallowlistの名前と一致しても`capability_denied`。名前はbrokerが固定の`PATH`の順に実行できる通常のファイルを探して解き（要求の`env`の`PATH`は探索に使わない）、見つからなければ`backend_error`。basenameが`git`なら（pathでも）allowlistに関わらず`capability_denied`。`argv`が空・`argv[0]`が空・引数にNULがあれば`invalid_request`
- cwdはworkspaceで、要求では指定しない（`ExecRequest`にcwdの欄は無く、あれば未知の欄で`invalid_request`）。workspaceはfsと同じく`--root`のfdから`openat(O_NOFOLLOW)`で1つずつ開き（symlinkを通るworkspaceは`workspace_violation`）、子で`fchdir`して入る（確かめたdirでそのまま走る）
- env: 空から始め、固定の`PATH=/usr/local/bin:/usr/bin:/bin`・`HOME=<execごとの一時のdir>`（`$TMPDIR`の下に`dagq-broker-home-<uuid>`をmode 0700で作り、execの後に消す。brokerのgitの`HOME`とは別）・`LANG=C.UTF-8`・`TERM=dumb`に、要求の`env`のうち`[broker] exec_env`（`--exec-env`）に名前があるものだけを足す（`LANG`・`TERM`は置き換えられるが、`PATH`・`HOME`は名前が許されていても置き換えない）。他の値は読まずに捨てる（名前はauditに残さない）。brokerのプロセス自身のenvは何も継がない。許した名前の値にNULがあれば`invalid_request`
- stdin: 要求の`stdin`（無ければ空で、すぐに閉じる）。`--output-limit-bytes`を超えれば走らせずに`output_limit`。読まないプログラムに書けなくなったら（EPIPE）残りを捨てる
- 子はprocess groupの長（`setpgid(0, 0)`）。stdout・stderr・stdinは非blockingにし、1つのthreadで`poll`して読み書きする
- timeout: 要求の`timeout_secs`（無ければ`[broker] exec_timeout_secs` = 60）と上限`exec_max_timeout_secs` = 300の小さい方。`0`は`invalid_request`。超えたらprocess groupにSIGKILLを送り`timeout`（`exit_code`はnull）。`setsid`で抜けた子はprocess groupでは止まらず、containerのpidsの上限が最後の歯止めになる
- 出力: stdoutとstderrの合計が`[broker] output_limit_bytes`（既定1 MiB）を超えたらprocess groupを止めて`output_limit`（`exit_code`はnull）。途中までの出力は返さない（ADR-t827-2決定8の「超えればプロセスを止めてerrorにする」に従い、truncatedの応答は持たない）。上限までの出力はそのまま返し、UTF-8でないbyteは置き換える（lossy）
- 子が自分で終わったときも、刈り取る前（`waitid`の`WNOWAIT`でgroupのIDを保ったまま）にprocess groupへSIGKILLを送り、backgroundに残した子を止める。その後の出力は500 msまで読み、groupを抜けた子がpipeを持ち続けても応答を待たせない
- 応答は`{exit_code, stdout, stderr, duration_ms}`で、`exit_code`はsignalで終わったとき`null`。auditには`program`・`argc`・`argv_sha256`・`exit_code`・`duration_ms`・`bytes_in`・`bytes_out`を残し、引数・env・stdin・出力は残さない。errorのmessageにも出力・stdin・envの値を入れない
- 走るのはcontainerの中で、containerのmemory・cpu・pidsの上限に入る。imageにはtoolchainが無く、軽いコマンド（`sh`・`ls`・`cat`・`grep`など、allowlistにあるもの）だけ。使い捨てのrepositoryの代表のtaskもそれで済むものにする（ADR-t827-3決定9）
- `sh`をallowlistに入れると、shellからimageの中の`git`も走らせられる（containerに資格情報もremoteへの経路の設定も無いので上流へのpushは通らないが、同じrepositoryの他のrefは書き換えうる）。使い捨てのrepositoryの検証のためだけに使い、既定には入れない
- allowlistが見るのは`argv[0]`だけなので、他のプログラムを走らせるもの（interpreter）を入れると、そこから`git`もworkspaceの外のcwdも走らせられる（誤りを止める仕組みで境界ではない）。serveのconfigの読み込み（`crates/dagq-broker/src/config.rs`の`INTERPRETERS`。`dagq.toml`の`[broker] exec_allow`・`exec_env`はdagqがserveのflagにして渡す予定（dagqはまだ読まない）なので、そうなれば両方に効く。task 917）は、`--exec-allow`の名前のbasenameが次の一覧にあれば拒まずに`dagq-broker serve: warning: `--exec-allow <name>` runs other programs; ...`を起動時にstderr（containerのlog）へ1行ずつ出す。使い捨てのrepositoryの検証が`sh`を使うので（ADR-t827-3決定9）拒まない
  - shell: `sh`・`bash`・`zsh`・`dash`・`ksh`・`mksh`・`ash`・`busybox`・`fish`・`csh`・`tcsh`
  - 引数のコマンドを走らせるもの: `env`・`xargs`・`find`・`nice`・`nohup`・`timeout`・`time`・`stdbuf`・`setsid`・`flock`・`chroot`・`sudo`・`doas`・`su`・`script`・`watch`・`parallel`・`make`
  - 言語のinterpreter: `python`・`python2`・`python3`・`perl`・`ruby`・`node`・`deno`・`bun`・`php`・`lua`・`tclsh`・`awk`・`gawk`・`mawk`・`nawk`
- `exec_env`（`--exec-env`）に`LD_PRELOAD`・`LD_LIBRARY_PATH`・`LD_AUDIT`などの`LD_*`と、`DYLD_INSERT_LIBRARIES`などの`DYLD_*`（子の読み込みを変えるenv）の名前があれば、正当な用途が無いのでserveは``--exec-env <name>` changes what the child loads; ...`のerrorで起動しない（config.rsの`LOADER_ENV_PREFIXES`。大文字小文字を区別した前方一致で、`ld_preload`や`OLD_PATH`は当たらない）
- 監視の1巡りで読むのはpipeごとに1回（64 KiB）までで、書く速さが読む速さを上回っても毎巡りで上限と期限を見る。期限の時点で子がもう終わっていれば`timeout`にせず、終わったものとして残りを読む
- `process.shell`（shellの文字列を受けるop）は作らない。要るならADRが別のcapabilityとして決める

## containerとPodman machine

### 人が行うこと

- podmanをhostに入れる: `brew install podman`（2026-09-28に人が入れた。`/opt/homebrew/bin/podman`）。dagqもworkerもpodmanを入れない。無ければ`dagq broker start`は`podman_missing`（「a person installs podman (brew install podman)」）で止まり、`broker status`と`doctor`の`broker`がそれを出す
- **machineの`init`は人が打たない。** dagq専用のmachine `dagq`は、必要になったとき（brokerの起動、podmanを要る`#[ignore]`のtestとスモーク）にruntimeが下の資源で`init`し、止まっていれば`start`する。人の既定のmachine（`podman-machine-default`など）と既定の接続には触らない

### 用意の手順（`dagq broker start`、`src/application/broker.rs`の`start`）

`dagq broker start`（管理のコマンド）とsupervisorの`ensure`（下の「supervisorの統合」）は、同じ`QueueBroker::start`（`src/infrastructure/broker_queue.rs`）で次を順に冪等に行う。2回呼んでも2回目は読むだけで何も作らない。queueごとのlock `<queue dir>/broker/lock`（flock）で同じqueueの`start`・`stop`を直列にする。

1. **machine**（`ensure_machine`）: host全体のlock `$XDG_CONFIG_HOME/dagq/podman-machine.lock`（無ければ`~/.config/dagq/podman-machine.lock`。`broker_podman::machine_lock_home`。podmanがmachineを見つける設定の場所に合わせ、`XDG_DATA_HOME`には置かない: e2eのfixtureは`XDG_DATA_HOME`を使い捨ての場所に向けるので、以前の`$XDG_DATA_HOME/dagq/podman-machine.lock`では並んで走る2つのe2eが互いを待たず、一方の`broker stop`が他方のimageのbuildの途中でmachineを止めていた。task 1162）の中で、`podman machine list --format json`に`dagq`が無ければ`podman machine init --cpus <cpus> --memory <MiB> --disk-size <GiB> --update-connection=false dagq`（rootless、既定のvolumeのまま）。止まっていれば`podman machine start --no-info --quiet --update-connection=false dagq`。startの後と、動いていると出ているmachineを使う前に、その接続が答えるかを`podman --connection dagq info --format {{.Version.Version}}`で確かめ、答えれば何もしない。startが失敗したか（podman 6.1.2で、止めた直後のstartが`Error: EOF`になり、listは動いていると出るのにmachineのsshのsocketが接続を拒むことがある）接続が答えないときは、同じlockの中で1回だけ`podman machine stop dagq`と同じ`start`をやり直し（stopの失敗はstartを妨げず、startも失敗したらmessageに添える）、接続が答えれば成功にする。やり直しは`MachineOutcome`の`restarted: true`と`restart_reason`（startの失敗か接続の失敗のpodmanの言葉）に出て、`dagq broker start`の出力と`broker_started`の`machine`に残る。やり直しても駄目なら、最初の理由とやり直しの失敗を並べた`machine_failed`にする（stopもstartも1回ずつしか増えない。task 1126）。**`podman system connection default`と`machine set-default`は打たず、`--update-connection=false`を付ける。** ただしpodman（6.1.2で確かめた）は、接続が1つも無いhostで最初に作ったmachineの接続を既定にする（この`--update-connection=false`は`init`では効かない）。人が既に既定の接続を持っていればそれは変わらない。人のmachineも接続も無いhostでは`dagq`が既定の接続になるので、人が後で自分のmachineを作ったら`podman system connection default <name>`で既定を選び直す 他のmachineが動いているときは`machine_busy`のerrorにし、startも人のmachineの停止もしない（接続の確かめもやり直しもしない）
2. **image**（`ensure_image`）: `podman --connection dagq image exists localhost/dagq-broker:<build tag>`が無ければ、build contextを`<queue dir>/broker/build-context`に埋めた材料から展開して（`EmbeddedSource::stage`。上の「配布と版」の「imageの材料」。終われば消す）`podman --connection dagq build --build-arg RUST_VERSION=<埋めたRustの版> --build-arg CARGO_BUILD_JOBS=1 --tag localhost/dagq-broker:<build tag> --file <context>/Containerfile <context>`。imageが無いと分かったら、buildの前に`state.json`の`state`を`building`にする（`StartRequest::on_build`）。`dagq broker start`はこれを同期に待ち、supervisorはjobのthreadで走らせてclaimを待たせない。buildにかかった時間は`StartReport::build_ms`
3. **container**（`ensure_container`）: `dagq-broker-<queue hash>`が同じimageと同じ引数（label `dagq.broker.spec`に`podman run`の引数のSHA-256を付け、portやmountが変われば違う）で動いていれば何もしない。別のimageか別の引数で動いていれば、`active/`に印がある間は残し（`kept_stale`）、無ければ消して作り直す。止まっていれば消して今の引数で作り直す（mountとportがいつも今のものになる）。無ければ作る。mountするものは先に作る（鍵は`ensure_key`、`runs`・`active`・`audit`・`<common>/hooks`のdir）。起動の形（`ContainerSpec::run_args`）:

   ```sh
   podman --connection dagq run --detach --name dagq-broker-<queue hash> \
     --userns=keep-id --read-only --tmpfs /tmp:size=64m \
     --cap-drop=all --security-opt no-new-privileges \
     --memory 512m --cpus 1 --pids-limit 256 \
     --publish 127.0.0.1:<port>:8750 \
     --volume <queue dir>/runs:<queue dir>/runs:rw \
     --volume <git common dir>:<git common dir>:rw \
     --volume <git common dir>/config:<git common dir>/config:ro \
     --volume <git common dir>/hooks:<git common dir>/hooks:ro \
     --volume <queue dir>/broker/key:<queue dir>/broker/key:ro \
     --volume <queue dir>/broker/active:<queue dir>/broker/active:ro \
     --volume <queue dir>/broker/audit:<queue dir>/broker/audit:rw \
     localhost/dagq-broker:<build tag> serve --container --listen 0.0.0.0:8750 \
       --key <queue dir>/broker/key --active <queue dir>/broker/active \
       --audit <queue dir>/broker/audit --root <queue dir>/runs
   ```

   `-e`・`--env`・`--env-host`・`--env-file`は付けず、hostのenv（upstreamの資格情報を含む）はcontainerに入らない。containerの中のbindは`0.0.0.0:8750`（containerのnetwork namespaceの自分のinterfaceで、LANではない）で、hostへのpublishは`127.0.0.1`だけ。unit test（`the_container_publishes_on_loopback_only_and_mounts_only_what_it_needs`）がpublishとmountの一覧とenvの無いことを固定する
4. **health**（`wait_healthy`）: `GET http://127.0.0.1:<port>/v1/health`が`status: ok`とprotocol 1を返すまで最大30秒待つ（0.5秒ごと）。答えなければ`unhealthy`のerror。dagqはHTTPのcrateを持たないので、healthは`src/infrastructure/broker_podman.rs`の手書きのHTTP/1.1のGETで見る（clientに`health`が入ったら`dagq-broker-client health --json`に替えるかは配布のtaskが決める）。healthの`build`とdagqのbuild識別子の比較とbuildし直しは`start`が行う（上の「配布と版」。task 843）
5. **古いimageとdanglingのimageの掃除**（`prune_images`・`prune_dangling`）: 今のtagと1つ前（作成時刻で）と、machineのcontainerが使うもの以外の`localhost/dagq-broker`のimageを`podman --connection dagq image rm`する。続けて`prune_dangling`がtagの無いimage（多段buildのbuild stageの`<none>`）を`podman --connection dagq image prune --force`（`--all`なし）で消す。失敗は`StartReport::images`（`failed`・`error`・`dangling_error`）に出るだけで`start`は成功する（上の「配布と版」。task 1089・1131）

- host全体のlockは`start`の間（machine・build・container・health）ずっと持つので、別のqueueの`stop`や`release_machine`がbuildやrunの途中でmachineを止めない。そのかわりbuildの間は他のqueueの`start`と`stop`が待つ
- **切れた接続の待ち**（task 1162）: 本物のports（`system_ports`）のpodmanは`Reconnecting`で包む。machineの接続の上のコマンド（`--connection dagq ...`）が、podmanのstderrに接続が途中で切れた言葉（stdoutは見ない。buildのstepの出力が載り、imageの中のdownloadの失敗は切れた接続ではないため。`LOST_CONNECTION_WORDS`: `ssh: handshake failed`・`connection reset by peer`・`connection refused`・`broken pipe`・`unexpected EOF`・`server probably quit`・`Error: EOF`・`Cannot connect to Podman`）で失敗したら、`podman --connection dagq info`を`RECONNECT`の`interval`（3秒）おきに`probes`（10回）まで聞き、答えたら同じコマンドをやり直す（`reruns`、3回まで）。固定のsleepで待ち切らずに`info`の答えで繋がったことを確かめて進む。答えなければ、またはやり直しても切れ続ければ、最後の出力に待ったことの注記（`podman --connection dagq info did not answer in 10 asks 3s apart: ...` / `the connection to dagq was lost again after 3 reruns`）を足して返し、呼び出し側が今までどおり`podman_failed`（`ensure_machine`の接続の確かめでは`machine_failed`）にする。`--connection`の無いコマンド（`machine start`など。startの`EOF`は上の1の起動し直しが扱う）と、接続が切れた言葉の無い失敗はそのまま返す。`ensure_machine`の`info`も包まれるので、動いているmachineの接続が一時的に切れただけなら起動し直さずに待つ。e2eの関門は同じ接続の待ちで`broker::connect`を呼び、podmanに繋がるかを確かめる（[Auto-update](supervisor-lifecycle/auto-update.md)、ADR-t1162-1）
- **関門のlockと失敗時の停止**: machineのhost全体のlock（`podman-machine.lock`）の待ちは30秒（`MACHINE_LOCK_LIMIT`。e2e実行の`--e2e-timeout`とは別）を上限とし、待ちきれなければ`broker::`を流さず残りで判定する。理由`timed out waiting for the machine lock after 30s`をlogと`E2eOutcome::skipped`に残す。確認から失敗時の後始末まで同じlockを保持し、自分でinit・startしたmachineに繋がらなければ`podman machine stop dagq`を打つ（stopの失敗は接続失敗の理由に追記する）。関門の前から動いていたmachineは接続を確かめるだけで、停止・再起動しない。
- 調べた原因（task 1162、2026-09-30の関門の05:12と06:45の失敗）: どちらも同じ時刻にworkerのe2eのbrokerのtestが同じ`dagq`のmachineを使っていた（run 03ebc442・9e78e1ea・ab21fc74などのreceiptのe2eの時刻と、run 8f573833のe2eのbrokerのtestも同じ`server probably quit: unexpected EOF`で落ちていたこと）。lockが`XDG_DATA_HOME`の下にあり、fixtureごとに別のlockだったので、一方の後始末の`release_machine`が他方のbuildの途中（containerがまだ無い）でmachineを止め、他方のbuildは`unexpected EOF`、次のコマンドは止まりかけのmachineのsshに`handshake failed ... connection reset by peer`になった。machineは1 CPU・1GiB（`MachineSpec`の既定）で、VMの`dagq.log`（`$TMPDIR/podman/dagq.log`）はstartごとに書き直されるので当時のconsoleは残っていない
- `image exists`・`container exists`はexit 0を有る、1を無いとし、それ以外（machineが答えないなど）は`podman_failed`にする（無いと取り違えてbuildやrunに進まない）
- portは`--port`、無ければ`state.json`の前のport、無ければ`127.0.0.1`の空いているport
- `dagq broker stop`: machineが動いていればcontainerを`podman --connection dagq rm --force --time 10`で止めて消し（状態は全てmountにあり、`start`は止まったcontainerを作り直すので残さない）、host全体のlockの中でmachineに動いているcontainerが無ければ`podman machine stop dagq`（`release_machine_with`）。続けて同じlockの中でmachineの孤児のgvproxyを片付け（下の「machineのgvproxyの後片付け」）、`StopReport`の`gvproxy`に載せる。2回目は何も止めない（片付けは0件として報告する）
- `dagq broker status`: 状態を変えない。`state`は`running`・`stopped`（containerが無いか止まっている）・`unhealthy`・`machine_missing`・`machine_stopped`・`machine_busy`か、errorのcode（`podman_missing`など）。`machine`・`image`（このdagqのbuildのimage）・`image_present`・`container_status`・`health`・`build`（dagqのbuild識別子）・`build_matches`（healthの`build`が一致するか。healthが無ければ`null`）・`client`（下の「status と doctor」）・`recorded`（`state.json`）を添える
- errorは構造化する: `BrokerFailure {code, message}`。codeは`podman_missing`・`podman_failed`・`machine_busy`・`machine_failed`・`image_source_missing`・`image_build_failed`・`container_failed`・`unhealthy`・`repository_unknown`・`client_missing`・`version_mismatch`と、読むだけのコマンド（`dagq broker logs`）だけが使う`machine_missing`・`machine_stopped`・`container_missing`（machineやcontainerを用意せず、無い・止まっていることをerrorにする。`start`・`stop`はこの3つを返さず、`broker status`の`state`の`machine_missing`・`machine_stopped`は同じ名前の状態でerrorではない）。CLIのerrorのJSONは`{"error": "...", "broker": {"code", "message"}}`で、`start`は失敗のcodeを`state.json`の`state`に残す。黙って続けない
- 権限: `broker start`・`broker stop`は`up`・`down`と同じ`service.lifecycle`（`Operation::Broker`）、`broker status`・`broker logs`・`broker audit`は`queue.read`（workerを含む全てのroleが打てる）。以前の案の新しいcapability `broker.manage`は足さなかった（同じroleの組（user・inbox・planner・supervisor）で足り、`service.lifecycle`がサービスの起動と停止を名指すため）
- podmanを要るtestは`tests/it/broker_podman.rs`（`#[ignore]`、`cargo test --locked --test it broker_podman:: -- --ignored`）。machineを用意の処理で用意し、終わりに`release_machine`で止める。unit testは`src/application/broker.rs`のstubの`Podman`で、machineが無い・止まっている・動いている・他のmachineが動いているの各状態からの手順と冪等性、podmanが無いときのerrorを見る。切れた接続の待ちは同じstubの`Reconnecting`で、切れた言葉とそれ以外の見分け（`lost_connections_are_told_from_commands_that_failed`）、`info`が答えた後のやり直し（`a_lost_connection_is_waited_for_and_the_command_runs_again`）、答えない・切れ続けるときの`podman_failed`（`a_connection_that_does_not_come_back_is_podman_failed`）、それ以外の失敗と`--connection`の無いコマンドを素通しすること（`other_failures_and_commands_pass_through_unchanged`）、`connect`の用意と接続の待ち（`connect_readies_the_machine_and_waits_for_its_connection`）、接続失敗時に自分で起動したmachineだけを同じlockの中で止め、以前から動いていたmachineは止めないこと（`connect_stops_only_its_own_machine_on_failure_while_holding_the_lock`）、最後の接続の確認が失敗しstopも失敗したときの理由の報告（`connect_reports_a_failed_stop_after_its_final_probe_fails`）を見る。`tests/it/e2e_gate_podman.rs`の`a_busy_machine_lock_skips_broker_and_records_why`が、lockを持ち続けるfakeで関門のlock待ちが30秒で終わり、`broker::`を流さず残りで判定して理由をlogと`E2eOutcome::skipped`に残すことを見る。lockの場所は`src/infrastructure/broker_podman.rs`の`the_machine_lock_follows_the_config_home_not_the_data_home`

- `<queue dir>/broker/state.json`: `port`・`container`・`image`・`state`（`building`・`running`・`stopped`か、`start`が失敗したcode）・`build`（containerを作ったdagqのbuild識別子）・`started_at`（healthが答えたunix秒）
- 管理のコマンド: `dagq broker status`・`dagq broker start`・`dagq broker stop`（task 836。上の「用意の手順」）。`status`は`mode`（下の「mode と設定」）も出す。`start`と`stop`はmodeに依らず動かす（人とtestとスモークが頼むため）。podmanは`--podman`、無ければ`host.toml`の`podman`、無ければPATH。`dagq broker logs`と`dagq broker audit`（下の「audit」。task 925。`src/application/broker_admin.rs`）
- `dagq broker logs [--tail N] [--podman PATH]`: queueのcontainer（`dagq-broker-<queue hash>`）の`podman --connection dagq logs --tail N <container>`（既定のNは200、`DEFAULT_LOG_TAIL`）を`{container, tail, stdout, stderr}`で出す（containerのstdoutとstderrを分けたまま）。読むだけで、machineを起動せずcontainerを作らない。先に`machine list`でdagqのmachineが動いていること、`container exists`でcontainerがあること（止まっていてもよい）を確かめ、podmanが無ければ`podman_missing`、machineが無ければ`machine_missing`、止まっていれば`machine_stopped`、containerが無ければ`container_missing`、`podman logs`の失敗は`podman_failed`の`BrokerFailure`（`{"error", "broker": {"code","message"}}`）で終わる。権限は`queue.read`で、queueのDBを開かない
- event（queueのevent）: `broker_started`（`mode`・`port`・`build`・`image`・`container`・`container_outcome`・`machine`（`initialized`・`started`・`restarted`・`restart_reason`と、片付けがあれば`gvproxy`（`GvproxyCleanup`の列））・`images`（上の「配布と版」の古いimageとdanglingのimageの掃除の`ImagePrune`: `removed`・`in_use`・`failed`・`error`・`dangling_removed`・`dangling_error`））・`broker_image_built`（`build`・`image`・`duration_ms`）・`broker_unhealthy`（attention。`reason`・`message`・`failures`・`restarted`）・`broker_healthy`（healthがまた答えた）・`broker_stop_requested`（`down`がsignalの前に、drainの終わりにbrokerを止めるよう頼んだsupervisor。`supervisors`（token）・`by: down`。`disabled`では書かない）・`broker_stopped`（`container`・`container_stopped`・`machine_stopped`・`gvproxy`（下の「machineのgvproxyの後片付け」の`GvproxyCleanup`）と、止めたのが頼まれたsupervisorなら`by: supervisor`、`down`自身なら`by: down`。`down`は何も止まらず、gvproxyも片付けず片付けの失敗も無かったとき（supervisorが先に止めた）は書かない）。supervisorが書くものは`supervisor`（token）を添える

### machineのgvproxyの後片付け（task 1579）

podmanのmachine（applehv・vfkit）は、startのたびにnetworkの`gvproxy`（`<podman>/libexec/podman/gvproxy`）をhostで起動する。引数に`-listen-vfkit unixgram://<podmanのruntime dir>/podman/<machine>-gvproxy.sock`と`-pid-file <podmanのruntime dir>/podman/gvproxy.pid`を持ち、親はlaunchdになる。

**記録とソースで確かめた事実**:

- 2026-10-03 19:37 JSTに、親launchdのgvproxyが128個（RSS合計約1.4GB、最古は3日超）残っていた。全てmachine `dagq`のsocketと`gvproxy.pid`を指し、machineは`stopped`でvfkitは無かった。人の指示でinboxが止めた。同じ時刻にload 82・空きメモリ150MBで自動更新のe2eが13本落ちた（ask 372）
- dagqがmachine `dagq`を止め、起動する経路は`src/application/broker.rs`の`ensure_machine`（`init`・`start`、startの失敗か接続の失敗の後の`restart_machine`の`stop`と`start`）と`release_machine`（`dagq broker stop`・`down`・supervisorのdrainの終わりの`stop`、`tests/it/broker_podman.rs`とe2eの後始末）と`connect`（関門が自分で起動したmachineへの接続失敗の後の`stop`）。task 1579の前は、どの経路もmachineの`stop`の後と、startの失敗・接続の失敗・podmanの呼び出しのerrorの出口でhostのprocessを見ず、gvproxyを片付けなかった
- `PodmanCli`（`src/infrastructure/broker_podman.rs`）はpodmanに期限を付けない。時間切れで殺されたstart（exit codeの無い失敗の出力）は、外から殺されたpodmanのstartとしてstartの失敗と同じ出口を通る。dagq自身がstartの途中で殺されたときは出口の片付けが走らないので、次の`start`の前の片付けが拾う
- e2eの関門は`broker::connect`でmachineを起動し、e2eのbrokerのtest（`tests/e2e/broker.rs`の`BrokerGuard`）と`tests/it/broker_podman.rs`は終わりに`release_machine`で止める。1日に何十回もstartとstopが繰り返される

**実podmanでしか確定できない仮説**（podman 6.1.2。workerは実podman・実machineで確かめない）:

- H1: `podman machine stop`がvfkitを止めてもgvproxyを終わらせない（または終了を待たない）ことがある
- H2: startが同じ`gvproxy.pid`を上書きするので、前のgvproxyをpodmanが見失い、後のstopやstartが前のものを片付けない
- H3: 失敗したstart（`Error: EOF`など）や殺されたstartが、起動済みのgvproxyを残す
- H4: 128個の多くは、e2e・`broker_podman`のtest・関門の`connect`の繰り返しと、`restart_machine`のstop→startで積み上がった

これらは、runtimeのe2e（`broker`のe2eの前後でmachine `dagq`のsocketを持つgvproxyの数を数える）か、人・inboxのopsで確かめる（machineを`start`・`stop`し、`ps -A -ww -o pid= -o args=`で同じsocketのgvproxyの数と`gvproxy.pid`を見る）。

**後片付けの規則**（`clean_gvproxy`、portは`HostProcesses`、本物は`SystemProcesses`の`/bin/ps -A -ww -o pid= -o args=`とkill(2)）:

- `podman machine list`でmachineが`stopped`と確かめられたときだけ片付ける。`running`（`skipped: running`）・machineが無い（`missing`）・listが失敗した（`state_unknown`と`state_error`）ときはどのprocessにも触れない
- 片付けるのは、実行ファイルの名前が`gvproxy`で、引数のどれかが`podman/<machine>-gvproxy.sock`（裸のpathか`unixgram://`・`unix://`のURL）のprocessだけ（`is_gvproxy_of`）。他の名前のmachine（`podman-machine-default`・`dagq2`など）のgvproxy、socketを名指すだけの別のprocess、人のpodmanには触れない（ADR-t827-3の人のmachineに触れない規則）。pidの0・1・dagq自身にはsignalを送らない
- 見つけた全てに`SIGTERM`をまとめて送って終わったことを確かめ（`GVPROXY_EXIT_WAIT`、5秒）、終わらなかったものにまとめて`SIGKILL`を送ってもう一度確かめる（何個あってもhost全体のlockを握る時間は待ち2回分まで）。既に居ないprocessは終わったものとする
- 呼ぶ場所: `release_machine_with`の後（stopした・止まっていた・stopが失敗した、のどれでも。`dagq broker stop`・`down`・supervisorのdrainの終わりの`QueueBroker::stop`）、止まっているmachineの`start`の前（`before_start`。以前の孤児を積み上げない）、`restart_machine`のstopの後でstartの前（`restart_stop`）、`ensure_machine_with`の失敗の出口（`failure`。最後のstartの失敗、接続の確かめの失敗、podmanの呼び出しのerrorでの早期return、時間切れで殺されたstart、`machine_busy`）。`stop`の最初のmachineの状態の読み取りが失敗したときは、host全体のlockの外なので片付けず、`state_unknown`として失敗のmessageに添える。片付けは全てhost全体のlockの中で行う（別のqueueの`start`が起動した直後のgvproxyを殺さない）
- 結果は`GvproxyCleanup`（`machine`・`after`（`stop`・`before_start`・`restart_stop`・`failure`）・`cleaned`（0を含む）・`skipped`・`state_error`・`failures`（processの列挙・signalの失敗・期限内に終わらないこと））。`StopReport::gvproxy`（`dagq broker stop`・`down`の出力と`broker_stopped`の`gvproxy`）、`MachineOutcome::gvproxy`（`broker_started`の`machine`）に載る。失敗の出口では元の`BrokerFailure`のcode（`machine_failed`など）を変えず、messageに`gvproxy of dagq after <after>: ...`を添える。片付けの失敗はstopやstartの成否を変えない。supervisorは片付けの失敗をwarnのlogにも出す
- 片付けるのは`Ports`に`processes`を持つ`start`・`stop`（`QueueBroker`）だけ。`ensure_machine`・`release_machine`・`connect`（e2eの関門と`tests/it/broker_podman.rs`が呼ぶ）は`None`のままprocessに触れない。e2eの関門の`connect`は自分で起動したmachineへの接続失敗時にlockの中でstopするが、gvproxyのprocessには触れない
- test: `src/application/broker.rs`のunit test（fakeのpodmanとfakeのprocess一覧）が、自分のsocketのgvproxyだけを見分けること（`only_the_gvproxy_on_the_machines_own_socket_is_its`）、stoppedでの片付けと終了の確認（`a_stopped_machines_gvproxy_is_ended_and_seen_to_exit`）、running・missing・状態を確かめられないときに触れないこと（`a_machine_not_known_to_be_stopped_keeps_its_gvproxy`）、列挙・signal・終了の確認の失敗の報告（`what_the_cleanup_could_not_do_is_reported`）、startの前（`a_stopped_machine_is_cleaned_before_its_start`）、restartのstopとstartの間（`a_restart_cleans_between_its_stop_and_its_start`）、各失敗の出口（`every_failure_of_ensure_machine_cleans_and_keeps_its_failure`）、`release_machine_with`と`stop`（`release_and_stop_clean_after_the_machine_stops`）を見る。`src/infrastructure/broker_podman.rs`の`ps_lines_are_pids_and_their_arguments`と`an_orphan_is_listed_ended_and_seen_gone`が`SystemProcesses`を、`tests/it/runtime_broker.rs`の`down`の2本が`broker_stopped`の`gvproxy`を見る

### supervisorの統合

ADR-t827-3の決定2・3。modeが`disabled`（既定。`[broker]`の無いqueueを含む）なら、supervisor・`up`・`down`はpodmanを探しも呼びもしない（`Ports::broker`が`None`。`tests/it/runtime_broker.rs`の`a_disabled_broker_calls_no_podman`）。

- **mode**: supervisorは起動時にmain checkoutの`dagq.toml`の`[broker]`と`host.toml`の`[broker]`を読み（`compose::load_broker_setup`）、modeを決める。起動の後の変更はsupervisorを起動し直すまで効かない。`required`はerrorで起動しない
- **ensure**: 最初のpassと、brokerが用意できていない間は各passのclaimの前に、`QueueBroker::start`（上の「用意の手順」）をjobのthreadで1本ずつ走らせる（`Supervisor::broker_pass`）。loopは待たず、後のpassで結果を刈り取る。imageのbuildの間`state.json`は`building`で、claimは止まらない。成功で`broker_started`（buildしたなら先に`broker_image_built`）。失敗（`machine_busy`・`podman_missing`・`image_build_failed`など）は`broker_unhealthy`（`reason`はそのcode）で、`BROKER_HEALTH_INTERVAL`（30秒）ごとにやり直す
- **health**: 用意できた後は`BROKER_HEALTH_INTERVAL`ごとに`state.json`のportの`/v1/health`を1回見る（これもjob）。`BROKER_FAILURES`（3）回続けて失敗したら、containerを1回起動し直す（`application::broker::restart`: host全体のlockの中で、machineが動いていればcontainerを消して作り直し、healthを待つ。machineが動いていなければ起動し直さずに`machine_failed`か`machine_busy`）。直れば`auto_repaired`（`repair: broker_restart`、`layer: runtime`、`conditions.failures`。ADR-0047の1層目）。起動し直しが失敗したら`broker_unhealthy`（`reason`はそのcode）を残し、用意できていないものとして次の間隔から`ensure`（machineから）をやり直す。起動し直した後にまた3回続けて失敗したら`broker_unhealthy`（`reason: unhealthy`）。healthが答えれば数えるのをやめ、attentionが立っていれば`broker_healthy`を残す（起動し直しもまた1回できる）
- **attention**: `broker_unhealthy`はinbox宛てのattention（`next: dagq broker status`、`status`は`reason`）で、最新の`broker_unhealthy` / `broker_healthy` / `broker_started` / `broker_stopped`が`broker_unhealthy`の間出る。立っている間は`reason`が変わっても書き直さない（1つの不調で1回知らせる）
- **claim**: `preferred`ではbrokerの状態でclaimを止めない。使えるbrokerならworkerにtokenとMCPの道具を渡し、使えなければ`broker_unavailable`を残して道具なしで動かす（上の「token」の発行）。このsupervisorの`ensure`がまだ返っていない間（起動の直後と`--once`）は、`state.json`が`running`でdagqのbuildを名指すbrokerのhealthを見て、答えればそのportを使う（`BrokerControl::running_port`）。負荷の高いhostで1回答えそこねただけでrun全体が道具なしにならないよう、healthが答えないときは`RUNNING_PORT_PROBES`（3）回まで`health_interval`（500ミリ秒）をあけて見直し、答えたがstatus・protocol・buildの合わない答えは見直さない。使えないときの`broker_unavailable`（`reason: not_ready`）の`message`は、stateの食い違いか、各回のhealthの失敗の理由を持つ（task 1255。e2eの`broker::a_preferred_worker_does_its_task_through_the_broker_and_lands`の1回の失敗は、着地したcommitにexec.txtだけが無くworktreeがcleanだったことから、stubがbrokerを使わない経路でcommitした、つまりclaimが道具を渡さなかったと消去法で推定した。当時のmessageは理由を持たず、このhealthの取りこぼしと確かめきれていない。stubは今、`E2E-BROKER`のtaskで道具が無ければその理由で止まる）
- **sweep**: 各passのclaimの前と止まる直前に、終わったrunのtokenを失効させ、期限の近いtokenを発行し直す（上の「token」の失効と期限）。`disabled`では残った全てのtokenを`mode_disabled`で失効させる
- **停止**: supervisorがbrokerを止めるのは、`down`が頼んだdrain（最新の`broker_stop_requested`が自分のtokenを挙げる）を終えたときだけで、自分の登録を消した後に、queueのほかのsupervisor（heartbeatの新しい登録）が残っていなければ、走っているjobの終わりを待ってから`QueueBroker::stop`（containerの停止と`release_machine`）を呼び、`broker_stopped`（`by: supervisor`）を残す（`Supervisor::stop_broker_after_down`。ほかのsupervisorが残れば、最後に終わるものに任せる）。`up`の入れ替えのdrainは頼まないので止めず、execの引き継ぎ（`install`と自動更新）はdrainしないので止めない。execの引き継ぎは走っているjobの終わりを待ち（execでpodmanのコマンドが孤児にならないように。その間新しいjobは始めない）、それ以外でloopが終わるときに走っているjobは待たずに放す。新しいsupervisorの`ensure`はimageのtag（build識別子から作る）が違えば作り直し、有効なtokenのrunが残る間は古いcontainerを残す（上の`kept_stale`）
- **`down`**: 生きているsupervisorにsignalを送る前に（launchdのunloadより前に）、modeが`disabled`でなければ`broker_stop_requested`を残して、drainの終わりにbrokerを止めるよう頼む（`lifecycle::BrokerLifecycle::request_stop`。`--force`は頼まない）。drainを待たない既定の`down`（`draining`）は自分では止めず、出力の`broker`に`stopped: false`と、supervisorがdrainの終わりに止めることを載せる。drainを見届けた`not_running`・`stopped`（`--wait`）・`killed`（`--force`）の後には、`dagq broker stop`と同じ`QueueBroker::stop`を自分でも呼ぶ（supervisorが止め損ねた・落ちていた・killされたときのため。何か止めたときだけ`broker_stopped`（`by: down`）を残す。`lifecycle::BrokerLifecycle::after_drain`）。止め損ないは`down`を失敗にせず`broker.error`に載せる
- **`up`のpreflight**: modeが`disabled`でなければ、`host.toml`の`podman`（無ければ`podman`）が`up`のPATHで解決できることを確かめ、無ければsupervisorを起動しない（`[run.env]`のプログラムの検査と同じ扱い。`lifecycle::BrokerLifecycle::preflight`）。`required`も拒む
- podmanを要る`#[ignore]`のtestとスモークも終わりに同じ`release_machine`を呼ぶ
- test: `tests/it/runtime_broker.rs`（podmanはportのfake）がdisabled・background build・3回の失敗と起動し直しとattention（`down`の頼まないdrainでは止めないことも）・`machine_busy`とclaim・handoff・`down`（`--wait`の後と、既定の`down`の後のsupervisorのdrainの終わり）を、`tests/it/lifecycle_up.rs`の`up_refuses_a_broker_mode_without_podman_on_its_path`が`up`のpreflightを見る

### 資源

| 値 | 既定（起点） | 決め方 |
| --- | --- | --- |
| machineの名前 | `dagq` | 固定。人の既定のmachineと分ける |
| machine CPU | 1 | |
| machine memory | 1024 MiB | imageのbuildが通らなければ512 MiBずつ上げ、通った最小をここに書く |
| machine disk | 10 GiB | buildのimage（rust）と実行のimageが入る最小 |
| container memory / cpus / pids | 512m / 1 / 256 | brokerとexecの軽いコマンドが通る最小 |
| buildの並列度 | `CARGO_BUILD_JOBS=1` | 最小のmachineでメモリを越えないため |

task 836で測った（2026-09-28、podman 6.1.2、applehv）: 起点の値（CPU 1・1024 MiB・10 GiB）で`machine init`（VMのimageのdownloadを含めて約5分）もimageのbuildも通り、上げる必要はなかった。machineのstartからimageのbuild（rustとalpineのpull、依存のcrateのdownload、`CARGO_BUILD_JOBS=1`のrelease build）・containerの起動・healthまでが約150秒、imageがあるときの`start`から`stop`までが約50秒（`tests/it/broker_podman.rs`の2本）。buildの最大のメモリとbrokerの常駐のメモリは測っていない（containerの上限512mで動いた）。値は`host.toml`の`[broker]`（下の「mode と設定」）で上書きできる: `machine_cpus`・`machine_memory_mib`・`machine_disk_gib`が`machine init`の引数に（`MachineSpec::with_host`。machineの名前は`dagq`のまま）、`container_memory`・`container_cpus`・`container_pids`が`podman run`の引数に（`ContainerLimits::with_host`）効く。machineの資源はinitのときにしか効かないので、作った後に変えるなら人が`podman machine rm dagq`してから次の起動に任せる。buildの並列度は固定

### image

`containers/broker/Containerfile`（dagqに埋める材料の1つ）の形:

- build context: dagqが埋めた材料（上の「配布と版」の「imageの材料」）。dev buildはContainerfile、`Cargo.lock`、`crates/dagq-broker-protocol`と`crates/dagq-broker`の`Cargo.toml`・`build.rs`・`src/`（testsは入れない）、この2つだけをmembersにしたworkspaceの`Cargo.toml`。releaseはContainerfileだけ
- buildのstage: `docker.io/library/rust:${RUST_VERSION}-alpine`（`RUST_VERSION`はbuild-argで、dagqが埋めたRustの版）に`musl-dev`を足し、dev buildは`# dagq:build-from-source begin`と`end`の間の`cargo build --release -p dagq-broker`（`CARGO_BUILD_JOBS`はbuild-arg。`--locked`は付けない）、releaseはそこを置き換えた`cargo install --locked dagq-broker@<version>`。muslの静的なバイナリ。版がbuild-argなのでdigestでは固定しない
- 実行のstage: `docker.io/library/alpine:3`に`git`だけを`apk add --no-cache`し、非rootの`USER 10001`にし（起動は`--userns=keep-id`でhostのユーザーのuidに写し、mountしたファイルの持ち主と揃える。imageの`USER`は`--userns`無しで起動されたときも非rootで動くため）、`ENTRYPOINT ["/usr/local/bin/dagq-broker"]`
- 言語のtoolchainは入れない（`process.exec`は軽いコマンドだけ。ADR-t827-3決定9）

## workerの道具（MCP）

- modeが`preferred`で、brokerが健康で版が一致し、providerがClaude Codeのworker（とresume、非対話のturn）にだけ渡す。Codexのworkerには渡さず、`broker_unavailable`（`reason: provider`）を残す（CodexのMCPの渡し方は後のtask。runの途中でCodexへ切り替わったworkerは`mcp.json`が残っていても受け取らない: `AgentProvider::broker_tools`の既定は何もしない）
- `<run dir>/broker/mcp.json`:

  ```json
  {"mcpServers":{"dagq-broker":{"command":"<dagqの隣>/dagq-broker-client","args":["mcp"],
    "env":{"DAGQ_BROKER_URL":"http://127.0.0.1:<port>","DAGQ_BROKER_TOKEN_FILE":"<queue dir>/broker/tokens/<run id>"}}}}
  ```

  を`HostActorExecutor`がClaude Codeの`--mcp-config <file>`で渡す。envにtokenの値は入れない
- 起動: `dagq-broker-client [--url URL] [--token-file FILE] mcp`（`mcp`はflagも引数も取らない）。URLが無いか誤っていればserverは始めずにstderrに理由を出してexit 3。token fileは呼び出しごとに読むので、無くても起動し、道具の呼び出しが`client_error`（`config`）になる。stdinが閉じたらexit 0
- 形: JSON-RPC 2.0を1行1 message（改行区切り）でstdin / stdoutに流す。batch（配列）は`-32600`。method: `initialize`（`protocolVersion`は`2025-06-18`・`2025-03-26`・`2024-11-05`のうち求められたものを、それ以外なら`2025-06-18`を返す。`capabilities`は`{"tools":{"listChanged":false}}`、`serverInfo`は`{"name":"dagq-broker-client","version":<BUILD>}`、`instructions`にworkspaceの相対パスとerror codeの説明）、`ping`（`{}`）、`tools/list`、`tools/call`。JSONでもUTF-8でもない行は`-32700`で答えて続ける。idの無いmessage（`notifications/initialized`・`notifications/cancelled`など）には答えない。未知のmethodは`-32601`、JSONでない行は`-32700`、`tools/call`の`name`が無い・未知の道具・`arguments`がobjectでないものは`-32602`
- 道具（`mcp__dagq-broker__<name>`。`tools/list`の順）と入力のschema（どれも`type: object`・`additionalProperties: false`で、欄はprotocolの要求の型と同じ名前。※は必須）。各道具の説明にworkspaceの相対パスであること（絶対パス・`..`・外へのsymlink・`.git`は拒まれる）と上限を書く:

  | 道具 | brokerのop | 入力 |
  | --- | --- | --- |
  | `read_file` | `fs.read` | `path`※（string）・`offset`（integer、0始まりの行）・`limit`（integer、行数） |
  | `list_dir` | `fs.list` | `path`※（`.`はworkspace） |
  | `write_file` | `fs.write` | `path`※・`content`※（string）・`create_dirs`（boolean、既定false） |
  | `edit_file` | `fs.edit` | `path`※・`old_string`※・`new_string`※・`replace_all`（boolean、既定false）。組み込みのEditと同じ |
  | `exec` | `process.exec` | `argv`※（stringの配列、1つ以上）・`env`（stringの値のobject）・`stdin`（string）・`timeout_secs`（integer） |
  | `git_status` | `git.status` | なし |
  | `git_diff` | `git.diff` | `staged`（boolean）・`paths`（stringの配列） |
  | `git_log` | `git.log` | `limit`（integer、既定20・最大200） |
  | `git_show` | `git.show` | `commit`（string、既定HEAD）・`paths` |
  | `git_add` | `git.add` | `paths`※ |
  | `git_commit` | `git.commit` | `message`※（string） |
  | `git_restore` | `git.restore` | `staged`（boolean）・`paths`※ |

  設計の初めの案の`write_file`（`path`・`content`）に、protocolの`WriteRequest`にある`create_dirs`を足した（新しいdirのファイルを作るため）
- 結果: 成功は`{"content":[{"type":"text","text":<opの応答のJSON>}],"isError":false}`。`exec`は子のexit codeに関わらず成功で、`exit_code`は応答の欄。brokerの拒否・失敗は`isError: true`で、textと`structuredContent`にbrokerのerror本体`{"error":{"code","message","request_id"}}`をそのまま入れる（codeは`unauthorized`・`capability_denied`・`workspace_violation`・`timeout`・`output_limit`・`backend_error`・`invalid_request`）。client側の失敗も`isError: true`で、`{"client_error":{"kind","message"}}`（`kind`は`invalid_arguments`（入力がschemaに合わずbrokerに送らない）・`config`・`transport`・`protocol`）。tokenの値はどこにも出さない
- 切り詰め: 応答の最上位の文字列の欄（`read_file`の`content`、`git_diff`の`diff`、`git_show`の`show`、`exec`の`stdout`・`stderr`）は40000 byte（`TEXT_LIMIT_BYTES`、UTF-8の文字の境で切る）、最上位の配列（`list_dir`と`git_status`の`entries`、`git_log`の`commits`）は1000件（`ITEM_LIMIT`）で切り、切った欄を`mcp_cut`（`{"<欄>":{"kept":N,"total":M}}`、byteか件数）で示す。brokerの上限（`fs_limit_bytes`・`output_limit_bytes`）はそれより前にserverが強制し、`read_file`・`git_diff`・`git_show`の`truncated`はbroker側で切ったことを示す
- server単位の`mcp__dagq-broker`を許す: settingsの`permissions.allow`ではなく、Claude Codeの引数`--allowedTools mcp__dagq-broker`で渡す（settingsの`permissions`は`mcp.json`の有無で変わらず、`disabled`のrunのsettingsは今までと同じ。`mcp.json`のあるrunのsettingsには組み込みの道具を数える`PreToolUse`のhookだけが足される。下の「組み込みの道具の数」）。`preferred`では組み込みの道具を拒まない
- workerのpromptに、brokerの道具があるとき（runのproviderがClaude Codeで`mcp.json`がある）だけ「ファイルの読み書き・置換、許されたコマンド、run branchのgitはbrokerの道具を優先し、拒まれたか届かなければ組み込みの道具を使う」段落（`prompt::BROKER_TOOLS`）を末尾に足す。claimでは道具を渡せたときにpromptを書き直す。tokenの値もtoken fileの場所も書かない
- 人の診断のCLI（task 834）: `dagq-broker-client [--url URL] [--token-file FILE] <command>`。`--url`と`--token-file`が無ければ`DAGQ_BROKER_URL`と`DAGQ_BROKER_TOKEN_FILE`を読む。tokenの値は引数にもenvにも取らず、出力にも出さない。token fileは要求ごとに読み直す
  - URLは`http://<loopbackのaddress>:<port>`だけ（`localhost`は`127.0.0.1`、末尾の`/`は許す）。https・path・user・loopbackでないaddress・port 0は設定の誤りとして送らない（tokenをhostの外へ送らない）
  - command: `health [--json]`（tokenを要らない。既定は`ok build <build> protocol 1`の1行）、`token inspect`（token fileのclaimsのJSONだけ。署名は確かめず、tokenと署名は出さない）、`fs read PATH [--offset N] [--limit N]`、`fs list PATH`、`fs write PATH [--content TEXT] [--create-dirs]`（`--content`が無ければstdin）、`fs edit PATH --old TEXT --new TEXT [--replace-all]`、`exec [--env NAME=VALUE]... [--stdin TEXT] [--timeout-secs N] -- PROGRAM [ARG]...`、`git status`、`git diff [--staged] [PATH]...`、`git log [--limit N]`、`git show [--commit REV] [PATH]...`、`git add PATH...`、`git commit --message TEXT`、`git restore [--staged] PATH...`（addとrestoreはpathが1つ以上）。token fileの中身が空白か制御文字を含めば送らない。push・fetch・remoteのcommandは無い
  - 出力と終了: 成功はopの応答のJSONをstdoutに1行で出してexit 0（`exec`は子のexit codeに関わらず0で、`exit_code`は応答の欄）。要求の本体が8 MiB（`MAX_REQUEST_BYTES`）を超えれば送らずにclient側の失敗にする。brokerの拒否・失敗はbrokerの`{"error":{"code","message","request_id"}}`をstderrに1行で出してexit 1。command lineの誤りはexit 2。client側の失敗（設定・接続・応答のprotocolの版の違いや形の違い）はstderrに理由を出してexit 3
  - 応答の`X-Dagq-Broker-Protocol`が`1`でないか無いときは、本体を解釈せずにprotocolの誤りにする（fail closed）

## audit

- 置き場所: `<queue dir>/broker/audit/<YYYY-MM-DD>.jsonl`（UTCの日付）。brokerが1要求1行で追記する。30日より古いファイルはbrokerの起動時に消す
- auditは記録で、改ざんへの耐性は持たない（上の「既知の制限」と、hostのworkerが同じファイルを書けること）
- 欄（この順。当てはまらない欄は`null`）: `ts`（RFC 3339のUTC、ミリ秒）・`request_id`・`jti`・`run_id`・`task_id`・`actor_id`・`backend`（`fs`・`process`・`git`。healthと未知のpathは`null`）・`op`（`fs.read`など。healthは`health`、未知のpathは`null`）・`capability`・`path`（workspaceからの相対で、workspaceそのものは`.`。要求のpathが1つのときだけ。字句の閉じ込めで拒んだpathは残さない。字句では中にありbackendがsymlinkなどで拒んだものは、その相対パスを残す）・`program`（execの`argv[0]`のbasename）・`argc`・`argv_sha256`（`argv[1..]`の各引数の後にNULを置いたbyte列のSHA-256、小文字の16進）・`result`（`ok`かerror code）・`exit_code`・`duration_ms`・`bytes_in`（要求の本体）・`bytes_out`（応答の本体）
- healthの要求も1行残す（`op: health`）
- `verify`が通らないtoken（書式・署名・claims・期限）の要求は`jti`・`run_id`などをnullにし、`result: unauthorized`だけを残す（claimsを信用しない）。署名の通ったtokenで有効な印だけが無いものは、claimsの`jti`・`run_id`・`task_id`・`actor_id`を残す
- `argv_sha256`は照合用で、推測しやすい引数はhashから総当たりで戻せる
- 残さないもの: token・署名・鍵、ファイルの中身、diff、execのstdout・stderr・stdin、envの名前と値、`argv`の引数、commit message
- 読む: `dagq broker audit [--run ID] [--task ID] [--since T] [--until T] [--limit N]`（task 925。状態を変えない）。queue DBには取り込まず、queueのDBも開かない（権限は`queue.read`）
  - `--since`・`--until`は`events`と同じUTCの`YYYY-MM-DD`（その日の0時）か`YYYY-MM-DDTHH:MM:SS[.fff]Z`。`--since`はその時刻を含み、`--until`は含まない。行の`ts`を時刻として比べる。日のファイルは、名前の日付が`--since`の日から`--until`の直前の時刻の日までのものだけを読む（`--until`がその日の0時ならその日のファイルは読まない。読む間に消えたファイルは無いものとする）（名前が`<YYYY-MM-DD>.jsonl`でないファイルは読まない）。`--run`は`run_id`、`--task`は`task_id`の一致
  - 出力: `{"entries": [...], "skipped": N, "dropped": M}`。`entries`は合う行を古い順に並べたJSONの配列で、各要素はbrokerが書いた行のJSONのobjectそのまま（欄を足さず削らない。objectの欄の順はJSONの表現で変わりうる）。`skipped`は読んだ日のファイルのうち、JSONのobjectとして読めない行か`ts`が時刻として読めない行（壊れた行と、書きかけで途中で切れた末尾の行）の数で、飛ばして続ける（空行は数えない）。`--limit`（既定1000、`DEFAULT_AUDIT_LIMIT`）は合う行のうち新しいN行を残し、`dropped`は残さなかった古い行の数
  - auditのdirが無ければ`{"entries": [], "skipped": 0, "dropped": 0}`

## 組み込みの道具の数

goal 59の(2)、task 839。`preferred`のあいだ、workerがbrokerを通さずに組み込みの道具で行った操作をrunごとに数え、どこがbrokerに移っていないかを見えるようにする（`src/domain/broker_usage.rs`）。

- 数える条件: brokerの道具を渡したrun（`<run dir>/broker/mcp.json`があり、executorが`--mcp-config`を渡すもの）だけ。`disabled`と、`broker_unavailable`で道具なしのrunはhookを持たず、settingsは今までと同じ（印の無い`disabled`のqueueで何も変わらないことは上の「token」のとおり）。`required`で拒まれた組み込みの道具の試みも、Claude Codeが`PreToolUse`のhookを呼ぶものは同じく数える
- hook: workerのsession・resume・非対話のturnのsettings（`claude-settings.json`・`claude-headless-settings.json`）に、`DIRECT_TOOLS`（`Bash`・`Edit`・`Glob`・`Grep`・`LS`・`MultiEdit`・`NotebookEdit`・`Read`・`Write`）の1つずつに名前の完全一致の`matcher`を持つ`PreToolUse`のhookを足す（`infrastructure::adapters::with_direct_tool_hooks`）。hookは入力を読み捨て、道具の名前だけを`<run dir>/broker-direct-tools.log`（`DIRECT_TOOLS_LOG`。tokenの失効が消す`<run dir>/broker/`の外）に1行追記し、何も出さず必ずexit 0で終わる（exit 2は道具を止めるため）。書けなければ数が欠けるだけ。plannerのturnとreviewには足さない
- 残さないもの: 道具の入力（path・ファイルの中身・コマンドの文字列）。制御側の`Bash`（`dagq ask`・receiptの書き込み）も`Bash`として数え、中身で分けない
- 記録: runが終わってsupervisorがそのtokenを失効させるsweep（上の「token」の失効。`reason`がrunのstatusのもの）で、失効の前に数え（`RunTokens::usage`）、1つ以上の印を失効させたときだけrunのevent `broker_tool_use`を1回残す（印が残ったまま失効が失敗すれば次のpassで数え直す。印を消した後のファイルの削除で失敗したときは、次のpassがそのrunを見ないので、そのrunの印が無ければその場で残す。判定は`domain::broker_usage::records_tool_use`、記録は`Supervisor::broker_record_usage`）。数が読めなければwarnだけで、sweepは止めない。workerが書けるhookのlogは、run dirのほかのfileと同じくlinkを辿らずに開いた記述子で、通常のfileだけを上限（`agent_dir::FILE_BYTES`）付きで読み、FIFOのopenで待たない（[非対話のworker](supervisor-lifecycle/headless-worker.md)の「run dirの`turns/`」、task 1184）。linkやFIFOに置き換えられていれば読めないものとしてwarnになり、そのrunの`broker_tool_use`は残らない。`disabled`のsweep（`mode_disabled`）と発行し直し（`reissued`・`renewed`）は残さない
- payload: `brokered`（auditのうちそのrunの`run_id`の行で、`op`があり`health`でないものの数。拒まれた要求も数える）・`brokered_by_op`（`op`ごと）・`direct`（hookの行のうち`DIRECT_TOOLS`の名前のもの。ほかの行はworkerが書けるので数えない）・`direct_by_tool`（道具ごと）。auditはrunの`created_at`の日（UTC）からの日のファイルを読む（`broker_admin::audit`）
- 出口: `show`（既定の圧縮形）が最新runの`runs[0].broker_tool_use`に、そのrunの最新の`broker_tool_use`のpayloadをそのまま載せる（`view::task_detail`、`domain::broker_usage::latest_tool_use`）。`--full`と`events`はeventとして出す。`status`と`doctor`には足さない（backend・enforcementはtask 738、brokerの`mode`と`health`は下の「status と doctor」）
- test: `domain::broker_usage`のunit test（数え方と、失効と一緒に1回だけ残す判定）、`infrastructure::broker_token`の`usage_reads_the_log_only_as_a_regular_file_and_never_waits`（logのlink・`/dev/zero`へのlink・FIFOを待たずに拒む）、`infrastructure::adapters`の`only_a_worker_with_the_brokers_tools_counts_its_built_in_tools`（`mcp.json`の有無とrole、hookの実行）、`view`の`task_detail_shows_the_latest_runs_brokered_and_direct_counts`、`tests/it/runtime_broker.rs`の`a_run_that_fails_or_is_interrupted_loses_its_token`（終わったrunの`broker_tool_use`と`show`）と`a_disabled_supervisor_revokes_the_tokens_an_earlier_mode_left`（`disabled`は数えない）

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
fs_limit_bytes = 4194304       # serveの --fs-limit-bytes
```

`host.toml`（`<queue dir>/host.toml`か`$XDG_CONFIG_HOME/dagq/host.toml`。hostの事情）:

```toml
[broker]
mode = "disabled"             # この host では mode を disabled に落とす。書けるのは "disabled" だけ（上げることはできない）
podman = "/opt/homebrew/bin/podman"   # 省略時は PATH
machine_cpus = 1
machine_memory_mib = 1024
machine_disk_gib = 10
container_memory = "512m"
container_cpus = "1"
container_pids = 256
port = 0                      # 0 は空いている port
```

- `[broker]`が無ければ`disabled`。`required`はerrorにし、supervisorを起動せず`up`も止まる（黙って`preferred`に落とさない。`BrokerMode::unsupported`）
- `dagq.toml`の`[broker]`は他の表と同じく未知のkey・重複・型の違う値・`exec_timeout_secs`が`exec_max_timeout_secs`より大きいことを行番号付きのerrorにする（`parse_config`、`load_broker_config`）。`exec_*`・`output_limit_bytes`・`fs_limit_bytes`は`serve`の既定と違うものだけがcontainerの`serve`の引数になる（`BrokerConfig::serve_args`。変えればcontainerの引数のhashが変わり、作り直される）
- `host.toml`の`[broker]`は`[update]`と同じく、queueの`<queue dir>/host.toml`にあればそれが丸ごと勝ち、無ければhost全体の`$XDG_CONFIG_HOME/dagq/host.toml`（`load_host_broker`）。`mode`に`"disabled"`以外（`"preferred"`・`"required"`）を書いても上げず、読めない値と同じく警告にして既定のままにする（`dagq.toml`のmodeが効く）。警告はsupervisorのlogと`up`の出力の`broker.warnings`に出る。`port`が0か無ければ、前に使ったport、無ければ空いているport
- `dagq.toml`の`[broker]`は[Run environment](supervisor-lifecycle/run-environment.md)の読み手（`parse_config`）が表として受け付ける（task 923）。`[broker]`を知らない固定バイナリは未知の表で止まるので、この repositoryの`dagq.toml`には置かない（本番queueはdisabled）。検証は使い捨てのrepositoryで行う

## status と doctor

- `actors`（task 738）の`backend: host`・`enforcement: advisory`・`sandboxed: false`は変えない（brokerを使うworkerも隔離されていない。ADR-t827-4決定5）
- `status`（roleなしと`--role inbox`）の別の欄`broker`（task 843、837）: `mode`（bindしたcheckoutの`dagq.toml`の`[broker]`を`host.toml`で落としたもの。読めなければ`{"error"}`）・`health`（`{state, reason, at}`。queueの最新の`broker_started` / `broker_healthy` / `broker_unhealthy` / `broker_stopped`から、`healthy`・`unhealthy`（`reason`はその`reason`）・`stopped`、何も無ければ`unknown`。`domain::broker::health_report`）・`active_tokens`（有効な印の数）・`state`と`port`（`state.json`）・`build`（dagqのbuild識別子）・`image`（そのbuildのimage）・`running_image`（`state.json`の、最後に動かしたimage）・`image_matches`・`client`（`path`・`build`・`matches`・`error`。`application::broker::client_report`）。podmanのコマンドもbrokerへの要求も打たない（`health`は記録を読むだけ）。`machine`は後のtaskが足す
- `doctor`の`broker`（task 836、843、837）: `mode`・`health`・`active_tokens`（`status`と同じ。schemaが読めないqueueでは`null`）・`podman`（解決したpath、無ければ`null`と`error`の`podman_missing`）・`machine`（`dagq`）・`build`・`image`・`client`（`status`と同じ）・`recorded`（`state.json`）。podmanのコマンドは打たない（machine・image・healthは`dagq broker status`が見る）。

## capabilityの関係

- brokerのcapability（`fs.read`・`fs.write`・`process.exec`・`git.read`・`git.write`）はprotocolの`BrokerCapability`で、queueの操作の`Capability`（[Authorization](authorization.md#capability)）とは別の名前空間
- `reserved.filesystem_read`・`reserved.filesystem_write`・`reserved.network`・`reserved.secret_read`は誰にも与えないまま（ADR-t827-4決定5）
- tokenを発行できるのは信頼する制御側のsupervisorだけ。AI actorにtokenを作るコマンドは無い

## 段階

- Phase 1（goal 58）: この文書。workerはhostのまま、`preferred`で契約を証明する
- Phase 2: `required`の強制（組み込みの道具を拒み、brokerが使えなければclaimしない）
- Phase 3以降: workerのcontainer化（`PodmanActorExecutor`）、runごとのmountでの閉じ込め、containerのworkerのqueueの操作（goal 38か後のgoal）

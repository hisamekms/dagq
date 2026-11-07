---
id: development-plugin-distribution
type: development
title: runtimeとpluginのインストールと開発時の確認
status: current
created: 2026-11-08
---

# runtimeとpluginのインストールと開発時の確認

配布の責務とコードへの入口は[plugin integration](../design/plugin-integration.md)にある。
このrepositoryの本番バイナリの更新は[operations](operations.md)に従い、下の手動インストールで置き換えない。

## Release archiveからのインストール

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

## crates.ioからのインストール

利用者は`cargo install --locked dagq`でsourceからbuildする（RustとCコンパイラが要る。対応はmacOS Apple Siliconだけで変わらない）。
入る場所は`~/.cargo/bin/dagq`なので、`~/.local/bin/dagq`と併用せずPATH上の`dagq`を1つにする。
更新は同じ`cargo install --locked dagq`で上書きしてから`dagq up`（version違いのsupervisorを入れ替える）。

## marketplaceからのインストール

repository rootの`.claude-plugin/marketplace.json`がこのrepository自身をmarketplace（名前は`dagq`）にする。
ユーザーの導線は2行。

```sh
claude plugin marketplace add hisamekms/dagq
claude plugin install claude-dagq@dagq
```

`add`はGitHubのrepositoryをcloneし、`install`はそのcloneの`./plugins/claude-dagq`からuser scopeに入れる。
更新は`claude plugin marketplace update dagq`と`claude plugin update claude-dagq@dagq`。
pluginはskillと`SessionStart` / `Stop` / `SessionEnd` hookだけでinstallに`-y`を要する宣言commandはなく、runtimeバイナリは同梱しない（`cargo install --locked dagq`で別に入れる。launcherのエラー文がその手順を持つ）。

## 開発時の読み込みと検証

人かinboxが実CLIで確かめる手順で、workerは実queueも使い捨てqueueも操作しない（[運用](operations.md#workerがhostと実queueでできないこと)）。

- 検証: `claude plugin validate plugins/claude-dagq`（hookとskillを含む）と`claude plugin validate .claude-plugin/marketplace.json`（`--strict`も通る）、inventory: `claude --plugin-dir plugins/claude-dagq plugin details claude-dagq`。
- 開発中の読み込み: `claude --plugin-dir /path/to/dagq/plugins/claude-dagq`（そのsessionのみ）。`up --plugin-dir`も開発用checkoutを指定する入口で、workerはpluginを読まない。
- marketplace経由のinstallは、使い捨ての`HOME` / `CLAUDE_CONFIG_DIR`でローカルpathを`marketplace add`して`install`し、`plugin list`と`plugin details`でskillが載ることを確認する。

## リリース

versionの確定からtagのpushと公開の確認までは[release skill](../../.claude/skills/release/SKILL.md)に従う。
Trusted Publisherは既存のcrateにしか登録できないので、最初の1回はユーザーが手で`cargo publish`し、crates.ioでTrusted Publisherを登録する（[ADR-0030](../adr/0030-publish-to-crates-io-on-tag-push-with-trusted-publishing.md)決定5）。
packageは`Cargo.toml`の`include`でbuildに要るものに絞り、`cargo publish --dry-run --locked`で確かめる。

---
name: release
description: dagq 自身のリリース手順。version を上げる、tag v* を切る、GitHub Release と crates.io に出す、release.yml の成功と crates.io の自動 publish（Trusted Publishing）を確認するときに使う。利用者向けではなく、この repository を開発する session 用。
---

# dagq をリリースする

根拠は [README の Release 節](../../../README.md#release) と [ADR-0030](../../../docs/adr/0030-publish-to-crates-io-on-tag-push-with-trusted-publishing.md)。tag `vX.Y.Z` の push で `.github/workflows/release.yml` が、tag と `Cargo.toml` の version の一致を検査し、`aarch64-apple-darwin` の `dagq` と `dagq-broker-client` のバイナリを build して GitHub Release に添付し、workspace の 4 つの crate を同じ version で `dagq-broker-protocol` → `dagq` → `dagq-broker-client` → `dagq-broker` の順に crates.io に publish する（crate ごとに、crates.io に既にあれば skip。[ADR-t827-1](../../../docs/adr/2026-09-28-t827-1-broker-crates-binaries-and-version-alignment.md) 決定 8）。4 つの crate の version は `Cargo.toml` と `crates/*/Cargo.toml` の `[package]` の `version` と、crate 間の依存の `version = "=X.Y.Z"` をそろえ、`scripts/check-plugin-version.sh` が一致を検査する。

main の version はリリースの直後に次の開発版 `X.Y.Z-dev` へ上げてあり（今は `0.4.0-dev`）、main のビルドは `dagq --version` で build 識別子 `X.Y.Z-dev+<commit>`（worktree が dirty なら `.dirty` が付く）を名乗る。リリースは `-dev` を外した `X.Y.Z` で、build 識別子も `X.Y.Z` だけになる（[ADR-0073](../../../docs/adr/0073-kind-additions-are-compatible.md) 決定 1・2。ADR-0045 を置き換えた。`build.rs` が埋め込む）。

以下 `X.Y.Z` は新しい version。1・3・4・5 のコマンドは repository の main checkout で打つ（読むだけで、main の作業ファイルは変えない）。2 と 6 の変更は task の worker が worktree で行う。tag の作成と push はユーザーの確認を取ってから行う。

## 1. 前提の確認

main checkout が clean で `origin/main` と同じ HEAD にあり、最新の main の CI が success で、`cargo publish --dry-run --locked -p dagq` が通ることを確かめる。コマンドと読み方（CI が success でなければリリースしない）は [reference/version.md](reference/version.md) の「1. 前提の確認」。

## 2. `-dev` を外して version を確定する

`Cargo.toml` と `crates/*/Cargo.toml` の `[package]` の `version`、`crates/*/Cargo.toml` の `dagq-broker-protocol` への依存の `version = "=X.Y.Z"`、`plugins/claude-dagq/.claude-plugin/plugin.json` の `"version"` を同じ `X.Y.Z` にし（main の `X.Y.Z-dev` から `-dev` を外す。上げる桁を変えるなら、ここで `-dev` の数字と違う `X.Y.Z` にしてよい）、`Cargo.lock` を更新する。tag は `-dev` の無い commit に打つ（`release.yml` は tag と `Cargo.toml` の version の一致を検査するので、`-dev` が残っていれば落ちる）。

- main に直接 commit しない（AGENTS.md）。この変更も dagq の task として登録し、`integrate` で main に着地させる
- worker のコマンド、登録の例、semver の目安（上げる桁の決め方）は [reference/version.md](reference/version.md) の「2. `-dev` を外して version を確定する」

## 3. tag を切る（ユーザーの確認を取ってから）

「`vX.Y.Z` を main の `<HEAD の短い SHA>` に打って push してよいか」をユーザーに聞き、了承を得てから:

```sh
git tag vX.Y.Z origin/main
git push origin vX.Y.Z
```

一度 publish した version は crates.io から消せない（yank だけ）。tag を打ち直すことになっても crates.io の version は上書きできないので、確認は push の前に行う。

## 4. 確認

`release.yml` の run の成功、GitHub Release の asset、crates.io の各 crate の `max_version` を確かめる。コマンドと、broker の crate の最初の publish（tag の push の前に人が行うこと）は [reference/publish.md](reference/publish.md) の「4. 確認」。

## 5. 初回の自動 publish の確認（Trusted Publishing に切り替えて最初のリリースだけ）

Authenticate と Publish の step の成功と、rerun で publish が skip されることを確かめ、planner に伝える。2 回目以降のリリースでは 5 は不要。手順は [reference/publish.md](reference/publish.md) の「5. 初回の自動 publish の確認」。

### 失敗したときの見どころ

tag と version の不一致・Trusted Publisher の不一致・id-token 権限・crates.io の API の失敗の見分け方と直し方は [reference/publish.md](reference/publish.md) の「失敗したときの見どころ」。

## 6. 次の開発版へ上げる

tag の run が成功したら（4 の確認の後）、main の version を次の開発版 `X.Y.Z-dev` に上げる task を登録する。ここでの `X.Y.Z` は次のリリースの見込みで、普段は minor を 1 つ上げる（`0.4.0` の後は `0.5.0-dev`）。互換を保つ修正だけのリリースが続くと分かっていれば patch でもよく、次のリリースの 2 で改めて決め直せる。2 と同じく `Cargo.toml`・`plugin.json`・`Cargo.lock` の 3 つを変える。登録の例と忘れたときに起きることは [reference/after-release.md](reference/after-release.md) の「6. 次の開発版へ上げる」。

## 7. 固定バイナリの更新

この skill の中では `~/.local/bin/dagq` を `cp` で置き換えない（macOS では走行中のプロセスが kill されうるうえ、前のバイナリが残らない）。更新は [ADR-0073](../../../docs/adr/0073-kind-additions-are-compatible.md) の `dagq install` と自動更新で行い、手順は AGENTS.md の「本番 queue と開発環境の境界」と [operations.md](../../../docs/development/operations.md) の「`up`のコマンド」と、plugin の `dagq-recover` skill の `reference/update.md` に従う。自動更新の有無と非互換の migration ごとの確かめ方は [reference/after-release.md](reference/after-release.md) の「7. 固定バイナリの更新」。

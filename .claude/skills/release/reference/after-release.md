# 次の開発版と固定バイナリの更新の詳細

[SKILL.md](../SKILL.md) の 6 の登録の例と、7 の更新の経路ごとの確かめ方。

## 6. 次の開発版へ上げる

```sh
dagq add 'release: version を次の開発版 X.Y.Z-dev に上げる' \
  --description 'vPREV のリリース後、Cargo.toml と plugins/claude-dagq/.claude-plugin/plugin.json の version を X.Y.Z-dev にし、Cargo.lock を更新する。.claude-plugin/marketplace.json の ref は vPREV のまま変えない' \
  --acceptance 'Cargo.toml・plugin.json・Cargo.lock の version が X.Y.Z-dev で、marketplace.json が変わらず、main のビルドの dagq --version が X.Y.Z-dev+<commit> を出す' \
  --paths Cargo.toml --paths Cargo.lock --paths 'plugins/claude-dagq/.claude-plugin/plugin.json' \
  --verify 'sh scripts/check-plugin-version.sh' \
  --verify 'cargo publish --dry-run --locked' --verify 'cargo test --locked --test plugin'
```

`--paths` に `.claude-plugin/marketplace.json` を含めない。marketplace の `ref` は次のリリースの変更（SKILL.md の 2）でだけ進め、`-dev` の間はリリースした `vPREV` の plugin を配る（ADR-t617-1 決定 2・4）。

これを忘れると、main のビルドがリリースと同じ `X.Y.Z` を名乗り、build 識別子に commit が入らないので、`up`・`dagq install`・自動更新がリリースのバイナリと開発中のビルドを見分けられない。

## 7. 固定バイナリの更新

- 本番の supervisor が `up --auto-update` で動いていれば、2 の `-dev` を外す commit と 6 の次の開発版へ上げる commit はどちらも `Cargo.toml` と `Cargo.lock` を変えるので、それぞれの着地で supervisor が main をビルドし、固定バイナリと自分を引き継ぎで入れ替える（走っている run は止まらない）。着地の後に inbox の `report the update` か `dagq status` の `auto_update` と `supervisors[].binary_version` で、6 の着地の後に `X.Y.Z-dev+<commit>` になったことを確かめる
- 自動更新が無効なら、6 の着地の後に inbox か planner の session から、ユーザーに報告してから `dagq install` を打つ（main checkout を build し、確認・差し替え・supervisor の引き継ぎまで行う）。リリースの間に非互換（`-- dagq-schema: breaking`）の migration が入っていて `install` が止まったら、開いた ask を人に見せ、ユーザーの了承を得てから `dagq install --allow-breaking`（drain して DB を退避し、migrate して起動し直す）を打つ
- 自動更新が有効でも、非互換の migration を含むビルドは入れ替えられず、inbox に `approve_update` の ask が出る。人が `install` と答えたら、問いに書かれた `install --allow-breaking` のコマンドを打つ。drain が起動し直す supervisor には止めた登録の `--auto-update` と `--max-waiting` が引き継がれるので、`up` の打ち直しは要らない

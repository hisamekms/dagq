---
id: design-supervisor-lifecycle-build-identifier
type: design
title: "Build identifier"
status: current
created: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
  - adr-0045
  - design-plugin-integration
---

# Build identifier

バイナリは自分をbuild識別子で名乗る（[ADR-0045](../../adr/0045-build-identifier-explicit-migrate-schema-compat-handoff-and-auto-update.md)の決定1〜3）。
`build.rs`がビルド時に埋め込み、バイナリが自分の版を出す所（`--version`、supervisorの登録の`binary_version`、`up`の結果、supervise logなど）はすべてこれを出す。
規則は`src/build_id.rs`（`dagq::build_id`）の1か所で、dagqの`build.rs`は同じファイルを`#[path]`で読む。
リリース・開発版・`.dirty`・`+unknown`の形と、`build.rs`を再実行させるsourceは、そのファイルのdoc commentとunit testが持つ。

- pre-releaseでは再実行のきっかけをgitの問い合わせより前に登録するので、`unknown`に落ちたビルドもsourceの変更で問い直す。
- gitの見張りはworktreeの`HEAD`・`index`・`packed-refs`と今のbranchのrefで、`refs`全体は全worktreeで共有なので見ない（他のworktreeのcommitでこのworktreeのビルドを作り直さないため）。
- commitとcheckoutとstageで識別子は追従するが、それ以外（docsなど）だけを変えたworktreeは、次に何かが`build.rs`を再実行させるまで`.dirty`にならない。
- `cargo package` / `cargo publish --dry-run`の検証は`target/package/`に展開した写しを元のcheckoutの`target/`でビルドし、cargoはこれを同じunitとみなすので、写しの`unknown`の出力がcheckoutの次のビルドに流用されうる。
  `unknown`のときは写しのrepository rootの`.git`（存在しない）を見張りに加え、存在しないpathは常に再実行させるので、checkoutの次のビルドはcommitを問い直す。

識別子が名乗るcommitを読むのは`build_id::named_commit`の1か所で、自動更新の比べる元（[Auto-update](auto-update.md)の`base`）と、依存先の着地を含むbuildを待つtaskの判定（[claimを控える](claim-defer.md#依存先の着地を含むbuildを待つtask)、ADR-t1632-1）が使う。

`up`は識別子の文字列の全体で比べるので、package versionが同じでもcommitかdirtyが違うsupervisorは入れ替えの対象になる。
pluginのlauncherがバイナリとの互換を見るのはmajor.minorだけ（[plugin-integration](../plugin-integration.md)）。
mainのversionの上げ方（tagで`-dev`を外し、リリース後に次の開発版へ上げる）はrelease skill（`.claude/skills/release`）が持つ。

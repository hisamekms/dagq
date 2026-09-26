---
id: adr-t617-1
type: adr
title: pluginはこのrepositoryのmarketplaceからClaude Codeの公式の手順で配り、marketplaceのentryを最新のリリースのtagに固定して、pluginのversionをバイナリのリリースと合わせる
status: accepted
created: 2026-09-27
updated: 2026-09-27
accepted_on: 2026-09-27
owners:
  - hisamekms
tags:
  - distribution
  - plugin
  - release
related:
  - adr-0030
  - adr-0052
  - adr-t598-1
  - adr-t614-1
  - adr-t617-2
  - design-plugin-integration
---

# ADR-t617-1: pluginはこのrepositoryのmarketplaceからClaude Codeの公式の手順で配り、marketplaceのentryを最新のリリースのtagに固定して、pluginのversionをバイナリのリリースと合わせる

## Context

`.claude-plugin/marketplace.json`のentryは`source: "./plugins/claude-dagq"`（repository相対）で、refの指定が無い。`claude plugin marketplace add hisamekms/dagq`はdefault branchをcloneするので、入るpluginはmainのHEAD（今は`0.4.0-dev`）になり、公開されたバイナリ（GitHub Releaseは`v0.2.0`、crates.ioは`0.3.0`）と合わない。launcherはmajor.minorの違いを警告するだけである（goal 52の(7)）。

人は2026-09-27に、pluginをClaude Codeの公式の手順で配布できるようにし、バイナリは`cargo install dagq`を基本にして新しいversionは準備ができたら人が出すと決めた。

Claude Codeの公式の文書（2026-09-27に読んだ）の要点:

- 配布の経路は3つ: marketplaceを使わない共有、自分のmarketplace（`.claude-plugin/marketplace.json`を置いたgitのrepository。提出の手続きは無い）、Anthropicのdirectory（claude.aiの開発者portalから提出。有料のplanが要り、claude.aiとCoworkで配られ、Claude Codeには`<name>@synced`として同期される）。公式のmarketplace`claude-plugins-official`はportalからの提出を受け付けず、Anthropicのpartnerの窓口経由だけ（[Publish and distribute a plugin](https://code.claude.com/docs/en/plugins/publish)）。
- pluginのversionは、pluginの`plugin.json`の`version`、marketplaceのentryの`version`、sourceから決まる値（gitならcommit SHA）の順で決まり、更新はこのversionが変わったときだけ入る。cacheは`~/.claude/plugins/cache/<marketplace>/<plugin>/<version>/`で、`${CLAUDE_PLUGIN_ROOT}`はここを指す（[Plugin loading reference](https://code.claude.com/docs/en/plugins/loading)）。
- 利用者をあるversionに留めるには、entryの`github`・`url`・`git-subdir`のsourceに`ref`（branchかtag）・`sha`を書くか、利用者が`marketplace add owner/repo#<ref>`で固定する（[Host and maintain a marketplace](https://code.claude.com/docs/en/plugins/host-marketplace)、[Marketplace reference](https://code.claude.com/docs/en/plugins/marketplace-reference)）。
- 第三者のmarketplaceの自動更新は既定でoffで、利用者は`claude plugin update <plugin>@<marketplace>`（marketplaceを読み直して、versionが変わっていれば入れ替える）か`claude plugin marketplace update <name>`で更新する（[Install and manage plugins](https://code.claude.com/docs/en/discover-plugins)）。

## Decision

1. **配布の経路は、このrepositoryを自分のmarketplaceにする公式の手順にする。** 利用者は`claude plugin marketplace add hisamekms/dagq`と`claude plugin install claude-dagq@dagq`で入れ（marketplaceの名前・pluginの名前はADR-0052のまま変えない）、`claude plugin update claude-dagq@dagq`で更新する。Anthropicのdirectoryと公式のmarketplaceには今は出さない。directoryはclaude.aiとCoworkに配るもので、dagqのpluginはhostの`dagq`バイナリとcmuxが無いと動かず、公式のmarketplaceは提出を受け付けていない。出すと決めるときは新しいADRにする。
2. **marketplaceのentryは、最新のリリースのtagの`plugins/claude-dagq`を指す。** entryのsourceをrepository相対のpathから、このrepositoryの`plugins/claude-dagq`を`ref`に最新のリリースのtag（`v<X.Y.Z>`）を書いて指すsourceに変える。利用者はrefを付けずに`marketplace add`し、mainを読むmarketplaceから、リリース済みのpluginを入れる。mainのHEADのpluginは配らない。
3. **pluginのversionはリリースのtagの`plugin.json`の`version`で、バイナリと同じ`X.Y.Z`にする。** リリースのtagでは`Cargo.toml`と`plugin.json`のversionが一致していることを、リリースのworkflowが検査して、違えばリリースしない。entryには`version`を書かない（`plugin.json`が先に効き、2か所に書くと食い違いうるため）。
4. **entryの`ref`は、リリースの変更（`-dev`を外してversionを`X.Y.Z`にする変更）で、これから打つtag`vX.Y.Z`に書き換える。** その変更をmainに着地させたらすぐにtagをpushする。こうするとtagの中のmarketplaceも自分のtagを指し、crates.ioへのpublish（tagのpushで走る）とmarketplaceのrefの前進がtagのpushの1回にそろう。着地からtagのpushまでの間は、mainのmarketplaceがまだ無いtagを指してinstallと更新が失敗するが、利用者の手元のpluginは壊れず、tagのpushで解消する。refが進んだmarketplaceを利用者が読むと、versionが変わるので`claude plugin update`が新しいpluginを入れる。
5. **利用者はバイナリとpluginを一緒に更新する。** バイナリは`cargo install --locked dagq`、pluginは`claude plugin update claude-dagq@dagq`で、どちらも同じリリースのversionになる。片方だけ古いときの手当てはlauncherのmajor.minorの警告のまま残す。外部のprojectの更新の仕組み（新しいリリースの検知とaskでの入れ替え）は別のADRが決め、その入れ替えにpluginの更新も含める。
6. **切り替えは次のリリースで行う。** 今あるtagは`v0.2.0`だけで（crates.ioの`0.3.0`にはtagが無い）、固定してよい今のリリースのtagが無い。entryを書き換えるのは次のリリース（`v0.4.0`）の変更で、それまではmainのHEADのpluginを配る今の形のままにする。

欄名・sourceの形・検査のコマンドは[plugin integration](../design/plugin-integration.md)が持つ。

## Alternatives

- **repository相対のpathのまま（公式の文書が同じrepositoryのpluginに勧める形）**: 相対のpathはmarketplaceをcloneしたrefで解決され、entryごとにrefを指定できない。mainを読むmarketplaceからリリースのpluginを配るには、同じrepositoryを`git-subdir`で指してrefを付けるしかない。
- **今のままmainのHEADを配る**: pluginがリリースされていないCLIを前提にし、`cargo install`したバイナリと合わない。mainの`-dev`のversionは次のリリースまで変わらないので、利用者は古いHEADのcacheに留まりもする。
- **利用者が`marketplace add hisamekms/dagq#v<X.Y.Z>`で固定する**: 次のリリースへ進むにはmarketplaceのsourceを変える必要があり、`marketplace remove`はそのmarketplaceのpluginをuninstallする。`plugin update`の1行で上がらない。
- **リリースのたびにworkflowが動かす`stable` branchをmarketplaceにする**: workflowにbranchへのpushの権限が要り、動く部品が増える。tagをentryに書けば同じことがmainの1ファイルで済む。
- **`version`を書かずcommit SHAで追わせる**: mainの全commitが更新になり、バイナリのリリースと合わない。

## Consequences

- `cargo install dagq`と公式の手順で入れたpluginが同じリリースのversionになる（goal 52の目標の(5)の前半）。
- リリースの手順に、tagでのversionの一致の検査と、リリースの変更でのentryの`ref`の更新が加わる。忘れるとmarketplaceは前のリリースのpluginを配り続ける（壊れはしない）ので、tagのmarketplaceのrefが自分のtagを指すこともリリースのworkflowが検査する。
- mainの`plugins/claude-dagq`の変更は、次のリリースまで利用者に届かない。dagqの開発はcheckoutのpluginを使う（ADR-t617-2）。
- 実装は別のtaskで行う。実装が入るまでは、marketplaceはmainのHEADのpluginを配る。

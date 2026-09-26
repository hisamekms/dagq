---
id: adr-t617-2
type: adr
title: runtimeが開くsessionはinstallしたpluginを使い、upとplanに--plugin-dirを付けるのはpluginを開発するとき（dagqのrepository）だけにする
status: accepted
created: 2026-09-27
updated: 2026-09-27
accepted_on: 2026-09-27
owners:
  - hisamekms
tags:
  - distribution
  - plugin
  - operations
related:
  - adr-0052
  - adr-t614-1
  - adr-t617-1
  - design-plugin-integration
  - design-supervisor-lifecycle-up-down
  - design-supervisor-lifecycle-session-prompts
---

# ADR-t617-2: runtimeが開くsessionはinstallしたpluginを使い、upとplanに--plugin-dirを付けるのはpluginを開発するとき（dagqのrepository）だけにする

## Context

`up`・`plan`の`--plugin-dir`は任意で、付けたときだけinboxとplannerの`claude`に`--plugin-dir`を渡す。付けなければ`claude`は利用者がinstallしたpluginを読む。ところがREADMEは`up`・`plan`に`--plugin-dir /path/to/dagq/plugins/claude-dagq`を付けさせ、`dagq-recover` skillは`up --plugin-dir "$CLAUDE_PLUGIN_ROOT"`を打たせる（goal 52の(7)）。

Claude Codeの公式の文書（[Plugin loading reference](https://code.claude.com/docs/en/plugins/loading)）によれば、marketplaceからinstallしたpluginの`${CLAUDE_PLUGIN_ROOT}`はversionごとのcache（`~/.claude/plugins/cache/<marketplace>/<plugin>/<version>/`）を指し、更新すると前のversionのdirectoryは印を付けられて14日後に消える。`--plugin-dir`で読んだpluginは同じ名前のinstall済みのpluginより優先される。

そのため、installしたpluginのsessionが`--plugin-dir "$CLAUDE_PLUGIN_ROOT"`で`up`を打つと、古いversionのcacheのpathがsupervisorの登録とinboxのworkspaceのコマンドに残り、pluginを更新しても常駐のsessionは古いpluginを読み続け、cacheが消えるとpluginなしで起動する。

## Decision

1. **runtimeが開くsession（inbox、planner、runtimeが立てるplanner）は、既定で利用者がinstallしたpluginを使う。** `up`・`plan`・`install`（引き継ぐsupervisorに渡す）は`--plugin-dir`が無ければ`claude`に何も足さず、runtimeが立てるplannerはsupervisorに渡された`--plugin-dir`だけを使う（今の振る舞いのまま）。利用者の導入の手順（README）とpluginのskillは、`up`・`plan`に`--plugin-dir`を付けさせない。
2. **skillは`$CLAUDE_PLUGIN_ROOT`を`--plugin-dir`に渡さない。** installしたpluginでは版ごとのcacheを指し、更新で古くなるからである。skillが`up`・`plan`を打つとき`--plugin-dir`を付けるのは、repositoryの指示（AGENTS.mdなど）がそのpathを指定しているときだけで、そのpathを使う。
3. **pluginを開発するrepositoryでは、`--plugin-dir`でcheckoutのpluginを使う。** dagqの開発（AGENTS.md）は今までどおり`up`・`plan`に`--plugin-dir <このrepository>/plugins/claude-dagq`を付け、未リリースのpluginを常駐のsessionで使う。`--plugin-dir`のpluginは同じ名前のinstall済みのpluginより優先されるので、開発者が公式の手順でもinstallしていても衝突しない。これは[ADR-t614-1](2026-09-27-t614-1-dagq-source-only-features-by-one-check.md)の「dagqのソースか」の判定で自動にはしない。`--plugin-dir`はどのpluginの開発にも使える明示の指定で、dagqの開発の運用を変えないためである。
4. **pluginが無いまま常駐のsessionを開かない。** `--plugin-dir`が無く、`claude`から`claude-dagq`がinstallされて有効になっていることを確かめられないとき、`up`と`plan`はsessionを開く前に止め、公式の手順のinstallのコマンドを案内する。pluginの無いinboxとplannerは、promptが名指すskillを持たず役目を果たせないからである。確かめ方と文言は実装のtaskが決めてdesignに書く。

## Alternatives

- **skillが`$CLAUDE_PLUGIN_ROOT`を渡し続ける**: 上のとおり、更新で古いcacheを指し続け、14日後に消える。
- **dagqのソースのrepositoryなら`up`・`plan`が自動でcheckoutのpluginを使う**: dagqの開発の運用が暗黙に変わり、`--plugin-dir`を付けない`up`で何が読まれるかが場所で変わる。開発の指定はAGENTS.mdの明示のコマンドのままにする。
- **pluginが無くても開いて警告だけにする**: 開いたinboxは何もできず、人は理由を追う羽目になる。

## Consequences

- 外部のprojectは`cargo install dagq`と公式の手順のpluginだけで`up`・`plan`を打て、pluginの更新が常駐のsessionの次の起動から効く。
- dagqの開発の運用（AGENTS.mdの`up`・`plan`のコマンド）は変わらない。
- 実装（skill・README・`up`と`plan`のpluginの確認）は別のtaskで行う。

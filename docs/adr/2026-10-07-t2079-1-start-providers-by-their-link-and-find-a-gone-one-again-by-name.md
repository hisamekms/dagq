---
id: adr-t2079-1
type: adr
title: runtimeはprovider（Claude Code・Codex）を起動するpathをsymlinkのまま持ち、実体に固定しない。起動の時点や入口でpathが無ければ名前でPATHから解決し直す（ADR-t813-3決定6のうちCodexを実体に解決して固定する部分をamends）
status: accepted
created: 2026-10-07
updated: 2026-10-07
accepted_on: 2026-10-07
amends:
  - adr-t813-3 decision 6
owners:
  - hisamekms
tags:
  - runtime
  - provider
related:
  - adr-t813-3
  - adr-t813-2
  - adr-t1857-1
  - adr-t1091-1
  - design-provider-lifecycle
---

# ADR-t2079-1: providerを起動するpathはsymlinkのまま持ち、無ければ名前で解決し直す（ADR-t813-3決定6をamends）

## Context

Claude Codeは`~/.local/bin/claude`を`versions/<version>`への symlink にして自分で更新し、更新は古い version の実体を消す。runtime は provider の path を実体に解決して（canonicalize）、supervisor の持つ path、`up`・`install`・自動更新が登録・exec で引き継ぐ argv、runner・planner-session・job に渡す`--claude`に使っていた。Codex も ADR-t813-3 決定6が「`codex`を実体に解決して固定し」と決めていた。

更新が実体を消すと、その path を持つ起動済みの runner の以後の turn はどれも起動できず（`launch agent: No such file or directory`）、runner は同じ引数で再試行するので provider の hold でも直らなかった。launchd に登録済みの supervisor は再起動の入口で path を解決できずに落ちる。Claude Code の実体への固定を決めた accepted の ADR は無い。

## Decision

1. **provider を起動する path は symlink のまま持つ。** 名前なら PATH で見つけた path（Codex は今までどおり cmux の shim の dir を除く）、path ならそのまま（相対なら絶対にするだけ）を使い、存在と実行できることだけを確かめる。実体には解決しない。provider の version の記録は今までどおり実体を読む。
2. **起動の時点で path が無ければ名前で解決し直す。** runner の turn・planner-session・job の agent の起動が実行ファイルを見つけられず、与えられた path が無いときは、provider の名前（`claude`・`codex`）を PATH で解決し直して1回だけ起動し直し、前の path・新しい path・actor を記録する。名前でも見つからなければ今までどおりの失敗にし、provider の切り替え（ADR-t813-2）と hold の判定は変えない。
3. **入口で path が無ければ名前で解決し直す。** supervisor の入口（`up`・`install`・自動更新・release が exec で引き継ぐ argv も同じ入口に来る）と、provider の path を受け取るコマンドは、与えられた path が無ければ名前で解決し直して続け、そのことを log に残す。名前でも見つからなければ今までどおり入口で失敗する（job の provider の切り替えを止める設定の規則、ADR-t1857-1 は変えない）。Claude を使わない運転では解決しない。
4. **解決し直すのは同じ install の別の version だけ。** 名前で見つけたものの実体が、消えた path の残っている最も近い祖先（root と home を除く）の下にあるとき、つまり provider の更新がその場で version を入れ替えたときだけ、消えた path の代わりにする。別の install の provider や、もともと provider の version でなかった path（test の stub など）を代わりにしない。

ADR-t813-3 決定6のうち「runtime は`codex`を実体に解決して固定し」だけをこの決定で置き換える。決定6の残り、設定（sandbox・`writable_roots`・network・承認）は exec と resume のたびに同じ`-c`で渡すこと、`~/.codex/config.toml`・`auth.json`・`~/.codex/rules`を書き換えず`CODEX_HOME`も変えないこと、Codex の hook を使わず hook の trust を外す flag も付けないこと、cmux の shim を使わないことは保つ。

## Alternatives

- **実体への固定を保ち、起動の失敗のときだけ解決し直す。** 前の binary が登録した argv にも効くが、更新のたびにすべての起動が1回失敗して解決し直しの記録が出続ける。退けた。
- **symlink のまま渡すだけにする。** 前の binary が登録した argv と起動済みの runner の argv は実体の path のまま残り、更新で止まる。退けた。
- **起動のたびに名前で解決する（与えられた path を使わない）。** 人が`--claude`で選んだ path が PATH の別のものに替わる。退けた。
- **path が無ければ、名前で見つけたものを無条件に使う（決定4を置かない）。** 人が別の install に移したときや、無い path を与えて provider が無いことを試す test が、PATH の別の provider を起動する。退けた。

## Consequences

- provider の自動更新が古い version を消しても、起動済みの supervisor・runner・job は symlink を通して新しい version を起動する。前の binary が実体の path で登録した argv も、入口と起動の時点で名前に解決し直される。
- 与えられた path が無く、PATH の provider が同じ install の別の version のときに限り、それを起動する。PATH にも無ければ今までどおり失敗し、切り替えと hold に進む。
- `status`・`doctor`の provider の解決先は symlink の path を示し、version の欄が実体の version を示す。

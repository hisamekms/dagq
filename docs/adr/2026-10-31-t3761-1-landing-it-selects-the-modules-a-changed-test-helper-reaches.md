---
id: adr-t3761-1
type: adr
title: 着地の絞ったITは、tests/commonのhelperの変更からそのhelperを使うtestのmoduleのITを選び、多くのITに当たるhelperだけITを全部流す（ADR-t2892-1決定3をamends）
status: accepted
created: 2026-10-31
updated: 2026-10-31
accepted_on: 2026-10-31
amends:
  - adr-t2892-1 decision 3
owners:
  - hisamekms
tags:
  - testing
  - integrate
related:
  - adr-t1925-1
  - adr-t2892-1
  - plan-landing-it-selection
---

# ADR-t3761-1: 着地の絞ったITは、変えたtestのhelperを使うmoduleで選ぶ

## Context

[ADR-t2892-1](2026-10-20-t2892-1-landing-it-leaves-added-sources-and-broad-entries-to-ci.md)決定3は、ITを全部流す条件の共通のファイルを、表が判断できないbuildの入力と共通のfixtureの一覧とした。
共通のfixtureの一覧は`tests/common/**`の全部で、helperの1つのファイルに触れた着地もITを全部流した。

直近14日のmainの着地602件のうち30件が`tests/common`に触れ、どれもその理由でITを全部流した。
全部の件の検証は負荷が低くても27〜30分で、helperの1つに触れたtaskの着地の検証が、integrateの検証のコマンドの上限（30分）で2回killされた。
helperの大半は少数のmoduleだけが使い、使うmoduleはtestのソースから静的に読める（[測定](../plans/landing-it-selection.md)の「tests/commonのhelperの選び方の測定」）。
helperの使い手の割合は二山で、使うmoduleのITが全体の10%以下のhelperと、94%以上のhelper（`mod.rs`と共通のfixture）に分かれる。

amendsにするのは、ADR-t2892-1の4つの決定のうち決定3の共通のファイルの範囲だけが変わり、他の決定（足したsrcのファイルと広い項目をCIに任せること・任せたファイルを出力に残すこと）とADR-t1925-1の決定は変わらないため（[ADR-t1091-1](2026-09-30-t1091-1-amend-or-replace-by-number-of-decisions.md)）。

## Decision

ADR-t2892-1決定3の共通のファイルから共通のfixtureの一覧を外し、testのhelperを次のとおり扱う。buildの入力の一覧と、他の全部流す条件は変えない。

1. **testのhelperの変更は、そのhelperを使うtestのmoduleのITを選ぶ。**
   使うmoduleは、着地の検証がcheckoutしたtreeのtestのソースを静的に読んで求める。
   helperを直接名指すmoduleに加えて、helperを名指す別のhelperの使い手と、それらのmoduleを名指すかそのmacroを呼ぶmoduleも使い手に数える。
   helperの集まりの入口（`mod.rs`）が出し直す名は、その名を使うmoduleを使い手に数える。
2. **使うmoduleのITが多いhelperは、ITを全部流す。**
   多いかどうかは、treeのITの本数（`#[test]`の数）に対する使い手のmoduleのITの本数の割合で測り、上限は広い項目（ADR-t2892-1決定2）と同じ割合にする。
   入口の`mod.rs`の変更は、`common`を名指す全部のmoduleが使い手になり、ここに当たる。
   共通のfixtureの壊れは使い手の大半に及ぶので、CIに任せず着地の検証で全部流す。
3. **helperの集まりの下のhelperのソースでないファイルは、使い手を読めないのでITを全部流す。**

上限の値と規則の置き場は[測定](../plans/landing-it-selection.md)の「決めたこと」と`scripts/landing-it.py`の冒頭である。選び方はrepositoryのscriptに置き、runtimeは変えない（ADR-t1925-1決定7）。

## Alternatives

- **多くのITに当たるhelperも全部流さず、常にCIに任せる**: 全部流す着地はさらに減り、検証の上限の中に収めやすい。CIを最終関門にする方針の範囲だが、共通のfixtureの壊れの見逃しは全ITに及び、着地の後に一斉に赤くなる。使い手が94%以上のhelperでは絞っても節約がほとんど無い。
- **今の規則のまま`tests/common/**`を全部流す**: 見逃しは最も少ないが、使い手の少ないhelperの変更も上限に近い全件の検証になり、着地が時間切れで止まる。
- **表を行の粒度にして、helperの変更を表で選ぶ**: 当たりの精度は上がるが、夜間のjobと表の形を変える別の変更になる。この決定はそれを妨げない。
- **使い手の数え方をmoduleでなくtestの関数の粒度にする**: 静的な読みでは関数の呼び出しの連鎖を辿れず、誤って外す危険が増える。

## Consequences

- 使い手の少ないhelperの変更の着地は、そのmoduleのITと差分の他の理由のITだけを流す。測定の14日では、共通のファイルで全部流した着地は45件から36件に減る。
- 静的な読みは名の一致で数えるので、使わないmoduleを数える側にずれる（同じ名の別の項目など）。外す側のずれ（名を書かずに使う経路）はCIが最終関門で拾う。
- helperが選ぶmoduleのITは、差分のtestのファイルが定めるITと同じく、全部流す閾値の見込みに数えない。使い手の多いhelperと他の変更が重なると、絞った検証も長くなりうる。
- `mod.rs`と共通のfixtureの変更は今どおり全件の検証になり、その時間は検証のコマンドの上限と全件の時間に左右される。

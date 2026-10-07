---
id: adr-t2032-1
type: adr
title: landing recheckのcommandは、mergeした木とmainの差分が`[recheck] paths`に触れるrunにだけ流す（ADR-0068決定2をamends）
status: accepted
created: 2026-10-07
updated: 2026-10-07
accepted_on: 2026-10-07
amends:
  - adr-0068 decision 2
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - performance
related:
  - adr-0068
  - adr-t1311-1
  - adr-t963-1
  - adr-t1453-1
  - design-supervisor-lifecycle-landing-recheck
---

# ADR-t2032-1: landing recheckのcommandは、mergeした木とmainの差分が`[recheck] paths`に触れるrunにだけ流す（ADR-0068決定2をamends）

## Context

[ADR-0068](0068-recheck-waiting-runs-after-each-landing.md)決定2は、merge-treeが衝突しなければ、着地を待つ全てのrunにmainへ載せた木で`[recheck] command`を流すと決めた。recheckはqueueで直列なので、commandは待つrunの数だけ順番待ちを作る。この repositoryのcommandはcompileの検査で、docsやconfigだけを変えるrunでは、mergeした木のcompileに関わる部分が着地のときに検証済みのmainと同じで、commandが新しく見つけるものがほぼ無い。

どの変更でcommandを流すべきかはrepositoryごとに違い、dagqはRust以外のrepositoryも使う汎用のruntimeなので、runtimeに決め打ちできない。差分のpathをglobで選ぶ設定は`[e2e] paths`（[ADR-t963-1](2026-09-29-t963-1-e2e-required-by-diff-and-run-in-full-before-auto-update.md)）と`[review.subagents.<agent>] paths`（[ADR-t1453-1](2026-10-03-t1453-1-review-subagents-named-by-path-run-inside-the-review-job.md)）に先例がある。

ADR-0068は番号付きの決定を6つ持ち、変えるのは決定2だけなので、[ADR-t1091-1](2026-09-30-t1091-1-amend-or-replace-by-number-of-decisions.md)に従ってamendsで直す。

## Decision

1. **`[recheck]`に、commandを流す差分のglob（`paths`）を置ける。** `paths`があるとき、recheckはmerge-treeが衝突せずcommandがあるrunのうち、mergeした木とmainの差分のpathがどれかのglobに当たるものにだけcommandを流す。どれにも当たらないrunはmerge-treeの検査だけで、きれいな結果として記録する。`paths`が無ければ今までどおり全てのrunにcommandを流す。globの規則と誤りの扱いは`[e2e] paths`と揃える。
2. **差分はmergeした木とmainの差分にする。** recheckが確かめる木そのものがmainと違うpathで、runのrebase前のbaseやmainの側の変更に依らない。
3. **pathsで飛ばしたことは、commandが無いときと区別して残す。** きれいな結果の記録はcommandを流さなかったことに加えて、`paths`で飛ばしたことを持つ。commandを流せる設定のsupervisorがcommandを流さなかったきれいな結果を確かめ直す判断で、`paths`で飛ばしたものを確かめ済みに数えられるようにするため。
4. 変えないもの: merge-treeの衝突の検査は全てのrunで続ける。commandを流したときの手順と失敗の扱い（ADR-0068の決定2の残りと決定3〜6）。

## Alternatives

- **runtimeにRustのglobを決め打ちする**: Rust以外のrepositoryで誤ったrunを飛ばすか、どのrunも飛ばさない。
- **runのbaseからheadの差分で選ぶ**: rebase前のbaseが古いと、mainがすでに持つ変更をrunの変更と取り違える。
- **taskの`--paths`で選ぶ**: 宣言しないtaskがあり、宣言は差分の上限で実際の差分ではない。

## Consequences

- docsやconfigだけのrunは、着地のたびのcommandの順番待ちを作らない。compileを壊す経路が`paths`の外にあれば（globの漏れ）recheckは見逃し、着地の検証が見つける。
- この repositoryの`dagq.toml`に`paths`を足すのは、固定バイナリがこのkeyを読めるようになってから（旧バイナリは未知のkeyでファイル全体を読めなくなる）。
- keyと記録の形は[Landing recheck](../design/supervisor-lifecycle/landing-recheck.md)と[Run environment](../design/supervisor-lifecycle/run-environment.md)が持つ。

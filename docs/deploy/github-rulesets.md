---
id: deploy-github-rulesets
type: deploy
title: チームの版のrepositoryに置くGitHubのruleset（R1〜R6の参考の構成と、制御側の関門が照らす範囲）
status: current
created: 2026-11-10
owners:
  - hisamekms
tags:
  - security
  - documentation
related:
  - adr-t4332-1
  - adr-t3987-1
  - adr-t3990-5
  - deploy-index
---

# GitHubのruleset

> GitHubでR1〜R6を表せるかは、試行用のrepositoryで人が確かめるまで仮である。

チームの版の制御側は、runnerのpushとmainの更新の禁止をrepositoryのGitHubのrulesetが持つと確かめられた間だけ、runnerに仕事を割り当ててrunner用のAppのtokenを出す（[ADR-t3987-1](../adr/2026-11-03-t3987-1-team-version-runners-pull-under-fenced-run-leases-ai-jobs-on-runners-pinned-images-two-github-apps-and-attributed-claude-credentials.md)決定6、[ADR-t4332-1](../adr/2026-11-10-t4332-1-ruleset-gate-checks-rules-targets-enforcement-and-every-bypass.md)）。
この文書は、配置する人がrepositoryに置くrulesetの参考の構成と、制御側がそれをどこまで確かめるかを書く。
2つのGitHub Appの権限は`github-apps.md`が持ち、ここには書かない。

## 置くruleset

GitHubのrulesetは規則に当たる操作を禁じ、bypassの一覧に置いたactorだけが越えられる。
bypassはruleset全体に効くので、迂回させてよい規則と誰にも迂回させない規則を別のrulesetに分ける。
名前は仮で、変えてよい（制御側は名前を見ない）。

| ruleset | 対象 | 規則 | bypass |
| --- | --- | --- | --- |
| R1 mainの更新 | `refs/heads/main` | `update` | main用のAppだけ |
| R2 mainの削除と強制 | `refs/heads/main` | `deletion`・`non_fast_forward` | なし |
| R3 名前空間の外のbranch | `~ALL`から`refs/heads/main`・`refs/heads/dagq/**`・`refs/heads/dagq-landing/**`を除く | `creation`・`update`・`deletion` | pjが選ぶ人の役・CIなど。runner用のAppは置かない |
| R4 tag | tagの`~ALL` | `creation`・`update`・`deletion` | releaseを出す人の役かworkflow。runner用のAppは置かない |
| R5 名前空間の強制 | `refs/heads/dagq/**`・`refs/heads/dagq-landing/**` | `non_fast_forward` | なし |
| R6 名前空間の削除 | `refs/heads/dagq/**`・`refs/heads/dagq-landing/**` | `deletion` | main用のAppだけ |

`POST /repos/{owner}/{repo}/rulesets`に渡す本文の例（`Integration`の`123456`はmain用のAppのidに置き換える）:

```json
{"name": "dagq-main-update", "target": "branch", "enforcement": "active",
 "conditions": {"ref_name": {"include": ["refs/heads/main"], "exclude": []}},
 "rules": [{"type": "update"}],
 "bypass_actors": [{"actor_type": "Integration", "actor_id": 123456, "bypass_mode": "always"}]}
```

```json
{"name": "dagq-main-no-delete-no-force", "target": "branch", "enforcement": "active",
 "conditions": {"ref_name": {"include": ["refs/heads/main"], "exclude": []}},
 "rules": [{"type": "deletion"}, {"type": "non_fast_forward"}],
 "bypass_actors": []}
```

```json
{"name": "dagq-outside-namespaces", "target": "branch", "enforcement": "active",
 "conditions": {"ref_name": {"include": ["~ALL"],
   "exclude": ["refs/heads/main", "refs/heads/dagq/**", "refs/heads/dagq-landing/**"]}},
 "rules": [{"type": "creation"}, {"type": "update"}, {"type": "deletion"}],
 "bypass_actors": [{"actor_type": "RepositoryRole", "actor_id": 5, "bypass_mode": "always"}]}
```

```json
{"name": "dagq-tags", "target": "tag", "enforcement": "active",
 "conditions": {"ref_name": {"include": ["~ALL"], "exclude": []}},
 "rules": [{"type": "creation"}, {"type": "update"}, {"type": "deletion"}],
 "bypass_actors": [{"actor_type": "RepositoryRole", "actor_id": 5, "bypass_mode": "always"}]}
```

```json
{"name": "dagq-namespaces-no-force", "target": "branch", "enforcement": "active",
 "conditions": {"ref_name": {"include": ["refs/heads/dagq/**", "refs/heads/dagq-landing/**"], "exclude": []}},
 "rules": [{"type": "non_fast_forward"}],
 "bypass_actors": []}
```

```json
{"name": "dagq-namespaces-delete", "target": "branch", "enforcement": "active",
 "conditions": {"ref_name": {"include": ["refs/heads/dagq/**", "refs/heads/dagq-landing/**"], "exclude": []}},
 "rules": [{"type": "deletion"}],
 "bypass_actors": [{"actor_type": "Integration", "actor_id": 123456, "bypass_mode": "always"}]}
```

R3とR4の`RepositoryRole`の`5`（admin）は例で、pjが選ぶ。
人のbypassを既定で置かない。
緊急の手当てでrulesetを一時的に外すと、外している間は下の関門に落ち、runnerへの割り当ては止まる。

## 制御側の関門が照らす範囲

制御側は起動時から短い間ごとに、runner用のAppがrepository 1つと`metadata: read`だけに絞ったtokenでrulesetの一覧（Organizationのrulesetを含む）と各rulesetの中身を読む。
このtokenはrunnerに渡らず、どちらのAppにもAdministrationの権限を求めない。

- 照らすもの: R1〜R6の各規則が、`enforcement`が`active`で、対象の種類（branchかtag）が合い、表の対象のrefを含みそれを除かず、bypassが表のとおりのrulesetのどれかにあること。
  1つの禁止の規則が別々のrulesetにあってもよい。
- bypassは全てのactorについて照らす。
  R2とR5は`bypass_actors`が空、R1とR6はmain用のApp（`Integration`のそのid）だけか空、R3とR4はrunner用のAppを含まないこと。
  runner用のAppのtokenでの読みの`current_user_can_bypass`が`never`であることも照らし、それ以外の値と応答に無いことは迂回できるとみなす。
- `bypass_actors`が応答に無いrulesetは、bypassを確かめられないので禁止を持たないとみなす。
  GitHubは`bypass_actors`をrulesetへのwriteを持つ呼び手にだけ返すので、`metadata: read`のtokenの読みでは関門は開かず、runnerは仕事を受けない。
  `bypass_actors`の読み方は、人が試行用のrepositoryで確かめた結果から決める。
- 照らさないもの: rulesetの名前。
- GitHubの読みは制御側の起動から自分の流れで続け、割り当ては覚えた結果だけで決める。
  読みが遅れて覚えた結果が古くなれば、確かめられないとして割り当てない。
- 一覧か中身が読めない、rulesetを使えないplan、規則の不足、bypassの違いのときは確かめられないとし、runにもjobにも割り当てず、runner用のAppのtokenも出さない。
  理由は`dagq status`の`attention`に出て、保留ごとに運用のaskが1度だけ開き、確かめられて割り当てが自動で戻るとそのaskは閉じる。

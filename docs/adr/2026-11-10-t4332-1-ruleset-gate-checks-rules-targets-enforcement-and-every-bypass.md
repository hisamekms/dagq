---
id: adr-t4332-1
type: adr
title: 制御側はrulesetの規則・対象・enforcementと全てのactorのbypassを決定6どおりにGitHubのAPIで確かめ、読みはrunner用のAppのmetadataのreadだけにして書き込みの列の外で行い、bypassが返らない間は確かめられないとする（ADR-t3987-1決定6をamends）
status: accepted
created: 2026-11-10
updated: 2026-11-10
accepted_on: 2026-11-10
amends:
  - adr-t3987-1 decision 6
owners:
  - hisamekms
tags:
  - architecture
  - security
  - queue-service
related:
  - adr-t3987-1
  - adr-t3990-5
  - plan-team-version-runner
  - deploy-github-rulesets
---

# ADR-t4332-1: rulesetの関門が確かめる範囲と読み方（規則・対象・enforcement・全てのactorのbypass）

## Context

[ADR-t3987-1](2026-11-03-t3987-1-team-version-runners-pull-under-fenced-run-leases-ai-jobs-on-runners-pinned-images-two-github-apps-and-attributed-claude-credentials.md)決定6は、rulesetが禁止(i)〜(iv)を持つことをGitHubのAPIで確かめられた間だけ、制御側がrunnerに仕事を割り当ててrunner用のAppのtokenを出すと決めた。
(i)の「mainの更新はmain用のAppにだけ許す」、(ii)と(iv)の「誰にも迂回させない」「削除はmain用のAppにだけ許す」は、rulesetの規則と対象だけでなく、bypassの構成でも決まる。

GitHubの公式文書（2026-11-06に読んだREST APIのrulesの頁・rulesetの作成の頁・Appの権限の頁とOpenAPIの記述）では次のとおりである。

- 一覧（`GET /repos/{owner}/{repo}/rulesets`）と1件（`GET /repos/{owner}/{repo}/rulesets/{ruleset_id}`）の読みはMetadataのreadで足り、installation tokenで読める。
- 1件の応答の`bypass_actors`は、rulesetへのwriteを持つ呼び手にだけ返る。
  writeはAdministrationのwriteで、Organizationのrulesetには OrganizationのAdministrationのwriteが要る。
- `current_user_can_bypass`は呼び手のbypassの種類（`always`・`pull_requests_only`・`never`・`exempt`）で、repositoryの単位の読みにだけ返る。

## Decision

1. 制御側がGitHubのAPIで確かめるのは、rulesetの規則・対象のref・enforcementが`active`であることと、bypassが決定6のとおりであることである。
   読んだrulesetで設計資料のR1〜R6の各規則が揃うときだけ確かめられたとし、読めない・rulesetを使えないplan・規則の不足・bypassの違いは全て確かめられないとして、割り当てずtokenも出さない（fail closed）。
   rulesetの名前では照らさない。
2. bypassは全てのactorについて照らす（2026-11-10の人の決定）。
   R2とR5は誰にも迂回させず、R1とR6はmain用のAppにだけ許し、R3とR4はrunner用のAppに許さない（ほかはpjが選ぶ）。
   `bypass_actors`が応答に無いか読めないrulesetは、bypassを確かめられないので禁止を持たないとみなす。
   runner用のAppのtokenでの読みの`current_user_can_bypass`は`never`でなければならず、それ以外の値と応答に無いことは迂回できるとみなす。
3. rulesetの読みのtokenはrunner用のAppのinstallation tokenで、repository 1つと`metadata: read`だけに絞り、runnerに渡さない。
   どちらのAppにもAdministrationの権限を頼まない。
   GitHubは`bypass_actors`をrulesetへのwriteを持つ呼び手にだけ返すので、この読みでは関門は開かず、runnerは仕事を受けない。
   `bypass_actors`をどう読むか（決定6を崩さずに読める手段）は、人が試行用のrepositoryで確かめた結果から別のtaskと別のamendsで決める。
4. GitHubの読みは書き込みの列の外で行い、割り当ては覚えた結果だけで決める。
   読みが遅れて覚えた結果が古くなれば、確かめられないとして割り当てない。

決定6のそのほかの部分（2つのAppの分け方、tokenの絞り方、事後に見つけて知らせる形を採らないこと、workflowの権限、人のbypassを既定で置かないこと）と[ADR-t3990-5](2026-11-04-t3990-5-team-version-deploy-documents-and-reference-configurations-without-iac.md)決定1は保つ。
決定6が変わるのは、確かめ方（読みの権限・列の外で読むこと）と、`bypass_actors`が読めない間は割り当てないことの明記だけである。

## Alternatives

- **bypassを照らさないか、runner用のAppの分だけ照らして残りを配置する側の約束にする**: main用のAppがR2・R5を、別のactorがR1・R6を迂回できる構成を通し、決定6の認可の境界を弱める。
  人が退けた（2026-11-10）。
- **main用のAppにAdministrationのwriteを与えて`bypass_actors`を読む**: 制御側の鍵が漏れれば自分を縛るrulesetを書き換えられる。
  repositoryの管理全般にも届き、Organizationのrulesetを読むにはOrganizationのAdministrationのwriteまで要る。
- **bypassを読むだけの3つ目のAppを置く**: 同じ秘密の置き場に置くので鍵の漏れで同じ害があり、Appを2つとした決定6に増やす。
- **割り当ての要求の中でGitHubを読む**: 読みの遅れが書き込みの列を止め、生存の報告と書き込みの係のleaseの更新を待たせる。

## Consequences

- 決定6の禁止とbypassは全て関門が照らし、配置する側の約束に残るものは無い。
- `metadata: read`の読みには`bypass_actors`が返らないので、決定3の読み方が決まるまで関門は開かず、チームの版のrunnerは仕事を受けない。
  危険は無く、割り当てが止まる理由は`status`と運用のaskに出る。
- 読みの権限はMetadataのreadだけになり、どちらのAppもrulesetを書き換えられない。
- GitHubでR1〜R6を表せないと分かっても、関門はfail closedで割り当てないので危険は無く、直すのは判断の関数の1か所である。

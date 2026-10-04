---
id: adr-t1570-1
type: adr
title: Codexのrunのreviewはworktreeのprojectをuntrustedにして起動し、workerが置ける.codexの層を読まない。他のheadless jobは変えない
status: accepted
created: 2026-10-04
updated: 2026-10-04
accepted_on: 2026-10-04
owners:
  - hisamekms
tags:
  - runtime
  - review
  - provider
  - security
related:
  - adr-t1470-1
  - adr-t1453-1
  - adr-t728-1
  - adr-t813-3
  - adr-t1207-1
  - plan-review-subagents-spike
  - design-provider-lifecycle
  - design-supervisor-lifecycle-review
---

# ADR-t1570-1: Codexのrunのreviewはworktreeのprojectをuntrustedにして起動し、workerが置ける.codexの層を読まない。他のheadless jobは変えない

## Context

Codexのrunのreviewは、workerのworktreeをcwdに`codex exec --json --skip-git-repo-check --sandbox read-only -C <worktree>`で起動し、promptはstdinで渡す（task 1560）。trustは渡さず、人の`~/.codex/config.toml`のtrustを継ぐ。このrepositoryのmain checkoutはそこで`trusted`なので、そのGitのworktreeもtrustされ、Codexはworktreeのprojectの層（`.codex/config.toml`など）を読む。2026-10-03にcodex-cli 0.160.0で、worktreeに置いた`.codex/config.toml`の`developer_instructions`がreviewのjobに効いた（[review-subagents-spike](../plans/review-subagents-spike.md)の6.）。[ADR-t1470-1](2026-10-03-t1470-1-all-claude-run-reviews-load-no-setting-sources.md)決定3は、これを塞ぐ方法をfollow_upで決めるとした。

workerが置き・変えられる設定がreviewを変えうるのは、workerの変更でreviewを消せない・変えられないという[ADR-t1453-1](2026-10-03-t1453-1-review-subagents-named-by-path-run-inside-the-review-job.md)決定4と、[ADR-t728-1](2026-09-27-t728-1-trust-domains-actors-and-default-deny-capability-authorization.md)の信頼の境目に合わない。Claudeのrunのreviewは`--setting-sources ""`で塞いだ（ADR-t1470-1決定1）。このrepositoryは`dagq.toml`の`[roles.review]`でrunのreviewをCodexで動かすので、実際に効く経路である。

2026-10-04にcodex-cli 0.160.0で確かめた（[review-subagents-spike](../plans/review-subagents-spike.md)の「7. Codexのrunのreviewとworktreeのprojectのtrust（task 1570）」）。`-c projects={"<worktree>"={trust_level="untrusted"}}`を渡すと、このrepositoryの本物のworktree（人の設定で信頼を継ぐ）でも、`-c`でmain checkoutを信頼させた使い捨てのrepositoryでも、worktreeの`.codex/config.toml`の`developer_instructions`と`.codex/rules`の`prefix_rule`が効かなくなった。read-onlyのsandboxは書き込みを拒み続け、`python3`・`git log`・`dagq --version`などのコマンドは承認で止まらずに動き、jobは終了コード0で終わった。permission profile `dagq_job`の`-c`に置き換えても同じだった。一方、Codexはworktreeの`AGENTS.md`を自分では読まなくなった（trustの無いrepositoryでは読む）。reviewのpromptの規則の1文（ADR-t1470-1決定2）を足したpromptでは、modelが`AGENTS.md`をshellで読んでその中身で答えた。

## Decision

1. **塞ぎ方。** Codexのrunのreviewの起動に`-c projects={"<worktree>"={trust_level="untrusted"}}`（workerのturnに渡す`trusted`と同じkeyの書き方で、pathはJSONのescapeで書いたTOMLの引用のkey）を足す。Codexはtrustをcwd・repositoryのrootの順に探すので、worktree自身のtrustがmain checkoutのtrustより先に見つかり、`untrusted`のprojectの層（`.codex/config.toml`・`.codex/rules`）は読まれない。`--sandbox read-only`（queue serviceに届くときは`dagq_job`のprofileの`-c`）・`--skip-git-repo-check`・`-C <worktree>`・modelとeffortの引数・promptをstdinで渡すことは変えない。承認の設定は足さない: `untrusted`でもjobのコマンドは承認で止まらず、read-onlyのsandboxが書き込みを拒むことを確かめた。人の`~/.codex/config.toml`・`auth.json`・`CODEX_HOME`は変えない（[ADR-t813-3](2026-09-28-t813-3-codex-worker-permissions.md)決定6）。
2. **当てる範囲。** runのreviewだけに当てる。runのreviewのcwdはworkerが書けるworktreeで、着地していない内容をreviewする。goal review・plan reviewのcwdはmain checkoutで、そのprojectの層は着地したcommitの内容（landingはreviewとintegrateを通る）か、人が置いたものなので、人のtrustの判断に従う。スループットの見直し・observer・復旧jobのcwdはGitの外のjob dirかrun dirで、projectのtrustもworkerの置いた`.codex`も無い。これらの起動は変えない。workerのturnに渡すtrust（`trusted`）も変えない。
3. **`AGENTS.md`の渡り方。** `untrusted`にするとCodexはworktreeの`AGENTS.md`をinstructionsとして読まない。規則はreviewのpromptの規則の1文（worktreeのrootの`AGENTS.md`・`CLAUDE.md`と名指す文書を読み、変更に当たる規則で判定する。ADR-t1470-1決定2）で渡り、modelがそれを読むことを確かめたので、渡し方は足さない。Claudeのreviewと同じく、規則はmemoryとして先に読まれるのでなくpromptの指示で読まれる。
4. **他のprojectの層。** `.codex/rules`（workerのturnにruntimeが書くrulesと、workerが置きうる他のrules）もreviewに効かなくなる。reviewはread-onlyのsandboxで書き込みも他のprocessへのsignalも拒まれるので、runtimeのrulesの`pkill`・`killall`の禁止を失っても守りは変わらない（[Agent provider lifecycle](../design/provider-lifecycle.md)の「settings」）。workerの置いたrulesがreviewのコマンドを拒んでreviewを変えることも無くなる。`.codex/config.toml`の`developer_instructions`以外のkey（`agents.*`・MCPのserver・hookなど）と、projectの`.codex/skills`などのほかの置き場は、projectの層ごと読まれないとみなすが、keyごとには確かめていない。reviewのsubagentの定義を`-c agents.*`で渡すことと、Codexでsubagentを動かせるかは、この起動を前提にtask 1476が確かめる。

## Alternatives

- **worktreeのtrustのentryをlevelなしで渡す（`projects={"<worktree>"={}}`）**: 試すと`.codex/config.toml`の`developer_instructions`は効いたままだった。trustの無いentryではrepositoryのrootの`trusted`に落ちる。
- **`--ignore-user-config`**: 人の`~/.codex/config.toml`全体を読まないので、main checkoutのtrustも消えて塞がる。ただしmodelのproviderの設定など、jobが人の設定から受けていたもの全部を捨て、reviewの振る舞いを塞ぐ範囲の外で変える。他のjobとも分かれる。
- **keyごとに`-c`で上書きする（`-c developer_instructions=""`など）**: projectの層の残りのkey（`agents.*`・MCP・hook・rules）は残り、Codexがkeyを足すたびに漏れる。
- **cwdをrunのdirなどGitの外に移す**: reviewはworktreeの相対のpathで差分を読み、promptもcwdをworktreeとして書いている。promptの資料と起動の両方が変わり、goal 90の比較の後も起動の引数だけに留める範囲を超える。`AGENTS.md`も読まれない点は同じ。
- **`CODEX_HOME`を別にする**: 認証が変わる。ADR-t813-3決定6に合わない。
- **全てのheadless jobに`untrusted`を渡す**: main checkoutの層は着地した内容か人の置いたもので、workerが変えられない。Gitの外のjobにはworktreeの層が無い。変えると人がmain checkoutに置いた設定をjobが黙って失い、塞ぐものの無いjobの起動を変える。
- **承認を`-c approval_policy="never"`で固定する**: `untrusted`でも承認で止まらなかったので足さない。足すとreviewの起動だけが承認の設定を持ち、他のjobと分かれる。

## Consequences

- Codexのrunのreviewは、workerが置き・変えられるworktreeの`.codex`の層から独立する。Claudeのreview（ADR-t1470-1）とそろい、runのreviewはproviderに依らずworktreeの設定を読まない。
- 起動の引数の今の姿は[Agent provider lifecycle](../design/provider-lifecycle.md)と[Review](../design/supervisor-lifecycle/review.md)が持つ。
- 必須のagentの無いreviewのprompt・資料・verdictの扱い・eventは変わらない。変わるのは起動の引数の1つだけ。
- codex-cliの版が上がってtrustの探し方や`untrusted`の扱いが変わると、塞ぎが効かなくなりうる。確かめた版はcodex-cli 0.160.0。

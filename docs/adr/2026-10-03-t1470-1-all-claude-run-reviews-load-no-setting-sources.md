---
id: adr-t1470-1
type: adr
title: Claudeのrunのreviewは、必須のagentの有無に依らずsetting sourcesを空にしてauto memoryも読まず、repositoryの規則はpromptが名指すinstructionsから読む
status: accepted
created: 2026-10-03
updated: 2026-10-03
accepted_on: 2026-10-03
amends:
  - adr-t1453-1 decision 8
  - adr-t1453-1 decision 10
owners:
  - hisamekms
tags:
  - runtime
  - review
  - provider
  - security
related:
  - adr-t1453-1
  - adr-t728-1
  - adr-t1091-1
  - adr-t1207-1
  - plan-review-subagents-spike
  - design-supervisor-lifecycle-review
---

# ADR-t1470-1: Claudeのrunのreviewは、必須のagentの有無に依らずsetting sourcesを空にしてauto memoryも読まず、repositoryの規則はpromptが名指すinstructionsから読む

## Context

Claudeのrunのreview（`ClaudeCode::review_command`）はworkerのworktreeをcwdにして、setting sourcesを既定のまま起動していた。[ADR-t1453-1](2026-10-03-t1453-1-review-subagents-named-by-path-run-inside-the-review-job.md)は必須のagentのあるreviewに限りsetting sourcesを空にし（決定8）、goal 90の比較のあいだ設定の無いrepositoryのreviewの起動の引数を変えないため（決定10）、必須のagentの無いreviewが今もworktreeの設定を読みうることをgoal 90の後に決めると残した（Consequences）。このtaskは、goal 90を待つ依存（goal_dependencies）が解けて着手したので、ここで決める。

既定のsetting sourcesでは、workerが置き・変えられるworktreeの`.claude/settings.json`（hooks・permissions）・`.claude/settings.local.json`・`.claude/agents`・`.claude/skills`・`.mcp.json`のserverがreviewの起動を変えうる。これはworkerの変更でreviewを消せない・変えられないという[ADR-t1453-1](2026-10-03-t1453-1-review-subagents-named-by-path-run-inside-the-review-job.md)決定4と、[ADR-t728-1](2026-09-27-t728-1-trust-domains-actors-and-default-deny-capability-authorization.md)の信頼の境目（workerの出力はデータで、reviewとsupervisorがそれを判定する）に合わない。

2026-10-03にClaude Code 2.1.288で、queueを使わない使い捨てのGitのディレクトリで確かめた（[review-subagents-spike](../plans/review-subagents-spike.md)の「6. review の setting sources と project の設定（task 1470）」）。既定ではworktreeの`SessionStart`と`Stop`のhookが動き、`.claude/agents`・`.claude/skills`・`.mcp.json`のserverが読まれ、`CLAUDE.md`がmemoryとして読まれる。`--setting-sources user`と`--setting-sources ""`ではどれも読まれず、reviewの`--settings`のdenyは効いたままである。ただしどちらでも、cwdのauto memory（`~/.claude/projects/<cwdの名前>/memory/`。同じworktreeで動くworkerのsessionが書きうる）は読まれ、`--settings`の`autoMemoryEnabled: false`で止まる。

## Decision

1. **setting sources。** Claudeの全てのrunのreview（必須のagentの有無に依らない）は`--setting-sources ""`で起動する。worktreeのprojectとlocalの設定・agent・skill・`.mcp.json`のserver・`CLAUDE.md`と、hostのuserの設定を読まない。runのreviewの`claude-review-settings.json`は`autoMemoryEnabled: false`を足し、worktreeのcwdのauto memoryも読まない。reviewの`--settings`（roleの`permissions.deny`と`autoMode.environment`）・`--allowedTools Read,Grep,Glob`・`--disallowedTools Bash,Edit,Write,NotebookEdit`・`--add-dir <run dir>`・`--debug-file`は今のまま保つ。必須のagentのあるreviewは、これに`--agents`と`--allowedTools Agent`を足すだけになる（task 1455の組み立てから`--setting-sources ""`を`review_command`に移す）。
   - 空（`""`）を選び、userを残さない理由: hostの`~/.claude/settings.json`がreviewに与えていたのは`model`だけで（他はtheme・tuiなど画面の設定。`env`・`enabledPlugins`・`hooks`は無い）、`--model`を渡さないreviewのmodelはuserありでも空でも`claude-opus-5-5`だった。認証はsettingsでなくClaude Codeの資格情報で、空でも通る。reviewのmodelとeffortを選ぶのはdagqの`[roles.review]`の`model` / `effort`で（[Actor model](../design/supervisor-lifecycle/actor-model.md)のprovider、[ADR-t1063-1](2026-09-29-t1063-1-headless-job-provider-per-role-with-intent-permissions.md)）、userの設定に頼らない。userの設定は人が対話の利用のために変えるもの（hooks・plugin・permissions・skill）で、reviewの振る舞いを黙って変えうるので、読まないほうが起動がdagqの渡すものだけで決まる。task 1455の必須のagentのあるreviewと同じ値にし、2つの起動を食い違わせない。
   - 読まなくなっても残るもの: Claude Codeの組み込みのplugin、claude.aiのconnectorのMCP（source `claudeai`）、組み込みのskill。どれもworkerが変えられない。runのreviewはMCPを止めない（`without_mcp`を使わない）のを今のまま変えない。
2. **repositoryの規則の渡し方。** setting sourcesを空にするとworktreeの`CLAUDE.md`（とそれが`@`で取り込む`AGENTS.md`）はmemoryとして読まれない（確かめた）。reviewが規則を黙って失わないように、runのreviewのprompt（`review_prompt`）に、資料の後の1文で、worktreeのrootのinstructions（`AGENTS.md`と`CLAUDE.md`のあるもの）とそれが名指す文書を読み、変更に当たる規則で判定することを足す（`REVIEW_RULES`）。providerに依らず同じ文にする: Codexのreviewは`AGENTS.md`を自分で読むが、名指しても振る舞いは変わらない。読むのはworktreeの版で今までと同じ（runの差分がinstructionsを変えるなら、その変更も資料の差分に見える）。
3. **Codexのrunのreview。** task 1455とspikeは、Codexのreview（`codex exec --sandbox read-only -C <worktree>`）がworktreeの`.codex/config.toml`を読むかを確かめていなかった。2026-10-03にcodex-cli 0.160.0で確かめた: このrepositoryのworktreeでは、main checkoutが人の`~/.codex/config.toml`で`trusted`なので、worktreeに置いた`.codex/config.toml`の`developer_instructions`がreviewのjobに効き、信頼の無い使い捨てのrepositoryでは効かなかった。task 1455はCodexのreviewの起動を変えておらず、塞がれていない。このADRとtaskでは直さず、receiptのfollow_upにする（`-c`で信頼を`untrusted`に固定するなどの塞ぎ方とその副作用は、そこで決める）。
4. **amendsする範囲。** ADR-t1453-1決定8の「必須のagentがあるreviewに限り」setting sourcesを空にする範囲を、Claudeの全てのrunのreviewに広げる（決定8のそれ以外、`--agents`と`Agent`の許可、Codexの扱い、切り替えは変えない）。決定10の「`[review.subagents]`の無いrepositoryのreviewは、資料・prompt・起動の引数・判定を今のまま変えない」のうち、起動の引数（決定1）とpromptの規則の1文（決定2）を変える。資料・判定・verdictの扱い・eventは変えない。決定10の旧バイナリとdagq.tomlの扱いはそのまま。

## Amendsの判断

ADR-t1453-1は番号付きの決定を10持ち、変えるのは決定8の範囲と決定10の一部だけなので、丸ごと置き換えずamendsにする（[ADR-t1091-1](2026-09-30-t1091-1-amend-or-replace-by-number-of-decisions.md)）。ADR-t1207-1（Codexのreview）とADR-0027（verdictの3値）は変えない。

## Alternatives

- **今のまま既定のsetting sourcesで読ませる**: workerがworktreeのhooks・permissions・agent・skill・MCPでreviewの起動を変えられ、ADR-t1453-1決定4とADR-t728-1の境目に合わない。
- **必須のagentのあるreviewだけに留める**: 設定の無いrepositoryと、差分がどのagentにも当たらないreviewが同じ穴を持ち続ける。2つの起動が分かれ、testと文書が2通りになる。
- **`--setting-sources user`**: userの設定にreviewが頼るものが無く（modelは同じに解決し、dagqは`[roles.review]`で選ぶ）、人の対話の利用のためのhooks・plugin・permissionsがreviewを黙って変えうる。task 1455の起動とも食い違う。
- **`CLAUDE.md`を`--append-system-prompt`などで渡す**: Claudeだけの経路になり、`CLAUDE.md`が`@`で取り込む先を自分で展開しなければならない。promptで名指して読ませれば、Codexとも同じ文で、今の読み方（worktreeの版）を保てる。
- **auto memoryを残す**: 同じworktreeで動くworkerのsessionが書いたmemoryをreviewが読み、workerの変えられるものがreviewに届く。

## Consequences

- Claudeのrunのreviewは、worktreeの`.claude`・`.mcp.json`・`CLAUDE.md`・auto memoryとuserの設定から独立する。名前・argvの今の姿は[Review](../design/supervisor-lifecycle/review.md)と[Agent provider lifecycle](../design/provider-lifecycle.md)が持つ。
- repositoryの規則は、memoryとして先に読まれるのでなくpromptの指示でreviewが読む。読む量はreviewの判断に任せる。
- Codexのreviewがworktreeの`.codex/config.toml`を読みうることは残り、follow_upで決める。

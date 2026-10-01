---
id: adr-t1228-2
type: adr
title: ADR-t1228-1のCLIが揃ったら、Claudeのinboxとplannerのsettingsのpermissions.denyにBash(cmux:*)を置き、inboxにはupが起動のcommandに渡すpermissions.denyだけのsettingsを新しく作る。これはguardrailでenforcementではなく、Codexのinboxは同じ趣旨をCodexの手段で持つ
status: accepted
created: 2026-10-02
updated: 2026-10-02
accepted_on: 2026-10-02
owners:
  - hisamekms
tags:
  - runtime
  - security
  - cmux
  - inbox
  - planner
related:
  - adr-t1228-1
  - adr-t728-1
  - adr-t728-3
  - adr-t813-3
  - adr-0044
  - design-authorization
  - design-security
  - design-supervisor-lifecycle-plan-planners
  - design-provider-lifecycle
---

# ADR-t1228-2: inboxとplannerの生のcmuxをguardrailとして拒む

## Context

[ADR-t1228-1](2026-10-02-t1228-1-inbox-and-planner-reach-sessions-through-the-dagq-cli.md)は、inboxとplannerがcmuxで行っていた操作をdagqのCLIに移した。CLIが揃っても、inboxとplannerが`cmux`を打てるままなら、判定と記録を通らない経路が残る。goal 81の人の決定（2026-10-01）は、揃ったらinboxとplannerのsettingsの`permissions.deny`に`Bash(cmux:*)`を置くこととした。

今、runtimeが立てるplannerも人が開くplannerもClaudeのsettings（`claude-settings.json`、roleの`permissions.deny`を含む）を持つ。inboxはsettingsを持たず、`up`がinboxのworkspaceで起動するClaudeのcommandは`--plugin-dir`とpromptだけを渡す。

## Decision

1. **いつ置くか。** ADR-t1228-1の決定2〜6のCLIと、それを使うskillとAGENTS.mdの手順が着地した後に置く。先に置くと、inboxとplannerは手順どおりの復旧ができなくなる。
2. **plannerは今のsettingsに足す。** 人が開いたplannerとruntimeが立てたplannerの両方のsettingsの`permissions.deny`に`Bash(cmux:*)`を足す。roleのdagqのコマンドの拒否と身元の環境変数の拒否は今のまま持つ。
3. **inboxにはsettingsを新しく作る。** `up`がinboxを開くとき、queueのディレクトリの下にinboxのsettingsのファイルを書き、起動のcommandに渡す。中身は`permissions.deny`だけ（inboxのroleの拒否・身元の環境変数の拒否・`Bash(cmux:*)`）とし、Stop hookとidle markerとサジェストの設定は入れない（inboxはidleで判定しないsessionで、人と対話する）。plugin（SessionStartのhookと`dagq-inbox`のskill）は今までどおり`--plugin-dir`で渡す。
4. **効かない場面の扱い。** `up`が`reused`で使い続ける既存のinboxのworkspaceと、workspaceで`claude`を打ち直したsessionにはsettingsが効かない。`up`は会話を捨てないよう、reusedのinboxを起動し直さない。代わりに、記録したinboxがguardrailつきで開かれたかを`status`か`doctor`が出し、`dagq-inbox`のskillは人に、引き継ぎの後にinboxを閉じて`up`で開き直すよう伝える。打ち直しのsessionは防がない（guardrailの限界として受け入れる）。plannerは`dagq plan`ごとに新しく開くので、置いた後に開いたplannerから効く。
5. **guardrailであってenforcementではない。** `permissions.deny`はClaude Codeのtoolの呼び出しの綴りで照合する助言的な抑止で、絶対pathの`cmux`やscriptの中の呼び出し、他のshellからの実行を止めない。hostの判定は助言的（[ADR-t728-1](2026-09-27-t728-1-trust-domains-actors-and-default-deny-capability-authorization.md)決定6）のままで、designとskillにsandboxやenforcementと書かない。目的は、指示の取り違えやprompt injectionで、記録の残らない経路にふだん流れないことである。
6. **Codexのinbox（goal 77）。** Codexのinboxを作るtaskは、同じ趣旨（`cmux`で始まるコマンドを拒み、dagqのCLIを使わせる）をCodexの手段（コマンドの規則など）で持たせる。手段が無ければ、Codexのinboxにguardrailが無いことをdesignに書き、skillの手順をCLIだけにすることで足りるとする。Codexのinboxを待たずに、Claudeのinboxとplannerに置く。
7. **範囲。** workerとjobの`cmux`は、隔離（goal 38・goal 82）が扱うので、ここでは拒まない。runtime自身（supervisor・session wrapper・`cmux notify`）のcmuxは変えない。人自身のterminal（`DAGQ_ROLE`なし）はsettingsを持たず、ADR-t1228-1が人に残した操作をそこで打つ。

settingsのファイルの名前と置き場所・denyの規則の綴り・`status`か`doctor`の欄名は実装のtaskが[docs/design/](../design/)に書く。

## Alternatives

- **repositoryの`.claude/settings.json`かuserのsettingsに置く**: 人自身がrepositoryで開くsessionや他のrepositoryにも効き、人に残した操作まで止まる。
- **inboxのsettingsにplannerと同じStop hookとidle markerを入れる**: inboxはidleを判定しないので使われず、hookの失敗だけが増える。
- **reusedのinboxを`up`が起動し直す**: inboxの会話と開いた件の引き継ぎを捨てる。人が区切りで開き直す方が安い。
- **CLIが揃う前に置く**: 復旧の手順が打てなくなり、人が毎回DAGQ_ROLEなしのterminalに移ることになる。
- **guardrailをenforcementと呼ぶ**: 迂回できるものを守りと書くと、隔離が要らないと誤る。

## Consequences

- 置いた後に開いたinboxとplannerは、`cmux`をtoolから打つと拒まれ、CLIを使う。
- 既存のinboxは開き直すまでguardrailが無く、その状態が`status`か`doctor`で見える。
- inboxがsettingsを持つので、後で他の拒否（例えば身元の環境変数）を足す置き場所ができる。

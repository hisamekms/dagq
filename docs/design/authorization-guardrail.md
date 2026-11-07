---
id: design-authorization-guardrail
type: design
title: Authorization guardrail
status: current
created: 2026-11-08
scope: runtime
related:
  - design-authorization
  - design-security
  - adr-t728-1
---

# Authorization guardrail

認可の全体の地図は[Authorization](authorization.md)。

## Claudeのpermissions.deny（guardrail）

多層防御の1枚として、runtimeがClaude Codeの設定を書くactor（worker・planner・review job・inbox。[Roles](supervisor-lifecycle/roles.md#actorの起動actorexecutor)）の`permissions.deny`に、roleのpolicyから作った規則を入れる（`src/application/execution.rs`の`permission_deny(role)`。規則の中身はそのdoc comment）。

コマンドの対応と除外の入口は`DAGQ_COMMANDS`、設定の並びは`infrastructure::adapters::headless_worker_settings`のdoc comment。

これはguardrailでenforcementではない。
Claude Codeの規則はコマンドの先頭の形しか見ないので、pathで打つ`~/.local/bin/dagq integrate`、pluginのskillが使う`"$DAGQ" ...`や`${CLAUDE_PLUGIN_ROOT}/bin/dagq ...`、subcommandの前にglobalのflagを置く`dagq --db X integrate`、`sh -c`、scriptの中からの呼び出しは通る。
拒む判定はCLIの`Authorizer`（[適用の範囲](authorization.md#適用の範囲)）がする。
どちらもhostではadvisoryで、隔離は将来のsandboxのbackend（[Roles](supervisor-lifecycle/roles.md#実行のbackendとenforcement)）が担う。

このすり抜けを塞ぐために、規則をwildcardで広げること（例: `Bash(* integrate*)`や`Bash(*dagq* integrate:*)`）はしない。
理由:

1. 誤検出が大きい。
   subcommandの名前は日常の語なので、workerが作業で打つ`git log --grep integrate`や`rg 'ready'`なども拒まれ、作業が止まる。
2. 広げてもenforcementにならない。
   `sh -c`・script・変数に入れたcommandの名前からは通るので、守れる範囲は少ししか増えない。
3. 拒む判定はCLIの`Authorizer`がすでにする。
   guardrailは「間違いで打つのを先に止める」ためのもので、pathやglobal flagの形で打った呼び出しもCLIが拒み、`authorization_denied`に記録する。

規則の形は`Bash(dagq <command>:*)`と識別の変数の書き換えのままにする。

Codexのinboxは同じ形（`deny_forms`）をPreToolUseのhook（`codex_inbox::guard`）で先頭だけ照らし、包んだ形（`&&`・`;`・パイプ・`sh -c`）はCLIの`Authorizer`が拒む。
trustされないhookは動かず、起動の記録では動くか分からない。
`status`と`doctor`はClaudeのinboxをsettings、Codexのinboxをhookを渡した記録で判定し、動いた記録を分けて出す。

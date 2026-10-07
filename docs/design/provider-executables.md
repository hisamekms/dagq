---
id: design-provider-executables
type: design
title: Provider executables
status: current
created: 2026-10-07
scope: provider
tags:
  - runtime
  - provider
related:
  - adr-t2079-1
  - adr-t813-3
  - design-provider-lifecycle
---

# Provider executables

runtimeがprovider（Claude Code・Codex）を起動するpathの持ち方と、そのpathが無いときの解決し直し。
providerは自分の更新で`versions/<version>`の実体を入れ替え、古い実体を消す（[ADR-t2079-1](../adr/2026-10-07-t2079-1-start-providers-by-their-link-and-find-a-gone-one-again-by-name.md)）。

| 知りたいこと | コードの入口 |
| --- | --- |
| 起動するpathの解決 | `adapters::provider_executable_on`（Claude Codeは`provider_executable`、Codexは`codex::executable_on`） |
| 入口の解決し直し | `adapters::claude_at_entry`・`codex::codex_at_entry`（`provider_at_entry`） |
| 起動の時点の解決し直し | `AgentProvider::relocated_executable`、`HostActorExecutor`の`spawn_agent` |

約束と落とし穴:

- providerを起動するpathはsymlinkを辿らずに持つ。
  supervisorの持つpath、`up`・`install`・自動更新・releaseが登録・execで引き継ぐargv、runner・planner-session・jobに渡す`--claude` / `--codex`はどれもsymlinkのまま。
  versionの記録（`claude_version`）だけが実体を読む。
- runnerのturn・planner-session・jobのagentが実行ファイルを見つけられず、与えたpathが無いときは、providerの名前（`claude` / `codex`）をPATHで解決し直して1回だけ起動し直す。
  そのことをrunの、runの無いactorはqueueの`provider_executable_relocated`に記録する。
  このeventはAIのactorの起動を持つhost運用のもので、`provider_*`の他の種類（実行と着地）と違う（[Architecture](architecture.md#host運用)）。
  名前で見つけたものが同じinstallの別のversion（実体が、消えたpathの残っている最も近い祖先の下）のときだけで、別のinstallのproviderを代わりにしない（`adapters::replaces`）。
  名前でも見つからなければ今までどおりの起動の失敗で、切り替えと控えの判定（[Provider lifecycle](provider-lifecycle.md#使えないproviderからの切り替え)）に進む。
- supervisorの入口と、providerのpathを受け取るコマンド（`up`・`install`・自動更新・release・observer・スループットの見直し）も、与えたpathが無ければ名前で解決し直して続け、logに残す。
  前のbinaryが実体のpathで登録したargvでも起動し直せるようにするため。
  名前でも見つからなければ今までどおり入口で失敗し、`--no-claude`のときは解決しない。

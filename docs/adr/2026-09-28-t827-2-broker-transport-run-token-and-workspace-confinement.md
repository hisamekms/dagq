---
id: adr-t827-2
type: adr
title: brokerは127.0.0.1だけのHTTP+JSONで話し、supervisorがclaimのときにqueueの鍵で署名したrunごとのtokenを発行して、runの終わりに失効させ、long-livedのcontainerにはqueueのrunsとgitの共通dirを同じ絶対パスでmountしてrunごとの閉じ込めはtokenのworkspaceでbrokerが行い、gitはpushを持たない
status: superseded
created: 2026-09-28
updated: 2026-09-28
accepted_on: 2026-09-28
superseded_by: adr-t2113-1
superseded_on: 2026-10-08
amended_by:
  - adr-t840-1
owners:
  - hisamekms
tags:
  - runtime
  - security
  - broker
related:
  - adr-t827-1
  - adr-t827-3
  - adr-t827-4
  - adr-t728-1
  - adr-t728-2
  - design-broker
---

# ADR-t827-2: brokerは127.0.0.1だけのHTTP+JSONで話し、supervisorがclaimのときにqueueの鍵で署名したrunごとのtokenを発行して、runの終わりに失効させ、long-livedのcontainerにはqueueのrunsとgitの共通dirを同じ絶対パスでmountしてrunごとの閉じ込めはtokenのworkspaceでbrokerが行い、gitはpushを持たない

> **置き換え済み（2026-10-08）**: このADRの決定は現在有効ではない。現行の決定は[ADR-t2113-1](2026-10-08-t2113-1-remove-the-resource-broker.md)を読む。

## Context

brokerはPodmanのcontainerで常駐し（人の決定、2026-09-27）、hostのworkerがrunごとにそれを使う。macOSのPodmanではunix socketのbind mountが安定しない（goal 38の調査）。long-livedのcontainerにはrunごとにmountを足せず、Gitのworktreeはmainの`.git/worktrees`にある共通のdirを要り、worktreeの`.git`ファイルは共通dirの絶対パスを指す。計画書3.3は「brokerはactiveなrunのworkspaceだけを見る」とした。着地とpushは信頼するIntegratorだけが行う（ADR-t728-2）。

## Decision

1. **transportはHTTP+JSONで、127.0.0.1だけ。** hostへのpublishは`127.0.0.1:<port>`だけにし、LANのinterfaceでは待ち受けない。pathは`/v1/<backend>/<op>`で、backendは`fs`・`process`・`git`。errorは構造化し、codeは`unauthorized`・`capability_denied`・`workspace_violation`・`timeout`・`output_limit`・`backend_error`・`invalid_request`の7つに限る。未知のop・未知の欄は`invalid_request`で拒む。
2. **tokenは信頼する制御側だけが発行する。** supervisorがclaimでworkerを起こすとき（とresumeのとき）にrunごとのtokenを発行する。AI actorがtokenを作るコマンドは持たせない。tokenはqueue dirの鍵で署名し（対称の署名。鍵はqueueごとで、brokerには読み取り専用でmountする）、claimsはactor_id・task_id・run_id・workspace（runのworktreeの絶対パス）・capabilities・iat・expを持つ。workerは自分のrunのtokenだけを持ち、envではtokenの値ではなくファイルの場所を渡す（envの値はcmuxのargvに出るため）。tokenのファイルはcontainerにmountしないqueue dirの場所に置く（runsの下に置くと、`process.exec`から他のrunのtokenを読めるため）。hostでは同じユーザーのプロセスが鍵もtokenも読めるので、tokenの偽造はhostでは防げない（ADR-t728-1決定6の助言的の範囲）。「AI actorにtokenを作るコマンドを持たせない」は誤りを止めるためのもので、境界ではない。
3. **期限と失効。** tokenは期限を持ち、supervisorは生きているrunのtokenを期限の前に発行し直す。runの終わり（着地・失敗・cancel・leaseの喪失・recover）とresumeでの発行し直しで古いtokenを失効させる。brokerはqueue DBを見ないので、失効はsupervisorが書きbrokerが読み取り専用で読む「有効なtokenの印」で伝え、印の無いtokenは拒む（supervisorが落ちても、印の消えたtokenが生き返ることはない）。
4. **fail closed。** 署名が合わない・期限切れ・印が無い・未知のcapabilityを含む・claimsが欠けたtokenは`unauthorized`で拒む。opが要るcapabilityをtokenが持たなければ`capability_denied`。
5. **mountと閉じ込め。** containerには、queueの`runs` dirと、repositoryのgitの共通dirを**hostと同じ絶対パス**でmountする（worktreeの`.git`ファイルの絶対パスがそのまま解決するように）。gitの共通dirのうちhost側でコードを走らせうる`config`と`hooks`は、その上に読み取り専用で重ねてmountする（containerからhookや設定を書かせ、supervisor・Integrator・人のgitでhostのコードが走ることを防ぐ）。それに鍵と有効な印（読み取り専用）とauditの置き場を足し、それ以外は何もmountしない。`$HOME`・`~/.ssh`・`~/.aws`・Podman / Dockerのsocket・queue DB・main checkoutの作業ファイル・他のqueueはmountしない。runごとの閉じ込めは、mountではなくtokenのworkspaceでbrokerが行う。fsのopは`..`・workspaceの外の絶対パス・symlinkの逃げ・worktreeの`.git`を`workspace_violation`で拒む。
6. **計画書3.3からの逸脱。** 1つのcontainerが同じqueueの全てのrunのworktreeとgitの共通dirを見るので、「activeなrunのworkspaceだけ」はmountでは成り立たず、brokerのコードの判定で成り立たせる。特に`process.exec`で走ったプロセスは、mountされた他のrunのファイルを読めうる。Phase 1ではexecのプロセスはbrokerと同じuidで動くので、他のrunのworktreeとreceiptの読み書き、mountした鍵と有効な印を読んでのtokenの偽造、auditの書き換え、他のbranchのrefの書き換えもできうる。Phase 1はbrokerの契約を証明するもので隔離を謳わず（hostのworkerはもともとこれら全てをhostで直接できる。ADR-t728-1決定6）、これらを既知の制限として記録する。execのallowlistの既定は空にし、有効にするのは使い捨てのrepositoryの検証だけにする。workerをcontainerにする段（Phase 3以降）では、runごとのmount（runごとのcontainerか、exec時のmountの分離）と、execをbrokerの秘密（鍵・印・audit）に届かない別のuidで走らせることで閉じ込めを作り直し、この逸脱を解く。
7. **gitはpushを持たない。** brokerのgitのopは、run branchの読み書き（status・diff・add・commit・logの類）だけにし、push・fetch・remoteの変更・credential helperの操作・他のrefの更新を持たない。commitはHEADがそのrunのbranchのときだけ行う。hookと資格情報は使わず、transportを閉じた設定で走らせる。`process.exec`で`git`を走らせることは設定に関わらず拒む。containerには上流の資格情報を置かない。
8. **`process.exec`の上限はserverが強制する。** timeoutと出力の上限はserver側で強制し、超えればプロセスを止めてerrorにする。envは許したnameだけを通し（allowlist）、それ以外の値は渡さない。走らせてよいプログラムもallowlistで絞る。

path・error codeとHTTPのstatusの対応・tokenの書式・claimsの欄名・期限の値・印とファイルの置き場・mountの一覧・gitの設定・exec・envの既定のallowlistは[Broker](../design/broker.md)に書く。

## Alternatives

- **unix socket**: macOSのPodmanのbind mountで安定しない。
- **runごとにbrokerのcontainerを起こしてmountを絞る**: 人の決定（long-lived）に反し、runの起動ごとにcontainerの起動が要る。Phase 3以降の閉じ込めの候補として残す。
- **brokerにqueue DBを読ませてrunの生死を判断する**: queue DBをcontainerに見せることになり、境界が崩れる。
- **公開鍵の署名**: 発行者と検証者が同じhostの信頼する側にあり、Phase 1では鍵の配り方が単純な対称の署名で足りる。
- **tokenをenvの値で渡す**: cmuxのargvに出て、他のユーザーからも見える。

## Consequences

- 同じqueueのworktreeが1つのcontainerに見えるので、閉じ込めはbrokerのコードの正しさに依存する。testでworkspaceの外・別のrun・symlinkの逃げを網羅する必要がある。
- supervisorはtokenの発行・更新・失効の責任を持ち、runの終わりの全ての経路で印を消す必要がある。
- `process.exec`はallowlistの外のプログラムを走らせられず、使い捨てのrepositoryの代表のtaskは軽いものに限られる。

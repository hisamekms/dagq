---
id: adr-t827-4
type: adr
title: workerはbrokerをclientのMCPの道具で使い（preferredでは組み込みの道具も残す）、brokerの全ての操作をqueue dirのauditにtokenと秘密なしで残し、modeはdisabled・preferred・requiredでrepositoryの方針はdagq.toml・hostの資源はhost.tomlに置き、resource brokerはADR-t728-1の助言的なhostとgoal 38のqueue serviceとは別の層にする
status: accepted
created: 2026-09-28
updated: 2026-09-28
accepted_on: 2026-09-28
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
  - adr-t827-2
  - adr-t827-3
  - adr-t728-1
  - adr-t728-2
  - adr-0049
  - adr-0051
  - design-broker
  - design-security
---

# ADR-t827-4: workerはbrokerをclientのMCPの道具で使い（preferredでは組み込みの道具も残す）、brokerの全ての操作をqueue dirのauditにtokenと秘密なしで残し、modeはdisabled・preferred・requiredでrepositoryの方針はdagq.toml・hostの資源はhost.tomlに置き、resource brokerはADR-t728-1の助言的なhostとgoal 38のqueue serviceとは別の層にする

## Context

計画書はworkerを`std::fs`を呼ぶRustのコードと見て`WorkerResources`を置いたが、dagqのworkerはcmuxのClaude Codeなので、brokerを使う手段はClaude Codeの道具になる（goal 58の計画書との違い）。人の決定（2026-09-27）で、Claude CodeのworkerはMCPの道具でbrokerを使い、同じclientのCLIは人の診断・test・スモーク用にする。brokerの操作は記録が要り、tokenと秘密とenvの値は残さない（goal 58のconstraints）。既定はdisabledで、この repositoryの本番queueでは有効にしない。ADR-t728-1はhost実行を助言的とし、予約のcapability（`reserved.filesystem_*`など）を誰にも与えない。goal 38（draft）の「broker」は実行側からqueue serviceへの出口の意味で、名前が重なる。task 738は`status`と`doctor`にactorごとのbackendとenforcementを出す。

## Decision

1. **workerの道具はMCP。** supervisorはbrokerを使えるrunのworker（とresume）に、clientのMCP serverを起動の設定（`--mcp-config`）で渡す。道具はfsの読み・書き・置換・一覧、`process.exec`、run branchのgitで、置換はClaude Codeの組み込みのEditと同じold / newの形にする。`preferred`では組み込みの道具（Read・Edit・Write・Bash）も残し、promptでbrokerの道具を優先させる。組み込みの道具を拒むのは`required`（Phase 2）の仕事。
2. **auditは全ての操作をbrokerが残す。** brokerは受けた全ての要求（拒んだものを含む）を、run_id・task_id・actor・op・capability・workspaceの中の対象・結果（成功かerror code）・所要時間つきで、queue dirのbrokerのaudit（日ごとのJSON lines）に残す。token・署名・ファイルの中身・diff・execの出力とstdin・envの値・commit messageは残さない。execの引数はそのままでは残さず、プログラム名と照合用のhashだけにする。auditはbrokerのファイルが正で、Phase 1ではqueue DBに取り込まない（brokerはqueue DBを見ない。[ADR-t827-2](2026-09-28-t827-2-broker-transport-run-token-and-workspace-confinement.md)決定3）。dagqは読み取りのコマンドでauditを読み、run・時刻で絞れるようにする。Phase 1ではexecのプロセスとhostのworkerがauditのファイルを書き換えうる（ADR-t827-2決定6）ので、auditは記録で、改ざんへの耐性は持たない。token の発行と失効はsupervisorがqueueのeventに残す（tokenの値は残さない）。
3. **modeは3つ。**
   - `disabled`（既定）: brokerを起こさず、tokenを発行せず、workerは今までどおり。
   - `preferred`: brokerを起こし、使えるときはworkerにtokenとMCPの道具を渡す。使えない（不健康・版の食い違い・machineが起動できない）ときは、claimを止めずworkerは道具なしで動き、runのeventと通知に残す。
   - `required`: 組み込みの道具を拒み、brokerが使えなければclaimしない強制の形。Phase 2のgoalが実装し、それまでは設定されたら起動を拒む（podmanのbackendと同じく、黙って弱い形に戻さない）。
4. **設定の置き場所。** modeとexecのallowlistと上限は、repositoryの方針としてmain checkoutの`dagq.toml`の`[broker]`に置く（`[run.env]`と同じくmain checkoutの作業ファイルを読む）。podmanの場所・machineとcontainerの資源・portはhostの事情として`host.toml`の`[broker]`に置く。`host.toml`はそのhostでbrokerを使わないと決めてmodeを`disabled`に落とせるが、modeを上げることはできない。この repositoryの`dagq.toml`には`[broker]`を置かない（本番queueはdisabled。`[broker]`を知らない固定バイナリは未知の表で止まるので、足すなら固定バイナリが対応してから）。
5. **ADR-t728-1との関係。** host実行は助言的のまま変わらない。`preferred`のworkerはbrokerを迂回して組み込みの道具やhostのファイルを直接使えるので、`status`と`doctor`のactorの`backend: host`・`enforcement: advisory`（task 738）は変えず、brokerの状態は別の欄で出す。brokerのcapability（fs・process・gitのop）はqueueの操作のcapability（`Capability`）とは別の名前空間で、protocolが持ち、dagqのroleからの写し（Phase 1ではworkerだけ）で与える。予約の`reserved.filesystem_*`は誰にも与えないまま残し、brokerのtokenはそれを与えない。予約のcapabilityの強制を「brokerを通したときだけ許す」形で埋めるのは、`required`とworkerのcontainerの段（Phase 2以降）が決める。
6. **goal 38との関係。** この resource brokerはfs・process・gitを仲介し、queueのAPIを持たない。goal 38のbrokerはqueue serviceへの出口で、別物である。文書ではこの brokerを「resource broker（dagq-broker）」と書いて区別する。containerのworkerのqueueの操作（ask・noteなど）はgoal 38か後のgoalが扱い、resource brokerに足さない。
7. **brokerの操作はADR-t728-2の境界を越えない。** 着地とpushはIntegratorだけで、brokerはrun branchの外のrefを動かさない（ADR-t827-2決定7）。

MCPの道具の名前と引数、設定のkeyと既定値、auditの欄と置き場所と保持の日数、dagqの読み取りのコマンド、eventのkind、`status`と`doctor`の欄は[Broker](../design/broker.md)に書く。

## Alternatives

- **組み込みの道具をhookで横取りしてbrokerに送る**: Claude Codeのhookの仕様に強く依存し、失敗が見えにくい。MCPの道具は明示的で、clientのCLIとtestを共有できる。
- **`preferred`でbrokerが使えなければclaimを止める**: disabledとの間の段で運用を止める理由が無く、止めるのは`required`の意味。
- **auditをqueue DBに書く**: brokerにqueue DBを見せることになる（ADR-t827-2）。
- **modeをhost.tomlにも置けて上げられる**: repositoryの方針がhostの設定で変わり、本番queueで意図せず有効になりうる。
- **`reserved.filesystem_*`をbrokerのcapabilityに流用する**: 予約の名前は自プロセスのアクセスを強制するsandboxのためのもので、hostでは強制できない。混ぜると隔離を謳うことになる。

## Consequences

- `preferred`は契約の証明と移行の段で、安全の境界ではない。記録と文書でそう示し続ける。
- MCPの設定と道具の説明をworkerのpromptとsettingsに足す必要があり、Claude Codeの`--mcp-config`の仕様に依存する。
- auditはqueueごとのファイルで、KPIやeventと突き合わせるには読み取りのコマンドを通す。

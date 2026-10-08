---
id: adr-t827-1
type: adr
title: resource brokerをroot package（dagq）はそのままにcrates/の3つのcrate（protocol・server・client）に分け、dagqはprotocolだけに依存し、clientはdagqの隣に同じbuildで置き、brokerのimageはdagqと同じsourceからbuildして、版が食い違えばbrokerを使わない
status: superseded
created: 2026-09-28
updated: 2026-09-28
accepted_on: 2026-09-28
superseded_by: adr-t2113-1
superseded_on: 2026-10-08
amended_by:
  - adr-t828-1
owners:
  - hisamekms
tags:
  - runtime
  - security
  - broker
related:
  - adr-t827-2
  - adr-t827-3
  - adr-t827-4
  - adr-0030
  - adr-0073
  - adr-0076
  - adr-t618-1
  - design-broker
---

# ADR-t827-1: resource brokerをroot package（dagq）はそのままにcrates/の3つのcrate（protocol・server・client）に分け、dagqはprotocolだけに依存し、clientはdagqの隣に同じbuildで置き、brokerのimageはdagqと同じsourceからbuildして、版が食い違えばbrokerを使わない

> **置き換え済み（2026-10-08）**: このADRの決定は現在有効ではない。現行の決定は[ADR-t2113-1](2026-10-08-t2113-1-remove-the-resource-broker.md)を読む。

## Context

goal 58（2026-09-27の人の計画のPhase 1）は、fs・process・gitを仲介するresource broker（`dagq-broker`）をPodmanのcontainerで常駐させ、hostのworkerがrunごとのtokenでそれを使えるようにする。人の決定（2026-09-27）で、worker側のclientはdagqとは別のバイナリにする（将来のworkerのcontainerにdagq本体は要らず、認証の仕方も違う）。今のcrateは1つ（workspaceのmembersは`.`）で、root packageの位置に既存のパス・scripts・登録済みのtaskの`--paths`・走っているrunが依存している。installとauto-updateはdagqの1本だけをbuildして差し替え（ADR-0073決定10〜17）、releaseはGitHub Releaseとcrates.io（ADR-0030、ADR-t618-1）に出す。brokerはLinuxのcontainerで動き、hostはmacOSである。

## Decision

1. **root packageはdagqのまま。** virtual workspaceにしてdagqを`crates/`に移すことはしない（既存のパス・scripts・登録済みのtaskの`--paths`・走っているrunを崩さないため。2026-09-27の人とplannerの判断）。workspaceに`crates/`を足し、計画書5の実用のV1の形の3つのcrateに分ける。
   - protocol（lib）: DTO・capability・error・tokenのclaimsと署名と検証
   - server（libとbin `dagq-broker`）: HTTPのserverとfs・process・gitのbackend、audit
   - client（libとbin）: brokerへのHTTPのclient、workerに渡すMCPのserverのsubcommand、人の診断・test・スモーク用のCLI
   これより細かく割らない。3つともlibを持ち、バイナリの`main`は薄くする（rootの`--lib`の絞り込みがbinだけのcrateで失敗しないためと、testのため）。
2. **依存の向き。** dagq本体はtokenを発行するためにprotocolだけに依存する。protocolの依存はserdeと署名に要る最小のものに保ち、HTTPのserverとclientの依存をdagqに持ち込まない。dagqがbrokerと話す必要（health）は、clientのバイナリを子プロセスで呼んで満たす。
3. **検証の関門を書き換えない。** workspaceの`default-members`に全てのcrateを入れ、rootの`cargo test`・`cargo clippy --all-targets`・`cargo llvm-cov nextest`が新しいcrateも覆う形にする。関門のコマンドに`--workspace`は足さず、登録済みのtaskのverifyとCIのコマンドはそのまま有効にする。workspaceを作るtaskは、関門のcoverageのreportが新しいcrateを含むことを確かめ、含まなければ関門を弱めずに人に聞く。coverageの80%は新しいcrateを含めた全体で守り、podmanを要らないtest（hostのプロセスとして127.0.0.1で起こしたserver）で覆う。podmanを要るtestは`#[ignore]`で、関門とCIに数えない。
4. **runtimeのパス。** auto-updateが入れ替えを判定するruntimeのパスに`crates/`を足す。clientとimageのsourceはdagqのbuildと一緒に配るので、`crates/`の変更もruntimeの変更である。
5. **clientはdagqの隣に同じbuildで置く。** installとauto-updateは、同じcheckoutと同じbuildでdagqとclientを作り、同じ確認（build識別子の一致）と同じrenameでdagqの隣に置く（`.previous`もそれぞれに残す。rollbackは両方を戻す）。HostActorExecutorは、動いているdagqと同じbuild識別子のclientだけを使う。
6. **imageはdagqと同じsourceからbuildする。** dagqのバイナリはbrokerのimageのbuildの材料を持ち、imageはそのdagqのbuild識別子をtagにして作る。これでどのrepositoryのqueueでも、dagqと同じsourceのbrokerが動く。材料は、checkoutからのbuild（dev build）ではprotocol・serverのsourceとlockを埋め、release（crates.ioから入れたもの）ではContainerfileだけを埋めてserverを同じ版のcrates.ioのcrateからbuildする（`cargo package`は別のmanifestを持つsubdirを含めないので、publishしたdagqにserverのsourceを入れられないため）。imageのbuildはbrokerを起動するときにtagが無ければ行い、installとauto-updateでは行わない（podmanを使わないhostとbrokerがdisabledのqueueを遅くしないため）。
7. **版が食い違えばbrokerを使わない（fail closed）。** dagq・client・imageのbuild識別子が一致しないとき、dagqはtokenを発行せず、workerにbrokerの道具を渡さない。混ざった版で話させることはしない。`preferred`ではworkerはbrokerの道具なしで動き、食い違いは記録と通知になる（[ADR-t827-4](2026-09-28-t827-4-worker-mcp-tools-audit-mode-and-relations.md)）。
8. **配布。** crates.ioにはprotocol・dagq・client・serverをこの順でpublishし、版は全てのcrateで1つにそろえる（dagqはprotocolを同じ版に固定する）。serverのcrateはreleaseのimageのbuildの材料としてだけ使う。GitHub Releaseにはdagqと同じ形でclientのassetを足す。container registryにimageは出さない。releaseのupdate（ADR-t618-1）はdagqとclientを同じ版でそろえて入れる。

crateとバイナリの名前、`default-members`の書き方、依存のcrateの名前、埋める材料の形、assetの名前は[Broker](../design/broker.md)に書く。

## Alternatives

- **virtual workspaceにしてdagqを`crates/dagq`に移す**: 形は素直だが、既存のパス・scripts・登録済みのtaskの`--paths`・走っているrunを一度に崩す。
- **clientをdagqの2つ目のbinにする**: 人の決定（別のバイナリ）に反し、将来のworkerのcontainerにdagqの依存を持ち込む。
- **関門を`--workspace`に変える**: 登録済みのtaskのverifyを書き換える必要が出る。`default-members`で同じ範囲を覆える。
- **imageをinstallやauto-updateでbuildする**: podmanの無いhostとdisabledのqueueのinstallが遅くなり、失敗の原因が増える。
- **imageをregistryに出してpullする**: registryの運用と版の対応が増え、dev buildの`X.Y.Z-dev+<commit>`に合わない。
- **版が食い違っても互換の範囲で話させる**: protocolの互換の判定を先に作る必要があり、Phase 1では一致だけを許す方が単純で安全。

## Consequences

- crates.ioのpublishが4本になり、順序（protocolが先）を守る必要がある。
- imageのbuildはmachineからcrates.ioとcontainerのregistryへのnetworkを要る。
- CI（macOSだけ）はLinuxのcontainerの中のコードの経路とContainerfileをbuildもtestもしない。それはpodmanを要る`#[ignore]`のtestとスモークが確かめる。
- runtimeを変える着地ごとにbuild識別子が変わるので、brokerを有効にしたqueueはその都度imageを作り直す（本番queueはdisabledなので影響しない）。
- installの確認と差し替えの対象が2本になり、片方だけが入れ替わった状態を戻す処理が要る。

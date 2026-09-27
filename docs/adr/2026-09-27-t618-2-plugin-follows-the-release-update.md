---
id: adr-t618-2
type: adr
title: リリースの更新はバイナリを先に入れ替え、その後にinstallしたpluginを同じリリースへ上げ、どちらか片方だけが古いときも同じaskで揃える
status: accepted
created: 2026-09-27
updated: 2026-09-27
accepted_on: 2026-09-27
owners:
  - hisamekms
tags:
  - runtime
  - plugin
  - release
related:
  - adr-t617-1
  - adr-t617-2
  - adr-t618-1
  - design-supervisor-lifecycle-release-update
  - design-plugin-integration
---

# ADR-t618-2: リリースの更新はバイナリを先に入れ替え、その後にinstallしたpluginを同じリリースへ上げ、どちらか片方だけが古いときも同じaskで揃える

## Context

[ADR-t617-1](2026-09-27-t617-1-plugin-marketplace-pinned-to-release-tag.md)でpluginはこのrepositoryのmarketplaceから最新のリリースのtagを配り、versionはバイナリと同じ`X.Y.Z`になる。利用者は`cargo install --locked dagq`と`claude plugin update claude-dagq@dagq`で両方を上げる（決定5）。決定5は、外部のprojectの更新の仕組みがpluginの更新も含めることを求めている。[ADR-t617-2](2026-09-27-t617-2-installed-plugin-by-default-plugin-dir-for-development.md)で、runtimeが開くsessionは`--plugin-dir`が無ければinstallしたpluginを使う。

片方だけが進むと壊れ方が違う。新しいpluginのskillは、古いバイナリが知らないコマンドやflagを打たせうる。古いpluginのskillと新しいバイナリは、CLIの追加が互換なら動き続ける。

## Decision

1. **順序はバイナリが先、pluginが後。** [ADR-t618-1](2026-09-27-t618-1-release-update-by-ask-from-crates-io.md)の入れ替えが引き継ぎと見張りまで成功してから、jobはsupervisorの`--claude`でmarketplaceを読み直し、`claude-dagq`を同じリリースへ更新する。バイナリの入れ替えが失敗して戻したときは、pluginに触らない。
2. **pluginの更新だけが失敗しても、バイナリは戻さない。** 新しいバイナリと古いpluginは1の理由で動き続けるので、ADR-0073の`update_failed`のask（`retry` / `skip`）で、失敗の理由と手で打つコマンドを知らせる。
3. **`--plugin-dir`で動いているときはpluginに触らない。** supervisorに`--plugin-dir`が渡されていれば（pluginを開発しているrepository。ADR-t617-2決定3）、pluginの更新は行わず、結果にもそう書く。
4. **検知はpluginのversionも見て、片方だけ古いときも同じaskで揃える。** installしたpluginのversionが読めれば、バイナリが最新でpluginだけが古いときも、同じ種類のaskで「pluginだけを上げる」と聞き（聞かない設定なら上げ）、答えでpluginだけを更新する。pluginが新しくバイナリが古いときは、バイナリが最新より古いので、ADR-t618-1のaskがそのまま出て、1の順で揃う。pluginのversionが読めないときはバイナリだけで判断し、pluginの更新は入れ替えの後に試みる。
5. **開いているsessionは次の起動から新しいpluginを読む。** 引き継ぎはsupervisorのプロセスだけを入れ替え、inboxとplannerのClaude Codeのsessionは前のpluginのまま動く。headless jobと新しく開くsessionは新しいpluginで起動する。入れ替えを知らせるattentionに、常駐のsessionを開き直すと新しいpluginが効くことを書く。runtimeは常駐のsessionを勝手に閉じない。

pluginのversionの読み方とコマンドは[Release update](../design/supervisor-lifecycle/release-update.md)が持つ。

## Alternatives

- **pluginを先に上げる**: 新しいskillが古いバイナリで失敗する時間ができ、バイナリの入れ替えが失敗して戻したときに、その食い違いが残る。
- **pluginの更新が失敗したらバイナリも戻す**: 動いている新しいバイナリを捨て、引き継ぎをもう1回起こす。古いpluginと新しいバイナリは動くので、戻す利点が無い。
- **pluginは利用者に任せ、launcherのmajor.minorの警告だけにする**: ADR-t617-1決定5の求め（更新の仕組みにpluginを含める）を満たさず、patchの違いは警告もされない。
- **常駐のsessionを自動で開き直す**: 人との会話と作業の途中を切る。開き直す時機は人が決める。

## Consequences

- askへの1回の答えで、バイナリとpluginが同じリリースに揃う。
- pluginの更新はhostの`claude`の設定（installしたscope）を変える。同じhostの他のrepositoryのsessionも次の起動から新しいpluginになり、それはバイナリがhostで共有されるのと同じ範囲である。
- 実装は別のtaskで行う。

---
id: adr-t813-3
type: adr
title: Codex の worker を workspace-write の sandbox で動かし、書いてよい場所を run の worktree の git の管理 dir・objects・run branch の ref・run の dir・cargo の registry に絞り、network は開け、承認は never にし、queue には書かせず run の dir を経て supervisor が取り込み、pkill / killall は sandbox が拒み run ごとの rules が補う。設定は人の設定を変えずに起動の引数で渡す
status: accepted
created: 2026-09-28
updated: 2026-09-28
accepted_on: 2026-09-28
owners:
  - hisamekms
tags:
  - runtime
  - security
  - provider
related:
  - adr-0004
  - adr-0049
  - adr-t728-1
  - adr-t728-2
  - adr-t813-1
  - adr-t813-2
  - plan-headless-worker-spike
---

# ADR-t813-3: Codex の worker を workspace-write の sandbox で動かし、書いてよい場所を run の worktree の git の管理 dir・objects・run branch の ref・run の dir・cargo の registry に絞り、network は開け、承認は never にし、queue には書かせず run の dir を経て supervisor が取り込み、pkill / killall は sandbox が拒み run ごとの rules が補う。設定は人の設定を変えずに起動の引数で渡す

## Context

2026-09-27 に人は Codex の worker の権限を「まず spike で決める」と答えた（goal 57）。goal 57 の受け入れは、Codex の worker で cargo の build と test・run branch への commit・`dagq ask`・receipt の書き込みが通り、pkill / killall が拒まれることを求める。制約は、`~/.codex/config.toml` と `~/.claude` の人の設定を書き換えず、run ごとの設定を起動の引数で渡すこと。

spike（[headless-worker-spike](../plans/headless-worker-spike.md) の 1・2・6）の結果:

- `codex exec` の workspace-write の sandbox（macOS は seatbelt）に `writable_roots` を足し、network を開ければ、cargo（sccache あり）・registry の取得・run branch への commit・receipt・queue への書き込みが通った。`refs/heads/main`・`$HOME` への書き込みと、sandbox の外の process への signal と一覧は拒まれた。
- git の共通 dir の全体を書けると main の ref も書き換えられた。worktree の管理 dir・`objects`・`refs/heads/dagq`・`logs/refs/heads/dagq` に絞ると run branch だけが書けた。
- network が無いと sandbox が sccache の server への接続を拒み、`SCCACHE_IGNORE_SERVER_IO_ERROR` も救わず build が止まる。registry の取得にも network と `~/.cargo/registry` が要る。
- queue の SQLite は同じ dir に journal を作るので queue の dir が要るが、その下には他の run の worktree（`runs/`）がある。
- `exec resume` は最初の exec の権限を引き継がず、毎回 `-c` で渡し直す必要がある。
- `--dangerously-bypass-approvals-and-sandbox` と `--approve-for-me` は、どちらも実際には sandbox の外で走った。
- worktree の `.codex/rules` の forbidden の規則は pkill / killall を拒んだが、`sh -c` で回避できた。Codex の hook は `--dangerously-bypass-hook-trust` が無いと走らない。
- cmux の terminal では `codex` が cmux の shim に解決し、hook を注入する。

[ADR-t728-1](2026-09-27-t728-1-trust-domains-actors-and-default-deny-capability-authorization.md) 決定 6 は、host の実行は助言的で sandbox でも隔離でもないとし、sandbox は同じ capability の模型の上に強制を足すだけだと書いた。

## Decision

1. **sandbox は workspace-write。承認は never。** Codex の worker は workspace-write の sandbox で動かし、承認は never（確認が要る操作は待たずに失敗させる）にする。sandbox を外す flag（bypass）と、承認を自動の review に回す flag は使わない。
2. **書いてよい場所を絞る。** workspace（run の worktree）に加えて書いてよいのは、git の共通 dir のうち run の worktree の管理 dir・`objects`・run branch の ref とその log（`refs/heads/dagq` と `logs/refs/heads/dagq`）、run の dir（receipt）、cargo の registry だけにする。git の共通 dir の全体・main の ref・`$HOME`・queue の dir は書かせない。`/tmp` と `$TMPDIR` は cargo と rustc が使うので sandbox の既定のまま書ける。
3. **queue には書かせない。** worker が queue に書く操作（`dagq ask`）は、Codex の worker では run の dir への要求の書き込みにし、supervisor（信頼する制御側）が検査して queue に取り込む。queue の dir を書けることは他の run の worktree を書けることを意味するので、受け入れない。worker の CLI の使い方は変えない（`dagq ask` を打つ）。
4. **network は開ける。** sccache の server への接続と registry の取得のため、network を開ける。閉じる案（sccache を使わず、依存の取得を起動の前に sandbox の外で行う）は、cache を失い run の build を遅くするので採らない（[ADR-0049](0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md) の cache の共有を保つ）。
5. **pkill / killall は sandbox が拒み、run ごとの rules が補う。** 主の防御は sandbox で、外の process の一覧と signal を拒む。補助として runtime が run の worktree に pkill / killall を forbidden にする rules を置き（git には入れない）、rules を無視する flag は付けない。rules は `sh -c` で回避できるので、それだけには頼らない。拒否は `--json` に出ないので、記録が要るなら stderr から読む。
6. **設定は起動の引数で渡し、人の設定を変えない。** sandbox・`writable_roots`・network・承認は、exec と resume のたびに同じ `-c` で渡す（resume は引き継がない）。`~/.codex/config.toml`・`auth.json`・`~/.codex/rules` は書き換えず、`CODEX_HOME` も変えない（認証が変わるため）。Codex の hook は使わず、hook の trust を外す flag も付けない。runtime は `codex` を実体に解決して固定し、cmux の shim を使わない。
7. **ADR-t728-1 との関係。** Codex の worker の sandbox は、ADR-t728-1 決定 6 の言う「同じ capability の模型の上に足す強制」の最初のもので、書き込み・signal を OS が止める。ただし隔離ではない: 同じユーザーとして動き、読むことは広く（他の run の worktree や人の設定も）でき、network は開いている。`status` と `doctor` の enforcement はこれを助言的（host）とも隔離とも別の値として見せる。Claude の worker（対話・非対話）は今までどおり助言的で、permission の仕組みと settings の deny に頼る。

`writable_roots` の組み立て、rules の中身とファイル名、enforcement の値の綴り、要求のファイルの形は後続の実装 task が [docs/design/](../design/) に書く。

## Alternatives

- **git の共通 dir の全体を書かせる**: 設定は簡単だが、worker が main の ref を書き換えられ、着地を integrator だけにする決定（ADR-t728-2）を sandbox が守れない。
- **queue の dir を書かせる**（spike の (a)）: `dagq ask` がそのまま通るが、他の run の worktree と queue DB を書ける。
- **queue を別の dir に分ける**（spike の (b)）: 全 queue の配置の変更と migration が要り、Codex の worker のためだけには重い。
- **network を閉じる**: sccache が使えず、run の build が遅くなる。依存を足す task は起動の前の取得が要る。
- **bypass か approve-for-me**: sandbox の外で走り、`$HOME` や main に書ける。
- **run ごとの `CODEX_HOME` で設定と rules を渡す**: 認証（`auth.json`）も変わり、人のログインが使えない。

## Consequences

- Codex の worker は run branch と run の dir の外に書けず、他の process を止められない。Claude の worker より強い強制になる。
- queue への書き込みが supervisor を経るので、ask の開く時刻は supervisor の次の pass まで遅れる。
- network を開けるので、外への送信は止めない。読むことも止めないので、秘密の読み出しは sandbox の範囲外で、隔離は後の goal（ADR-t728-1 決定 6 の sandbox）に残る。
- git の依存（`~/.cargo/git`）と `git gc --auto` の G 直下への書き込みは未測定で、要れば実装の task が `writable_roots` を足すか design に書く。

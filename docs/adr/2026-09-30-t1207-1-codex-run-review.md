---
id: adr-t1207-1
type: adr
title: 通常 run の review を Codex の read-only job でも実行する
status: accepted
created: 2026-09-30
updated: 2026-10-02
accepted_on: 2026-09-30
amends:
  - adr-t1063-1 decision 1
  - adr-t1063-1 decision 4
owners:
  - hisamekms
tags:
  - runtime
  - review
  - provider
related:
  - adr-t1204-1
  - design-supervisor-lifecycle-review
  - design-supervisor-lifecycle-actor-model
---

# ADR-t1207-1: 通常 run の review を Codex の read-only job でも実行する

## Context

Claude を使わない運転で通常 run の review が毎回 `provider_disabled` となり、手動の `approve_landing` に止まる。手動の差し戻し理由は `review_finished` に残らず、worker の resume に正しく届かない。Codex の goal review は既に read-only の headless job として動く。

役割を Codex に移す順は ADR-t1063-1 決定 7（goal review → observer → plan review → review → 復旧）だったが、ADR-t1204-1 決定 3 がそれを緩め、goal review の後に着地を自動化する review を先に移すことを許した。人は 2026-09-30 に、task 1206 の後に review を Codex に対応させると決めた。この ADR はその順で review を移す。

## Decision

1. `[roles.review] provider = "codex"` を許し、通常 run の `review.md` と既存の verdict prompt を worktree の `codex exec --json --sandbox read-only` に渡す。job は `review-job` の権限だけを持ち、worker の worktree を変更しない。
2. Codex の最後の agent message を既存の `ReviewVerdict` として読み、`pass`・`revise`・`concern` と理由を Claude の review と同じ `review_finished`、差し戻し、着地の経路へ渡す。job の provider と session を記録する。
3. `[roles.review]` に provider を書いた review は、goal review と同じく ADR-t1063-1 決定 4・5 に従う。その provider が使えない（実行ファイルが無い・起動できない・認証・利用上限）ときは、Claude を使える運転ではもう一方の provider で起動し直し、Codex の控えは ask を開かない。一般の失敗（verdict が読めない・非 0 の終了・時間の上限）は切り替えず、今までどおり手動 review に渡す。
4. `--no-claude`（ADR-t1204-1）では review を Claude に戻さない。Codex が使えないか止まって切り替え先の無い review は、ADR-t1063-1 決定 4 の「控えで待つ」に代えて、理由を付けて手動 review（`approve_landing`）に渡す。待つと run が lease と slot を持ったまま Codex の控えが解けるまで止まり、ADR-t1204-1 決定 2 が受理した成果を手動の review に渡すと決めたためである。`[roles.review]` に provider を書かない repository の既定は Claude のままで、`--no-claude` では手動 review に渡す。

## Consequences

ADR-t1204-1 決定 2 の「Codex に対応済みの役割は設定に従って動かし続ける」に通常 run の review が加わるだけで、その決定は変えない。ADR-t1063-1 決定 5（控えは provider ごと）は run の review にもそのまま効く。本番の `dagq.toml` に `[roles.review]` を足すのは対応バイナリに更新した後とする。plan review や recovery など未対応の役割は引き続き手動対応する。

---
id: plan-headless-worker-measurement
type: plan
title: 本番の queue での Claude の非対話の worker と対話の worker の比較と、既定を切り替えるかの推奨
status: active
created: 2026-09-30
updated: 2026-09-30
owners:
  - hisamekms
tags:
  - measurement
  - worker
  - provider
related:
  - plan-headless-worker-spike
  - adr-t813-1
  - adr-t813-2
  - adr-0051
---

# 本番の queue での Claude の非対話の worker と対話の worker の比較と、既定を切り替えるかの推奨

goal 57（task 821）の測定。Claude の worker を非対話の経路（[ADR-t813-1](../adr/2026-09-28-t813-1-headless-worker-path.md)）で動かした本番の run を、同じ期間の対話の run と比べ、Claude の既定を非対話に切り替えるかを推奨する。この task は測って書くだけで、既定は変えていない（`add` で経路を指定しない task は今も Claude の対話の経路）。

## 結論

**推奨は出さない。** 本番の queue に非対話の Claude の run は **0 本**で、task の定めた 5 本に **5 本足りない**。Codex の run と provider の切り替えも 0 件だった。下の「測定を続けるには」のとおりに planner が非対話を指定した run を作り、5 本以上が着地してから、同じ手順でこの文書を書き直す（別の task）。

## 読んだ範囲と方法

- 読んだ時点: 2026-09-30（JST）。最後の claim は 2026-09-29T16:48Z、kpi の窓の終わりは 2026-09-29T17:46Z
- 期間: 非対話の経路が本番に入った task 815 の着地（2026-09-27T19:41Z）の後。数字は切りのよい 2026-09-28T00:00Z（UTC）から
- 固定バイナリ `~/.local/bin/dagq`（`0.4.0-dev+8bb2fcc`）で、状態を変えないコマンドだけを使った

```sh
dagq stats --full                                   # 全 run（627 本）。runs[].provider / actual_provider / route / turns
dagq stats --since 2026-09-28T00:00:00+09:00 --full # 183 本（JST の起点。下の値は --full の runs を UTC の claimed_at で絞った）
dagq events --all --full --kind turn_started        # 0 件
dagq events --all --full --kind provider_switched   # 0 件
dagq events --all --full --kind run_claimed --since 2026-09-28   # 137 件、全て provider=claude・worker_mode=interactive
dagq kpi --since 2026-09-28T00:00:00Z --by provider --by route --area runtime
```

task の description は `kpi --kind runtime` を名指すが、task の kind は task 984 で消えたので（[ADR-t980-1](../adr/2026-09-29-t980-1-classify-runs-by-declared-change-and-diff-derived-area.md)）、同じ層を `--area runtime` で読んだ。

## 非対話で動かした run の一覧

| task | run | change / area | 経路 | 結果 |
|---|---|---|---|---|
| （無し） | | | | |

- `stats --full` の 627 本の `route` は `interactive`（167 本）か `null`（task 819 の前の 460 本。全て対話）だけで、`headless` は無い
- `actual_provider` は全て `claude`、`provider_switches.count` は 0
- `turn_started` の event（非対話の turn の始まり）は queue に 1 件も無い
- goal 57 自身の task（812〜820、862、863、892、898）の run も全て対話の経路だった。材料の作り方（planner が task 814・815・817 の着地の後に、goal 57 の残りの task と小さな task に `edit` で非対話を指定する）は行われていない

## 同じ期間の対話の run（比べる相手の基準値）

非対話の側が無いので比較はできない。次に測るときの基準として、2026-09-28 以降の対話の run の値を残す。

`dagq kpi --since 2026-09-28T00:00:00Z --by provider --by route --area runtime`（135 run、うち着地 130）の要約（中央値。`provider=claude` と `route=interactive` の層は `all` と同じ値）:

| 値 | all | area=runtime |
|---|---|---|
| phase.work（秒） | 1179（n=130） | 1452（n=82） |
| phase.startup（秒） | 935（n=130） | 1114（n=82） |
| session_active.worker（秒） | 1133（n=134） | 1345（n=82） |
| resumes_per_run | 0.274（n=135） | 0.366（n=82） |
| lead_time（秒） | 38069（n=130） | 41177（n=82） |
| asks_per_landing | 0.438（n=130） | — |

`stats --full` の runs（2026-09-28T00:00Z（UTC）以降に claim され着地した 128 本）から求めた値:

- claim から着地まで: 中央値 2074 秒
- needs_session の回数: 平均 0.33 / run、resume: 平均 0.23 / run
- `integrate_attempts` が 2 以上（integrate の検証の失敗などでやり直した）: 20 本
- worker の token: total の中央値 約 625 万（cache read を含む）、output の中央値 約 3.3 万。`cost_usd` は対話の経路では記録されない（非対話は turn ごとに `total_cost_usd` を取る。task 819）

非対話で起きた問題（出力の途絶え＝`turn_silence_secs` の停止、permission の拒否＝`stalled`/`permission_denied`、催促＝`HEADLESS_NUDGES`）は、非対話の run が無いので 0 件で、観測もしていない。

## 交絡の扱い（次の測定で守ること）

[ADR-0051](../adr/0051-kpi-time-series-report-and-push.md) の前後比較の規則に従い、非対話の run が揃ったら次のように比べる。

- **同じ期間で比べる**: 経路は時刻で切り替わらない（task ごとの指定）ので、前後比較（`--compare`）ではなく、同じ窓の中の `--by route` の層を並べる。窓の中の印（build・`--parallel`・Claude Code の version・`[run.env]` の変化）は `kpi` の `marks` に出るので、非対話の run の期間に印があれば、その前後で分けて読む
- **task の種類と大きさ**: `--area runtime` の層だけで work・startup・token を比べ、docs だけの run と混ぜない。`--by change` でも分け、`fix`・`test`・`refactor` の小さな task が非対話の側に偏っていないかを書く。plan review の予測（`prediction.size`）が揃うなら大きさ S / M で分けて並べる
- **host の負荷**: run の `load_band`（claim の時点の load）と `claim_parallel` を両方の側で数え、負荷の帯の分布が違えば帯ごとの中央値を並べる
- **人の答え待ち**: lead_time と claim から着地までには人の答え待ち（ask・approve_landing）が入るので、`land_phases.ask` を除いた値も並べる
- **少ない本数**: `kpi` の判定は小さな標本（`small_sample`）で judged にならない。5〜10 本では中央値と個々の run の値を並べて示し、差を断定しない

## 測定を続けるには（planner 向け）

足りない本数: 非対話の Claude の run が **5 本以上**（`area=runtime` を含めて）。次の目安で、planner が `add --headless`（draft / submitted のうちは `edit ID --headless`）を付ける（[provider の選び方](../../plugins/claude-dagq/skills/dagq/reference/provider.md)。人が Claude の非対話を言ったときだけ付ける規則なので、この測定のために付けることを人が了承していることを task の context に書く）。

- runtime の小さな task（`fix`・`test`・`refactor`、plan review の予測で S〜M）を 3 本以上。work・startup・resume を対話と比べる主な材料になる
- docs か plugin の文書だけの task を 1〜2 本。startup と token の差（画面の判定や Stop hook が無いぶん）を見る
- 人の答えが要りそうな task（worker_question を出しやすいもの）を 1 本。answer が次の turn として届く流れと ask の数を見る
- 大きな task（L）と、`[e2e] paths` に触れる task は最初は避ける。途絶えの停止（900 秒）や turn の上限（14400 秒）と、e2e の長い待ちの組み合わせで失敗すると、経路ではなく task の性質を測ることになる

集めた後は、この文書の「同じ期間の対話の run」の表に非対話の列を足し、非対話で起きた問題（`turn_finished` の `outcome`・`failure`、`stall_*` の reason、催促の回数、`permission_denials`）を run ごとに書き、推奨（切り替える・切り替えない・条件付き）と、切り替えるなら要る変更（既定の経路を決める ADR と、`add` の既定・`dagq.toml` の設定・plugin の文書）を書く。

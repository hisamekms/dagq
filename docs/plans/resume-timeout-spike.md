---
id: plan-resume-timeout-spike
type: plan
title: スパイク：人の send_back・review の差し戻しの後の resume が resume_timeout で receipt 無しに終わる原因（T1〜T4）
status: completed
created: 2026-10-28
owners:
  - hisamekms
tags:
  - planning
  - measurement
  - resume
related:
  - plan-review-rounds-resume-spike
---

# スパイク：resume が resume_timeout で receipt 無しに終わる原因

goal 228 の Spike。[review-rounds-resume-spike.md](review-rounds-resume-spike.md) の表 B（以下「表 B」）で、人の send_back の後の数える resume 112 件のうち 25 件が約 1 時間を越えて `exit_requested`・`session_closed` になり、receipt 無しで終わった。その原因を、仮説 T1〜T4 に 1 件ずつ割り当てて判定する。

## 結論

- T1（resume の turn で手元の検証を流し直す）: **holds**。割り当てた 23 件のうち 22 件（95.7%）。
- T2（[fresh_session] で切り替えた新しい session）: **unresolved**。その resume で切り替えた 6 件はどれも 2026-10-19 の件で、時間の記録が無く、読み直しと検証のどちらが時間を使ったかを示せない。閾値を 400000 に上げた後の 23 件では、その resume で切り替えた件は 0。
- T3（background の待ち・host の負荷）: **unresolved**。background の待ちは 0 件で成り立たない。host の負荷で検証のコマンドが遅くなった分は、記録が検証の時間と分けて持たず、件ごとには判じられない。見積もりでは検証の時間の中央値 8.7%（最大 23.0%）で、それを除いても T1 の 21 件はどれも検証が最大のまま。
- T4（その他）: **does_not_hold**。1 件（host の停止）。
- 判じられない: 7 件（T1 か T2 が 6 件、記録の欠けが 1 件）。
- 主の仮説: **T1**。判じられない 7 件がすべて T1 以外でも 22 / 30 = 73.3% で過半のまま。T3 の host の負荷の見積もりの分を除いても割り当ては変わらない。
- 人の send_back の後の 25 件は表 B と一致した。同じ窓・同じ終わり方は、review の差し戻しの後で 5 件、verification の失敗の後で 0 件、その他で 0 件。
- 推奨: resume の turn で流す検証の範囲を絞る task（下の「推奨」）。resume_timeout は延ばさない（検証の時間の多くが、規則が要らないとする流し直しと全体の IT のため）。

## 環境

- 期間 W = [2026-10-19T00:00:00.000Z, 2026-10-27T12:00:00.000Z)（UTC、半開区間、`resume_started` の `created_at` で切る）。表 B と同じ。
- main の commit: `fd801cb225eedcca881d535bad7ebd5154aa785e`（この run の base）。
- dagq のバイナリ: `~/.local/bin/dagq`、`dagq 0.4.0-dev+e084df48285edc6284f506060b94dd28d4b416b6`。
- python 3.9.6（`python3 -I`）、macOS 14.7.4（arm64）。
- 読んだのは本番 queue の event と timeline（読むだけのコマンド）と、各 run の dir の wrapper の log（`session-resume-<attempt>.log`。tool の呼び出しの 1 行の要約で、時刻を持たない）。数えた script と中間の JSON は repository に置かない（run の `$TMPDIR` に置き、run の後に消える）。

## 打ったコマンド

```sh
# 数える resume と対応する resume_finished（W の前後を含めて読み、W で切る）
dagq events --full --limit 1000 --kind resume_started --kind resume_finished --since 2026-10-18T15:00:00.000Z --until 2026-10-29
# 各件の run の event と timeline（gap の理由・重い command）
dagq events --full --run <RUN> --all --limit 5000
dagq timeline <RUN> --gap 300
# 区間に重なった他の run の着地の検証・e2e の区間（phase が landing・e2e の間）と、他の session の重い command
dagq events --full --limit 5000 --kind run_phase_changed --since 2026-10-18T18:00:00.000Z --until 2026-10-27T16:00:00.000Z   # --after で頁を送る
dagq events --full --limit 5000 --kind session_exited    --since 2026-10-18T18:00:00.000Z --until 2026-10-27T16:00:00.000Z
# 区間が 4 時間になった 1 件（task 2412）の間の queue 全体の event
dagq events --all --limit 2000 --since 2026-10-27T07:15:00.000Z --until 2026-10-27T10:15:00.000Z
```

## 数え方

- 数える resume: W の `resume_started` のうち `payload.counted` が true のもの。reason の分類は先頭の文字列で、人の send_back は「a person sent the run back in ask」、review の差し戻しは「the review's findings were sent back by」、verification の失敗は「verification command」、それ以外はその他（`after task` の衝突・e2e の失敗・supervisor に殺された session ほか）。
- 各件に、同じ run で `payload.attempt` が同じでそれより後の最初の `resume_finished` を対応づけた（表 B と同じ）。区間の秒数は `resume_started` から `resume_finished` まで。
- 対象（以下「時間切れ」）: `resume_finished` の outcome が unresolved で、区間が 3,600 秒を越え、区間に `exit_requested` と `session_closed` があるもの。
- fresh_session の切り替え: 区間に `session_renewed` があるもの（その resume の起動の前に新しい session に切り替えた）。
- 時間の内訳: `resume_finished` の `work_breakdown.secs`。検証の時間 V = `build`・`test`・`fmt`・`full_test`・`other_command` の和。`other_command` は command の規則に当たらない command で、wrapper の log では大半が `sh scripts/cargo-brief.sh cargo nextest ...` と `cd <worktree> && ...` の連なり（どちらも検証）で、残りは数秒の `python3 -` の編集だった。`work_breakdown` の `idle` が半分を越える件は、内訳が記録されていない（記録なし）とした。
- 秒数と割合は小数を切り捨てた。
- 重なり: 区間と、自分以外の run の実行の区間が重なった秒数。区間の前に始まったものと後に終わったものも含める。
  - 着地の検証: `run_phase_changed` の phase が `landing` になってから、同じ run の次の `run_phase_changed` まで（`integrate` の rebase と検証のコマンド）。件数と、そのうち resume の前から続いていた件数、重なった秒数の和。
  - e2e: 同じく phase が `e2e` の区間（host で流す e2e）の重なった秒数の和。
  - 他の session の build・test: W の前後の全ての `session_exited` の `work_breakdown.heavy` のうち、category が `build`・`test`・`fmt`・`full_test` の command（worker・resume の session が流したもの）の重なった秒数の和を区間の秒数で割った値（区間に平均して何本走っていたか）。
  - 拾えないもの: review のプログラムの build、landing recheck、host の他の process（人の作業・sccache の server など）の負荷は、実行の区間を持つ記録が無く、数えていない。10/19 の 7 件の頃は、他の session の `work_breakdown` も command を持たず、build・test の列は記録なしとした。
- 割り当て（1 件を 1 つ）:
  1. 区間で切り替えた件は、時間の記録が無く、読み直しと検証のどちらが時間を使ったかを分けられないので、判じられない（T1 か T2）。
  2. 記録のある件は、V・`wait`・`model` などのうち最大のもので決める。V が最大なら T1、`wait` が最大で自分の検証の待ちでなければ T3。host の負荷で検証のコマンドが遅くなった分は、そのコマンドの `build`・`test` などに入り V と分けられないので、割り当てでは T1 に含め、判定の「T3」で別に見積もる。
  3. 切り替えていない記録なしの件は、最後の `turn_finished` の `message` と wrapper の log が検証の流し直しを示せば T1、そうでなければ判じられない。
  4. 区間の大半で queue 全体が止まっていた件は T4（host の停止）。

## 表 1（reason ごとの時間切れの数）

| reason | 数える resume（分母） | 時間切れ | 割合 | うち exhausted |
| --- | --- | --- | --- | --- |
| 人の send_back | 112 | 25 | 22.3% | 7 |
| review の差し戻し | 22 | 5 | 22.7% | 1 |
| verification の失敗 | 19 | 0 | 0.0% | 0 |
| その他 | 9 | 0 | 0.0% | 0 |
| 計 | 162 | 30 | 18.5% | 8 |

表 B との突き合わせ: 人の send_back の分母 112 と時間切れ 25（exhausted 7）は表 B と一致した。差は無い。表 B が書いた「3,601〜14,422 秒」は `work_breakdown.total_secs`（session の時間）で、この表の区間の秒数（`resume_started` から `resume_finished`）は 3,604〜14,536 秒になる。差は session の起動の前と終了の後の数秒〜数分と、後に書く task 1527 の supervisor の引き継ぎの分。

ほかの終わり方: verification の失敗の後の 2 件（task 3125、13,333 秒と 3,946 秒）と人の send_back の後の 1 件（task 3125、3,711 秒）は 3,600 秒を越えたが resolved で終わり、時間切れに入れていない。review の差し戻しの 1 件（task 2792）は対応する `resume_finished` が無かった。表 B の unresolved の残り 2 件（ENOTFOUND・`tool_use` の途中の停止）は 3,600 秒に届かず、対象の外。

## 表 2（時間切れの各件）

最後の turn は、どの件も区間に 1 つだけで、`failure` は `other`（「the supervisor asked the session to exit」）。最後の turn の内容はその `message` の要約。重なりの 3 列の意味は「数え方」の「重なり」。

| run | task | reason と attempt | 区間（秒） | exhausted | 切り替え | V / 記録の計（秒） | `wait`（秒） | 着地の検証（件・重なり） | e2e の重なり | 他の session の build・test の平均の本数 | 最後の turn の内容 | 仮説 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 4d5007cb | 1672 | 人 3 | 3,731 | false | あり | 記録なし | 0 | 1（前から 0）・328 秒 | 0 | 記録なし | 文書の検査が通り、commit して関係する検査を流し直す | 判じられない（T1 か T2） |
| 44793a6f | 2746 | review 1 | 3,633 | false | あり | 記録なし | 0 | 1（前から 0）・536 秒 | 0 | 記録なし | review の subagent が worktree で試すのを待つ | 判じられない（T1 か T2） |
| f62bc27a | 1821 | review 1 | 3,604 | false | あり | 記録なし | 0 | 0（前から 0）・0 秒 | 0 | 記録なし | commit し、最終の tree で手元の検査を流し直す | 判じられない（T1 か T2） |
| 2edc1303 | 2394 | review 5 | 3,723 | false | なし | 記録なし | 0 | 2（前から 1）・957 秒 | 0 | 記録なし | unit の stress が通り、次は IT を分けて流す | T1 |
| 79c5d61e | 1882 | review 1 | 3,713 | false | あり | 記録なし | 0 | 2（前から 1）・961 秒 | 0 | 記録なし | 境界と文書の script を流し直し、subagent に見せる | 判じられない（T1 か T2） |
| 2edc1303 | 2394 | review 6 | 3,694 | true | あり | 記録なし | 0 | 2（前から 1）・872 秒 | 0 | 記録なし | 直した tree で検査を流し直す | 判じられない（T1 か T2） |
| f62bc27a | 1821 | 人 3 | 3,731 | true | あり | 記録なし | 0 | 1（前から 0）・5 秒 | 0 | 記録なし | IT の helper を一般化して test を足す | 判じられない（T1 か T2） |
| 66a77d60 | 2936 | 人 1 | 3,614 | false | なし | 2,312 / 3,604（64%） | 457 | 1（前から 0）・450 秒 | 0 | 0.00 | verify の script 7 本が通り、review を待つ | T1 |
| 0af1cbdd | 1673 | 人 4 | 3,989 | false | なし | 1,712 / 3,699（46%） | 0 | 4（前から 0）・1,205 秒 | 0 | 0.00 | job に読みを足す直しの途中 | T1 |
| 0af1cbdd | 1673 | 人 6 | 3,800 | true | なし | 2,043 / 3,707（55%） | 0 | 7（前から 0）・684 秒 | 0 | 0.11 | subagent が見つけた退行 3 件を直す | T1 |
| 501d99db | 1673 | 人 7 | 3,781 | true | なし | 1,896 / 3,658（51%） | 0 | 3（前から 0）・909 秒 | 0 | 0.22 | test が通り、commit して最終の tree で全部を流し直す | T1 |
| 5d7c85f0 | 2978 | 人 1 | 3,720 | false | なし | 2,262 / 3,653（61%） | 0 | 2（前から 0）・1,639 秒 | 127 秒 | 0.14 | commit を amend し、最終の tree で検査を流し直す | T1 |
| ff37d50d | 3105 | 人 5 | 3,680 | true | なし | 2,864 / 3,660（78%） | 5 | 0（前から 0）・0 秒 | 0 | 0.81 | test 1,328 本のうち 1,028 本が通り、残りを待つ | T1 |
| 8462685c | 3109 | 人 2 | 3,664 | false | なし | 1,827 / 3,646（50%） | 1,084 | 0（前から 0）・0 秒 | 0 | 0.77 | 流し直しが 10 分の上限に当たり、上限を延ばして流し直す | T1 |
| 8462685c | 3109 | 人 3 | 3,685 | false | なし | 2,858 / 3,632（78%） | 142 | 2（前から 0）・30 秒 | 0 | 0.06 | （空） | T1 |
| d75e6bda | 3177 | 人 2 | 3,612 | false | なし | 2,438 / 3,606（67%） | 0 | 2（前から 1）・1,012 秒 | 0 | 0.00 | 検査の位置を直す直しの途中 | T1 |
| 48631a16 | 3105 | 人 2 | 3,720 | false | なし | 2,306 / 3,663（62%） | 0 | 2（前から 0）・498 秒 | 161 秒 | 0.27 | 同じ形の test を探す直しの途中 | T1 |
| 0f53c23c | 3109 | 人 4 | 3,648 | false | なし | 1,875 / 3,644（51%） | 0 | 1（前から 0）・300 秒 | 244 秒 | 0.03 | build が通り、commit して最終の tree で検査を流し直す | T1 |
| 2495b2f7 | 1525 | 人 1 | 3,620 | false | なし | 1,995 / 3,616（55%） | 0 | 2（前から 0）・698 秒 | 156 秒 | 0.15 | IT が通ったが時間の関門を越え、原因を調べる | T1 |
| 0c70fe22 | 3109 | 人 1 | 3,697 | false | なし | 2,397 / 3,647（65%） | 0 | 2（前から 0）・2,223 秒 | 0 | 0.12 | 最終の tree で clippy・script・unit・全体の IT を流し直す | T1 |
| 493e949a | 3107 | 人 4 | 4,340 | true | なし | 記録なし（idle 84%） | 0 | 1（前から 0）・3,661 秒 | 0 | 0.00 | 直しの途中（use case の配線） | 判じられない |
| ea1c16cb | 3125 | 人 2 | 3,615 | false | なし | 2,711 / 3,607（75%） | 0 | 5（前から 1）・1,644 秒 | 227 秒 | 0.09 | IT の一部が通り、landing と supervisor の IT を流す | T1 |
| c42db599 | 3107 | 人 4 | 3,607 | false | なし | 1,975 / 3,604（54%） | 67 | 3（前から 0）・675 秒 | 203 秒 | 0.19 | commit し、最終の tree で検査を流し直す | T1 |
| e5b6be50 | 3109 | 人 4 | 3,656 | true | なし | 2,653 / 3,650（72%） | 0 | 0（前から 0）・0 秒 | 0 | 0.25 | fmt・clippy・unit が通り、次は IT | T1 |
| 40e3b8b6 | 3125 | 人 1 | 3,604 | false | なし | 2,266 / 3,601（62%） | 0 | 0（前から 0）・0 秒 | 0 | 0.17 | review を待つ間に最終の tree の候補で unit の stress を流す | T1 |
| 5c7565f3 | 3294 | 人 3 | 3,744 | false | なし | 1,936 / 3,679（52%） | 539 | 2（前から 0）・1,318 秒 | 243 秒 | 0.03 | build が通り、unit・IT の test を足す | T1 |
| c55e1162 | 3109 | 人 2 | 3,762 | false | なし | 2,092 / 3,684（56%） | 1 | 1（前から 0）・1,640 秒 | 0 | 0.00 | 文書の検査が通り、clippy、commit、全体の検査へ | T1 |
| c55e1162 | 3109 | 人 3 | 3,725 | true | なし | 1,677 / 3,686（45%） | 829 | 0（前から 0）・0 秒 | 0 | 0.00 | 全部が通ったが IT の時間の注記が古く、直して stress を流し直す | T1 |
| a4601533 | 2412 | 人 1 | 14,536 | false | なし | 13,578 / 14,423（94%） | 0 | 2（前から 1）・13,450 秒 | 151 秒 | 0.00 | 全部が通り、doc comment を直して commit し関門を流し直す | T4 |
| a5bd6fef | 1527 | 人 3 | 4,308 | false | なし | 2,485 / 3,731（66%） | 654 | 2（前から 1）・4,301 秒 | 0 | 0.00 | subagent の報告を待ち、最後の小さな直しと commit へ | T1 |

各行の補足:

- 記録なしの 7 件は、どれも 10/19 の件で、`work_breakdown.secs` が `idle` だけだった。切り替えた 6 件は判じられない（T1 か T2）にし、切り替えていない task 2394 の attempt 5 は、最後の turn と wrapper の log（`cargo test` 3 回・`scripts/` 12 回）から T1 にした。
- task 3107（493e949a、attempt 4）は、区間の途中の `run_adopted`（12:46Z）の後に command の記録が無く、`idle` が 84% だった。wrapper の log は終わりまで編集と `cargo clippy` を示すが、時刻が無く、検証と直しのどちらが時間を使ったかを分けられないので、判じられないにした。
- task 2412（a4601533）は、2026-10-27T07:12Z に background で始めた 1 本の command（`fmt` に分類）が 11,023 秒続いた。07:15Z〜10:15Z の 3 時間、queue 全体の event が 0 件だったので、host か supervisor が止まっていたと読み、T4（host の停止）にした。区間は turn の上限（`limit_secs` 14400）の近くで終わり、3,600 秒では `exit_requested` が出ていない。
- task 1527（a5bd6fef）は、区間の途中で supervisor の heartbeat が queue DB の lock で失敗し（`runtime_error` の `lease_lost`）、次の supervisor が `run_adopted` で引き継いだ。session は止まらず検証を続けたので、T1 のままにした。
- 重なり: 30 件のうち 24 件で他の run の着地の検証が重なり（30 件の件数の中央値 2）、7 件ではそれが resume の前から続いていた。task 1527 と task 2412 は 1 本の着地の区間が resume の区間のほぼ全部（4,301 秒・13,450 秒）に重なった。他の session の build・test の平均の本数は、記録のある 23 件で 0〜0.81（task 3105 の ff37d50d が 0.81、task 3109 の 8462685c attempt 2 が 0.77）。

## 判定

割り当てた 23 件（判じられない 7 件を除く）を分母にする。

| 仮説 | 件数 | 割合（÷ 23） | 判定 |
| --- | --- | --- | --- |
| T1 検証の流し直し | 22 | 95.7% | holds（主） |
| T2 fresh_session の切り替え | 0 | 0.0% | unresolved（判じられない 6 件） |
| T3 background の待ち | 0 | 0.0% | unresolved（T3 全体。下を参照） |
| T3 host の負荷 | 件ごとに分けられない | - | unresolved（T3 全体。下を参照） |
| T4 その他 | 1 | 4.3% | does_not_hold |
| 判じられない（T1 か T2） | 6 | - | - |
| 判じられない（記録の欠け） | 1 | - | - |

- **T1 holds**: 記録のある 22 件はどれも V が最大で（そのうち 21 件が T1、残りが task 2412）、V の中央値は 2,264 秒（1,677〜13,578）、T1 の 21 件では記録の計の 45〜78%。`model` は 272〜1,342 秒。T1 の残りの 1 件（task 2394 の attempt 5）は記録なしで、最後の turn と wrapper の log から割り当てた。最後の turn は 22 件のうち 15 件が検証の途中か流し直しの直前で、そのうち 6 件は「commit（か amend）して最終の tree で流し直す」だった（2978・3109 の 0f53c23c・0c70fe22・3107 の c42db599・1673 の 501d99db・3109 の c55e1162 の attempt 2）。全体の IT かそれに近い数の test を流している件（3109 の 0c70fe22 の「full IT modules」、3105 の ff37d50d の 1,328 本）は、[手元の検証](../development/local-checks.md)の「resumeでの再現」が全体の test を流してよいとする verification の失敗の resume ではなく、人の send_back の resume だった。
- **T2 unresolved**: その resume で切り替えた 6 件はどれも 2026-10-19 で、`peak_context` の閾値が 150000 だった日。`session_renewed` は切り替えた事実を示すだけで、新しい session が何に時間を使ったかは `work_breakdown` に無い（`idle` だけ）。wrapper の log は時刻を持たず、最後の turn は 6 件のうち 4 件が検証の流し直しだったので、T1 と T2 を分けられない。dagq.toml の [fresh_session] のコメントの 10/19 の観測（切り替えた session の 33% が時間切れ）とは矛盾しないが、原因の裏づけにはならない。閾値を 400000 に上げた後（10/19 の後）の 23 件で、その resume で切り替えた件は 0。
- **T3 unresolved**（background の待ちと host の負荷に分けた）:
  - background の待ち: 成り立たない（0 件）。`wait`（`sleep` などの待ちの command の時間）が最大の件は 0。`wait` が 20% を越えた 2 件（3109 の 8462685c attempt 2 の 1,084 秒、c55e1162 attempt 3 の 829 秒）は、どちらも自分で background に置いた test の待ちで、T1 に入れた。timeline の gap の理由に `background` は 0 秒（`idle` と、task 1527 の supervisor の入れ替わりの `unknown` 366 秒だけ）。
  - host の負荷: 判じられない。CPU の取り合いや build の lock の待ちで遅くなった時間は、遅くなった検証の command の `build`・`test` などに入り、V と分けて記録されない。見積もりとして、W の前後の `session_exited` の重い command 1,275 本について、走った間（その中点）に他の build・test か着地の検証が 0 本だったものと 1 本だったものの時間の中央値を比べた。build は 65 秒と 83.5 秒（1.28 倍）、test は 73 秒と 86 秒（1.18 倍）、2 本と重なった test は 99 秒（1.36 倍）。重なった割合（着地の検証の重なり ÷ 区間 ＋ 他の session の build・test の平均の本数、1 で頭打ち）に 1.3 倍の遅れを当てると、負荷による分は T1 の記録のある 21 件で V の中央値 8.7%、最大 23.0%（task 1527 の a5bd6fef。着地の 1 本が区間の全部に重なった）だった。これを V から除いても 21 件はどれも V が最大で、割り当ては変わらない。ただしこの見積もりは別々の command の中央値の比で、件ごとの因果ではなく、review のプログラムの build・landing recheck・host の他の process の負荷も入っていない。どの件も時間切れで打ち切られていて、残っていた作業の量が分からないので、負荷が無ければ 3,600 秒に収まったかも判じられない。
- **T4 does_not_hold**: host の停止 1 件（task 2412）。API の error と `tool_use` の途中の停止は、時間切れの 30 件には 0 件（表 B の 2 件は 3,600 秒に届かず対象の外）。
- 主: **T1**（22 / 23 = 95.7%）。判じられない 7 件がすべて T1 以外でも 22 / 30 = 73.3% で過半のまま。T3 の host の負荷は unresolved だが、見積もった分を除いても T1 の割り当ては変わらないので、主の判定も変わらない。

## 推奨

T1 が主なので、worker の resume の prompt か [手元の検証](../development/local-checks.md)で、resume の turn に流す検証の範囲を絞る task を推奨する。根拠と、絞る先の候補:

1. **commit の後の流し直し**: 「commitの後に1行でも直せば流し直す」（local-checks.md の「記録」の節）に沿って、resume の終わりに amend と全部の流し直しを繰り返す件が 22 件のうち 6 件あった。resume では、直しを commit してから 1 回だけ流す順番にし、流し直しは `sh scripts/check-local-gates.sh` が名指す stress と時間の関門に限る、と resume の prompt か local-checks.md に書く。
2. **全体の IT**: 人の send_back・review の差し戻しの resume で全体の IT かそれに近い数の test を流した件が少なくとも 2 件あった。local-checks.md の「resumeでの再現」は全体の test を verification の失敗の resume に限り、それ以外は関係する test だけとしているので、規則を変えるのでなく、resume の prompt にこの区別と resume_timeout（3,600 秒）を見せて、関係する test だけを流すようにする。
3. resume_timeout は延ばさない。検証の時間の中央値 2,264 秒は、規則が要らないとする全体の IT と流し直しの繰り返しを含み、検証が 3,600 秒に収まらない必然は示されていない（受け入れ条件の検証は `integrate` が流す）。

T2 は unresolved だが、今の設定（400000）ではその resume で切り替えた件が 0 なので、fresh_session の引き継ぎの prompt を直す task は推奨しない。T3 の background の待ちと T4 は件数が無いか 1 件なので、打ち手を推奨しない。T3 の host の負荷は unresolved で、見積もりでは T1 より小さい。先に T1 の範囲を絞り、その着地の後に同じ数え方で時間切れの件数を数え直して、なお残れば負荷を測る記録（下の 1 つ目）を足す task を推奨する。同じ問いの Spike の繰り返しは推奨しない。

記録の抜け（T2 と T3 の host の負荷を unresolved にした理由で、次に同じ数え方をするときに効く）:

- 負荷で遅くなった時間を測る記録が無い。重い command（`work_breakdown.heavy`）ごとに、その間の host の負荷（load average か、同時に走った build・test・着地の検証の本数）を持たせれば、T3 の host の負荷を件ごとに分けられる。review のプログラムの build と landing recheck は実行の区間の記録が無い。
- 10/19 の 7 件と task 3107 の `run_adopted` の後は、`work_breakdown` に command の記録が無かった。切り替えた session の内訳が今の記録で取れているかは、400000 の下で切り替えた件が W に無く、確かめていない。
- `sh scripts/cargo-brief.sh` で包んだ `cargo nextest` は command の規則に当たらず `other_command` に入るので、T1 の時間を `test` と分けて読めない。
- task 2412 は、queue が 3 時間止まった間を含めて 4 時間近く `exit_requested` が出なかった。resume_timeout の時計（`resume_deadline` は monotonic の時計で測る）が host の停止の間に進まなかったかは確かめていない。

## 上限と使った時間

上限は合わせて 2 時間（件の抽出と表 B との照合 30 分、各件の読みと割り当て 1 時間、文書 30 分）で、1 周。使った時間は worker の session の時刻（`date -u`）で区切った。

- 件の抽出と表 B との照合: 約 3 分（2026-10-28T14:40Z〜14:43Z、上限 30 分）。
- 各件の読みと割り当て: 約 4 分（〜14:47Z、上限 1 時間）。
- 文書: 約 5 分（〜14:52Z、subagent の点検と commit を含む、上限 30 分）。
- review の差し戻しの後の手直し: 約 6 分（15:04Z〜15:10Z、commit を含む）。重なりを実行の区間で数え直すための新しい入力（`run_phase_changed`・`session_exited`）を読み、T2 と T3 の判定を直した。同じ入力の読み直しによる数え直しはしていない。

合わせて約 18 分で、どれも上限の内。数えた resume は 162 件（うち時間切れ 30 件を 1 件ずつ読んだ）。上限で読めなかった件は 0。周回は 1 回。build・試作・test は流していない。

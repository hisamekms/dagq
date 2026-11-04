---
id: plan-review-agent-settings-spike
type: plan
title: スパイク：review の agent 7 本ごとに、判定の正しさの閾値を満たす一番軽い設定（provider・model・effort）はどれか
status: completed
created: 2026-11-04
owners:
  - hisamekms
tags:
  - planning
  - measurement
  - review
related:
  - design-agent-eval
---

# スパイク：review の agent ごとの軽い設定

問いは「`.dagq/agents/` の review の agent 7 本（adr-rules・config-rules・migration-rules・plugin-generic・receipt-evidence・security・test-rules）のそれぞれで、判定の正しさが閾値を満たす中で一番軽い設定はどれか」。候補は (a) claude・claude-haiku-5-5・low、(b) claude・claude-sonnet-5-5・medium、(c) codex の役割の既定（上書きなし）。

全体の判定は **unresolved**。(a)・(b) の 14 周は `dagq agent eval` の依頼の時点で 14 件とも拒まれ、Claude の成績が 1 つも無いので、軽い候補を比べられなかった。測れたのは今の設定である (c) だけで、比べてよく閾値を満たす adr-rules だけ (c) を選んだ（未測定の候補との軽さの優劣は不明）。agent ごとの判定は「選んだ組と判定」に書く。

## 環境

- 対象の commit: 周の開始時の定義の commit は `c03037760`（migration-rules・security・test-rules）、`2ee4e6de1`（adr-rules・config-rules・receipt-evidence）、`06ee0baf4`（plugin-generic）。途中の着地で landing branch が進んだため。`.dagq/agents` と `dagq.toml` はこの 3 つの commit の間で変わっていない。周ごとの digest は `agent result` の `started` の `definition_digest`・`case_set_digest` に残っている。
- 道具の版: 固定バイナリ `dagq 0.4.0-dev+80d5e902f`（依頼の時）。周の途中で自動更新が進み、supervisor が周を引き取った（確かめた版は `+2ee4e6de1` まで）。Codex は `codex-cli 0.165.0`。
- この run の provider と経路: worker は Claude の headless の session（claude-opus-5-5）。依頼は client mode の `dagq agent eval`（queue service の socket）で、周は本番の supervisor が流す。(c) の job は `codex exec`。model は設定に書かれず（成績の `launch.model` は null）、host の `~/.codex/config.toml` の `gpt-6.1-sol` で動く。effort は medium。
- host: Darwin 27.2 arm64。`dagq.toml` に `[eval]` は無いので、既定の max_runs 120・max_cost_usd 30・concurrency 4・threshold 0.9 で、`[eval.providers.codex]` がある。
- 期間: claim は 2026-11-03T22:44:25Z で、依頼の上限は claim から 8 時間（2026-11-04T06:44:25Z）。全ての周は依頼から順に流れ、最後の周が 2026-11-04T04:55:54Z に終わった（claim から約 6 時間 11 分）。

## (a)・(b) が拒まれた理由

14 件とも同じ応答だった（`--effort` と `--k` は下の表の値）。

```text
agent eval: provider claude is not one dagq.toml uses for the agent (codex) or [roles.review] (codex)
```

`domain::agent_eval::round::check_requested_launch` は、依頼の provider を `dagq.toml` がその agent か `[roles.review]` に使うものに限る（agent-eval.md「依頼と認可」）。今の `[roles.review]` は codex で、7 本の `[review.subagents.<name>]` は `paths` だけを持つ。`--provider` を外して `--model claude-haiku-5-5` だけにしても、継いだ codex に合わない model として拒まれる（`model claude-haiku-5-5 is Claude's; with provider codex name a model of Codex's`）。worker には迂回の経路が無く、人・inbox・planner の依頼も同じ検査を通る。

費用の上限ではないので、description の規則どおり流し直していない。拒まれた依頼は周の記録を作らないので、eval の id は無い。

## 周の k

同じ agent の候補に同じ k を使う。ケースの組の k は 7 本とも 3 で、ケースの数 × k が max_runs 120 を超える agent は超えない一番大きい k にした。

| agent | ケースの数 | agent の k | 依頼の回の数 |
|---|---|---|---|
| adr-rules | 53 | 2 | 106 |
| config-rules | 56 | 2 | 112 |
| migration-rules | 11 | 3 | 33 |
| plugin-generic | 114 | 1 | 114 |
| receipt-evidence | 104 | 1 | 104 |
| security | 31 | 3 | 93 |
| test-rules | 41 | 2 | 82 |

費用の上限で拒まれた周は無く、低い k での流し直しは 0 周である。

## 表

値は `dagq agent result <id>` の周の成績の写し。完了した回は `ended` が真で `abandoned` が偽の回で、回あたりの token はその種類の和 ÷ tokens のある回の数、回あたりの費用は周の費用 ÷ 完了した回、回あたりの時間は回の duration の和 ÷ 完了した回で出した。`—` は周が無いので値が無いことを、`null` は成績の値が null であることを示す。

| agent | provider | model | effort | k | eval の id | 周の状態 | 判定 recall | 判定 precision | 規則 recall | 規則 precision | error | 完了した回 | input | cache_read | cache_creation | output | tokens の無い回 | 費用（USD） | 周の時間（秒） | 回の duration の和（秒） | 回あたり input | 回あたり cache_read | 回あたり cache_creation | 回あたり output | 回あたり費用（USD） | 回あたり時間（秒） |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| adr-rules | claude | claude-haiku-5-5 | low | 2 | なし（依頼の時に拒まれ、周の記録なし） | 拒まれた（provider が設定に無い） | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — |
| adr-rules | claude | claude-sonnet-5-5 | medium | 2 | なし（依頼の時に拒まれ、周の記録なし） | 拒まれた（provider が設定に無い） | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — |
| adr-rules | codex | null | medium | 2 | 1192338 | 完了 | 1.000 | 1.000 | 1.000 | 1.000 | 0 | 48 | 1266257 | 5922688 | 0 | 28478 | 0 | 3.410 | 1913 | 3670 | 26380 | 123389 | 0 | 593 | 0.0710 | 76.5 |
| config-rules | claude | claude-haiku-5-5 | low | 2 | なし（依頼の時に拒まれ、周の記録なし） | 拒まれた（provider が設定に無い） | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — |
| config-rules | claude | claude-sonnet-5-5 | medium | 2 | なし（依頼の時に拒まれ、周の記録なし） | 拒まれた（provider が設定に無い） | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — |
| config-rules | codex | null | medium | 2 | 1192339 | 完了 | 1.000 | 0.949 | 0.929 | 0.703 | 0 | 110 | 4091287 | 18466176 | 0 | 81387 | 0 | 10.843 | 2753 | 7603 | 37194 | 167874 | 0 | 740 | 0.0986 | 69.1 |
| migration-rules | claude | claude-haiku-5-5 | low | 3 | なし（依頼の時に拒まれ、周の記録なし） | 拒まれた（provider が設定に無い） | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — |
| migration-rules | claude | claude-sonnet-5-5 | medium | 3 | なし（依頼の時に拒まれ、周の記録なし） | 拒まれた（provider が設定に無い） | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — |
| migration-rules | codex | null | medium | 3 | 1192335 | 完了 | 1.000 | 1.000 | 1.000 | 1.000 | 0 | 30 | 526481 | 2268544 | 0 | 10751 | 4 | 2.355 | 2241 | 2732 | 17549 | 75618 | 0 | 358 | 0.0785 | 91.1 |
| plugin-generic | claude | claude-haiku-5-5 | low | 1 | なし（依頼の時に拒まれ、周の記録なし） | 拒まれた（provider が設定に無い） | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — |
| plugin-generic | claude | claude-sonnet-5-5 | medium | 1 | なし（依頼の時に拒まれ、周の記録なし） | 拒まれた（provider が設定に無い） | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — |
| plugin-generic | codex | null | medium | 1 | 1192341 | 完了 | 0.696 | 1.000 | 0.679 | 0.905 | 1 | 114 | 3867010 | 15351808 | 0 | 63452 | 4 | 10.486 | 3690 | 7472 | 33921 | 134665 | 0 | 557 | 0.0920 | 65.5 |
| receipt-evidence | claude | claude-haiku-5-5 | low | 1 | なし（依頼の時に拒まれ、周の記録なし） | 拒まれた（provider が設定に無い） | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — |
| receipt-evidence | claude | claude-sonnet-5-5 | medium | 1 | なし（依頼の時に拒まれ、周の記録なし） | 拒まれた（provider が設定に無い） | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — |
| receipt-evidence | codex | null | medium | 1 | 1192340 | 完了 | 1.000 | 0.844 | 0.778 | 0.575 | 1 | 104 | 3394197 | 14942080 | 0 | 82118 | 3 | 9.573 | 4372 | 5746 | 32637 | 143674 | 0 | 790 | 0.0920 | 55.2 |
| security | claude | claude-haiku-5-5 | low | 3 | なし（依頼の時に拒まれ、周の記録なし） | 拒まれた（provider が設定に無い） | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — |
| security | claude | claude-sonnet-5-5 | medium | 3 | なし（依頼の時に拒まれ、周の記録なし） | 拒まれた（provider が設定に無い） | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — |
| security | codex | null | medium | 3 | 1192336 | 完了 | 1.000 | 1.000 | 1.000 | 0.793 | 2 | 93 | 2834303 | 13698944 | 0 | 71219 | 0 | 7.751 | 3232 | 6988 | 30476 | 147300 | 0 | 766 | 0.0833 | 75.1 |
| test-rules | claude | claude-haiku-5-5 | low | 2 | なし（依頼の時に拒まれ、周の記録なし） | 拒まれた（provider が設定に無い） | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — |
| test-rules | claude | claude-sonnet-5-5 | medium | 2 | なし（依頼の時に拒まれ、周の記録なし） | 拒まれた（provider が設定に無い） | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — |
| test-rules | codex | null | medium | 2 | 1192337 | 完了 | 0.976 | 1.000 | 0.951 | 0.886 | 1 | 82 | 2405755 | 10140416 | 0 | 50935 | 2 | 6.829 | 3638 | 5783 | 29338 | 123664 | 0 | 621 | 0.0833 | 70.5 |

読むときの注意が 3 つある。

- 判定と規則コードの値と error は `disputed` のケースを除いた主の指標で、完了した回は `disputed` の回を含むので `scores.runs` より多い。
- migration-rules（1192335）の 4 回、test-rules（1192337）の 2 回、receipt-evidence（1192340）の 3 回、plugin-generic（1192341）の 4 回は、supervisor の引き継ぎで捨てられた回（`abandoned`）で、tokens が無く、費用に見込みの額が足されている（それぞれ $0.968・$0.494・$0.469・$0.582）。回はやり直されて成績には入らないが、この 4 周は「tokens の無い回が 0」を満たさず、回あたりの費用は実際より高い。
- 完了した回の数が依頼の回の数より少ない周（adr-rules は 48 / 106）は、programs が止めたケース（`program_stopped`）の回を job にしないため。

## 閾値

- 既定: `[eval]` の threshold 0.9（判定と規則コードの recall・precision の 4 つ）と error 0。
- 案: security と receipt-evidence は判定と規則コードの recall を **0.95** に上げ、precision は 0.9 のままにする。
  理由は、この 2 本の見落としは review を素通りして着地するが、誤検出は worker か人が見て退けられるので、痛さが非対称なこと。1.0 にしないのは、回の数が少ない周で 1 回の揺れが不合格になるため。security の dev は陽性 16 件 × k 3 = 48 回で、0.95 は 2 回までの見落としを許す。receipt-evidence は 55 件 × k 1 で、許せる見落としは 2 回である。
- 今の `[eval]` の threshold は 1 つの値で agent ごとに持てない。0.95 で着地の前の dev を止めるには仕組みが要る。この Spike では成績の値と手で比べた。

## 選んだ組と判定

比べてよい候補は、完了・error 0・tokens の無い回 0・費用が null でないものに限る。(a)・(b) は拒まれたので順位に入れず、その「値なし」を軽い根拠にも重い根拠にもしない。選ぶのは比べてよく閾値を満たす候補の中で一番軽いものなので、該当が (c) だけの agent は (c) を選ぶ。(a)・(b) は未測定なので、(c) がそれらより軽いか重いかは不明である。

| agent | (c) の閾値 | 比べてよいか | 選んだ組 | 判定と次 |
|---|---|---|---|---|
| adr-rules | 満たす（4 つとも 1.000、error 0） | はい | (c)（比べてよく閾値を満たす唯一の候補。未測定の (a)・(b) との優劣は不明） | 閾値を満たす組がある。(c) を hold-out で確かめる。(a)・(b) が測れれば比べ直す |
| config-rules | 下回る（規則 precision 0.703、11 コードが個別に下回る。A-042 は 0.36） | はい（閾値は下回る） | なし | unresolved。今の設定でも満たさないので、(a)・(b) を測るのに加えてケースか定義を見直す |
| migration-rules | 満たす（4 つとも 1.000、error 0） | いいえ（捨てられた回が 4） | なし | unresolved。(c) は値としては満たす。流し直して tokens の無い回 0 で測り、(a)・(b) と比べる |
| plugin-generic | 下回る（判定 recall 0.696、規則 recall 0.679、error 1。P-19a〜j の見落としが主） | いいえ（error 1・捨てられた回 4） | なし | unresolved。今の設定でも満たさないので、(a)・(b) を測るのに加えて P-19 の見落としのケースか定義を見直す。k 1 で規則ごとの陽性が少ない |
| receipt-evidence | 下回る（判定 precision 0.844、規則 recall 0.778・precision 0.575、error 1）。判定 recall は 0.95 の案も満たす（1.000）が、規則 recall は既定の 0.9 にも届かない | いいえ（error 1・捨てられた回 3） | なし | unresolved。今の設定でも満たさないので、(a)・(b) を測るのに加えて `near-` と `clean-` の誤検出、A-062・A-076・A-077 の見落としのケースか定義を見直す。k 1 で規則ごとの陽性が少ない |
| security | 下回る（規則 precision 0.793、SEC-4 0.73・SEC-5 0.53、error 2）。recall は 0.95 の案も満たす（判定・規則とも 1.000） | いいえ（error 2） | なし | unresolved。今の設定でも満たさないので、(a)・(b) を測るのに加えて SEC-4・SEC-5 の誤検出のケースか定義を見直す |
| test-rules | 下回る（規則 precision 0.886、A-115・A-118・A-120・A-262 が個別に下回る、error 1） | いいえ（error 1・捨てられた回 2） | なし | unresolved。今の設定でも満たさないので、(a)・(b) を測るのに加えて A-120 の陽性などのケースか定義を見直す |

description の 3 つの次のうち、閾値を満たす組があると言えるのは adr-rules の (c) だけで、これは今の設定なので軽くなるわけではない。config-rules・plugin-generic・receipt-evidence・security・test-rules は (c) で下回ったが、(a)・(b) を測っていないので「どの候補も満たさない」とも言えず、unresolved のまま見直しの材料だけを挙げる。「ケースの組が小さく差が読めない」を判じられる差も、候補が 1 つなので無い。

## 推奨

1. **Claude の周を流せるようにしてから、(a)・(b) の 14 周をやり直す Spike を登録する。** 決めるのは送り先の検査の扱いで、これは送り先を設定に限る安全の検査なので、緩める形は planner か人が決める。形の案は、`[eval.providers.claude]` のように eval の送り先を設定に宣言する形がよい。7 本の表に `provider = "claude"` を書く形は本番の review まで動かすので、測るためには使わない。この Spike の worker は `dagq.toml` と runtime を変えられない。
2. **Claude の周の見込みを同時に見る。** 実績が無い agent の 1 回の見込みは `DEFAULT_CLAUDE_RUN_USD` の $0.40 で、$30 の上限に収まるのは 75 回まで。今の k では migration-rules（33 回）しか通らず、流し直しの低い k でも security は k 2、adr-rules・config-rules・test-rules は k 1 に下がる。plugin-generic（114 件）と receipt-evidence（104 件）は k 1 でも上限を超えて流せない（見積もりの計算で、実際に拒まれたものではない）。(c) の実績は回あたり $0.07〜0.10 だった。やり直しの前に `[eval.providers.claude]` の `default_run_usd` か max_cost_usd を実績に合わせて決め、agent ごとに (a) を先に流す（実績ができれば (b) の見込みはその最大に替わる）。
3. **捨てられた回の扱いを決める。** supervisor の引き継ぎは 1 周のうち数回を捨てるので、着地が続く本番では長い周ほど「tokens の無い回が 0」を満たせない（7 周のうち 4 周で起きた）。比べてよい条件は捨てられた回を除いて数えるか、やり直しを前提にするかを、やり直しの Spike の前に決めるとよい。
4. **dagq.toml に入れる組は adr-rules の (c) だけで、hold-out で確かめる組も同じ。** (c) は役割の既定なので `dagq.toml` の値は変わらない（固定するなら `[review.subagents.adr-rules]` に `provider = "codex"` を書く）。planner か inbox が hold-out の周を依頼する。hold-out の 1 回の制限のキーは組を含まないので、既に hold-out を流した定義とケースの組では、人の再実行（`--rerun`）の指定が要る。
5. **(c) の結果は見直しの材料に使う。** 今の本番の設定で閾値を満たさない agent が 5 本あるので、model を軽くする前にケースか定義を見直す候補になる。config-rules・security・test-rules は判定の recall が 0.97 以上で、規則 precision が 0.9 を下回る（規則コードの付け違いが主）。receipt-evidence は判定 recall 1.000 だが、誤検出と規則コードの見落としがある。plugin-generic は判定 recall 0.696 で、見落としが主である。これらの agent が本番の review で同じように間違える頻度は、ここでは測っていない。

## 使った時間と周

- 使った時間: claim から全ての周の終わりまで約 6 時間 11 分で、8 時間の上限の内。上限に達しておらず、打ち切った周は無い。周が終わった後の receipt は、session の再開の上限の扱いを聞いた人の答えの後に書いたので、claim から約 12 時間後である（新しい周は依頼していない）。
- 依頼した周: 21 周（(a)・(b) の 14 周と (c) の 7 周）。28 周の上限の内。
- 完了した周: 7 周。incomplete・未完了: 0 周。
- 拒まれた周: 14 周（provider が設定に無い）。費用の上限で拒まれた周: 0 周。流し直した周: 0 周。
- 全ての周の token の和: input 18,385,290・cache_read 80,790,656・cache_creation 0・output 388,340。費用の和: $51.25（捨てられた回の見込み $2.51 を含む）。

---
id: plan-worker-question-topics
type: plan
title: worker の問い（worker_question）の中身の分類と、ラベルの定義案
status: completed
created: 2026-09-28
owners:
  - hisamekms
tags:
  - measurement
  - asks
  - worker
related:
  - adr-0047
  - adr-0071
  - adr-t598-1
  - plan-night-human-wait-measurement
  - plan-review-sendback-reasons
---

# worker の問い（worker_question）の中身の分類と、ラベルの定義案

task 950（goal 64）。worker が作業中に出した `worker_question` の ask を 1 件ずつ読み、何が決まらなかったのかを分類した。件数・答えまでの時間（夜と昼）・答えが worker の推奨どおりだったか・答えの後に何が起きたかを出し、runtime で使うラベルの定義案と、worker が自分で決めてよい範囲を広げる候補を置く。この文書は決定をせず、task も登録しない。

今の `worker_question` は人が要る理由（`reason_category`、[ADR-0047](../adr/0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md) 決定 41）の `scope` / `discard` しか分類を持たず、問いの中身は `question` の自由文にしか無い。[night-human-wait-measurement](night-human-wait-measurement.md) は夜の答え待ちの打ち手に「worker が自分で決めてよい範囲を広げる」を挙げ、[review-sendback-reasons](review-sendback-reasons.md) は review の concern 22 回のうち 21 回で worker が逸脱を自分で決めて receipt に書いていたと数えた。この文書はその反対側の、worker が作業中に人に聞いた側を数える。

## 要点

- **worker_question は 13 件だけで、run 438 件の 3.0%**。記録のある全期間（2026-09-24 04:00 UTC〜2026-09-28 13:25 UTC）がそのまま直近 7 日に入るので、2 つの期間の数字は同じ。13 件とも `reason_category` は `scope`（`dagq asks --all` の値。最初の 5 件の `ask_opened` の payload には欄が無い）で、`discard` は 0 件。
- **答えまでの合計は 896 分、そのうち夜（JST 22〜07 時に開いたもの）の 5 件が 535 分**。中央値は夜 13.9 分・昼 3.2 分。長いのは task 770（夜、392 分）、828（昼、187 分）、128（昼、139 分）、816（夜、120 分）の 4 件で、合計の 93% になる。残りの 9 件は 25 分以内に答えが返った。
- **worker が推奨を書いた 10 件は、10 件とも推奨どおりに答えられた**。推奨の無い 3 件は、選択肢の先頭（537）、worker が挙げた 2 つのうちの 2 つめ（252）、人の別案（816）だった。
- **中身は 7 つに分かれ、実装の選び方だけを聞いたものは無い**。多いのは ADR との食い違い（`adr_conflict` 3 件、213 分）、受け入れ条件がそのままでは満たせない（`acceptance_infeasible` 2 件、397 分、どちらも夜）、範囲の外の変更が要る（`out_of_scope_change` 2 件、122 分）。
- **答えの後に人がもう一度呼ばれた run が 4 件ある**。770 は答えどおりに進めたのに、review が同じ「原因の特定」の未達を concern にして `approve_landing` になった。460 と 252 は答えが「failed で返す」で、failed の receipt が復旧 job を経て `decide` の ask になった。209 は問いと同時に `approve_landing` が開いた。ask を減らしても、review の concern と復旧の `decide` で同じ判断をもう一度人に聞けば、人に届く数は減らない。
- **ADR の番号の衝突（204・209）と、paths の外が要るときの問い（252）は、今の AGENTS.md では ask にしないことになっている**（ADR の ID は task の ID から決まる ADR-t598-1、宣言外のパスは failed の receipt に書く）。どちらも規則ができる前か、規則に反した問いだった。

## 方法

- 期間: `ask_opened` の記録は 2026-09-24 04:00 UTC（ask 1、observer の `blocked`）から始まり、それより前の worker の問いは記録に無い。記録のある全期間と直近 7 日（2026-09-21 13:25 UTC〜）は、どちらも 2026-09-24 04:00 UTC〜2026-09-28 13:25 UTC になる。
- 分母: 同じ期間の `run_claimed` 438 件（run 438 件）。
- 読んだもの（固定バイナリ `~/.local/bin/dagq` の読み取り専用のコマンドだけ）: `dagq events --full --kind ask_opened --kind ask_answered --kind ask_closed`、`dagq asks --all`（`question`・`options`・`answer`・`answered_by`。`events` の payload には問いの文と答えの文が無く、読めるのはこのコマンドだけ）、13 task の `dagq show --full`（acceptance・description・答えの後の event・receipt の要約・review の verdict・follow-up）、`dagq stats --full` の `waiting`、`dagq events --full --kind run_claimed`。queue の状態を変えるコマンドは打っていない。
- 夜と昼: [night-human-wait-measurement](night-human-wait-measurement.md) と同じく、ask を開いた時刻が JST 22〜07 時なら夜。
- 答えまでの時間: `ask_opened` → `ask_answered` の壁時計の時間。ADR-0071 の待ちが入った後の 7 件（ask 110 以降）は `run_waiting_started` → `run_waiting_ended` の待ちとほぼ一致する（`stats` の `waiting.waited.worker_question` は 7 件で計 42,766 秒）。答えの後に slot が空くまでの待ち（`run_slot_regained` の `slot_wait_secs`、90〜957 秒）は含めない。
- 推奨どおり: 問いの文に「推奨」「recommended」か、`(A) 推奨` のように推す選択肢が書かれ、答えがその選択肢か同じ中身だったもの。
- 分類: 問いの文と、その task の acceptance・description・答えの後の経過を 1 人（この task の worker）が読んで、主のラベルを 1 つと、当てはまれば副のラベルを付けた。主は worker が止まったきっかけ（最初に満たせなくなったもの）、副はそれを解くのに一緒に決める必要があったもの。

## 問いの一覧

| ask | task | 開いた時刻（JST） | 夜 | 答えまで（分） | 答えた人 | 主のラベル | 副のラベル | 推奨 → 答え | 答えの後 |
|---|---|---|---|---|---|---|---|---|---|
| 30 | 128 | 09-24 15:34 | | 139.2 | （記録なし） | `acceptance_conflict` | | A → A | 17 分で着地。newtype の判断を follow-up 167 に |
| 37 | 204 | 09-24 22:18 | 夜 | 5.0 | （記録なし） | `task_overlap` | | 0035 → 0035 | 5 分で着地。番号の付け替えを follow-up 212 に |
| 39 | 209 | 09-24 22:31 | 夜 | 13.9 | （記録なし） | `task_overlap` | `adr_conflict` | A → A | 同時に `approve_landing` が開き同じ時刻に答え。衝突の resume 2 回の後に着地 |
| 57 | 287 | 09-25 09:01 | | 24.3 | （記録なし） | `adr_conflict` | | A → A | 1 分で着地。統合は follow-up 315 に |
| 94 | 392 | 09-26 09:20 | | 1.4 | （記録なし） | `adr_conflict` | `out_of_scope_change` | A → A | 45 分で着地 |
| 100 | 460 | 09-26 11:49 | | 1.1 | （記録なし） | `precondition_missing` | | wait → wait | failed で返し、復旧 job の `decide`（ask 101）→ 登録し直し。次の run は 09-27 に review の concern（note の権限）で `approve_landing` の後に着地 |
| 110 | 537 | 09-26 19:15 | | 1.5 | planner | `precondition_missing` | | なし → A | 12 分で着地。再測定を follow-up 563 に |
| 128 | 486 | 09-27 14:13 | | 3.6 | inbox | `host_environment` | | A → A（人が host を直す） | 27 分で着地（検証の失敗の resume 1 回） |
| 131 | 252 | 09-27 18:53 | | 2.8 | inbox | `out_of_scope_change` | | なし → failed | failed で返し、`decide`（ask 132）→ retry。約 10.6 時間後に着地 |
| 142 | 669 | 09-27 22:15 | 夜 | 4.3 | inbox | `acceptance_infeasible` | | A → A | 50 分で着地（slot の空きを 16 分待った） |
| 152 | 770 | 09-28 00:29 | 夜 | 392.4 | inbox | `acceptance_infeasible` | | A → A | review が同じ未達を concern（ask 157）、10 分後に `land`。答えから 38 分で着地 |
| 155 | 816 | 09-28 05:05 | 夜 | 119.5 | inbox | `out_of_scope_change` | `adr_conflict` | なし → 人の別案 (c) | 65 分で着地。follow-up 4 件 |
| 165 | 828 | 09-28 09:17 | | 187.3 | inbox | `adr_conflict` | `acceptance_infeasible`、`out_of_scope_change` | A → A | 23 分で着地。follow-up 4 件 |

「答えた人」が（記録なし）の 6 件は、`answered_by` を記録する前の runtime のもので、人が直接答えたか inbox が代行したかは分からない。

## 分類ごとの集計

| 分類 | 件数 | 夜: 件数・中央値・合計（分） | 昼: 件数・中央値・合計（分） | 推奨どおり | 代表の task |
|---|---|---|---|---|---|
| ADR・design との食い違い（`adr_conflict`） | 3 | 0・—・0 | 3・24.3・213.0 | 3/3 | 287, 392, 828 |
| 受け入れ条件がそのままでは満たせない（`acceptance_infeasible`） | 2 | 2・198.4・396.7 | 0・—・0 | 2/2 | 669, 770 |
| 範囲の外の変更が要る（`out_of_scope_change`） | 2 | 1・119.5・119.5 | 1・2.8・2.8 | 0/0（推奨なし 2） | 252, 816 |
| 他の task との重なり（`task_overlap`） | 2 | 2・9.5・18.9 | 0・—・0 | 2/2 | 204, 209 |
| 進める材料がまだ無い（`precondition_missing`） | 2 | 0・—・0 | 2・1.3・2.6 | 1/1（推奨なし 1） | 460, 537 |
| 受け入れ条件どうしの食い違い（`acceptance_conflict`） | 1 | 0・—・0 | 1・139.2・139.2 | 1/1 | 128 |
| host の環境・ツール（`host_environment`） | 1 | 0・—・0 | 1・3.6・3.6 | 1/1 | 486 |
| 実装・設計の選び方（`design_choice`） | 0 | — | — | — | — |
| 成果を捨てるか（`discard_work`） | 0 | — | — | — | — |
| 合計 | 13 | 5・13.9・535.1 | 8・3.2・361.2 | 10/10（推奨なし 3） | — |

- 副のラベルも数えると、`adr_conflict` 5 件、`out_of_scope_change` 4 件、`acceptance_infeasible` 3 件。ADR と範囲の外の変更は、主でなくても問いに添えて出ることが多い。
- 夜の 5 件のうち、答えが朝まで待ったのは 770（00:29 → 07:02）と 816（05:05 → 07:05）の 2 件。22 時台の 3 件（204・209・669）は 4〜14 分で答えが返った。
- 昼でも長いのは 828（187 分）と 128（139 分）。128 は同じ run の `decide`（ask 31）と並んで開いていて、両方が同じ時刻に答えられた。
- 答えが「failed で返す」だった 2 件（460 の wait、252）は、どちらも復旧 job の `decide` でもう一度人に届き、着地は 460 で約 1.8 日、252 で約 10.6 時間遅れた。worker が最初から failed の receipt を書いていれば、人に届くのは `decide` の 1 回で済んだ。
- 09-24〜25 の 4 件（128・204・209・287）は、ADR の ID の規則（ADR-t598-1）も待ちで slot を空ける規則（ADR-0071）も無かった時期のもの。ask 110 以降の 7 件は答えを待つあいだ slot を空けていた（[night-human-wait-measurement](night-human-wait-measurement.md) の 770 の見立てのとおり、遅れたのはその task の着地だけ）。

## ラベルの定義案

goal 64 の後続（ADR と runtime の実装）の材料。worker が `dagq ask --kind worker_question` を打つ時点で、自分でラベルを付ける前提で書く。ラベルの名前は snake_case の英語。件数はこの期間の「主 / 副を含めて付いた件数」。

### 付け方の規則（案）

1. worker は問いに主のラベルを 1 つ必ず付け、他に当てはまるものを副のラベルとして 0 個以上付ける（例えば `--topic adr_conflict --topic out_of_scope_change` の先頭が主）。
2. 主は worker が止まったきっかけ（最初に満たせなくなったもの）にし、それを解くのに一緒に決める必要があるもの（ADR の置き換え、範囲の外の変更など）を副にする（例: task 209 は他の task の着地との衝突がきっかけなので主は `task_overlap`、ADR の番号の付け替えは副の `adr_conflict`）。きっかけが 2 つ同時で決められないときだけ、人の判断の重い方を主にする。順は `discard_work` > `adr_conflict` > `acceptance_conflict` > `acceptance_infeasible` > `out_of_scope_change` > `task_overlap` > `precondition_missing` > `host_environment` > `design_choice` > `other`。
3. どれにも当たらなければ `other` にし、短い説明を添える。この期間に `other` は無かった。
4. `reason_category` とは別の欄に持つ（次の節）。runtime は付いたラベルを `ask_opened` の payload に記録し、`stats` と `kpi` はラベルごとに件数・答えまでの時間（夜と昼）・答えの後の経過（着地・failed・concern）を出す。過去の ask にはラベルが無いので、集計は `unlabeled` として数え、書き換えない。
5. worker の推奨を問いの文とは別に持たせる（`--recommend <option>` のような欄）。推奨どおりだった割合は、今は問いの文を読まないと数えられない。

### ラベル

| ラベル | 定義 | 判定の例 | 件数（主 / 付いた） |
|---|---|---|---|
| `adr_conflict` | task の条件か、条件を満たすやり方が、accepted の ADR・design・人の決定（goal の constraints、AGENTS.md に書かれたユーザー決定）と食い違う。ADR を置き換えるか、amends にするか、ADR の決定が実際には成り立たないのをどう扱うかを聞く | task 392: sccache 用の dagq.toml を置くと ADR-0040 決定 3 の「dagq.toml を置かない」を変える。task 828: ADR-t827-1 決定 3 の前提が cargo-llvm-cov では成り立たない | 3 / 5 |
| `acceptance_conflict` | 同じ task の受け入れ条件どうし、または条件と description が両立しない。どちらを優先するかを聞く | task 128: description の newtype 化をすると「tests/cli.rs を変更なしで通す」を満たせない | 1 / 1 |
| `acceptance_infeasible` | 条件が、調べた事実（ツールの挙動・再現しない現象・権限）のためにそのままでは満たせない。条件を狭めるか、やり方を変えるか、止めるかを聞く | task 669: separate-git-dir の repository では linked worktree から main worktree を知る手段が無い。task 770: 250 回流しても hang を再現できず「原因の特定」を満たせない | 2 / 3 |
| `out_of_scope_change` | 条件を満たすのに、task の paths・description・verify に無い変更（宣言外のファイル、migration、ADR、CI）が要る | task 252: tests/plugin.rs が paths の外。task 816: codex を書くのに非互換の migration が要る | 2 / 4 |
| `task_overlap` | 並行する他の task や、直前に main に着地した変更と重なる・衝突する（番号、同じファイルの書き換え、同じ決定） | task 204: 予約した ADR-0032 が main で使用済み。task 209: task 204 が同じ番号 0035 と README を書き換えて着地した | 2 / 2 |
| `precondition_missing` | 着手の前提（測る対象のデータ、先行の task の着地、必要な件数）がまだ揃っていない。待つか、今あるもので進めるかを聞く | task 460: sccache の導入後の着地が 10 run に満たない。task 537: nextest で流れた integrate が 3 件だけ | 2 / 2 |
| `host_environment` | host のツール・設定・版が作業か着地を妨げ、worker は host に手を入れられない | task 486: global の mise の rust が RUSTUP_TOOLCHAIN=1.93.0 を supervisor に入れていて、1.98 への引き上げが着地すると queue が止まる | 1 / 1 |
| `design_choice` | 条件・ADR・範囲に触れない、実装や設計の選び方だけの問い。AGENTS.md では worker が決めて receipt に書くもので、ask にしない | この期間は無い | 0 / 0 |
| `discard_work` | できた成果を捨てるか、やり直すか | この期間は無い（460 と 252 の「failed で返す」は答えの側で決まったもので、問いは scope だった） | 0 / 0 |
| `other` | どれにも当たらない。説明を添える | この期間は無い | 0 / 0 |

`design_choice` は、worker が ask にしてはいけないことを runtime が数えるためのラベル。付いた ask が出たら、worker への指示か prompt の直しどころを示す。

### reason_category との関係

`reason_category`（ADR-0047 決定 41）は「なぜ人が要るか」（権限の理由: `scope`・`discard`・`authentication`・`cost`・`recovery_failed`）で、ask にしてよいかの関門と、inbox がどう見せるかに使う。この文書のラベルは「何が決まらなかったか」（問いの中身）で、減らす手を選ぶための集計に使う。置き換えずに両方を持つ。

- 対応の目安: `discard_work` は `discard`、他のラベルは `scope`。`host_environment` は今は `scope` で打たれている（task 486）が、中身は人が host に手を入れる依頼で、受け入れ条件は変わらない。`reason_category` を増やすかは後続の ADR の判断に残す。
- `reason_category` が `scope` の ask の中の内訳がラベルになる。runtime は `--because` と別の flag で受け、どちらかを他方から推さない。`design_choice` だけは「人が要る理由が無い」ことを示すラベルなので、`--because` と組み合わせたときに矛盾として記録するか、ask を拒むかも後続の判断に残す。
- review の差し戻しのラベル（[review-sendback-reasons](review-sendback-reasons.md)）とは、`adr_conflict`（review では `adr_design_mismatch`）、`acceptance_conflict`、`out_of_scope_change` が同じ種類の問題を指す。worker が作業中に問うたか、作業の後に review が concern にしたかを 1 つの集計で追えるよう、名前を揃えるとよい（review の `acceptance_conflict` は、この文書の `acceptance_conflict` と `acceptance_infeasible` の両方を含む）。

## worker が自分で決めてよい範囲を広げる候補

決定はしない。見込みはこの期間（約 4.4 日、worker_question 13 件、答えまで計 896 分）の実績をそのまま当てはめたもの。答え待ちの時間はその task の着地の遅れで、ADR-0071 の後は slot を空けているので、夜の処理量（件/時）はほとんど変わらない（[night-human-wait-measurement](night-human-wait-measurement.md)）。

1. **`acceptance_infeasible` と `precondition_missing` のうち、worker の推奨が「条件を狭め、今ある材料で書き、残りを follow-up にする」ものは ask にせず、receipt の `summary` に狭めた内容と理由を、follow-up に残りを書かせる**。対象は 669・770・537 の 3 件、答え待ち 398 分（うち夜 397 分）。460（推奨が「待つ」）のように成果を出さない選択は ask か failed の receipt に残す。前提: review が、開示され follow-up のある条件の狭めを concern にしない基準を持つこと（[review-sendback-reasons](review-sendback-reasons.md) の候補 4）。770 は答えの後に review が同じ未達を concern にして `approve_landing`（10 分）になったので、review の基準が変わらなければ待ちが ask から concern に移るだけになる。
2. **`acceptance_conflict` のうち、description の主目的を満たすと付随の条件（「X を変えない」など）を最小限だけ破るものは、worker が主目的を優先し、破った範囲と理由を `summary` に書く**。対象は 128 の 1 件、答え待ち 139 分（昼）。条件が ADR や人の決定を写しているときは `adr_conflict` として ask に残す。
3. **`adr_conflict` のうち、ADR の決定の中身を変えず、書き方（新しい ADR にするか統合するか、amends の先）だけが問われるものは、ADR-t598-1 決定 5（小さな ADR の amends）に従って worker が決め、統合は follow-up にする**。対象は 287 の 1 件、答え待ち 24 分。392（AGENTS.md のユーザー決定を変える）と 828（ADR 自体が「止めて人に聞く」と書く）は決定の中身を変えるので対象の外で、ask に残る。見込みは小さく、主な効き目は、問いを出すかどうかの境を ADR の規則として書けること。
4. **今の規則ですでに ask にしないはずの問い（`task_overlap` の ADR の番号の衝突、paths の外が要る `out_of_scope_change`）を、worker の prompt で ask の前に当てはめさせる**。対象は 204・209・252 の 3 件、答え待ち 22 分。時間の見込みは小さいが、252 は ask と復旧の `decide` で人に 2 回届いたので、failed の receipt に直行すれば人に届く回数が 1 回減る。204・209 は ADR-t598-1 の後は起きないので、残るのは main に着地したばかりの変更との内容の衝突（209 の README）で、main を正として合わせ `summary` に書くのを worker の側に寄せる。

4 つを全部当てはめると、対象は 8 件・答え待ち約 583 分（全体の 65%、夜 416 分）。ask に残るのは 392・828（ADR の決定を変える）、816（非互換の migration と provider の方針）、486（host に手を入れる）、460（待つか）の 5 件で、どれも人の判断か人の作業が要るものだった。

## 限界

- 件数が 13 件と少なく、分類ごとには 1〜3 件。中央値と割合は目安にしかならない。
- 分類は 1 人の読み手（この task の worker）が付けたもので、別の読み手との一致は測っていない。`adr_conflict` と `acceptance_infeasible`（828）、`out_of_scope_change` と `adr_conflict`（816）の境は判断が入る。
- 答えまでの時間は壁時計の時間で、夜と人の不在を含む。09-26 より前の 6 件は答えた人が記録に無い。
- worker が ask にせず自分で決めた件（receipt と review の concern に出たもの）はこの文書では数えていない。その数は [review-sendback-reasons](review-sendback-reasons.md) の concern の集計にある（22 回のうち 21 回で逸脱は receipt に開示済み）。
- 問いの文と答えの文は `dagq asks --all` でしか読めず、`events` の payload には無い。後続の実装でラベルを `ask_opened` の payload に入れれば、この読み方は要らなくなる。
- 集計に使った中間のファイル（events・asks・show の JSON）は commit していない。上の一覧の ask ID から読み直せる。

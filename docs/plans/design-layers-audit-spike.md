---
id: plan-design-layers-audit-spike
type: plan
title: スパイク：docs/design の全ての文書のうち、どれがどの節で 4 層の規則に反し、細部をどこへ移せば地図の形と 20 KiB の予算に収まるか（全文の 1 回きりの点検）
status: completed
created: 2026-11-04
owners:
  - hisamekms
tags:
  - planning
  - documentation
related:
  - development-documents
  - development-task-registration
---

# スパイク：docs/design の層の点検

問いは「docs/design の文書のうち、どれがどの節で 4 層の規則（[documents.md](../development/documents.md)の「design」と `.dagq/agents/design-layers/AGENT.md` の規則コード）に反し、細部をどこへ移せば地図の形と 20 KiB の予算に収まるか」。
review の段は差分にしか動かないので、既存の全文はこの 1 回だけ点検した。
この文書は点検の結果だけを持ち、文書もコードも直していない。

## 判定と推奨

判定: **違反の多い文書がある。文書ごとの直しの task を計画する。**

根拠:

- 読んだ 25 本（708626 byte）で違反の範囲は 338 件・2312 行・407580 byte で、大きさの約 58% に当たる。
  これは上限で、範囲に地図の行が混ざる（下の「確かさ」の再判定では 25 件のうち 10 件が混ざり、2 件は地図だった）。
  混ざりを除いても、多くの文書で細部が本文の大きな部分を占めるという結論は変わらない。
  DL-1（場面ごとの手順）と DL-5（文章で書いた細部）がほとんどで、DL-2 は 52 件、DL-4 は 4 件だった（1 件に複数の規則がある）。
- 移し先に同じ事実が「有」は 338 件のうち 203 件（60%）、「一部」は 132 件、「無」は 3 件だった。
  再判定の標本では「有」の 15 件のうち 6 件が事実の一部しか持たなかったので、この割合は高めに出ている。
  それでも移し先のコードの多くに事実の全部か一部があるので、書き起こしより突き合わせが主な作業になる。
- 違反がほとんど無いのは `supervisor-lifecycle/commands.md` だけで、`architecture.md`・`security.md` が少ない。
  残りの 22 本は 3 割以上（多くは 5 割以上）が細部である。
- 数を絞る余地は無いので「直しを数本に絞る」は当たらない。
- 迷った行は 75 件あったが規則の直しを先にする理由にはならない（下の「判定器の規則について」）。

次（planner が goal 245 に登録する）:

1. 下の「直しの task の案」の順に、1 文書 1 task で登録する。
   `measurement.md` は別の task（3941、着地済み）で直ったので除く。
2. 上限のすぐ下で読んでいない文書（`agent-eval.md` 30,699 byte・`supervisor-lifecycle/claim-hold.md` 30,666 byte）と残りの読んでいない文書（`measurement.md` を除く）は、機械の値で順を決め、直す task がその文書を読んで判定する。
   この点検をもう一度は行わない。
3. 読みの判定の粗さは、下の「確かさ」にある。
   消す行の一覧でなく直す節の案内として使い、消す前に「形と予算」の 1・2・4 で残る場所を確かめる。

## 環境

- 対象の commit: `7098468a251aad4ac9bdeb158404f71c38065c85`（main、2026-11-04 時点）。
- 判定に使った `AGENT.md` の commit: `7e813fdeff122348deefe8c650cc81ed24e26481`。
- 道具の版: `/bin/sh`（GNU bash 3.2.57、arm64-apple-darwin26）、awk 20200816、git 2.39.3（Apple Git-146）、ripgrep 15.2.0、集計は Python 3.15.2。
- provider と経路: provider は claude（model は `claude-opus-5-5`）、この run の headless の worker（非対話の wrapper が background で起動）。
  読みの判定は、この worker が Agent ツールで起動した同じ model の subagent 6 本に文書の組を分けて任せ、結果を worker が検算した。
  判定器（`design-layers`、`claude-haiku-5-5`）そのものでは流していない（worker から全文を流す経路が無いため）。
- host: macOS 27.2（Darwin 27.2.0）、arm64。

## 機械の一覧

出し方:

```sh
sh scripts/check-design-identifiers.sh --all 2>ident.txt   # 識別子の多い行（A）と列挙の塊（B）
sh scripts/check-design-docs.sh --print-allow              # 大きさの測り方の確かめ（下の注）
```

- 大きさ: `check-design-docs.sh` の測り方で、frontmatter を除いた byte 数（改行を含む）。
  上限を超える文書の値は `--print-allow` の値と一致する。
- 識別子の多い行: `--all` の出力の `N inline code` の行の数（検査の（A）、8 個以上）。
- 列挙の塊: `--all` の出力の `list of N items` を N 行で 1 つに数えた塊の数（検査の（B）、7 本以上）。
- inline code の数: `check-design-identifiers.sh` の `count_code` と同じ数え方を、frontmatter と fenced code block の外の全ての行に当てた数。
  検査の対象と違って表と見出しも数える（表に識別子が多い文書を見逃さないため）。
  /KB は大きさを 1,024 で割った値で割る。

対象の commit で `--all` は 68 本のうち（A）54 行・（B）51 塊・合わせて 556 件を出した。
この一覧は対象の commit から作り直せる測定の出力で、新しい記録の仕組みではない。

| 文書（`docs/design/` の下） | 大きさ（byte） | 識別子の多い行 | 列挙の塊 | inline code | inline code/KB |
| --- | --- | --- | --- | --- | --- |
| `authorization.md` | 40622|2|1|762|19.2|
| `supervisor-lifecycle/run-environment.md` | 36344|0|2|359|10.1|
| `supervisor-lifecycle/observer.md` | 34657|3|1|378|11.2|
| `supervisor-lifecycle/draft-planners.md` | 34006|0|1|253|7.6|
| `supervisor-lifecycle/auto-update.md` | 30720|1|1|283|9.4|
| `container-runtime.md` | 30719|2|0|190|6.3|
| `plugin-integration.md` | 30717|3|4|313|10.4|
| `supervisor-lifecycle/headless-worker.md` | 30715|0|1|166|5.5|
| `security.md` | 30710|0|1|192|6.4|
| `supervisor-lifecycle/run-worktrees.md` | 30710|0|1|222|7.4|
| `supervisor-lifecycle/prompt.md` | 30708|0|2|183|6.1|
| `agent-eval.md` | 30699|0|1|165|5.5|
| `queue-service.md` | 30683|2|1|362|12.1|
| `supervisor-lifecycle/claim-hold.md` | 30666|0|1|175|5.8|
| `persistence.md` | 30639|0|4|260|8.7|
| `supervisor-lifecycle/triage.md` | 30585|5|1|284|9.5|
| `supervisor-lifecycle/needs-session.md` | 30447|4|0|246|8.3|
| `supervisor-lifecycle/roles.md` | 30360|4|0|355|12.0|
| `supervisor-lifecycle/up-down.md` | 28752|0|2|242|8.6|
| `supervisor-lifecycle/validation.md` | 28610|1|3|208|7.4|
| `queue-service-host-cli.md` | 26962|4|2|289|11.0|
| `architecture.md` | 23405|2|4|227|9.9|
| `supervisor-lifecycle/background-recovery-job.md` | 22908|0|0|130|5.8|
| `supervisor-lifecycle/goal-review.md` | 22601|5|1|245|11.1|
| `supervisor-lifecycle/waiting.md` | 20936|5|1|200|9.8|
| `supervisor-lifecycle/finding-planners.md` | 20931|0|1|194|9.5|
| `queue-broker.md` | 20930|0|2|134|6.6|
| `supervisor-lifecycle/install.md` | 20507|0|1|185|9.2|
| `provider-lifecycle.md` | 20062|1|0|170|8.7|
| `supervisor-lifecycle/throughput-review.md` | 19970|1|1|190|9.7|
| `measurement.md` | 19733|0|0|128|6.6|
| `toolchain-profiles.md` | 17680|0|2|126|7.3|
| `supervisor-lifecycle/integrate.md` | 17333|0|1|129|7.6|
| `supervisor-lifecycle/supervise.md` | 17311|0|0|116|6.9|
| `supervisor-lifecycle/ask.md` | 16715|4|1|221|13.5|
| `overview.md` | 16271|0|0|138|8.7|
| `supervisor-lifecycle/actor-model.md` | 15605|1|0|179|11.7|
| `supervisor-lifecycle/review.md` | 15399|0|1|99|6.6|
| `supervisor-lifecycle/host-metrics.md` | 15220|0|0|93|6.3|
| `supervisor-lifecycle/handoff.md` | 14916|0|1|98|6.7|
| `supervisor-lifecycle/release-update.md` | 14577|0|0|142|10.0|
| `supervisor-lifecycle/ci-watch.md` | 14570|0|0|142|10.0|
| `supervisor-lifecycle.md` | 13775|1|0|65|4.8|
| `supervisor-lifecycle/plan-review.md` | 13302|0|0|86|6.6|
| `supervisor-lifecycle/headless-job-processes.md` | 12601|1|1|128|10.4|
| `supervisor-lifecycle/push.md` | 12561|0|1|120|9.8|
| `supervisor-lifecycle/plan-planners.md` | 11991|0|0|105|9.0|
| `supervisor-lifecycle/commands.md` | 11698|1|1|181|15.8|
| `supervisor-lifecycle/worker-model.md` | 11402|0|0|73|6.6|
| `domain-model.md` | 10467|0|0|109|10.7|
| `supervisor-lifecycle/review-programs.md` | 9368|0|0|75|8.2|
| `execution-tokens.md` | 9289|0|0|50|5.5|
| `follow-up-membership.md` | 9256|0|0|75|8.3|
| `supervisor-lifecycle/setup-review.md` | 9147|0|0|56|6.3|
| `measurement-environment.md` | 9084|0|0|42|4.7|
| `supervisor-lifecycle/source-repository.md` | 8728|1|0|95|11.1|
| `attribute-records.md` | 6794|0|0|66|9.9|
| `supervisor-lifecycle/backend-call-failures.md` | 6381|0|0|72|11.6|
| `measurement-commands.md` | 6128|0|0|48|8.0|
| `measurement-otel.md` | 5904|0|0|83|14.4|
| `supervisor-lifecycle/language.md` | 5765|0|0|62|11.0|
| `supervisor-lifecycle/queue-hold.md` | 5495|0|0|47|8.8|
| `supervisor-lifecycle/ask-push.md` | 5007|0|0|38|7.8|
| `main-history.md` | 4959|0|1|26|5.4|
| `README.md` | 4639|0|0|7|1.5|
| `supervisor-lifecycle/review-builtin.md` | 2409|0|0|7|3.0|
| `provider-executables.md` | 2360|0|0|15|6.5|
| `supervisor-lifecycle/build-identifier.md` | 1575|0|0|19|12.4|

## 読みの判定

### 読んだ文書と選んだ基準

25 本（上限）を次の基準で機械に選んだ。

- 予算の上限にいる文書（11 本）: 許可の一覧で大きさを超える 4 本（`authorization.md`・`supervisor-lifecycle/run-environment.md`・`supervisor-lifecycle/observer.md`・`supervisor-lifecycle/draft-planners.md`）と、30,700 byte 以上の 7 本（`supervisor-lifecycle/auto-update.md`・`container-runtime.md`・`plugin-integration.md`・`supervisor-lifecycle/headless-worker.md`・`security.md`・`supervisor-lifecycle/run-worktrees.md`・`supervisor-lifecycle/prompt.md`）。
- inline code/KB の上位 7 本（10,240 byte 以上。小さい文書は密度の揺れが大きく減らせる量も小さいため）: `supervisor-lifecycle/commands.md`・`supervisor-lifecycle/ask.md`・`queue-service.md`・`supervisor-lifecycle/roles.md`・`supervisor-lifecycle/actor-model.md`・`supervisor-lifecycle/goal-review.md`・`queue-service-host-cli.md`。
- 識別子の多い行と列挙の塊の和の上位 7 本（上と重ならないもの。同点は大きい文書を先に取る）: `supervisor-lifecycle/triage.md`・`supervisor-lifecycle/waiting.md`・`architecture.md`・`persistence.md`・`supervisor-lifecycle/validation.md`・`supervisor-lifecycle/needs-session.md`・`supervisor-lifecycle/up-down.md`。

### 読んでいない文書

次の 43 本は読んでいない（読む文書の上限 25 本）。
時間の上限には達していない。
判定は上の機械の一覧の値だけで、違反の有無を言えない。

`agent-eval.md`、`supervisor-lifecycle/claim-hold.md`、`supervisor-lifecycle/background-recovery-job.md`、`supervisor-lifecycle/finding-planners.md`、`queue-broker.md`、`supervisor-lifecycle/install.md`、`provider-lifecycle.md`、`supervisor-lifecycle/throughput-review.md`、`measurement.md`、`toolchain-profiles.md`、`supervisor-lifecycle/integrate.md`、`supervisor-lifecycle/supervise.md`、`overview.md`、`supervisor-lifecycle/review.md`、`supervisor-lifecycle/host-metrics.md`、`supervisor-lifecycle/handoff.md`、`supervisor-lifecycle/release-update.md`、`supervisor-lifecycle/ci-watch.md`、`supervisor-lifecycle.md`、`supervisor-lifecycle/plan-review.md`、`supervisor-lifecycle/headless-job-processes.md`、`supervisor-lifecycle/push.md`、`supervisor-lifecycle/plan-planners.md`、`supervisor-lifecycle/worker-model.md`、`domain-model.md`、`supervisor-lifecycle/review-programs.md`、`execution-tokens.md`、`follow-up-membership.md`、`supervisor-lifecycle/setup-review.md`、`measurement-environment.md`、`supervisor-lifecycle/source-repository.md`、`attribute-records.md`、`supervisor-lifecycle/backend-call-failures.md`、`measurement-commands.md`、`measurement-otel.md`、`supervisor-lifecycle/language.md`、`supervisor-lifecycle/queue-hold.md`、`supervisor-lifecycle/ask-push.md`、`main-history.md`、`README.md`、`supervisor-lifecycle/review-builtin.md`、`provider-executables.md`、`supervisor-lifecycle/build-identifier.md`。

`measurement.md` は別の task で直ったので読む対象から外した。
`overview.md` は概念の文書で、許可の一覧に残るのは 1 行の長さだけである（既存の task 2143 が持つ）。

### 判定の当て方

- `AGENT.md` は差分の行を見る定義なので、既存の全文を足した行と見なして全ての行に当てた。
- DL-3 は削除の規則なので当てていない。
- 移し先の確かめは subagent が `rg` で打った結果を file:line で書いた（短い file 名もある）。
- 検算（機械）: 全ての行の範囲が文書の中にあること、移し先に `.rs`・`.md` の完全な path を持つ 285 件の path（335 個）が全て実在すること、file:line の形の証拠が 338 件のうち 328 件にあることを確かめた。

### 確かさ

- 組ごとに読んだ者が違うので、厳しさに揺れがある。
- 行の範囲は節ごとにまとめたので、範囲の中に地図に残す約束が混ざる。
  下の「違反を除いた大きさの見積り」は違反の範囲を全て消して案の 1 行を足した値で、下限である（文書ごとの注の値が高いのはそのため）。
- 独立の再判定: 別の subagent に、当時の表から乱数の種 20261104 で選んだ 25 件を読ませた（節の名前と行の範囲は先に見ているが、規則コードは見ずに決めた）。
  判定は一致 13・混ざる 10・不一致 2、証拠は正しい 16・部分的 8・誤り 1 だった。
  不一致の 2 件（`supervisor-lifecycle/ask.md` 53 行、`security.md` 93-102 行）は迷った行へ移し、証拠の過大と誤りの 7 件は「一部」に、約束を落としていた残す案の 5 件は約束を足して直した。
  標本の外は直していないので、同じ割合の混ざりと証拠の過大が残るとみる。
  このため直す task は、表の範囲を消す一覧にせず、範囲の中の約束を行ごとに読み分け、移す前に移し先を開いて確かめる。
- 文書ごとの案の注の値（「約 15KB」など）は読んだ者の見積りで、「KiB」と書いたものも KB の意味のことがある。
  短い file 名の証拠（`update.rs` など）は path を省いたもので、同じ名前が複数あるので案内として読む。

### 判定器の規則について

迷った行は 75 件で、67 件は地図に、8 件は詳細に寄せた。
迷った行は違反の表と重なることがあり、重なる行は違反の範囲を消す前にこの判定を見る。
型は次のとおりで、規則の直しを先に計画する理由にはならない（判定がどちらでも、直す task が節を読めば決められ、この点検の結論を変えない）。

- 同じ約束が doc comment にも同文である行（最も多い）: 約束なら地図に残すので、規則どおり地図でよい。
- 1 行に約束と手順が混ざる行（多い）: 行を分けて約束だけ残せば済む。
- 「権限の表を写す文書」の規則が名指す表と DL-2（`authorization.md` 113-127 行、`security.md` 62-75 行）: 表は規則が残せと言うので違反にしていない。
  ただし `AGENT.md` はこれを書いていないので、判定器が差分のこの表を DL-2 にするおそれがある。
- 経緯か今も効く互換か（`actor-model.md` 45-47 行、`headless-worker.md` 253 行、`roles.md` 22・67・82・100 行など）: DL-4 の文面で決めきれない。
- 利用者や開発者の手順の置き場（`plugin-integration.md` 66-77・279-281 行）: コードに移す先が無く、規則に置き場の定めが無い。

後ろの 3 つを `AGENT.md` と dev のケースに足すかは planner が決める。
足さなくても直しの task は進められる。

### 文書ごとの集計

「違反を除いた大きさの見積り」は 大きさ − 違反の範囲の byte + 地図に残す案の byte の下限で、「有/一部/無」は移し先に同じ事実があるかの件数。

| 文書 | 大きさ | 違反の件数 | 違反の行 | 違反の byte | 違反を除いた大きさの見積り | 有/一部/無 | DL-1/2/4/5 | 迷った行 | 全体 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
|`authorization.md`|40622|15|166|20962|20644|7/8/0|5/8/0/8|4|多い|
|`supervisor-lifecycle/run-environment.md`|36344|14|108|17077|20201|7/7/0|7/0/0/9|2|多い|
|`supervisor-lifecycle/observer.md`|34657|15|138|25216|10400|11/4/0|11/3/0/9|2|多い|
|`supervisor-lifecycle/draft-planners.md`|34006|15|152|28218|6887|13/2/0|8/1/0/10|2|多い|
|`supervisor-lifecycle/auto-update.md`|30720|17|101|17607|14356|12/5/0|11/1/0/10|3|多い|
|`container-runtime.md`|30719|16|76|12854|19227|12/4/0|8/4/0/11|3|中|
|`plugin-integration.md`|30717|14|85|15349|16570|11/3/0|11/2/0/3|3|多い|
|`supervisor-lifecycle/headless-worker.md`|30715|13|56|10092|21819|8/5/0|10/0/0/9|3|中|
|`security.md`|30710|7|37|6560|24329|7/0/0|2/4/0/5|5|少ない|
|`supervisor-lifecycle/run-worktrees.md`|30710|19|122|24224|7953|16/3/0|15/3/2/14|4|多い|
|`supervisor-lifecycle/prompt.md`|30708|15|83|13129|18166|10/5/0|7/2/0/11|4|中|
|`queue-service.md`|30683|13|121|16789|14786|7/6/0|8/6/0/7|4|多い|
|`persistence.md`|30639|18|97|13706|17473|7/10/1|8/2/0/17|3|中|
|`supervisor-lifecycle/triage.md`|30585|16|111|23200|8444|9/7/0|16/1/0/3|3|多い|
|`supervisor-lifecycle/needs-session.md`|30447|15|134|27042|4494|9/5/1|14/1/1/6|4|多い|
|`supervisor-lifecycle/roles.md`|30360|14|116|21630|9615|5/8/1|7/1/0/7|1|多い|
|`supervisor-lifecycle/up-down.md`|28752|16|106|19061|10755|11/5/0|14/1/0/5|3|多い|
|`supervisor-lifecycle/validation.md`|28610|18|112|19547|10567|7/11/0|14/3/0/11|3|多い|
|`queue-service-host-cli.md`|26962|17|88|17117|11171|10/7/0|8/1/1/9|3|多い|
|`architecture.md`|23405|6|25|4524|19112|5/1/0|1/1/0/5|2|少ない|
|`supervisor-lifecycle/goal-review.md`|22601|13|82|16671|6823|5/8/0|7/1/0/5|3|多い|
|`supervisor-lifecycle/waiting.md`|20936|10|67|14556|7208|4/6/0|8/3/0/5|2|多い|
|`supervisor-lifecycle/ask.md`|16715|12|72|13262|4248|7/5/0|6/2/0/6|3|多い|
|`supervisor-lifecycle/actor-model.md`|15605|9|54|8698|7647|3/6/0|1/1/0/7|4|多い|
|`supervisor-lifecycle/commands.md`|11698|1|3|489|11287|0/1/0|0/0/0/1|2|少ない|

## 直しの task の案

- 単位は 1 文書 1 task で、`measurement.md` は除く。
- 順の群: 1 は許可の一覧で予算を超える文書、2 は予算の上限にいる文書、3 は残りのうち 26 KB 以上の文書、4 はそれより小さい文書。
  群の中は上から順に流す。
- `supervisor-lifecycle/commands.md` は違反が 1 件（3 行）で直しの task を立てない。
  63・70 行のコードとの食い違いの疑いは層の違反でなく、このスパイクの run の follow_up に書いた。
- 各 task は `docs/design/<文書>`、移し先の `src/`、`.config/design-docs-tight.toml`（許可の一覧の文書は `.config/design-docs-allow.toml` も）を変える。
  このため file の数は 移し先の src（読んだ者の見積りと、移し先の列に書いた別々の src の数の大きい方）+ 文書と一覧（許可の一覧の文書は 3、分割が要の文書は新しい文書の分も足す）とした。
  分割の要否は読んだ者の見積りで、下限の値とは別の値である。
  `authorization.md`（下限 20,570 byte）と `supervisor-lifecycle/run-environment.md`（下限 20,201 byte）は 20,480 byte の前後で、注の値によらず分割を用意する。
- 直した文書は同じ変更で層を直した一覧に足す（[documents.md](../development/documents.md)の「形と予算」）ので、段6 の 20 KiB はここで当たる。
- doc comment を足す行の予想は、移し先の「一部」と「無」の件数 × 3 行のあらましで、「有」にも足りないものがある（「確かさ」）ので下限である。
- 大きい文書を節で分けるのは、doc comment を足す行が多い文書（`persistence.md`・`supervisor-lifecycle/validation.md`）で 1 task に収まらないときだけでよい。

既存の task の扱いの決め方: 同じ文書の分割や作り直しを求める task は吸収して cancel する。
1 行の記述を直す task は残し、層の直しの前に流れればそのまま、後なら直しの task が確かめて閉じる。
既存の `overview.md` の task 2143 はこの案に入れず残す（読んでいないため。今の大きさは概念の予算の中で、残りは 1 行の長さ）。
task 3390（`measurement.md` を 2 文書に分ける）は task 3941 に吸収されて cancel 済みで、扱いは要らない。

| 順 | 群 | 文書 | 予想の大きさ | file の数 | 直した後の下限（byte） | 分割の要否（読んだ者の見積り） | 既存の task との重なり |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | 1 | `authorization.md` | 消す最大 166 行・doc comment に足す約 24 行 | 12 + 3 | 20644 | 不要（移すと約20KB。超えればguardrailの節を別文書へ） | 吸収: 2530（同じ直しで予算内に入れ、一覧から外す）。残す: 2714（節の中身の修正）、2043（参照の向け直し）。 |
| 2 | 1 | `supervisor-lifecycle/draft-planners.md` | 消す最大 152 行・doc comment に足す約 6 行 | 8 + 3 | 6887 | 不要（移すと約15KB） | 吸収: 2514（分割より層の直しが先で、移せば分割は要らない）。 |
| 3 | 1 | `supervisor-lifecycle/observer.md` | 消す最大 138 行・doc comment に足す約 12 行 | 9 + 3 | 10400 | 不要（移すと約15KB） | 吸収: 2514。残す: 1937・3200（記述の修正）、2678（リンク）。先に流すか、この task の中で確かめて閉じる。 |
| 4 | 1 | `supervisor-lifecycle/run-environment.md` | 消す最大 108 行・doc comment に足す約 21 行 | 12 + 4 | 20201 | 要（移しても約22KB。sccacheの節を別文書へ） | 吸収: 2516。残す: 2052・2639（記述の修正）。 |
| 5 | 2 | `supervisor-lifecycle/auto-update.md` | 消す最大 101 行・doc comment に足す約 15 行 | 10 + 2 | 14356 | 不要: 畳むと約14.5KiB | 吸収: 3688（欄名の書き写しを外す、同じ節の直し）。 |
| 6 | 2 | `container-runtime.md` | 消す最大 76 行・doc comment に足す約 12 行 | 15 + 2 | 19227 | 不要: 畳むと約19.1KiB。余白が薄く追記で分割が要りうる | 吸収: 2559（分割を畳む直しへ置き換える。畳んだ後も余白が薄ければ分ける）。残す: 3149（fix）。 |
| 7 | 2 | `plugin-integration.md` | 消す最大 85 行・doc comment に足す約 9 行 | 3 + 2 | 16570 | 不要: 畳むと約16.6KiB | 残す: 3054（記述の修正）。 |
| 8 | 2 | `supervisor-lifecycle/headless-worker.md` | 消す最大 56 行・doc comment に足す約 15 行 | 10 + 3 | 21819 | 要: 畳んで約21.7KiB。receiptの欄の約束を別文書へ | 残す: 2043。 |
| 9 | 2 | `security.md` | 消す最大 37 行・doc comment に足す約 0 行 | 3 + 3 | 24329 | 要。箇条を移しても約22.6 KiBで、将来の道筋を別文書に分ける | 重なりなし。 |
| 10 | 2 | `supervisor-lifecycle/run-worktrees.md` | 消す最大 122 行・doc comment に足す約 9 行 | 10 + 2 | 7953 | 不要。移せば約8 KiB | 残す: 2654（足す内容は doc comment へ置けるか確かめる）。 |
| 11 | 2 | `supervisor-lifecycle/prompt.md` | 消す最大 83 行・doc comment に足す約 15 行 | 9 + 2 | 18166 | 不要。移せば約18 KiB | 重なりなし。 |
| 12 | 3 | `queue-service.md` | 消す最大 121 行・doc comment に足す約 18 行 | 9 + 2 | 14786 | 不要。移せば約15 KiB | 残す: 2883（operations.md の直し）。 |
| 13 | 3 | `persistence.md` | 消す最大 97 行・doc comment に足す約 33 行 | 16 + 2 | 17473 | 不要（箇条の移動で約11KB減り約20KB、足りなければRuntime ownershipを分ける） | 残す: 2199・3372（記述の修正）、2830（migrations.md の直し）。 |
| 14 | 3 | `supervisor-lifecycle/triage.md` | 消す最大 111 行・doc comment に足す約 21 行 | 9 + 2 | 8444 | 不要（1〜9を移せば約12KBで収まる） | 重なりなし。 |
| 15 | 3 | `supervisor-lifecycle/needs-session.md` | 消す最大 134 行・doc comment に足す約 18 行 | 8 + 2 | 4494 | 不要。doc commentへの移動で20 KiBを大きく下回る見込み | 残す: 2948・3432・3823（足す内容は doc comment へ置けるか確かめる）。 |
| 16 | 3 | `supervisor-lifecycle/roles.md` | 消す最大 116 行・doc comment に足す約 27 行 | 9 + 2 | 9615 | 不要。移せば約12KBで収まる | 重なりなし。 |
| 17 | 3 | `supervisor-lifecycle/up-down.md` | 消す最大 106 行・doc comment に足す約 15 行 | 8 + 2 | 10755 | 不要。doc commentと既存文書への移動で20 KiBに収まる見込み | 残す: 2680。 |
| 18 | 3 | `supervisor-lifecycle/validation.md` | 消す最大 112 行・doc comment に足す約 33 行 | 14 + 2 | 10567 | 不要。手順の移動で20 KiBに収まる見込み | 残す: 3149・4068。 |
| 19 | 3 | `queue-service-host-cli.md` | 消す最大 88 行・doc comment に足す約 21 行 | 13 + 2 | 11171 | 不要。27KBで移せば約12KB | 重なりなし。 |
| 20 | 4 | `supervisor-lifecycle/goal-review.md` | 消す最大 82 行・doc comment に足す約 24 行 | 7 + 2 | 6823 | 不要。23KBで移せば約8KB | 重なりなし。 |
| 21 | 4 | `supervisor-lifecycle/waiting.md` | 消す最大 67 行・doc comment に足す約 18 行 | 7 + 2 | 7208 | 不要（節を移せば約10KBで収まる） | 残す: 2874・2992・3673（足す内容は doc comment へ置けるか確かめる）。 |
| 22 | 4 | `supervisor-lifecycle/ask.md` | 消す最大 72 行・doc comment に足す約 15 行 | 7 + 2 | 4248 | 不要。17KBで移せば10KB以下 | 残す: 3349。 |
| 23 | 4 | `supervisor-lifecycle/actor-model.md` | 消す最大 54 行・doc comment に足す約 18 行 | 3 + 2 | 7647 | 不要。16KBで移せば約8KB | 重なりなし。 |
| 24 | 4 | `architecture.md` | 消す最大 25 行・doc comment に足す約 3 行 | 6 + 2 | 19112 | 不要（約4.7KB移せば約19KB） | 残す: 2386（測定）。 |

## 文書ごとの違反の表

行は対象の commit の file の行番号（frontmatter を含む）。
「移し先に同じ事実」は「有」「一部」「無」と grep の結果で、file:line は対象の commit のもの。

### `authorization.md`

入口の後ろに表と手順が多い。表をdoc commentへ移し、Policy表とguardrailの落とし穴を残す
丸ごと移せる節: Capability、Resourceの後半、対話と操作系の表、着地とpushの箇条。
分割: 不要（移すと約20KB。超えればguardrailの節を別文書へ）。
同じ事実を持つ別の文書: docs/design/security.md, docs/design/supervisor-lifecycle/roles.md, docs/design/supervisor-lifecycle/integrate.md, docs/design/queue-service.md, docs/design/queue-service-host-cli.md。

| 節 | 行 | 規則 | 細部 | 移し先 | 地図に残す 1 行 | 移し先に同じ事実 |
| --- | --- | --- | --- | --- | --- | --- |
| Capability | 50-95 | DL-2 | CLIとcapabilityの対応の全行の並び | src/main.rs の requests の doc comment とmatch、Capability の各variantのdoc comment | CLI→capabilityは`main.rs`の`requests`。予約は誰にも与えない | 有: src/main.rs:2640 requests の網羅match、src/domain/authorization.rs:22 Capability |
| Resource | 104-109 | DL-5 | proposalの持ち主の決め方とfail closedの場合分け | src/infrastructure/proposals の owner_actor の doc comment、Resource::Task の doc comment | 持ち主の入口は`owner_actor`。不明なら拒む（fail closed） | 一部: owner_actor doc に revise と記録無し。submit --proposal の不変は無 |
| Policy | 133-138 | DL-5 | 優先度の拒否の位置とaskのkindの判定 | src/domain/authorization.rs の check_priority_change と opens_ask の doc comment | 人の優先度はuserとinboxだけが変える（ADR-t2373-1） | 有: src/domain/authorization.rs:981 check_priority_change、:792 opens_ask |
| 計画系のコマンド（application） | 150-162 | DL-2 DL-1 | コマンドごとのcapabilityとresourceと状態の分岐の表 | src/application/commands/planning.rs の Planning の各メソッドの doc comment | コマンド→capabilityの入口は`Planning`。役割の許可はPolicyの表 | 一部: planning.rs:1 module doc は規則の要点のみ。写しはコード planning.rs:133 |
| 計画系のコマンド（application） | 164-169 | DL-5 | 終了runのverifyとeditの空欄・状態の照合の細部 | PlanningStore::edit_task の doc comment（src/application/ports/planning.rs） | 認可に使った状態はstoreが同じtransactionで照合する（170行を残す） | 一部: ports/planning.rs:113 edit_task doc。verify_edit・空欄の拒否は無 |
| 計画系のコマンド（application） | 173-179 | DL-1 DL-2 | roleごとに計画系で許す範囲とstoreを読む前の拒否の順 | docs/design/authorization.md のPolicyの表、Gate の doc comment | 残さない。roleの範囲はPolicyの表、拒否の順はGateが持つ | 一部: Policy表 113-127行と同じ事実。Gate src/application/commands/mod.rs:33 に順は無 |
| 対話と記録のコマンド（application） | 193-205 | DL-2 | 対話のコマンドとcapability・resourceの表 | src/application/commands/dialogue.rs の Dialogue の module doc と各メソッド | 対話の入口は`Dialogue`（判定は全roleでstoreの前） | 一部: dialogue.rs:1 module doc に role の要点。resource の対応はコードのみ |
| 対話と記録のコマンド（application） | 210-218 | DL-2 DL-5 | request・replan・holdの表と記録の欄をactorから入れること | src/application/commands/requests.rs の module doc、Replans・Holds の doc comment | 依頼は`Requests`、再計画は`Replans`、保留は`Holds`が判定する | 一部: requests.rs:1 と dialogue.rs:9（asked_by・by）に有。Replans・Holds は未確認 |
| runtimeの操作系のコマンド（application） | 238-253 | DL-2 | 操作系のコマンドとcapability・resourceの表 | src/application/commands/operations.rs の Operation::request の doc comment | 操作系の写しは`Operation::request`（既定の枝なし） | 有: src/application/commands/operations.rs:117 Operation::request |
| runtimeの操作系のコマンド（application） | 257-267 | DL-1 DL-5 | roleごとに操作系で通す範囲とclose-workspaces・run screenの拒否 | operations.rs の module doc、Policyの表 | 残さない。roleの範囲はPolicyの表とoperationsのmodule doc | 有: src/application/commands/operations.rs:1-17 に同じ role の範囲 |
| runtimeの操作系のコマンド（application） | 271-275 | DL-5 | session-eventの記録先と拒否の記録の経路 | src/main.rs の check_access と CommandDenials の doc comment | 拒否はserviceが呼び出し元のprincipalで記録し、CLIはqueueを開かない。約束を残す: 記録できなくても拒否のまま返す（274） | 一部: src/main.rs:3069 check_access doc。再判定: src/main.rs:3069は273-274だけ。272はexecuteのコメント |
| 着地とpush（Integrator） | 300-307 | DL-1 | 着地の依頼・検査・記録・pushの手順 | src/application/integrate.rs の IntegrationRequest・landing_refusal・PushGrant の doc comment | 着地はIntegratorだけ。入口は`land`・`landing_refusal`・`PushGrant` | 有: src/application/integrate.rs:801 :812 :982 landing_refusal doc |
| queue serviceのユースケース（service側） | 313-323 | DL-1 DL-2 | ユースケースごとにどのcapabilityをGateに通すか | src/application/queue_service.rs と supervisor_service.rs の doc comment、starts・issue_held | serviceはtokenのprincipalで認可し、一覧に無い操作は拒む（default deny） | 一部: supervisor_service.rs:990 :1310 integrator_scope、authorization.rs:850 starts |
| Claudeのpermissions.deny（guardrail） | 340-346 | DL-2 DL-5 | DAGQ_COMMANDS・IDENTITY_ENV・設定の並びの細部 | src/application/execution.rs の permission_deny・DAGQ_COMMANDS の doc comment | 規則を作るのは`permission_deny`（338行を残す） | 有: src/application/execution.rs:308 DAGQ_COMMANDS、:394 permission_deny doc |
| CIの見張り（ADR-t1920-1） | 373-374 | DL-5 | ci.readのdeny規則とfinding dismissのcapability | DAGQ_COMMANDS の doc comment、Dialogue | 残さない。capabilityの表の入口とDAGQ_COMMANDSで分かる | 有: src/application/execution.rs:308、Capability::CiRead authorization.rs:29 |

迷った行（違反の範囲と重なることがある）:

| 行 | 当てた規則 | 迷った点 | 寄せた側 |
| --- | --- | --- | --- |
| 113-127 | documents.md「権限の表を写す文書」とDL-2 | DL-2の欄の並びだがskillと一致させる正の表と規則が名指し、authorization.rs:10もこの表を指す。grantsのコピーでもある。 | map |
| 222-229 | AGENT.md「複数のmoduleや仕組みにまたがる約束」 | answerの権限は型(answerer)で読めるが「answerの文やpromptから推さない」「綴りを変えない」は保証の言い方で境界が決めきれない。 | map |
| 282-292 | AGENT.md「場面・eventごとの箇条の列挙」と「部品の外の者が頼る結果」 | 起動の経路ごとの列挙の形だが、誰の環境で判定されるかは複数プロセスにまたがり1つの定義に無い。 | map |
| 327-334 | AGENT.md「1つの型・関数・moduleの定義」 | check_accessのdocに大半があるが、CLIがDBを開かない約束はクライアントとserviceにまたがる。別文書とも重なる。 | map |

### `supervisor-lifecycle/run-environment.md`

設定表の案内は地図で正しい。起動のつど読む手順とsccache・プログラム検査の細部をdocへ移す
丸ごと移せる節: `[run.env]`が名指すプログラムの検査、定期draftの本文。
分割: 要（移しても約22KB。sccacheの節を別文書へ）。
同じ事実を持つ別の文書: docs/design/supervisor-lifecycle/draft-planners.md（recurring）, docs/design/supervisor-lifecycle/claim-hold.md。

| 節 | 行 | 規則 | 細部 | 移し先 | 地図に残す 1 行 | 移し先に同じ事実 |
| --- | --- | --- | --- | --- | --- | --- |
| Run environment（設定の表の一覧） | 36-39 | DL-5 | [run.env]の書式と誤りの種類 | src/infrastructure/run_env.rs の parse_config の doc comment | 書式の入口は`parse_config`。`DAGQ_`のkeyと知らないkeyは誤り | 一部: run_env.rs:741 escape・key名。:1134 unknown tableはコードのみ |
| Run environment（設定の表の一覧） | 43-53 | DL-1 | 渡し先(a)〜(d)ごとの読む時・並び・読めないときの扱い | run_env.rs の module doc と load_run_env の doc comment、各呼び出しの doc comment | 渡し先はworker・検証・review・resume。読めなければ起動しない | 一部: run_env.rs:1 は渡し先の概略のみ、:3629 load_run_env に場面は無 |
| Run environment（設定の表の一覧） | 60-63 | DL-5 | [review.subagents]のkey名と誤りの扱い | run_env.rs の REVIEW_SUBAGENTS_PREFIX、domain::review_builtins の doc comment | 書式は`REVIEW_SUBAGENTS_PREFIX`と`review_builtins` | 一部: run_env.rs:198 は目的のみ。名前とdirの関係は無 |
| [supervisor] | 75-84 | DL-1 DL-5 | 優先順と読み直しの手順、値が誤るときの扱い | domain::slot_limits の module doc、ClaimState::reread_limits の doc comment | 各passで読み直し着地で効く。flag起動は引き継ぎ後もflagのまま | 有: domain/slot_limits.rs:1、execution/slot_limits.rs:26 reread_limits doc |
| [tasks] | 107-112 | DL-1 | コマンドごとにchangeとtitleを拒むか指摘するかの分岐 | domain::ChangeSet・TitlePrefixes の doc comment、run_env.rs の module doc | 値は`ChangeSet`・`TitlePrefixes`。runtimeのdraftは検査しない | 一部: domain/title_prefix.rs:1 module doc に有。domain/change.rs:73 に分岐無 |
| [goals] | 114-118 | DL-1 | goalのラベルをコマンドごとに拒むか指摘するかの分岐 | domain::TagSet の doc comment（src/domain/goal_tag.rs）、compose::max_spike_cycles | ラベルの集合は`TagSet`、意味はtask-registration.md | 一部: goal_tag.rs:78 TagSet doc は形のみ。compose.rs:732 max_spike_cycles |
| [headless] | 131-132 | DL-5 | wrapperの欄が効かないことと警告 | run_env.rs の module doc と HEADLESS_TABLE の doc comment | 残さない。効かない欄を消す前提（133行）だけ残す | 有: src/infrastructure/run_env.rs:35 :239 :3386 |
| sccacheのserver | 169-180 | DL-1 DL-5 | guardの適用先とserver確認の手順 | src/application/supervise/host_ops/sccache.rs の module doc、domain::sccache の doc comment | serverを起動するのはsupervisorだけ。入口は`host_ops::sccache` | 有: host_ops/sccache.rs:1-20、domain/sccache.rs:1 :271 ERROR_LOG_VAR |
| 外部serverと失敗の偏り | 183-193 | DL-5 | identityの照合、壊れた印、再起動の失敗の扱い | application::sccache の owner、domain::sccache の doc comment | 人はdoctorで確かめhostでserverを止める（194-195行を残す） | 有: src/application/sccache.rs:60 owner、host_ops/sccache.rs:13 |
| main checkoutの決め方 | 211-214 | DL-1 | main checkoutが無いときのコマンドごとの動き | src/infrastructure/adapters.rs の main_checkout_of の doc comment | 無いときは起動先のdagq.tomlで代わりにしない（210行を残す） | 一部: adapters.rs:1331 に代替禁止あり。コマンドごとの動きは無 |
| `[run.env]`が名指すプログラムの検査 | 220-225 | DL-5 | 検査する変数と解決の規則 | run_env.rs の PROGRAM_VARIABLES・check_run_env_programs の doc comment | 入口は`check_run_env_programs` | 有: run_env.rs:551 :3749、src/domain/run_env.rs:36 |
| `[run.env]`が名指すプログラムの検査 | 226-240 | DL-1 | up・claim・着地・integrateの場面ごとの検査の手順 | execution/claim.rs の check_run_env_programs、Phase::AwaitingSlot の doc comment | 見つからない間はclaimと新しいresumeと着地の開始を控える | 一部: claim.rs:564、execution.rs:1700 AwaitingSlot。upとintegrateは未記述 |
| `[run.env]`が名指すプログラムの検査 | 241-245 | DL-5 | 変更の印のhashとattentionとdoctorの欄 | claim.rs の run_env_changed の記録、domain::marks、domain のattentionのdoc | 変更の印は値を書かない（saltで鍵付けしたhash） | 有: claim.rs:676、domain/marks.rs:132、domain/mod.rs:2513 |
| 定期draft | 254-266 | DL-5 | 読み手・誤り・材料・期限・見え方の細部 | domain::recurring の module doc、infrastructure::recurring_config と recurring_drafts | 欄は`RecurringDraft`、読み手は`recurring_config`。設定の誤りをaskにしない | 有: domain/recurring.rs:1、recurring_config.rs:1、recurring_drafts.rs:1 |

迷った行（違反の範囲と重なることがある）:

| 行 | 当てた規則 | 迷った点 | 寄せた側 |
| --- | --- | --- | --- |
| 89-92 | AGENT.md「いつ測れるか」と「読むだけを書く行はDL-5」 | 読む時・読めないときは部品の読み方だが、設定を書く人が頼る。同じ型の行が94・97・100・105・121・155にもある。 | map |
| 167-168 | AGENT.md「コードが書かない保証」 | 起動させない理由はADR由来の約束だが、module docにも同文があり、地図に残す範囲がどこまでか決めきれない。 | map |

### `supervisor-lifecycle/observer.md`

observeの手順を番号で書き写した本体が大半。手順とprompt文面をdocへ移し、約束だけ残す
丸ごと移せる節: promptの入力の上限と選ぶ順、observeの0〜5、timer、Codexの行き先以降。
分割: 不要（移すと約15KB）。
同じ事実を持つ別の文書: docs/design/supervisor-lifecycle/prompt.md, docs/design/queue-service.md, docs/design/supervisor-lifecycle/finding-planners.md。

| 節 | 行 | 規則 | 細部 | 移し先 | 地図に残す 1 行 | 移し先に同じ事実 |
| --- | --- | --- | --- | --- | --- | --- |
| Observer（`dagq observe`） | 46-54 | DL-1 DL-5 | skipの条件とcursor・入力の集め方 | src/application/observer/mod.rs の skip・alert_keys・observer_input の doc comment | 変化もalertの増加も無ければagentを起動せずskippedを記録する | 有: observer/mod.rs:740 skip、:705 alert_keys、:652 observer_input |
| Observer（`dagq observe`） | 55-73 | DL-1 DL-5 | promptの文面の書き写し（役割・flaky・読み方） | observer_prompt の文字列と doc comment（src/application/observer/mod.rs） | promptは`observer_prompt`。input.jsonをファイルで読ませない | 有: observer/mod.rs:898 observer_prompt、文面はコード自身 |
| Observer（`dagq observe`） | 74-79 | DL-1 | dir作成・env・timeoutのkillの手順 | application::observer::observe の doc comment、infrastructure::observer の run_agent | agentの子孫はtimeoutでpidごと止まり残らない | 一部: observer/mod.rs:164 observe doc。dir・envの順は無 |
| Observer（`dagq observe`） | 80-89 | DL-1 DL-2 | observe_finishedの欄とcursor保存とattentionの条件 | observe の doc comment、domain の CONSECUTIVE_FAILURES・OBSERVER_FAILURES_TOLD | 期間集計は足し合わせず1回の値で読む（85-86行を残す） | 有: domain/mod.rs:3306 :3311、observer/mod.rs:166 cursor |
| Observer（`dagq observe --history`） | 93-94 | DL-5 | 失敗時の分類をprovider別に誰へ渡すか | observe の doc comment、signals の説明 | 残さない。分類は`observe`のdoc commentが持つ | 有: observer/mod.rs:167-168 signals と provider の job_failure |
| Observer（権限） | 98-100 | DL-1 | クライアントモードでの拒否の経路と通るコマンド | docs/design/queue-service.md#クライアントモード、docs/design/authorization.md | 認可はserviceのみ。Codexにhookを作らない（101-102行を残す） | 有: queue-service.md のクライアントモード、Authorization の Policy 表 |
| Observer（timer） | 103-114 | DL-1 | 期日の判定、起動、supervisorのerror・handoffでのkill | supervise/observation/observer.rs の observer_due・start_observer の doc comment | 同時に1つ、run slotを使わない。serviceが居るときだけ起動 | 一部: observer.rs:199 :222 :315。期日の起点はコードのみ。tick.rs:241 |
| Codexで動かす | 133-137 | DL-1 | 行き先の分岐（provider別、--no-claude、fallback off） | domain::actor_model の job_start_route・job_route の doc comment | 行き先は`job_start_route`（スループットの見直しと共有） | 有: domain/actor_model.rs:650 job_start_route doc、:607 job_route |
| Codexで動かす | 138-144 | DL-1 DL-5 | 子プロセスのlaunchと実行ファイルの解決、Codexのthreadの記録 | supervise/observation/observer.rs、domain::headless_job の JobSession の doc comment | 残さない。起動の入口だけobserver.rsを名指す | 一部: domain/headless_job.rs JobSession、observer.rs:1。解決の分岐は無 |
| Codexで動かす | 145-153 | DL-1 | Codex・Claudeが使えないときの控えと起動し直しの場面分け | supervise/observation/observer.rs の module doc と observer_again の doc comment | 使えないと記録されたjobは控え、次の起動はprovider_fallbackに従う | 有: observation/observer.rs:1-8、observation.rs:34 observer_again |
| promptの入力の上限と選ぶ順 | 161-189 | DL-2 DL-5 | 上限の定数・節の表・選び方・縮め方 | src/application/observer/input.rs の module doc と Spec・STATS_ORDER・fit の doc | 入口は`input`。省いたものは必ず読む方法を書く（ADR-t1566-1） | 有: input.rs:1-12、:166 STATS_ORDER、:23-27 TEXT_CHARS・NESTED_ITEMS |
| KPIの目標割れと改善の上限 | 196-207 | DL-1 DL-5 | 入力kpiの組み立てとpromptの読み方の書き写し | compose/observation.rs の observer_kpi、domain::kpi::observe、observer_prompt | 数字は`kpi`の出力を写し、目標割れはaskにしない | 有: compose/observation.rs:1979、domain/kpi/observe.rs:47、kpi_push.rs:100 |
| 完了見込みの誤差 | 219-223 | DL-5 | forecastのfindingの欄とsnapshotが起こさないこと | domain::kpi::observe の doc comment、domain::kpi::BOOKKEEPING_KINDS | forecastのfindingはKPIの目標割れと同じ（217-218行を残す） | 一部: domain/kpi.rs:51-56 記帳は有。subject の形は kpi/observe.rs のコードのみ |
| 人が要る見立てだけをblockedにする | 230-236 | DL-1 DL-5 | promptの見立ての指示とCLIの--recommendの検査 | observer_prompt、NewAsk::check_asker の doc comment（src/domain/mod.rs） | 人の判断か手の操作を要るものだけをblockedにする（ask.md） | 有: domain/mod.rs:1671 check_asker doc、observer/mod.rs:898 |
| 人が要る見立てだけをblockedにする | 237-239 | DL-5 DL-2 | findings_without_askの数え方とstatsの欄 | run_log.rs の written_by の doc comment、domain/stats/recommendations.rs | 集計は`written_by`と`stats`の`recommendations` | 有: runtime_store/run_log.rs:550-555、stats/recommendations.rs:27-38 |

迷った行（違反の範囲と重なることがある）:

| 行 | 当てた規則 | 迷った点 | 寄せた側 |
| --- | --- | --- | --- |
| 29-42 | AGENT.md「複数のmoduleや仕組みにまたがる約束」 | 冒頭のproviderとtokenの説明は概念と地図の中間で、起動の形の細部（MCPを読まない・stdin）がmodule docにもある。 | map |
| 115-121 | AGENT.md「部品の外の読み手・人が頼る結果」 | 出力の扱いはfindingsの読み手が頼る約束だが、finding kindの列挙とaskのoptionの細部を含み切り分けが微妙。 | map |

### `supervisor-lifecycle/draft-planners.md`

番号付きの手順と再検討の節の大半がdocと同文。doc側は揃うので地図の約束と入口へ縮める
丸ごと移せる節: 再検討の時刻の本文、人のplannerの廃止の箇条、3・5・7の手順。
分割: 不要（移すと約15KB）。
同じ事実を持つ別の文書: docs/design/supervisor-lifecycle/plan-planners.md, docs/design/supervisor-lifecycle/plan-review.md, docs/design/follow-up-membership.md, docs/design/supervisor-lifecycle/run-environment.md（定期draft）。

| 節 | 行 | 規則 | 細部 | 移し先 | 地図に残す 1 行 | 移し先に同じ事実 |
| --- | --- | --- | --- | --- | --- | --- |
| Draft planners（1 出どころ） | 36-46 | DL-1 | 出どころごとの登録とreopenの行の持ち方 | src/domain/follow_up.rs の module doc、integrate.rs の register_follow_ups、draft_planners.rs | readyに直接戻す経路は作らない（47-48行を残す） | 有: domain/follow_up.rs:6-15、integrate.rs:1765、ports/planning.rs:334 |
| Draft planners（2 対象） | 49-57 | DL-5 | 対象の条件と束の鍵の書き写し | src/infrastructure/draft_planners.rs の targets、domain::follow_up の bundle_key | 対象の条件は`targets`。passは同じreceiptのdraftを全件か0件で見る | 有: draft_planners.rs:44-59 targets doc、follow_up.rs:361 bundle_key |
| Draft planners（3 立てる） | 59-71 | DL-1 | 立てる手順と初期promptの中身 | supervise/planning/draft_planner.rs の open_draft_planners、prompt::draft_planner_prompt | 上限は全runtimeのplannerで共有。2つのsupervisorが同じdraftに立てない | 一部: draft_planner.rs:1 :170、draft_planners.rs:446。promptの中身はコードのみ |
| Draft planners（4 上限まで） | 72-77 | DL-1 DL-5 | 回数の数え方と使い切ったdraftの行き先 | domain::follow_up の MAX_DRAFT_PLANNERS の doc comment | 上限を越えたdraftはattentionになり人かinboxの依頼を待つ | 有: follow_up.rs:61-67 MAX_DRAFT_PLANNERS doc |
| Draft planners（5 planner_question） | 78-99 | DL-1 | answerの行き先(a)〜(d)ごとの配送とclaimの手順 | ports/planning.rs の PlannerAnswerRoute、draft_planners.rs の claim_planner_answer の doc comment | 行き先は`planner_answer_route`。answerは二重に送らない。閉じた時刻と比べない | 一部: ports/planning.rs:288、draft_planners.rs:671（どちらも typing の古い語） |
| Draft planners（6 採用の記録と上限） | 100-104 | DL-5 | adopt無しで出せないdraftの条件と深さを0に戻す場合 | draft_planners.rs の check_adoptions、domain::follow_up の FOLLOW_UP_ASK_DEPTH | runtimeのplannerは上限のdraftを人のadopt無しに出せない（ADR-t808-1） | 有: draft_planners.rs:1719 check_adoptions doc、follow_up.rs:70 |
| Draft planners（7 終わり） | 105-114 | DL-1 | 終了の依頼とturnの上限の場面ごとの扱い | docs/design/supervisor-lifecycle/plan-review.md、plan-planners.md の終わりの節 | 終わりはplan-review.mdとplan-planners.md（画面は読まない） | 有: 107-113行が plan-review.md・plan-planners.md の節を名指す |
| Draft planners（8 記録と読み方） | 115-120 | DL-2 DL-5 | 結末のevent・showの欄・statsの欄 | domain::follow_up の DraftOutcome、views、stats::follow_up_categories | 結末は`DraftOutcome`。元のrunから`events`で各draftまで引ける | 有: follow_up.rs:509-515 DraftOutcome doc、domain/views.rs:236 |
| 再検討の時刻 | 128-154 | DL-1 DL-5 | 持ち方・付ける・戻す・立てる・promptの細部 | domain::DraftRevisit、draft_planners.rs の revisit_draft、prompt の revisit_section | 使った時刻では二度と立たない。残すなら新しい時刻を付ける | 有: follow_up.rs:374 :420、draft_planners.rs:320 :98、prompt/planners.rs:859 |
| follow_upの種類と判断の集計 | 161-163 | DL-5 | promptの種類の行とcancelの理由の読み方 | prompt の follow_up_category_line、domain/stats/follow_up_categories.rs | 集計は`stats`の`follow_up_categories`。新しいeventは足さない | 有: prompt/planners.rs:930、stats/follow_up_categories.rs:1-2 |
| 推奨が出せればplannerが決める（ADR-t451-1） | 171-176 | DL-5 | promptの決め方と問いの条件の書き写し | prompt::DECIDE_YOURSELF・RUNTIME_PLANNER_ASK、draft_planner_prompt の doc comment | 聞くのは(a)scope/discard(b)低確信(c)自動採用しない上限だけ | 有: prompt/planners.rs:1000 DECIDE_YOURSELF、:35 RUNTIME_PLANNER_ASK |
| 人のplannerの廃止と非対話の経路 | 186-189 | DL-5 | answerを読み終えたかの判定 | domain::turn::request_read、plan_review.rs の answer_taken、headless_answer_taken | 読み終えた判定は`request_read`と`answer_taken` | 有: domain/turn.rs:200、plan_review.rs:2293、planner_turns.rs:148 |
| 人のplannerの廃止と非対話の経路 | 191-199 | DL-1 | draftの行き先と非対話の経路の再説明 | 同じ文書の 4・5・7 と docs/design/supervisor-lifecycle/plan-planners.md | 残さない。人のterminalのsubmitは深さを0に戻す事実だけ6へ | 有: 104行（深さ0）、194行は6と同じ。plan-planners.md を196-199行が名指す |
| 所属の判断 | 210-220 | DL-5 DL-1 | 所属の判断のpromptの文面の書き写し | prompt の follow_up_membership_step・follow_up_proposal_line、follow-up-membership.md | promptは`follow_up_membership_step`。acceptanceを弱めず人に聞く | 有: prompt/planners.rs:988-994 :957、docs/design/follow-up-membership.md |
| recurringのdraft | 229-238 | DL-5 | recurringの登録の欄、束、採否と深さの細部 | infrastructure/recurring_drafts.rs の module doc、domain::recurring、draft_planner_prompt | 登録はtransactionで再検査し引き継ぎでも二重にしない（ADR-0072） | 有: recurring_drafts.rs:1-8、domain/recurring.rs:1-15 |

迷った行（違反の範囲と重なることがある）:

| 行 | 当てた規則 | 迷った点 | 寄せた側 |
| --- | --- | --- | --- |
| 78-99 | AGENT.md「コードから読めない約束」 | 96-97行の時刻を比べない理由は落とし穴だがplanner_turns.rs:148のdocにもある。列挙の中で残す範囲を決めきれない。 | detail |
| 203-208 | AGENT.md「複数のmoduleや仕組みにまたがる約束」 | 所属の判断とsubmit・ready・lintの関係は規則で地図だが、follow-up-membership.mdとの重複もあり残す範囲が決めきれない。 | map |

### `supervisor-lifecycle/auto-update.md`

本文の大半がjobの段の手順の書き写しで、多くはdoc commentに既にある。行を消して入口と約束だけ残す
丸ごと移せる節: 後始末、流し直し、入れ替え、見張り、supervisorを戻す。
分割: 不要: 畳むと約14.5KiB。
同じ事実を持つ別の文書: docs/design/supervisor-lifecycle/install.md、docs/design/queue-service.md、docs/design/queue-broker.md。

| 節 | 行 | 規則 | 細部 | 移し先 | 地図に残す 1 行 | 移し先に同じ事実 |
| --- | --- | --- | --- | --- | --- | --- |
| きっかけ | 37-49 | DL-1 DL-5 | auto_update_passの判定の段とbaseの決め方 | src/application/supervise/host_ops/update.rs の HostOpsState::auto_update の doc comment | passごとにmainを見てbase以降のruntime変更でjobを1つ起動（auto_update_pass） | 一部: host_ops/update.rs:125 コードとコメント、段の一覧のdocは無い、base_commit doc update.rs:475 |
| job（起動） | 52-55 | DL-5 | jobの起動の形（current_exe・setsid・logの置き場・記録） | src/application/supervise/host_ops/update.rs の start_update_job の doc comment | 残さない: 独立したsessionで動くことはmodule docにある | 一部: host_ops/update.rs 冒頭 'runs in a session of its own'、:422 new_session |
| 1 ビルド／着地の検証との順番 | 61-63,68-73 | DL-1 DL-5 | checkoutとbuildの段、番のlockを取る順と確かめの手順 | src/application/heavy_turn.rs の module doc と HeavyTurn::try_check、update::run の doc | 関門と着地の検証は番を交代し、jobは走行中の検証の終わりまで待つ（heavy_turn） | 有: domain/heavy_turn.rs:9 順序、application/heavy_turn.rs:1 lock、update.rs:1 module |
| e2eの関門 | 74-76,79-80 | DL-5 | envの作り方、上限での停止、落ちたtestの読み方 | src/infrastructure/e2e_gate.rs の module doc と PASSED_ENV | envの規則はe2e_gateのmodule docが持つ。lockの共有（77-78）は残す | 有: e2e_gate.rs:1 module doc、:46 PASSED_ENV、:579 stop_group |
| 後始末 | 85-92 | DL-1 | TMPDIRの片付けの手順と消せないときの分岐 | src/infrastructure/e2e_gate.rs の clean_up の doc comment（前の関門の残りの片付けは足す） | 残さない: 後始末はe2e_gateのmodule docが持つ | 一部: e2e_gate.rs:1 module doc・:660 clean_up、残りの片付けはfinished(:626)に短いdocのみ |
| 流し直し | 93-97 | DL-1 DL-5 | 流し直しの条件・上限・flakyの決め方 | src/infrastructure/e2e_gate.rs の run と流し直しの関数の doc comment | 落ちたtestだけを名前で1回流し直す（ADR-t1165-1、e2e_gate::run） | 有: e2e_gate.rs:227 rerun対象 doc、:240 落ちた判定 doc |
| 印 | 103-109 | DL-5 DL-2 | 印の効く条件、続けての失敗の数え方、errorに載る欄 | src/domain/e2e_quarantine.rs の module doc と failures_in_a_row、e2e_verdict::judge | 判定はe2e_verdict::judge。自動更新・install・runのe2eが共有し失敗を合わせて数える | 有: e2e_quarantine.rs:14 効く条件、:355 failures_in_a_row doc、e2e_verdict.rs:1 |
| e2eの関門（結果） | 110,114-116 | DL-1 | 落ちたときの記録・askの開き方・retryの動き | src/application/update.rs の run の doc comment | 落ちれば何も置き換えずapprove_updateも開かない（113を残す） | 一部: update.rs:559 run doc は outcome のみ、module doc に 'replaces nothing' |
| 入れ替え | 122-129 | DL-1 DL-5 | 引き継ぎの待ち方・猶予・後継の照合・全員か一部の失敗の分岐 | src/application/lifecycle.rs の hand_off と successor、Handed の doc comment | 入れ替えはinstall::install、引き継ぎの待ちはlifecycle::hand_offが持つ | 有: lifecycle.rs:1457 hand_off doc、:1938 successor、:1386 Handed |
| 見張り | 130-137 | DL-1 | settleの範囲と見張りの成否の条件、失敗の分岐 | src/application/update.rs の settle と watch の doc comment | 全員失敗のときだけbinaryを戻す（ADR-t632-1）。人のinstallもsettleを共有する | 有: update.rs:1103 settle doc、:1642 watch doc |
| supervisorを戻す | 138-144,146-147 | DL-1 | 失敗の段ごとに戻す相手とmodeごとの起動し直し | src/application/update.rs の bringing_back・Recovery・restart の doc comment | in-cmuxの登録はjobが起こせず、askに載る`up`を人が打つ（145を残す） | 有: update.rs:1851 bringing_back doc、:2165 launchdは任せる、:2187 restart_by_update |
| 経過の記録 | 152,155 | DL-5 | update_eventsの読みと並べ直し | src/domain/mod.rs の in_job_order と runtime_store/run_log.rs の update_events | 残さない: 151のkind一覧とrun_eventsの行で足りる | 有: domain/mod.rs:2934 in_job_order、run_log.rs:29 update_events doc |
| 経過の記録（timeline・stats） | 159-165 | DL-5 | timelineの並べ方とstatsのupdatesの数え方 | src/domain/timeline.rs の merged と src/domain/stats/updates.rs の module doc | `stats`のupdatesの意味はdomain::stats::updatesのdoc comment | 有: domain/timeline.rs:25 merged、stats/updates.rs:40 人のinstallの分離・:49 関門 |
| inboxに知らせる | 167,169-171 | DL-5 | 古いaskの閉じ方と入れ替えが閉じる失敗のask | src/infrastructure/asks.rs の open_update_ask、update.rs の close_settled_failures | jobのaskと人のinstallのaskは互いに閉じない（168を残す） | 有: asks.rs:483 open_update_ask doc、update.rs:262・:290 settled doc |
| update_failed | 176-179 | DL-1 | retry・skipの適用とリリースのjobの段の除外 | src/application/supervise/host_ops/update.rs の apply_update_answers の doc comment | 答えの適用はsupervisorの`apply_update_answers`。人のinstallの答えはinboxが閉じる（180） | 有: host_ops/update.rs:214 doc、update.rs:340 failed_release doc |
| queue serviceとqueueのbroker | 189-190 | DL-1 | 入れ替えの順の書き写し（他の3文書にある） | docs/design/queue-service.md・docs/design/queue-broker.md の「起動と停止」 | 引き継ぎはどちらも止めない（188）。順はQueue service・Queue brokerへのリンク | 有: install.md:113 同文、queue-service.md:266、queue-broker.md:149 |
| dagqのソースのrepositoryだけ | 204-205 | DL-1 | ソースでないときの回ごとの動き | src/application/supervise/host_ops/update.rs の auto_update の doc comment | ソースでなければbuildに進まない（202を残す） | 一部: host_ops/update.rs:150 コメントとwarn、log_nothing_built :207 |

迷った行（違反の範囲と重なることがある）:

| 行 | 当てた規則 | 迷った点 | 寄せた側 |
| --- | --- | --- | --- |
| 33-35 | 地図の約束に当たるのは…部品の外の読み手・書き手・人が頼る結果 | フラグの有無で設定が変わるのは人が頼る結果だが、set_auto_updateで書く場面の列挙はDL-1にも読める | map |
| 82-84 | DL-5: 列挙や識別子の形をとらずコードから読める細部 | 今の外したケースの有無はコードでなくtesting.mdの規則の参照で、状態の記述をどの層に置くか決めきれない | detail |
| 194 | DL-2: test名の並び | test名でなく層ごとの受け持ちの一覧で、testing.mdの一般規則の写しか地図の入口か決めきれない | map |

### `container-runtime.md`

節ごとの約束は地図として正しいが、手順と部品の中身がmodule docと二重。多くは消すだけで済む
丸ごと移せる節: transcript、見張り、imageのtag・build・失敗。
分割: 不要: 畳むと約19.1KiB。余白が薄く追記で分割が要りうる。
同じ事実を持つ別の文書: なし。

| 節 | 行 | 規則 | 細部 | 移し先 | 地図に残す 1 行 | 移し先に同じ事実 |
| --- | --- | --- | --- | --- | --- | --- |
| 適用先と設定の読み込み | 40-43 | DL-5 | 読み直しの頻度と束縛の読み方、役割の拒み方 | src/infrastructure/run_env.rs の parse_config・BoundRuntimes の doc comment | 束縛が解決できなければ読むたびにerror（wrapperのturnはhostで起動しない） | 一部: run_env.rs:3532 BoundRuntimes doc、container_runtime.rs:84 refuse_container |
| fail closedの境界 | 50,58 | DL-2 DL-5 | gateの型の並びと問う順、actorのbackendの決め方 | src/application/ports/execution.rs の ContainerRuntime、session.rs の actors の doc | 使えるかはContainerRuntimeに問い、部品ごとのgateが理由つきで答える | 一部: ports/execution.rs:2648 部品の列挙、順はcompose/execution.rs:690、session.rs:93 |
| 見せ方 | 75 | DL-2 DL-5 | runtimesとenforcementの値の並び | src/application/health/host.rs の add_runtimes の doc comment | `status`・`doctor`の`runtimes`の入口は`health::add_runtimes` | 有: health/host.rs:355 add_runtimes doc（host advisory・container sandbox） |
| dagq専用のmachine（起動と停止） | 84-90 | DL-1 DL-5 | 既定の接続の戻し、ensure・releaseの呼ばれ方、使う者の数え方 | src/application/podman_machine.rs の ensure・release、infrastructure/podman_machine.rs の module doc | 並行しても作るのと起動は1回（host全体のlock）。人の既定の接続は変えない | 有: app/podman_machine.rs:483 ensure、:551 release、infra同:1 既定接続の戻し |
| メモリの予約 | 97-99 | DL-1 | 予約の持ち期間と待つ係の場面ごとの列挙 | src/application/podman_machine.rs の module doc と try_reserve・wait_for_memory の doc | 予約の判断はdomain::machine_memory。待ちはrunのeventとstatusに出る | 有: application/podman_machine.rs:11、machine_memory.rs:1、run_container.rs:479 柵 |
| named volume・使えるかの答え | 108,114 | DL-5 | closeが消す物、MachineGateの見方 | src/application/podman_machine.rs の look と src/application/broker_relay.rs の close | 残さない: 同じ事実がlookのdocにある。volumeの範囲の約束（106-107）は残す | 有: app/podman_machine.rs:622 look doc、app/broker_relay.rs:1 close |
| image（base・client） | 121-125,127 | DL-5 | baseの中身、Claude Codeの版、clientの取り方と検査 | src/domain/container_image.rs の module doc と client_source の doc | 未pushのcommitではbaseのbuildが落ちる（126を残す）。約束を残す: 言語の道具を残さない（122）・hostにcheckoutは要らない（127） | 有: domain/container_image.rs:1 module doc、:98 client_source、:22 CLAUDE_CODE_VERSION |
| image（tag・buildと片付け・失敗） | 132,135-142 | DL-1 DL-5 | tagの入力、prepareの手順、失敗の記録と控え | src/domain/container_image.rs の Images::of・steps・failure_holds、app の prepare | 秘密とhostのpathはimageに入らない（139の一部）。入口は`container_image::prepare` | 有: domain同:187 Images::of、app同:147 prepare、:330 failure_holds |
| brokerへの経路（socket・relay・転送） | 149-154,156-158 | DL-1 DL-5 | relayの起動の手順とuid・mode、転送のportとenv | src/domain/broker_relay.rs の module doc・relay_run_args・container_env | relayと転送はtokenを持たず宛ても判じない（155を残す）。入口はbroker_relay | 有: domain/broker_relay.rs:1、:76 keep-id、:44 HOST_GATEWAY、infra同:1 |
| 見張り | 162-169 | DL-1 | relayの見張りの分岐と起動し直しの段 | src/application/supervise/host_ops/broker_relay.rs の module doc、domain の relay_fate・relay_action | attentionが立つ間はRelayGateが使えないと答える（170を残す） | 有: host_ops/broker_relay.rs:1 module doc、domain/broker_relay.rs:255・:310 |
| git（cloneの作成・事実・main） | 178,180-182,188 | DL-5 | cloneの取り込みの手順、bundleの読み方、mainの渡し方 | src/infrastructure/adapters/run_clones.rs の module doc、Repository::create_clone 等の doc | remote・資格情報は置かない（179を残す）。読み方の分岐はrun_gitの1か所 | 一部: run_clones.rs:1 module doc、:572 create_clone、:1043 hand_mainはport参照のみ |
| 事実だけのturn | 184 | DL-2 | 10分と11分30秒の内訳 | src/infrastructure/run_container.rs の FACTS_TURN_TIMEOUT の doc comment | cold buildは拒む（supervisorのpassを塞がない）入口はFACTS_TURN_TIMEOUT | 有: infra/run_container.rs:73 FACTS_TURN_TIMEOUT doc、FACTS_CLEANUP_TIMEOUT |
| workerの実行（準備・container・中） | 201,205-206,213-214 | DL-1 DL-2 | turn前の手順、hardeningの並び、中の処理 | src/domain/run_container.rs の module doc、src/infrastructure/container_turn.rs の module doc | 準備が落ちればturnを起動しhostへ戻さない（202）。hardeningはworker_run_args | 有: domain/run_container.rs:1 hardening、container_turn.rs:1 順、run_container.rs:1 |
| transcript | 215-217 | DL-5 | transcriptの置き場と探す順 | src/infrastructure/transcripts.rs の module doc | 残さない: ClaudeTranscriptsのmodule docが同じ事実を持つ | 有: transcripts.rs:1 module doc（hostが先、run dirの下、linkを辿らない） |
| 止める | 219-221 | DL-1 | 消去の経路の列挙とbest effortの扱い | src/application/run_container.rs の RunContainers::remove、recording.rs の RecordingSessions の doc | podmanを止めてもcontainerは残るのでturnごとに消す（218）。どの経路も消す | 一部: run_container.rs:161 remove doc、recording.rs:323、経路の列挙は無い |
| verificationの実行 | 232,234-235,237-239,242,245 | DL-1 DL-5 | 準備の手順、dirの条件、準備コマンド、上限、sccacheの外し方 | src/application/integrate.rs の ContainerVerification、run_clones の create_verification_clone 等 | mountの約束（240-241）と制御側のgit禁止（236）は残す。入口はContainerVerification | 有: run_clones.rs:665 doc、run_env.rs:370 TABLE doc、integrate.rs:2978 sccache |

迷った行（違反の範囲と重なることがある）:

| 行 | 当てた規則 | 迷った点 | 寄せた側 |
| --- | --- | --- | --- |
| 55-57 | 約束・不変条件を並べた箇条の一覧に足した約束は、場面ごとの列挙に当たらない | 切替前のrunもhostで起動しない約束だが、wrapperのturn起動の場面の手順とも読める | map |
| 104 | 地図の約束に当たるのは…部品の外の読み手・書き手・人が頼る結果 | 作った時点のqueueだけ共有される落とし穴だが、host_volumesのdocに同じ事実があり重複の詳細とも読める | map |
| 203-204 | DL-5: コードから読める細部 | tokenの値がargvに無いのは安全の保証だが、run dirのfileで渡す手順はdomainのdocで分かる | map |

### `plugin-integration.md`

Rustでなくscript・skill・他のdesignの写しが中心。移すより指す形に縮め、skillの契約は3節とも消せる
丸ごと移せる節: skillの契約（登録・起動・着地）、決定済み・未実装の箇条。
分割: 不要: 畳むと約16.6KiB。
同じ事実を持つ別の文書: docs/design/supervisor-lifecycle/up-down.md、docs/design/supervisor-lifecycle/review.md、.claude/skills/release/SKILL.md。

| 節 | 行 | 規則 | 細部 | 移し先 | 地図に残す 1 行 | 移し先に同じ事実 |
| --- | --- | --- | --- | --- | --- | --- |
| tagとversionの一致規則 | 52 | DL-1 | workflowのstepの順と比べ方 | scripts/check-plugin-version.sh の冒頭のcomment、.github/workflows/release.yml | tagとversionが違えばbuildの前に止まる（検査はcheck-plugin-version.sh） | 有: check-plugin-version.sh:19 --tag の説明、release.yml:46 |
| crates.io | 82-86,92-93 | DL-1 DL-2 | publishの段と認証、リリースの手順、初回のpublish | release.yml のcomment と .claude/skills/release/SKILL.md | crates.ioは追加の経路（ADR-0030）。手順はrelease skill | 有: release.yml:97 skip comment、.claude/skills/release/SKILL.md:8、:42 |
| Claude Code plugin | 118 | DL-2 | skillごとのdescriptionの言い換え | plugins/claude-dagq/skills/*/SKILL.md の description | descriptionはtriggerが重ならないように書き分ける（SKILL.mdのdescriptionが正） | 有: skills/dagq-inbox/SKILL.md:3 ほか各SKILL.md:3 |
| 起き直しhook | 129-132,135-137,141-143 | DL-1 | sourceとroleごとの出力の分岐 | plugins/claude-dagq/hooks/session-start.sh の冒頭のcomment、hooks.json | 先頭の1行が必須（JSONだと捨てられる、140）。分岐はsession-start.shのcomment | 有: session-start.sh:1-25 comment に source・role・1行目・exit 0 |
| watchを張らせるStop hook | 153-155 | DL-1 | hookの配線と働く条件 | plugins/claude-dagq/hooks/stop-watch.sh の冒頭のcomment | Stop hookがwatchの無いturnの終わりを止める（151）。条件はscriptのcomment | 有: stop-watch.sh:1-18 comment |
| sessionの区間hook | 167-170,178-179 | DL-1 | hookの配線、渡す条件、失敗時の出力 | plugins/claude-dagq/hooks/session-event.sh の冒頭のcomment | 区間のeventだけを書きrunの状態を変えず、失敗でsessionを止めない | 有: session-event.sh:1-17 comment |
| sessionの区間hook（閉じ方） | 172-176 | DL-5 | 同じ・別のsession_idの区間の続け方と閉じ方 | src/domain/sessions.rs の SessionHook と開閉の判断の doc comment | `/clear`とcompactionで二重に数えない（判断はdomain::sessions） | 一部: sessions.rs:996 同じsessionのresume、:999 時間で閉じない、:1911 test |
| launcher | 185-186,190,192-194 | DL-1 DL-5 | バイナリの解決の順、--resolve、不一致の警告 | plugins/claude-dagq/bin/dagq の冒頭のcomment | launcherは解決してexecするだけでDBを開かない（183）。振る舞いはscriptのcomment | 一部: bin/dagq:3-11 comment、:71 警告のcomment（skillの報告はSKILL側） |
| skillの契約（登録） | 199-206 | DL-1 | 登録の手順とgoal close・follow_upの流れの写し | plugins/claude-dagq/skills/dagq/reference/register.md・goal-close.md、draft-planners.md | 完了はshowで判定し、readyはplan reviewだけ、goal closeはplannerの手順でない | 有: dagq/reference/register.md:31 draftからready、goal-close.md:3 |
| skillの契約（supervisorの起動） | 207-215 | DL-1 | up・downと各attentionでの人の操作の写し | docs/design/supervisor-lifecycle/up-down.md、skills/dagq-recover/reference/up-down.md | 起動はdagq-recoverのsection 5。upはinboxとplannerを開かない | 有: up-down.md にsupervisor_duplicate、domain/mod.rs:3505、dagq-recover/SKILL.md |
| skillの契約（着地） | 216-224 | DL-1 | reviewと着地、needs_sessionの場面ごとの手順の写し | docs/design/supervisor-lifecycle/review.md・integrate.md、reference/review-by-hand.md | 着地はintegrateだけが行い、inboxは答えるだけ。手順はreview-by-hand.md | 有: review.md:62-64 concern・approve_landing、reference/review-by-hand.md |
| marketplaceとinstall（決定済み・未実装） | 245-252,257-260 | DL-1 | 実装後の姿としてのentryの形と更新の手順 | docs/adr/2026-09-27-t617-1-…md、.claude/skills/release/SKILL.md、release-update.md | 決定はADR-t617-1・t617-2。リリースの手順はrelease skill | 有: scripts/check-plugin-version.sh:9-22、release/SKILL.md:20、ADR-t617-1:52 |
| installしたpluginの確認 | 267-270 | DL-1 | 確かめない場面の列挙とenvの理由 | src/application/lifecycle.rs の require_installed_plugin・UP_RESTART_ENV の doc comment | --plugin-dir無しのupとinboxは何かを開く前に確かめ、runtimeが打つupは確かめない | 有: lifecycle.rs:128 UP_RESTART_ENV doc、:180 require_installed_plugin doc |
| installしたpluginの確認（確かめ方） | 272-275 | DL-5 | 実行のcwd、読みの共有、止まるときの文言 | src/infrastructure/adapters.rs の plugin_state、lifecycle.rs の require_installed_plugin | 判定と文言の入口はlifecycle::require_installed_plugin | 一部: adapters.rs:4060 plugin_state、lifecycle.rs:180、cwdはdocの'in cwd' |

迷った行（違反の範囲と重なることがある）:

| 行 | 当てた規則 | 迷った点 | 寄せた側 |
| --- | --- | --- | --- |
| 66-77 | 書くのは…コードへの入口。書かないのはDL-1 | 利用者のinstall手順でコードに写す先が無く、地図か利用者文書かを規則が決めていない | map |
| 243 | DL-4: 経緯（「以前は」「〜から変えた」） | 未実装の決定はこれからの姿で経緯ではないが、地図の層にも当たらない。253-256は実装済みに見える | detail |
| 279-281 | DL-1: 誰が何をどの順にするかの列挙 | 開発者が打つ検証のコマンドで、コードから読めず手順でもあり、manual-smoke.mdの方が近いと決めきれない | map |

### `supervisor-lifecycle/headless-worker.md`

冒頭の概念と入口の表は良い形。場面ごとの箇条の本体をdoc commentへ畳み、約束と落とし穴を残す
丸ごと移せる節: turnの後の扱い、receiptとsessionの終わり、wrapperの起動と停止。
分割: 要: 畳んで約21.7KiB。receiptの欄の約束を別文書へ。
同じ事実を持つ別の文書: docs/design/provider-lifecycle.md、docs/design/supervisor-lifecycle/plan-planners.md。

| 節 | 行 | 規則 | 細部 | 移し先 | 地図に残す 1 行 | 移し先に同じ事実 |
| --- | --- | --- | --- | --- | --- | --- |
| session wrapper | 115-117 | DL-1 | 登録と最初のturn・後のturnのagent行の扱い | src/application/session.rs の module doc、runtime_store/coordination.rs の register_turn_agent | 起動の入口はsession.rsのrun_session（表にある）。残さない | 有: session.rs:1 module doc に登録、coordination.rs:683 register_turn_agent doc |
| run dirのturns/ | 128-133 | DL-5 | 誰がどのfileを書くか、依頼の取り方と番号の数え方 | src/domain/turn.rs の module doc と request_to_take の doc、headless.rs の prepare_turns | 1つのsessionの依頼は1つのwrapperが1つずつ取る（不変条件にある）。入口はdomain::turn | 有: turn.rs:1 module、:116 request_to_take、:427 次の番号、headless.rs:279 |
| wrapperがturnを止めるとき | 158,161-162 | DL-1 | 止める条件の列挙、終わり方ごとの止める範囲 | src/application/headless_session.rs の module doc と stop_turn の doc comment | groupへのsignalは子孫に届かず、groupの前にpidで集める（160を残す） | 有: headless_session.rs:1 module に条件、:388 stop_turn doc、process.rs:191 |
| turnの後の扱い | 166-170,173-177 | DL-1 | turnの終わりの場面ごとにsupervisorとwrapperがすること | src/application/supervise/execution/stall.rs の observe_turn、domain/run.rs の session_end_status | receiptの再確認の落とし穴（171-172）とproviderの壁の行き先のリンクを残す | 有: stall.rs:1394 observe_turn doc、domain/run.rs:414 doc、turn.rs:94 |
| receiptとsessionの終わり | 181-186 | DL-1 DL-5 | idle markerの読み方、終了の依頼の順、無くした操作の並び | src/application/supervise/execution/exit.rs の ExitWatch::poll と session_idle の doc | turnの終わりはidle markerだけ（不変条件）。ADR-t1404-1決定3へのリンク | 有: exit.rs:21 poll doc、domain/turn.rs:1 module、domain::exit::ExitConfig |
| 黙ったwrapper | 190-192 | DL-1 DL-5 | 沈黙の記録と印の消し方、再試行の合計 | src/application/supervise/execution/exit.rs の wrapper_pulse と note_heartbeat_expired の doc | 黙っただけではkillしない（不変条件）。固定時刻のClockの落とし穴（193）を残す | 有: exit.rs:79 wrapper_pulse doc、:128 note_heartbeat_expired doc、session.rs:296 |
| workerへの答えの配送 | 199,201,203-204 | DL-1 DL-5 | 送る時点、記録とcloseの順、閉じたaskの扱い | src/application/supervise/execution/session.rs の deliver_answers の doc comment | 答えは次のturnの依頼。closeとの間にsupervisorが死ぬと二重に送りうる（202） | 一部: session.rs:1095 deliver_answers doc は打鍵の語で古い、配送の記録は同じ |
| 待ちの最中に失ったsessionの開き直し | 245,248 | DL-5 | 開き直しの対象、試みの状態の持ち方 | src/application/supervise/execution/reopen.rs の module doc | 同じ依頼を2つのwrapperが取らない（246）。wrapperを失ったturnのmarker（247） | 一部: reopen.rs:1 module doc の対象、:19 REOPEN_ATTEMPTS、試みの状態はコードのみ |
| 対話と記録されたtaskのclaimとresume | 257-258 | DL-5 | 過去のinteractiveのrunの読みと置き換え | docs/design/provider-lifecycle.md の workerのproviderと経路、domain/run.rs の doc | 残さない: 拒否はProvider lifecycleへのリンクで足りる（113の参照先も直す） | 一部: domain/run.rs:54 a run from before the mode existed is interactive |
| workspaceなしのbackgroundのwrapper（起動・handle・入口・env） | 266-272 | DL-1 DL-5 | 起動の手順、handleの記録の順、入口、envの外し方 | src/infrastructure/background.rs の module doc と TERMINAL_PANE_PREFIXES、compose::wrapper_entry | logはworkerが書けるのでshellに開かせない（266）。入口は表のBackgroundWrappers | 有: background.rs:1 module、:39 PREFIXES、compose.rs:600 doc、background_wrapper.rs:1 |
| workspaceなしのbackgroundのwrapper（停止・記録・出力） | 276-277,279,281-282 | DL-1 DL-5 | signalの順、残ったturnの止め方、StopRouteの記録、log | src/infrastructure/background.rs の module doc、StopRoute、session_log の module doc | SIGTERMかKILLかの数は評価が読む（280）。ADR-t813-1決定5の読み替え（278）を残す。約束を残す: 書けなくても止めない（279・282） | 一部: background.rs:1 SIGTERM・SIGKILL、background_wrapper.rs:408、session_log.rs:1。再判定: 「書けない行は捨てて止めない」はheadless_session.rs:461のコメント |
| 再計画の保留 | 290 | DL-1 | holdのときのturnの終わりと工程の止まり方 | src/application/supervise/execution/replan_park.rs の module doc | taskのholdも同じ柵で別の止め方を持たない（289） | 有: replan_park.rs:3 module doc の hold の park、:6 held run's turn |
| 非対話のruntimeのplanner | 298,302-303,305 | DL-5 DL-1 | 持ち主ごとの違い、起動とhandle、tokenの発行の手順 | src/application/planner.rs の launch_planner の doc、TurnOwner::Planner の doc | 壁のturnのmarkerの落とし穴（299-300）とproviderの切り替え無しを残す | 一部: planner.rs:195 launch_planner doc、headless_session.rs:1 module、handle待ちは無 |

迷った行（違反の範囲と重なることがある）:

| 行 | 当てた規則 | 迷った点 | 寄せた側 |
| --- | --- | --- | --- |
| 105-106 | 地図の約束に当たるのは…部品の外の者が頼る結果 | sessionの名前とthreadの扱いはdomain::turnで読めるが、provider間の違いを述べる流れの説明でもある | map |
| 146-148 | DL-5: コードから読める細部（何を読む） | Claudeが使用中の名前を拒むのは外部仕様の落とし穴だが、resumeの判定の中身の書き写しでもある | map |
| 253 | DL-4: 経緯 | 過去の`intervene`の答えの扱いは今も効く互換の挙動だが、画面の時代の経緯の説明とも読める | detail |

### `security.md`

信頼の模型の地図で大半は約束。表の後の権限の箇条をAuthorizationへ寄せ、道筋を分ける
丸ごと移せる節: 77-123の箇条の大半。
分割: 要。箇条を移しても約22.6 KiBで、将来の道筋を別文書に分ける。
同じ事実を持つ別の文書: docs/design/authorization.md, docs/design/queue-service.md, docs/design/container-runtime.md, docs/design/queue-broker.md。

| 節 | 行 | 規則 | 細部 | 移し先 | 地図に残す 1 行 | 移し先に同じ事実 |
| --- | --- | --- | --- | --- | --- | --- |
| actor × capability | 77-81 | DL-2 DL-5 | review.submit・queue.export・ci.readのrole別の持ち主 | docs/design/authorization.md の Policy／src/domain/authorization.rs の grants | 残さない（表とAuthorizationのPolicyが持つ） | 有: authorization.md:115,124 src/domain/authorization.rs:768 |
| actor × capability | 82-87 | DL-2 DL-5 | judge-follow-up・revisit・verify_editのrole別の可否 | docs/design/authorization.md の Capability・計画系のコマンド | 残さない（表の計画の列とAuthorizationへのリンクで足りる） | 有: authorization.md:61,155,157 src/domain/authorization.rs:39 |
| actor × capability | 88-91 | DL-5 | 人の優先度の拒否と所属の移動での扱いの細部 | docs/design/authorization.md の Policy／src/domain/task.rs の set_goal | 人の出どころの優先度はuserとinboxだけが変える（ADR-t1975-1） | 有: authorization.md:133 src/domain/task.rs:542 |
| actor × capability | 103-113 | DL-2 DL-5 | 計画の依頼・再計画・hold・evalのcapabilityの持ち主 | docs/design/authorization.md の 対話と記録のコマンド | 残さない（同じ事実がAuthorizationにある） | 有: authorization.md:77,78,82,207,214 |
| actor × capability | 114-120 | DL-1 DL-5 | executor-telemetryの発行・登録・失効の手順 | docs/design/queue-service.md の principalとtoken／src/application/telemetry_tokens.rs の module doc | tokenの対象のsessionだけ送れ、agentのenvとargvに渡さない（hostでは助言的） | 有: src/application/telemetry_tokens.rs:1 queue-service.md:102 |
| actor × capability | 122-123 | DL-2 | 予約のcapabilityの説明 | src/domain/authorization.rs の Capability の予約のコメント | 残さない（Authorizationの予約の行にある） | 有: src/domain/authorization.rs:136 authorization.md:97 |
| 判定の場所 | 146-147 | DL-1 | guardrailのsettingsの置き場とroleごとの経路 | docs/design/authorization.md の Claudeのpermissions.deny | 残さない（145・148のguardrailの約束とリンクで足りる） | 有: authorization.md:336-345 src/application/execution.rs:402 |

迷った行（違反の範囲と重なることがある）:

| 行 | 当てた規則 | 迷った点 | 寄せた側 |
| --- | --- | --- | --- |
| 93-102 | AGENT.mdの「部品の外の者がしてはいけないこと」 | してはいけないことと表の読み方の注で地図。再判定でDL-2・DL-5から外した。消すならAuthorizationとの重複を確かめる | map |
| 62-75 | documents.md「権限の表を写す文書」とDL-2 | grantsの要約でDL-2に見えるが、文書の規則がこの表を一致の基準に名指すので移せない | map |
| 92 | AGENT.md DL-4「以前は」 | 「この段で運用の権限を変えていない」は経緯にも、skillと合わせる今の規則にも読める | map |
| 210-217 | AGENT.md「複数のmoduleや仕組みにまたがる約束」 | 道筋の第1段が今のcontainerの事実の重複だが、安全の境界の要約として読者が頼る | map |
| 223-225 | AGENT.md DL-5と「部品の外の者が頼る結果」 | brokerの宛先の限定と資格情報の差し替えは保証だがQueue brokerと重なる細部でもある | detail |

### `supervisor-lifecycle/run-worktrees.md`

掃除の場面ごとの手順の写しが大半で、cleanupのmodule docと各関数のdocが既に同じ事実を持つ
丸ごと移せる節: 終わっていないrunのビルド成果物の箇条の大半、loopの外で掃除するの箇条の大半。
分割: 不要。移せば約8 KiB。
同じ事実を持つ別の文書: docs/design/supervisor-lifecycle/claim-hold.md, docs/design/supervisor-lifecycle/waiting.md。

| 節 | 行 | 規則 | 細部 | 移し先 | 地図に残す 1 行 | 移し先に同じ事実 |
| --- | --- | --- | --- | --- | --- | --- |
| Run worktrees（冒頭） | 15 | DL-5 | 掃除の候補のrunの状態とleaseの条件 | src/infrastructure/runtime_store/run_log.rs の ended_run_worktrees の doc comment | 対象は生きたleaseの無い終わったrun（入口はended_run_worktrees） | 有: src/infrastructure/runtime_store/run_log.rs:1062 |
| Run worktrees（冒頭） | 21-22 | DL-2 DL-5 | scratchのpathの形とhashの桁 | src/domain/views.rs の recheck_scratch_name の doc comment | runのworktreeと3つのscratchは名前が重ならない | 有: src/domain/views.rs:596 |
| Run worktrees（冒頭） | 23-28 | DL-1 DL-4 DL-5 | 古い配置のworktreeとscratchの場面ごとの扱い | src/domain/views.rs の with_layout_of／src/domain/replan_snapshot.rs の legacy_scratch_worktree | 古い配置のrunも見つけて扱う（入口はRunPaths::with_layout_of） | 有: src/domain/views.rs:640 src/domain/replan_snapshot.rs:68 |
| Run worktrees（冒頭） | 29-31 | DL-1 DL-5 | addが競合で落ちたときのやり直しと片付け | src/infrastructure/adapters.rs の worktree_command・clear_failed_add | 同時のaddの競合は片付けて流し直す（入口はworktree_command） | 有: src/infrastructure/adapters.rs:760,2206 |
| Run worktrees（冒頭） | 35-41 | DL-1 DL-4 | taskが終わったrunの削除とプロセスの停止の順 | src/application/supervise/host_ops/cleanup.rs の request_cleanup／src/application/integrate.rs の remove_landed_worktree | 消す前にそのrunのworktreeで動くプロセスを止める（入口はrequest_cleanup） | 有: src/domain/disk.rs:240 src/application/integrate.rs:3938,4016 |
| Run worktrees（冒頭） | 42-52 | DL-1 | removeの失敗からrepair・dirの削除への分岐 | src/application/supervise/host_ops/cleanup.rs の owns_admin・may_remove_broken | 他のworktreeのadmin dirとbranchには触れない（入口はowns_admin） | 有: src/application/supervise/host_ops/cleanup.rs:2526,2557 |
| Run worktrees（冒頭） | 53-56 | DL-1 DL-5 | 終わっていないtaskのビルド成果物の消し方 | src/application/supervise/host_ops/cleanup.rs の remove_build_outputs の doc comment | 次のrunが引き継ぐのでworktreeとbranchは残しビルド成果物だけ消す | 一部: src/application/supervise/host_ops/cleanup.rs:2169 |
| 終わっていないrunのビルド成果物 | 62-68 | DL-1 | 対象のrunと場面ごとの消す時点の列挙 | src/application/ports/execution.rs の WorktreeCleanup の各variant | 答え待ちは毎回、他は空き容量のときだけ消す（入口はWorktreeCleanup） | 有: src/application/ports/execution.rs:982 cleanup.rs:41 |
| 終わっていないrunのビルド成果物 | 70,73-84 | DL-1 DL-5 | 着地slot待ちのrunの渡し方・順・予約の手順 | src/application/supervise/execution/sweeps.rs の landing_worktrees／cleanup.rs の landing_candidate・Cleaning | 着地が始まったrunのビルド成果物は残る（入口はlanding_worktrees） | 有: src/application/supervise/execution/sweeps.rs:454 cleanup.rs:787,561 |
| 終わっていないrunのビルド成果物 | 85-94 | DL-1 DL-5 | 古いincrementalのsessionの消し方と記録 | src/application/supervise/host_ops/cleanup.rs の remove_stale_entries・leave_pruned | 次のbuildで戻るので空くのはbuildの止まる間だけ（入口はremove_stale_entries） | 有: src/application/supervise/host_ops/cleanup.rs:2262,715 |
| 終わっていないrunのビルド成果物 | 96-100 | DL-1 DL-5 | runnerとalready_satisfiedのscratchの消し方 | src/application/supervise/host_ops/cleanup.rs の remove_run_runner・satisfied_scratch_of | 残さない（入口の名前だけcleanupのmodule docにある） | 有: src/application/supervise/host_ops/cleanup.rs:1863,2002 |
| 終わっていないrunのビルド成果物 | 102-108 | DL-1 DL-5 | scratchpadの名前・親・消す条件・記録 | src/application/supervise/host_ops/cleanup.rs の scratchpad_dir_name／src/infrastructure/run_files.rs の claude_scratchpad_roots | 求めたdirの外と人のsessionのscratchpadには触れない（入口はscratchpad_dir_name） | 有: host_ops/cleanup.rs:1082 src/infrastructure/run_files.rs:24 |
| 終わっていないrunのビルド成果物 | 110-118 | DL-1 DL-5 | turnと検証ごとのTMPDIRの作り方と置き場 | src/application/ports/execution.rs の RUN_TMP_DIR・run_tmp_dir・host_verification_tmp_path | run_dirのtmpと検証の短いdirをTMPDIRに渡す（入口はRUN_TMP_DIR） | 有: src/application/ports/execution.rs:25,46,60 |
| 終わっていないrunのビルド成果物 | 121,124-127,129-130 | DL-1 DL-2 | TMPDIRを渡さない経路と消す条件・記録 | src/application/supervise/host_ops/cleanup.rs の run_tmp_dirs の doc comment | 残さない（122-123・128の約束は残す） | 一部: host_ops/cleanup.rs:1913 execution/e2e.rs:185（渡さない経路はe2eのコメントのみ） |
| loopの外で掃除する | 141,143-147 | DL-1 DL-5 | 頼み・予約・lock・読み直しの手順 | src/application/supervise/host_ops/cleanup.rs の module doc と CleanupWatch | どちらが先でも掃除中のworktreeのrunはclaimされない（入口はrequest_cleanup） | 有: src/application/supervise/host_ops/cleanup.rs:13-22,203 |
| loopの外で掃除する | 149-152 | DL-5 | 片付け済みのrunの覚え方と忘れる条件 | src/application/supervise/host_ops/cleanup.rs の pick_candidates・Cleaning::settled | 片付け済みは同じプロセスの次の掃除で候補にしない（入口はpick_candidates） | 有: src/application/supervise/host_ops/cleanup.rs:24,575,692 |
| loopの外で掃除する | 155-165,167-168 | DL-1 | stop・handoff・空き容量のjobの場面ごとの扱い | src/application/supervise/host_ops/cleanup.rs の module doc と Request | stopとhandoffでも空き容量のjobは選んだ分を終える（入口はcleanupのmodule doc） | 一部: src/application/supervise/host_ops/cleanup.rs:18-22,163 |
| loopの外で掃除する（節の後） | 170,172-174 | DL-1 DL-5 | 消す契機の列挙と測れないときの記録 | src/application/supervise/execution/sweeps.rs の sweep_ended_runs／cleanup.rs の Size | 計測の失敗で掃除は止まらない（175）。入口はsweep_ended_runs | 有: src/application/supervise/execution/sweeps.rs:487 cleanup.rs:933 |
| loopの外で掃除する（節の後） | 177-179 | DL-2 DL-5 | sessionの停止とworkspace_closed_atの意味 | src/domain/run.rs の workspace_closed_at の doc comment | 止めていないsessionをcleaned扱いにしない（180） | 有: src/domain/run.rs:93-96 |

迷った行（違反の範囲と重なることがある）:

| 行 | 当てた規則 | 迷った点 | 寄せた側 |
| --- | --- | --- | --- |
| 119-120 | AGENT.md「複数のmoduleや仕組みにまたがる約束」 | promptの案内の書き写しだが、TMPDIRの片付けの前提を複数の部品で揃える約束にも読める | map |
| 131-132 | AGENT.md「部品の外の者が頼る結果」 | container・client modeとの関係は今の事実の説明だが、将来の変更者が頼る境界にも読める | map |
| 139 | AGENT.md DL-4 | 採らなかった方式の理由で経緯に近いが、ADRが無く地図の設計理由としてここにしか無い | map |
| 153-154 | AGENT.md「コードが書かない保証」とDL-5 | 部品の中の設計理由だが、残る隙の受け入れはdoc commentに無い落とし穴でもある | map |

### `supervisor-lifecycle/prompt.md`

文面の要約が各所に残るが約束と入口も多い。定数のdocが既に持つ節を消して縮める
丸ごと移せる節: 受け入れ条件の対応づけ、文書の照合の大半、復旧jobのprompt。
分割: 不要。移せば約18 KiB。
同じ事実を持つ別の文書: docs/design/supervisor-lifecycle/review.md, docs/design/supervisor-lifecycle/language.md。

| 節 | 行 | 規則 | 細部 | 移し先 | 地図に残す 1 行 | 移し先に同じ事実 |
| --- | --- | --- | --- | --- | --- | --- |
| workerのprompt | 96-102 | DL-1 | resumeの種類ごとの検証の頼み方の文面 | src/application/prompt/requests.rs の ResumeKind と resume_request／worker.rs の local_checks | 検証の正はintegrateの1回（103）。入口はlocal_checksとResumeKind | 有: src/application/prompt/worker.rs:1361 src/application/prompt/requests.rs:114-124 |
| workerのprompt | 108-109 | DL-1 | e2eの1行と落ちたrunのresumeの文面 | src/application/prompt/worker.rs の e2e_line の doc comment | 残さない（107の約束と110の書かない理由は残す） | 一部: src/application/prompt/worker.rs:2095-2099 resumeの再現は requests.rs:98 |
| workerのprompt | 113-116 | DL-1 DL-5 | holdのresumeの文面とgoal依存の節の中身 | src/application/prompt/requests.rs の ResumeKind::Hold／worker.rs の prompt・GOAL_TASK_SUMMARY_CHARS | 残さない（入口はResumeKindとprompt） | 有: src/application/prompt/requests.rs:108 src/application/prompt/worker.rs:106 |
| workerのprompt | 118-120 | DL-5 | 切ったものごとの読む方法の列挙 | src/application/prompt/worker.rs の WORKER_PROMPT_LIMIT の doc comment | 省いたら読む方法か読めないことを書く（入口はWORKER_*） | 一部: prompt/worker.rs:1382 検索語「no way to read」はworker.rsに0件 |
| workerのprompt | 124,126 | DL-5 | 人の判断の節の上限と読むコマンドの例外 | src/application/prompt/worker.rs の PreviousDecisions の doc comment | 残さない（122-123の約束と落とし穴は残す） | 一部: src/application/prompt/worker.rs:326 |
| workerのprompt | 127-129 | DL-2 DL-5 | prompt_bytesの記録の時点と新しいsessionの文 | src/application/prompt/mod.rs の PromptBytes／src/domain/turn.rs の new_session_prompt | 残さない（入口は上の表とPromptBytes） | 有: src/application/prompt/mod.rs:86 src/domain/turn.rs:355 |
| 受け入れ条件の対応づけ | 137-141 | DL-5 | 定数の置き場所と文面の要約 | src/application/prompt/worker.rs の ACCEPTANCE_MAP・ALREADY_SATISFIED・ACCEPTANCE_REMAP | receiptの前に対応づけを置く（ADR-t1420-1、入口はACCEPTANCE_MAP） | 有: src/application/prompt/worker.rs:769,776,795 |
| 文書の照合 | 146-153,157-158 | DL-5 | DOCS_CHECKの候補・条件・行き先の文面の要約 | src/application/prompt/worker.rs の DOCS_CHECK／run_review.rs の REVIEW_DOCS_CHECK | 入口はDOCS_CHECK。文書の範囲はdocuments.mdが持つ（154-155は残す） | 有: src/application/prompt/worker.rs:790 src/application/prompt/run_review.rs:293 |
| workerが読んだ指示の版 | 185-187 | DL-2 | hashの3つの対象の並び | src/domain/instructions.rs の module doc と REPOSITORY_INSTRUCTIONS | 残さない（184の入口と188-189の約束で足りる） | 有: src/domain/instructions.rs:1,24 src/application/prompt/worker.rs:1909 |
| subagent review | 194-197 | DL-1 DL-5 | providerごとのsubagent reviewの文面と理由 | src/application/prompt/worker.rs の review_line の doc comment | 残さない（入口は193、198-200の約束と落とし穴は残す） | 有: src/application/prompt/worker.rs:813 |
| 復旧jobのprompt | 205-212 | DL-1 DL-5 | 復旧のpromptの節の中身と各定数の説明 | src/application/prompt/run_review.rs の recovery_prompt と各定数／satisfied_review.rs の claim_review_prompt | 入口はrecovery_prompt。verdictの規則は末尾で切られない | 有: src/application/prompt/run_review.rs:610,619,950,965 satisfied_review.rs:50 |
| repositoryの規則を読む順 | 219-226 | DL-1 | roleごとの規則の探し方と置く位置 | src/application/prompt/planners.rs の repository_rules／run_review.rs の review_prompt | 残さない（217の約束と入口のrepository_rulesで足りる） | 一部: src/application/prompt/planners.rs:29,39 run_review.rs:96 にdocなし |
| 次のturnの文の上限 | 279-285,287-289 | DL-5 | 依頼の切り方・退避のfile名・記録の手順 | src/application/prompt/requests.rs と src/application/supervise/execution/resume.rs／worker.rs の restored_request | 入口は表の行のmoduleと定数（277-278で足りる） | 一部: src/application/supervise/execution/resume.rs:1208 worker.rs:851,1223 |
| goal review・runのreview・復旧job・runtimeのplannerの上限 | 295-298,305 | DL-5 | 選ぶ順と切り方・必須の節の扱い | src/application/prompt_fit.rs の module doc と Fit | 入口はprompt_fit（306-307の不変条件は残す） | 有: src/application/prompt_fit.rs:1,234,299 |
| goal review・runのreview・復旧job・runtimeのplannerの上限 | 300-304,308-311 | DL-1 DL-5 | 引き継ぎの節・固定バイナリの節・記録の細部 | src/application/prompt/planners.rs の PLANNER_HANDOVER_BYTES／run_review.rs の binary_sections | 前のplannerはresumeせず記録から材料を集める（ADR-t1704-1） | 有: src/application/prompt/planners.rs:305 run_review.rs:824 |

迷った行（違反の範囲と重なることがある）:

| 行 | 当てた規則 | 迷った点 | 寄せた側 |
| --- | --- | --- | --- |
| 105-106 | AGENT.md「その部品の外の者がしてはいけないこと」 | 文面の要約だがtaskのpathsを変えられないplannerとの約束の説明にも読める | map |
| 130 | AGENT.md「読み手が踏む落とし穴」 | tests/e2e.rsのstubの読み方はtestの定義で読めるが、節を足す人が踏む落とし穴でもある | map |
| 164-171 | AGENT.md「入口の地図の表の1行」 | 表でなく箇条の入口の一覧で、1行1名なのでDL-2の並びとも入口とも読める | map |
| 233-245 | AGENT.md「複数のmoduleや仕組みにまたがる約束」 | ADRの決定の要約で、全jobに及ぶ約束だがADRの写しでもある | map |

### `queue-service.md`

service自身の手順と一覧の写しが多い。module docが既に持ち、約束と入口だけ残せば収まる
丸ごと移せる節: telemetry_report、ユースケースの表、起動と停止の大半。
分割: 不要。移せば約15 KiB。
同じ事実を持つ別の文書: docs/design/authorization.md, docs/design/queue-service-host-cli.md, docs/design/security.md, docs/design/measurement.md。

| 節 | 行 | 規則 | 細部 | 移し先 | 地図に残す 1 行 | 移し先に同じ事実 |
| --- | --- | --- | --- | --- | --- | --- |
| 置き場所と形 | 48-55 | DL-2 DL-5 | service dirのfileごとの中身の表 | src/infrastructure/queue_service.rs の module doc | tokenの値は呼び出し元のenvとargvに置かない（ADR-t1233-4決定4） | 有: src/infrastructure/queue_service.rs:8-21,134,5222 |
| 置き場所と形 | 57-58,60,64,66 | DL-2 DL-5 | 要求の形・上限・not_leader・startupの記録 | src/domain/queue_service.rs の MAX_* ／infrastructure の ServiceWriteLine | 1接続1要求（入口はMAX_ANSWER_BYTES）。書き込みの列の順は残す | 有: src/domain/queue_service.rs:207,213,224 src/infrastructure/queue_service.rs:3536 |
| principalとtoken | 81-82,84,86-94 | DL-1 | actorごとのtokenの依頼の時点と経路 | src/application/queue_service.rs の ServiceAccess／src/domain/queue_service.rs の reissue | tokenの発行はserviceだけでAI actorの口は無い（83、入口はServiceAccess） | 一部: src/application/queue_service.rs:2518 src/domain/queue_service.rs:1949 |
| principalとtoken | 95-101 | DL-1 | tokenごとの失効の時点と担当の列挙 | src/application/queue_service.rs の ServiceAccess::revoke／supervisor_service.rs の revoke_principal | runの終わったtokenはserviceが断る（入口はrun_holds_token） | 一部: src/application/queue_service.rs:742 src/application/supervisor_service.rs:1838 |
| principalとtoken | 102-105 | DL-1 DL-5 | executor telemetryのtokenの対象と発行と失効 | src/application/telemetry_tokens.rs の module doc／session_step.rs の StepTarget | 制御側だけが発行し、agentのenvとargvには渡さない（入口はtelemetry_tokens） | 有: src/application/telemetry_tokens.rs:1 src/domain/session_step.rs:129 |
| ユースケース | 114,116-118 | DL-1 DL-5 | handleの検査の順と拒否の記録・応答の形 | src/application/queue_service.rs の QueueService::handle・respond／ServiceDenial | 拒否はCLIと同じに記録する（入口はQueueService::handle） | 一部: src/application/queue_service.rs:474,857 src/domain/queue_service.rs:1095 |
| ユースケース | 123-143 | DL-2 | use_case・コマンド・capabilityの一覧の表 | src/domain/queue_service.rs の UseCase の各 variant の doc comment／docs/design/authorization.md | 一覧とコマンドの写しはUseCase::of_command、認可はAuthorization | 一部: src/domain/queue_service.rs:725 authorization.md:311-323 |
| telemetry_report | 154-170 | DL-1 DL-2 DL-5 | 版・上限・認可・書き込み・除外の細部 | src/domain/telemetry.rs の module doc と定数／src/application/queue_service.rs の認可の箇所 | tokenの対象の登録済みsessionだけ受ける（入口はdomain::telemetry） | 有: src/domain/telemetry.rs:1,28,45 src/application/queue_service.rs:1888,5696 |
| 読み取りのユースケース | 176-179,182 | DL-5 | 読み取りの答え方・paramsの読み方・今の時刻 | src/application/queue_reads/mod.rs の module doc と QueueRead | CLIとserviceは同じanswerで答える（入口はqueue_reads） | 有: src/application/queue_reads/mod.rs:1-8,49 |
| 読み取りのroleとworkerが打つコマンド | 208-211 | DL-1 DL-2 | ユースケースの無いコマンドの列挙 | src/domain/queue_service.rs の UseCase::of_command の doc comment | 残さない（入口はof_command） | 有: src/domain/queue_service.rs:725 src/main.rs:4614 |
| クライアントモード | 218-219,223-228,230,232-234,236-240 | DL-1 DL-5 | 宛て・写し方・断りの形・locate・wrapperの場面ごと | src/infrastructure/queue_service.rs の ClientRoute・ClientFailure・reach_of／src/main.rs の client_request | どの断りでもDBを開くことに戻らない（220・229・235は残す） | 一部: src/infrastructure/queue_service.rs:1981,2014,2418 main.rs:4002 |
| 起動と停止 | 251-254,256-261,264-267,269 | DL-1 | up・supervisor・down・installごとの起動の手順 | src/application/supervise/host_ops/queue_service.rs の module doc／lifecycle.rs の down／ensure_replaced | 居ない間は新しいclaimを控える（262-263）。入口はSystemQueueService | 有: host_ops/queue_service.rs:1 application/lifecycle.rs:2826,queue_service.rs:2647 |
| 権限 | 296-297 | DL-2 | service系コマンドのcapabilityとdenyの写し | docs/design/authorization.md の Capability／src/application/execution.rs の DAGQ_COMMANDS | 残さない（Authorizationが持つ） | 一部: src/application/execution.rs:308 authorization.md:86 |

迷った行（違反の範囲と重なることがある）:

| 行 | 当てた規則 | 迷った点 | 寄せた側 |
| --- | --- | --- | --- |
| 73-74 | AGENT.md「コードが書かない保証」 | 版の判定はanswer_ofのdocにあるが、古いbinaryとの間の互換の約束として読者が頼る | map |
| 194-204 | AGENT.md「入口の地図の表の1行」 | 呼び出し元ごとの入口の表で地図だが、promptとskillの列挙の写しにもなりうる | map |
| 244-247 | AGENT.md「複数のmoduleや仕組みにまたがる約束」 | providerのsandboxとserviceの到達の約束だが、関数のdocと重なる | map |
| 276-282 | AGENT.md「部品の外の者が頼る結果」 | 人がする次の手は約束だが、記録の流れはhost_opsのmodule docの写しでもある | map |

### `persistence.md`

storeごとの細部の箇条が多く大半はdocと重複。入口と不変条件を残しstoreのdocへ移す
丸ごと移せる節: asks・draft planners・planning requests・findings・session steps・関連・CIの見張り。
分割: 不要（箇条の移動で約11KB減り約20KB、足りなければRuntime ownershipを分ける）。
同じ事実を持つ別の文書: docs/design/follow-up-membership.md, docs/design/supervisor-lifecycle/ask.md, docs/design/supervisor-lifecycle/draft-planners.md。

| 節 | 行 | 規則 | 細部 | 移し先 | 地図に残す 1 行 | 移し先に同じ事実 |
| --- | --- | --- | --- | --- | --- | --- |
| 不変条件 | 84-85 | DL-1 DL-5 | 再計画の操作の開く・引き継ぐ・閉じる手順 | src/domain/task_replan.rs の carry のdoc comment | 残さない（83行目の不変条件が残る） | 有: domain/task_replan.rs:806 のdocが引き継ぎと閉じない条件を書く |
| 表 | 94-95 | DL-5 | replan_plans・replan_replacementsの列と使い方 | migrations/0087_replan_plans.sql と 0079_task_replanning.sql のコメント | 残さない（93行目が表の説明の置き場を指す） | 有: migrations/0087_replan_plans.sql:4、0079_task_replanning.sql:22 |
| 集約の読み書き | 103,108-112 | DL-5 | next_id・eventの生成・refusals.log・循環・noteの表 | sqlite.rs の next_id・waits_for、run/recorded.rs、runtime_store/mod.rs の REFUSALS_LOG | domainの拒否は巻き戻るのでrefusals.logに残す（REFUSALS_LOG） | 一部: sqlite.rs:3071,:3036、run/recorded.rs:1、mod.rs:300。noteの行はdoc無 |
| asks | 119-129 | DL-1 DL-5 | askの行・eventの規則、回答の印、subjectの理由 | src/infrastructure/asks.rs のmodule doc と domain の Ask のdoc comment | askの正本はasks.rsとdomainのAsk。未回答のaskはcloseできない | 一部: asks.rs:1134,:437、domain/mod.rs:1434,:1603。runtime_deliversの文は無 |
| draft planners | 136-145 | DL-1 DL-5 | draft_originsの書き手、対象の選び、結末、回答の行き先 | src/infrastructure/draft_planners.rs のmodule doc と planner_drafts・route_of | 残さない（133行目の案内と134行目の入口が残る） | 一部: draft_planners.rs:1,:1544、ports/planning.rs:356。結末の文は未統合 |
| planning requests | 152-156 | DL-1 DL-5 | 依頼のevent・再検査・却下の権限・回答の行き先 | src/infrastructure/plan_requests.rs のmodule doc と route_of・draft_route_of | 残さない（149行目の案内と150行目の入口が残る） | 一部: plan_requests.rs:1,:807,:825、ports/planning.rs:228。却下の権限は無 |
| findings | 163-166 | DL-5 | 統合と再オープン、eventの付け先、一覧の絞り方 | src/domain/finding.rs の merge、src/infrastructure/findings.rs のmodule doc | 残さない（入口の行が残る） | 一部: domain/finding.rs:262、findings.rs:1。SQLで絞らない理由はdoc無 |
| Runtime ownership | 174-188 | DL-5 DL-1 | leaseと登録の行の書き手・消し方の条件 | src/infrastructure/runtime_store/coordination.rs のmodule doc、renew_lease、adopt_run、lifecycle の prune_supervisor | leaseを奪うのはadoptだけ、登録行の削除は決まった経路だけ（coordination） | 一部: coordination.rs:1,mod.rs:347,recovery.rs:98。moddocが条件は本書と指す:要修正 |
| Runtime ownership | 192-193 | DL-5 | workspace_closed_atを列に持つ理由と失敗時 | src/domain/run/ の TaskRun::workspace_closed_at のdoc comment | 残さない（列の意味はdocで分かる） | 無: 検索語 workspace_closed_at をsrc/domain/run/ で検索。///の説明なし |
| session steps | 209-219 | DL-1 DL-2 DL-5 | UNIQUE・抜けの数え方・保持の手順・90日・sample・owner | src/domain/session_step.rs のmodule doc、src/infrastructure/runtime_store/session_steps.rs | 保持で消したseqは抜けと数えない（session_step） | 一部: domain/session_step.rs:8,:26、session_steps.rs:127,:140。削除の手順は分散 |
| follow-upの所属の判断 | 228-229 | DL-5 | 登録時の読み直しと判断の行の書き方 | docs/design/follow-up-membership.md の「transactionと所属」 | 残さない（227行目の同じトランザクションの約束と入口が残る） | 一部: docs/design/follow-up-membership.md:47 にBEGIN IMMEDIATEの判断の説明 |
| Transactions and constraints | 240-243 | DL-5 | WriteTxの入口・lock長期保持の記録・insert_id | src/infrastructure/sqlite.rs の WriteTx・insert_id のdoc comment | queue DBのtransactionはWriteTxの入口で始める（検査はwrite_tx.rs） | 有: sqlite.rs:277,:506,:527 |
| Transactions and constraints | 246-247 | DL-1 | taskを終えるときのholdの閉じ方 | src/infrastructure/task_holds.rs の end_with_task のdoc comment | 残さない（245行目の順序の保証が残る） | 有: infrastructure/task_holds.rs:557 のdocが同じ手順を書く |
| Queue location | 283-284 | DL-5 | --dbのrun dirとLaunchAgentのlabelの決め方 | src/infrastructure/location.rs の QueueLocation::label のdoc comment | 残さない（269行目の入口が残る） | 有: location.rs:6,:44,:144 |
| Database setup and migrations | 295-299,304-305,310-312 | DL-1 DL-5 | initの範囲・版の決め方・互換の宣言・read-only・非互換の手順 | src/infrastructure/schema.rs のmodule doc、sqlite.rs の migrate 系のdoc comment | openだけではmigrateしない（300-303を残す） | 一部: schema.rs:1、sqlite.rs:996,:1022,:1305。application_idはdoc無 |
| 全文検索（search） | 323-325,328 | DL-5 | FTS5の表の理由・trigram・landed_commitsの埋め方 | src/domain/search.rs のmodule doc、migrations/0026_search.sql のコメント | triggerを持つ表を作り直すmigrationはtriggerも作り直す（326-327を残す） | 有: domain/search.rs:1、infrastructure/search.rs:36、0026_search.sql:50 |
| 関連（related） | 335-336 | DL-5 DL-2 | 点数の式と重みの変え方 | src/domain/related.rs のmodule doc | 残さない（333行目の入口が残る） | 有: domain/related.rs:1 に手がかり・rarity・testとの合わせを書く |
| CIの見張り | 343-345 | DL-5 | 落ちているtestの導出・1件の記録・covered_by_task | src/infrastructure/ci_watch_store.rs のmodule doc、domain::ci_watch | 残さない（341行目の入口が残る） | 一部: domain/ci_watch.rs:4、ci_watch_store.rs:1,:40。外部キーの理由は無 |

迷った行（違反の範囲と重なることがある）:

| 行 | 当てた規則 | 迷った点 | 寄せた側 |
| --- | --- | --- | --- |
| 102,104-107 | AGENT.md「複数のmoduleにまたがる約束」と「1つのmoduleの定義で分かる」 | 全storeの共通の書き方で書き手の約束だが、sqlite.rsのmodule docにほぼ同じ文があり部品の説明とも読める | map |
| 271-280 | documents.md「入口」とAGENT.md DL-2 | queueのdirectoryの木は複数の部品にまたがる入口だが、location.rsのmodule docが一部を持ち並びとも読める | map |
| 313-314 | AGENT.md「読み手が踏む落とし穴なら地図」 | migration書き手の落とし穴だがmigrations.mdに無く、runnerの実装の説明も混じる。移し先ならmigrations.md | map |

### `supervisor-lifecycle/triage.md`

番号付き1〜9の21KBが手順の書き写しでdocに大半ある。起動順の概略と保証だけ残す
丸ごと移せる節: Triage本体の1〜9の各段（約半分はdoc済み）、Codexの起動・読み取り・失敗。
分割: 不要（1〜9を移せば約12KBで収まる）。
同じ事実を持つ別の文書: docs/design/supervisor-lifecycle/background-recovery-job.md, docs/design/supervisor-lifecycle/prompt.md。

| 節 | 行 | 規則 | 細部 | 移し先 | 地図に残す 1 行 | 移し先に同じ事実 |
| --- | --- | --- | --- | --- | --- | --- |
| 1. 自動recover | 20-23 | DL-1 | 孤児runをinterruptedにする対象とleaseの場合分け | src/application/supervise/execution/triage.rs の Supervisor::recover_dead_runs のdoc comment | 死んだrunはinterruptedにしreadyへ直さない（recover_dead_runs） | 有: execution/triage.rs:32 のdocが対象・stale lease・interrupted化を書く |
| 2. alert | 24-28 | DL-1 DL-5 | 候補の条件・alertの決め方・行き先が待つ間の候補 | src/application/supervise/execution/triage.rs の triage_candidates と domain::triage_state のdoc comment | 候補はtriage_candidatesとtriage_stateが決める | 有: execution/triage.rs:415、domain/mod.rs:1037、domain/recovery.rs:82 |
| 3. 開始 | 32-40 | DL-1 DL-2 | begin_triageの手順・jobのfile名と環境・上限時の分岐 | src/infrastructure/runtime_store/recovery.rs の begin_triage、triage.rs の triage_run、domain::recovery::round_start | 上限後のjobは人がverifyを直した回答だけ（ADR-t883-1）。他はescalate | 一部: runtime_store/recovery.rs:1039、domain/recovery.rs:82。起動環境の文はdocに無い |
| 4. prompt | 42-49 | DL-1 | promptの節の順と各規則定数の中身 | src/application/prompt/run_review.rs の recovery_prompt と RECOVERY_*_RULE のdoc comment | promptの中身はrecovery_promptのdocが持つ（41行目を残す） | 有: prompt/run_review.rs:965,:813,:950、domain/recovery.rs:97 |
| 5. verdictの適用（retry・retry_inherit） | 50-62 | DL-1 DL-5 | 適用条件とretry/retry_inheritの前提・HEADの読み方 | src/application/supervise/execution/triage.rs の act_on_recovery のdoc comment、src/application/run_git.rs の host_head | commitのあるrunのやり直しは成果の破棄なので適用しない（act_on_recovery） | 一部: run_git.rs:101 にHEADの読み方、triage.rs:843,:881 に前提。container分は無 |
| 5. verdictの適用（resume） | 63-68 | DL-1 | resumeの前提・再検査の理由・last_errorへの書き込み | src/domain/telemetry.rs の run_revivable、src/infrastructure/runtime_store/recovery.rs の finish_triage | 適用時に再検査し、後のrunがあれば戻さない（run_revivable） | 一部: telemetry.rs:319、recovery.rs:1124。last_errorはコードのみ（triage.rs:944） |
| 5. verdictの適用（hand_off） | 69-75 | DL-1 | hand_offの前提の並びと適用後の状態の進み方 | src/domain/recovery.rs の hand_off_refusal、src/domain/satisfied.rs の handed_off、run_git::hand_off_reads | hand_offは確かめの経路へ渡すだけで閉じる判断をしない（ADR-t1818-2） | 一部: domain/recovery.rs:508、satisfied.rs:165、run_git.rs:198。再判定: 挙げた所が持つのは主に69-72。73-75は無い |
| 5. verdictの適用（wait・他の操作・記録） | 76-80 | DL-1 | wait等の扱いとfinish_triageが書くeventの並び | src/infrastructure/runtime_store/recovery.rs の finish_triage、domain::recovery::ENDED_ACTIONS | 残さない（適用の記録はfinish_triageのdocで分かる） | 有: runtime_store/recovery.rs:1124、domain/recovery.rs:97 |
| 6. escalate | 81-92 | DL-1 | askのoptionsの組み立て・寄せ外し・questionの文面 | src/application/supervise/execution/triage.rs の escalate_ended、domain::recovery::sort_job_options のdoc comment | 前提が崩れた判定は人のdecide askにしrunは動かさない（ADR-t1818-2） | 一部: triage.rs:1064、domain/recovery.rs:747。ask取り下げはコメントのみ（triage.rs:1137） |
| 7. sessionの停止 | 93-99 | DL-1 | triage_finished後の停止・記録・askを閉じる順 | src/application/supervise/execution/triage.rs の end_round のdoc comment | triage_finished後の失敗はlogだけでtriage_failedにしない（end_round） | 一部: triage.rs:1156 のdocは閉じる・解放・logのみ。stuck_exitとapprove_landingは無 |
| 8. jobの失敗 | 100-102 | DL-1 | 失敗の種類ごとのevent記録とrunの扱い | src/application/supervise/execution/triage.rs の fail_recovery のdoc comment | triage_failedはattentionで人かinboxが動かす（plannerはready不可） | 有: execution/triage.rs:1188 のdocがevent・lease・provider_unusableを書く |
| 9. askの回答 | 103-110 | DL-1 | 回答ごとの適用と再検査、jobのoptionの扱い | src/application/supervise/execution/triage.rs の apply_triage_answers、runtime_store/recovery.rs の decide_triage | 残さない（適用はapply_triage_answersのdocで分かる） | 有: execution/triage.rs:342、runtime_store/recovery.rs:1287、domain/recovery.rs:677 |
| 再計画の診断 | 120-122 | DL-1 | 診断askの条件・options・答えの適用の確かめ直し | docs/plans/task-replanning.md、src/application/supervise/execution/replan_diagnosis.rs | 再計画の診断は読み取りだけのjobでrun slotの外。詳細はtask-replanning | 一部: execution/replan_diagnosis.rs:1 のmodule docがtask-replanning.mdを指す |
| 人の手での引き継ぎ | 142,148-151 | DL-1 | 確定の前提の並びと確定が書くask・event | src/infrastructure/runtime_store/recovery.rs の確定の関数、domain::inherit_by_hand::check | 残さない（競合と順序の保証の箇条は別に地図へ残す） | 有: runtime_store/recovery.rs:1567,:1572、application/inherit.rs:218 |
| Codexで動かす（行き先） | 160-166 | DL-1 | providerの有無・hold・fallback設定ごとの行き先 | src/application/supervise/execution/recovery.rs の Supervisor::recovery_route のdoc comment | 行き先はrecovery_route。Claudeのholdはcodexのjobを止めない | 有: execution/recovery.rs:411 のdocがADR番号付きで分岐を書く |
| Codexで動かす（起動・読み取り・失敗） | 167-168,171-177 | DL-1 DL-5 | codex execの作法、threadとmodelの読み先、失敗の分岐 | execution/recovery.rs の start_job、AgentProvider::job_reply・job_session、triage.rs の fail_recovery | Codexのjobの失敗はClaudeに回さない。jobは書けない（169-170を残す） | 有: recovery.rs:1356、ports/execution.rs:436,:446、triage.rs:1188 |

迷った行（違反の範囲と重なることがある）:

| 行 | 当てた規則 | 迷った点 | 寄せた側 |
| --- | --- | --- | --- |
| 111-114 | AGENT.md「読み手が踏む落とし穴なら地図」と「1つの関数のdocで分かるなら詳細」 | 操作名だけの回答を断るのは人が踏む落とし穴だが、asks.rsのrefuse_bare_actionのdocと同じ事実で両方に当たる | map |
| 29 | AGENT.md「コードが書かない保証」と「1つの部品が何を読むかだけの行はDL-5」 | 同じrunを2 slotで進めない保証だがslotとleaseの扱いの手順も含み、triage_candidatesの部品の話とも読める | map |
| 182-184 | AGENT.md「部品の外の者が頼る結果」と「場面ごとの列挙はDL-1」 | holdと復旧の組の約束だが場面ごとの扱いの列挙でもある。task-hold.mdへの案内が既にあり移せば消える | detail |

### `supervisor-lifecycle/needs-session.md`

resumeの手順と試行の数え方の書き写しが本文の大半で、doc commentがすでに持つものが多い。約束とADRだけ残す
丸ごと移せる節: 試行の数え方の4段落、手順0〜6、taskのholdの184-197。
分割: 不要。doc commentへの移動で20 KiBを大きく下回る見込み。
同じ事実を持つ別の文書: docs/design/supervisor-lifecycle/worker-model.md, docs/plans/task-replanning.md, docs/design/supervisor-lifecycle/integrate.md。

| 節 | 行 | 規則 | 細部 | 移し先 | 地図に残す 1 行 | 移し先に同じ事実 |
| --- | --- | --- | --- | --- | --- | --- |
| needs_session | 25-28 | DL-1 DL-5 | 着地のparkの記録とworktreeの置き場、hostの失敗 | src/domain/run.rs の defer_integration の doc comment／integrate.md | runはTaskを占有し続け`ready`/`cancel`できない（29行）を残す | 一部: src/domain/run.rs:709 遷移だけ。worktreeの置き場は書かない |
| needs_session | 31-34 | DL-4 DL-5 | session_gone_parkedを書かなくなった経緯と読み方 | src/domain/reason.rs の SessionGone、src/application/prompt/requests.rs の ResumeKind::SessionGone | 残さない（古い記録の読み方はコードのdocが持つ） | 有: src/domain/reason.rs:83、src/application/prompt/requests.rs:71 |
| needs_session | 36-42 | DL-1 | 場面ごとのparkのevent・依頼文・試行の数え方 | src/application/prompt/requests.rs の ResumeKind の各 variant | parkする場面は[integrate]のe2eとlanding recheckが持つ。依頼文は`ResumeKind` | 一部: requests.rs:71 E2eとRecheckは有。閉じていないaskの扱いのdocは無 |
| needs_session | 44-46 | DL-1 | resumeのmodelを段上げする条件と記録 | docs/design/supervisor-lifecycle/worker-model.md の 段上げ | resumeのmodelはWorker model | 有: docs/design/supervisor-lifecycle/worker-model.md:61 |
| 試行の数え方 | 52-77 | DL-1 DL-2 DL-5 | 衝突だけ・kill・hold・providerの試行の数え方と上限 | src/domain/resume.rs の module doc、ResumeCount、resume_start_plan | 数え方はrunのeventだけから`domain::resume`が決める。ADR-0047とその改定へのリンク | 有: src/domain/resume.rs:34、:241、:388、:656、ResumeCount の各欄 |
| needs_session | 83-88 | DL-1 | resumeの候補から飛ばす条件と残ったwrapperの止め方 | src/application/supervise/execution/resume.rs の resume_candidates の doc comment | 同じworktreeと同じrunで2つ目のsessionやslotを始めない約束を残す | 一部: src/application/supervise/execution/resume.rs:37 飛ばす条件の一部だけ |
| needs_session | 92-105 | DL-1 DL-5 | 解消済みの確認の条件と承認の有無ごとのその後 | src/application/supervise/execution/resume.rs の resolved_head、skip_resume、skips_resume | 前の試行で解消済みならsessionを開かず試行も数えない。入口は`resolved_head` | 有: src/application/supervise/execution/resume.rs:287、:337、:1075 |
| needs_session | 106-110 | DL-1 | begin_resumeの確認と記録の順、依頼文の選び方 | src/infrastructure/runtime_store/recovery.rs の begin_resume の doc comment | 状態は足さず`needs_session`のまま、進行は`resume_started`/`resume_finished`で読む | 有: src/infrastructure/runtime_store/recovery.rs:785 |
| needs_session | 111-124 | DL-1 | wrapperの起動・依頼・監視の手順と時間の測り方 | src/application/supervise/execution/resume.rs の resume_run、ResumeWatch の doc comment | resumeの起動と監視の入口は`resume_run`と`ResumeWatch` | 一部: execution/resume.rs:130 は1文、:1279 ResumeWatch は監視の並びだけ |
| needs_session | 126-131 | DL-1 | 解消・失敗・未解消ごとのその後の工程 | src/application/supervise/execution/resume.rs の finish_resumed_session の doc comment | 入口は`finish_resumed_session` | 有: src/application/supervise/execution/resume.rs:868 |
| needs_session | 132-144 | DL-1 | 引き継ぐretryと復旧jobに渡す条件と記録 | src/domain/resume.rs の inherits_on_exhaustion、execution/resume.rs の retry_inheriting と exhaust_resumes | 使い切りは衝突だけなら1回引き継ぐretry、他は復旧jobへ（ADR-0047決定24・39） | 有: src/domain/resume.rs:754、execution/resume.rs:386、:460、recovery.rs:1198 |
| needs_session | 145-150 | DL-1 | resumeのerrorの記録と残したwrapperの扱い | src/application/supervise/execution/resume.rs の resume_run の doc comment | 残さない（記録の手順はコードのdocへ） | 無: rg "outcome.*error" は src でtestだけ（history.rs:1698）。docに記述なし |
| 再計画で保留したrun | 163-170 | DL-1 DL-5 | 保留後の照合・復元・失敗の場面ごとの手順 | src/application/supervise/execution/replan_park.rs の module doc／docs/plans/task-replanning.md | 保留中は自動resumeの対象にせず、手順は`replan_park`とtask-replanningへ | 有: execution/replan_park.rs:1 と :92 ParkFor、docs/plans/task-replanning.md:64 |
| supervisorの入れ替わり | 177-180 | DL-1 | 引き継がないrunの条件と依頼の2回目 | src/application/supervise/execution/resume.rs の adopt_resume の doc comment | 依頼は死ぬ位置により2回届きうる落とし穴（180行）だけ残す | 一部: src/application/supervise/execution/resume.rs:808 再構築だけ、引き継がない条件は無 |
| taskのhold | 184-197 | DL-1 DL-5 | holdの保留と解除の分岐、引き継ぐheadと拒む条件 | src/domain/task_hold.rs の module doc、ends_run、release_decision、ProgressVersion | holdはresumeと使い切りの後始末を止める。解除の分岐は`domain::task_hold` | 有: src/domain/task_hold.rs:247、:267、:702、src/domain/resume.rs:794 |

迷った行（違反の範囲と重なることがある）:

| 行 | 当てた規則 | 迷った点 | 寄せた側 |
| --- | --- | --- | --- |
| 48-50 | DL-1と地図の約束 | 承認で着地できるrunの範囲は外の者が頼る約束だが、event記録とpushの手順が同じ段落に混ざる | map |
| 153-154 | 入口の地図の表と関数名の並び | statusの`next`は人とinboxが頼る結果だが、条件の列挙はhealth::attentionのコードで読める | map |
| 156-159 | DL-1とsessionの外の約束 | sessionが書くreceiptの結果がintegrateでどうなるかは外の者の約束で、手順の書き写しとも読める | map |
| 198-199 | してはいけないこと | 再計画との重なりの約束とhold_parkのlease引き継ぎの手順が続き、後者の層が決めきれない | detail |

### `supervisor-lifecycle/roles.md`

前半の役割の定義は地図で、Actors以下が手順の書き写し。表と分岐を各jobの文書とdocへ移す
丸ごと移せる節: headlessのjobのverdictの表と記録、eventのactorの場面、CLIでの解釈の分岐。
分割: 不要。移せば約12KBで収まる。
同じ事実を持つ別の文書: docs/design/supervisor-lifecycle/review.md、plan-review.md、triage.md、goal-review.md、docs/design/security.md。

| 節 | 行 | 規則 | 細部 | 移し先 | 地図に残す 1 行 | 移し先に同じ事実 |
| --- | --- | --- | --- | --- | --- | --- |
| 環境変数 | 60-74 | DL-1 DL-2 | actorごとの起動元と環境変数の表 | src/application/actor_executor.rs の actor_env のdoc comment | AI actorは全てroleとactor idを環境に持ち、環境はactor_envの1か所で作る | 一部: src/application/actor_executor.rs:425 module docは「one place」のみ |
| 環境変数 | 76-78,80-82 | DL-5 | DAGQ_QUEUE等を持つroleと環境の組む順 | src/domain/queue_service.rs の client_role と actor_env のdoc | DAGQ_ACTOR_IDなどは[run.env]で上書きできない（79行） | 一部: src/domain/queue_service.rs:2308,2321 |
| actorの起動（ActorExecutor） | 88,90-93 | DL-5 | specの整合の検査の項目とHostActorExecutorの部品 | src/application/actor_executor.rs の ActorExecutionSpec::check | AI actorはspawnだけを通り、hostのspecは記録と検査だけで隔離でない | 有: src/application/actor_executor.rs:336 |
| actorの起動（ActorExecutor） | 95-99 | DL-1 | roleごとのClaudeのsettingsを誰がどこで書くか | src/infrastructure/adapters.rs の agent_settings | Claudeのsettingsはroleからadapterの1か所で決まる（入口agent_settings） | 一部: src/infrastructure/adapters.rs:4555、src/application/execution.rs:402 |
| 実行のbackendとenforcement | 108-110,113-117 | DL-5 | backendと強制の型の意味と起動を拒む条件 | src/application/execution.rs のmodule docと各型 | hostに黙って戻さない（fail closed）。隔離はsandboxだけ | 有: src/application/execution.rs:1,84,129,148 |
| 実行のbackendとenforcement | 118-123 | DL-5 | status・doctorのactorsの行の作り方と理由 | src/application/execution.rs の ExecutionConfig::profiles | host実行がsandboxでないことをstatus・doctorが実行時にも示す | 一部: src/application/execution.rs:170,282 行を分けない理由は無 |
| CLIでの解釈 | 127-144 | DL-1 | execute()のコマンドごとの分岐とqueueの扱い | src/main.rs の execute のdoc comment（無いので足す） | 宛ての無いworker・job・observerはqueueを開かずに断る（入口はexecute） | 一部: src/main.rs:4719に///は無いが、executeの本体に経路ごとの//がある。check_accessのdoc（:3062付近）に一部 |
| CLIでの解釈 | 149-151 | DL-5 | from_envの読み方（空のrole・読めないID） | src/domain/actor.rs の ActorContext::from_env | 残さない。読めない環境はfail closedで145行が言う | 有: src/domain/actor.rs:300 |
| eventのactor | 164-171 | DL-5 | EventActorsの仕組みとプロセスのactorの決まり方 | src/infrastructure/event_actor.rs のmodule docと src/main.rs の event_actor | eventは全てactorを持ち、制御側のeventはsessionのactorにならない | 有: src/infrastructure/event_actor.rs:1、src/main.rs:3187 |
| eventのactor | 172-180 | DL-1 | 場面ごとのrequested_byの有無 | Supervisor::for_job・for_escalation のdoc comment | verdictの適用はactorがsupervisorでrequested_byがjob。jobの失敗は持たない | 一部: execution.rs:2079 recovery.rs:313 診断の場面は無 |
| headlessのjobのverdictと遷移 | 189-202 | DL-1 | jobとverdictごとにsupervisorがすること | docs/design/supervisor-lifecycle/ の review.md・triage.md・plan-review.md・goal-review.md | 残さない。186-187行の「verdictはデータで遷移はRust」と各jobの文書へのリンク | 一部: satisfied_judgment.rs:151 recovery.rs:470、plan-review.md:40 |
| headlessのjobのverdictと遷移 | 204-208 | DL-5 | verdictの読みの入口とpredictionsの型の理由 | src/domain/plan_review.rs の PlanReviewVerdict と各verdictの型 | 未知の欄を含む出力は読めない出力として扱う | 一部: src/domain/plan_review.rs:168、src/domain/prediction.rs:52 |
| headlessのjobのverdictと遷移 | 210-215 | DL-1 | 壊れた出力のjobごとのやり直しとask | 各jobの文書（review.md・triage.md・plan-review.md・goal-review.md） | 残さない。209行のfail closedの約束だけ残す | 有: docs/design/supervisor-lifecycle/goal-review.md:92 |
| headlessのjobのverdictと遷移 | 216-223 | DL-1 | approve_landingのaskのrequested_byと引き継ぎの復元 | src/application/supervise/execution.rs の AfterExit のdoc comment | jobの失敗と適用後の自身の判断のaskはrequested_byを持たない | 無: src/application/supervise/execution.rs:1885 docは1行 |

迷った行（違反の範囲と重なることがある）:

| 行 | 当てた規則 | 迷った点 | 寄せた側 |
| --- | --- | --- | --- |
| 22,67,82,100 | DL-4 経緯（以前は） | 前のbinaryのin-cmuxのsupervisorは今も受ける振る舞いだが、module docは退役と書き経緯にも読める | map |

### `supervisor-lifecycle/up-down.md`

`up`の分岐と`dagq inbox`の手順の書き写しが多く、doc commentが多くを持つ。約束と落とし穴の行だけ残す
丸ごと移せる節: 入れ替えの引き継ぎとdrain、modeの記録、`dagq inbox`の手順。
分割: 不要。doc commentと既存文書への移動で20 KiBに収まる見込み。
同じ事実を持つ別の文書: docs/design/supervisor-lifecycle/handoff.md, docs/design/queue-service.md, docs/design/queue-broker.md, docs/design/provider-lifecycle.md。

| 節 | 行 | 規則 | 細部 | 移し先 | 地図に残す 1 行 | 移し先に同じ事実 |
| --- | --- | --- | --- | --- | --- | --- |
| `up` / `down` | 48-52 | DL-1 | [run.env]・main checkout・着地先・自動更新の確認の列挙 | src/application/lifecycle.rs の up の doc comment | 何も起動する前に止まる約束（40行）と各判断先へのリンクを残す | 一部: src/application/lifecycle.rs:396 順はあるが着地先と自動更新は無 |
| `up` / `down` | 53-57 | DL-1 DL-5 | stale登録の消す条件と消さない条件 | src/domain/supervisor_prune.rs の pid_taken_over の doc comment | `run_leases`は触らない（doctor / recoverの仕事）だけ残す | 有: src/domain/supervisor_prune.rs:26 |
| `up` / `down` | 58-59,61 | DL-1 | supervisorのreuseと入れ替えの条件の分岐 | src/application/lifecycle.rs の agent_lacks_process_type と up の doc comment | 60行の落とし穴（人が一度`up`を打つまで制限の下）だけ残す | 有: src/application/lifecycle.rs:1220、:408 up の doc |
| `up` / `down` | 66-70 | DL-1 | 登録済みin-cmuxのsupervisorの場面ごとの扱い | src/application/lifecycle.rs の module doc、domain::SupervisorMode | 廃止したmodeの扱いは`lifecycle`のmodule docとADR-t1433-4 | 有: src/application/lifecycle.rs:12、src/domain/mod.rs:143 |
| `up` / `down` | 72-77 | DL-1 DL-5 | modeを誰がいつ登録に書くかと信号の選び方 | src/infrastructure/runtime_store/coordination.rs の set_supervisor_mode の doc comment | 残さない（信号の使い分けは`down`のdocへ） | 一部: src/infrastructure/runtime_store/coordination.rs:127、src/domain/mod.rs:143。再判定: 72・74と75の一部。77はsrc/application/lifecycle.rs:767。76は未確認 |
| `up` / `down` | 81-83 | DL-1 | 引き継ぎかdrainかを決める条件 | src/application/lifecycle.rs の agent_takes_handoff の doc comment | 全部が受けられるなら引き継ぎ、他はdrain。入口は`agent_takes_handoff` | 有: src/application/lifecycle.rs:1319、:1220 も ProcessType の理由を持つ |
| `up` / `down` | 84-90 | DL-1 | migrateの場面ごとのserviceとupの止まり方 | src/application/queue_service.rs の ensure_replaced の doc comment／docs/design/queue-service.md | `up`のプロセスはmigrateしない。止まる条件は[Queue service]の「入れ替えの口」 | 一部: docs/design/queue-service.md:256、src/application/queue_service.rs:2647 |
| `up` / `down` | 91-107 | DL-1 | 引き継ぎとdrainの待ちと拒みの手順 | src/application/lifecycle.rs の hand_off、PartialHandoff、replace_supervisors、no_wait_refusal | 待ちは検証1回分に及ぶ（93行）。drainは上限なし。ADR-t3330-1へのリンクを残す | 有: src/application/lifecycle.rs:1482、:709、:788、:1253 |
| `up` / `down` | 115-117 | DL-5 | ProcessTypeの効果とplistの--cmuxとownerの渡し方 | src/application/lifecycle.rs の PROCESS_TYPE と LaunchAgentSpec::xml、src/main.rs の RetiredCmux | 残さない（114行の入口のままdoc commentへ） | 一部: src/main.rs:48、src/application/lifecycle.rs:3096 は理由を xml の doc に委ねる |
| `up` / `down` | 118-122 | DL-1 DL-5 | bootstrapの手順とmacOSの外の動き | src/infrastructure/launchd.rs の module doc と install | macOSの外は人かservice managerが`dagq supervise`を動かす（ADR-t1433-4） | 有: src/infrastructure/launchd.rs:8、:102、:225 |
| `up` / `down` | 131 | DL-2 | upの結果に添えるものの並び | src/application/lifecycle.rs の up と open_work の doc comment | 残さない（結果の欄はdoc commentの中身） | 一部: src/application/lifecycle.rs:2754 open_work のみ |
| `dagq inbox` | 137-150 | DL-1 | providerの選び方と使えないときの分岐 | src/application/lifecycle.rs の inbox_route、domain::actor_model::inbox_launch、codex_inbox::check_usable | Claudeへ替えず、askを開かず、人のfileを書かない約束を残す。入口は`inbox_route` | 有: lifecycle.rs:2510、src/domain/actor_model.rs:541、codex_inbox.rs:690 |
| `dagq inbox` | 152-161 | DL-1 | Claudeのinboxの拒み・preflight・記録とexecの順 | src/application/lifecycle.rs の inbox の doc comment | inboxの中では別のinboxを開かない。入口は`lifecycle::inbox` | 有: src/application/lifecycle.rs:2600 |
| `dagq inbox` | 162-169 | DL-1 DL-5 | settingsの中身とguardrailの判定の分岐 | src/infrastructure/adapters.rs の inbox_command、src/application/inbox_guardrail.rs の judge | guardrailの判定の入口は`inbox_guardrail::judge`。限界（170-172行）は残す | 有: src/infrastructure/adapters.rs:4356、src/application/inbox_guardrail.rs:41 |
| `up` / `down` | 176-186 | DL-1 | downの登録の分け方と信号の選び方 | src/application/lifecycle.rs の down と signalled_by_bootout の doc comment | bootoutが届けたpidへ2回目を送らない落とし穴と`LaunchAgent`を必ず外すことを残す | 有: src/application/lifecycle.rs:2826、:1076 |
| `up` / `down` | 192-194 | DL-1 | in-cmuxのworkspaceを閉じず人に任せる手順 | src/application/lifecycle.rs の down の doc comment | 残さない（ADR-t2159-1へのリンクは4の節が持つ） | 有: src/application/lifecycle.rs:2826 の doc の第2段落 |

迷った行（違反の範囲と重なることがある）:

| 行 | 当てた規則 | 迷った点 | 寄せた側 |
| --- | --- | --- | --- |
| 41-44 | 約束と手順の列挙 | 起動前に止まることは外への約束だが、確かめる項目の並びは`up`の手順でもある | map |
| 62-64 | 入口の地図の1行 | doc commentを指す入口の形で、別のqueueに触れない約束も持つが、場面ごとの動作の列挙にも読める | map |
| 188-190 | 複数の部品にまたがる順 | service・broker・supervisorの起動順の約束だがqueue-broker.mdの節と`down`のdocに同じ事実がある | map |

### `supervisor-lifecycle/validation.md`

検証の手順の列挙とこのrepositoryのITの選び方が本文の過半。項目の手順をdoc commentへ移し約束だけ残す
丸ごと移せる節: runtimeが流すe2eの箇条の大半、着地の検証の154-163。
分割: 不要。手順の移動で20 KiBに収まる見込み。
同じ事実を持つ別の文書: docs/design/container-runtime.md, docs/design/supervisor-lifecycle/headless-worker.md, docs/plans/landing-it-selection.md。

| 節 | 行 | 規則 | 細部 | 移し先 | 地図に残す 1 行 | 移し先に同じ事実 |
| --- | --- | --- | --- | --- | --- | --- |
| Validation | 30-37 | DL-1 | 受理の確認項目1〜5の順と各項目の条件 | src/domain/validation.rs の judge と Fact の doc comment | 順と理由のcodeは`validation::judge`。最初に外れた項目が`last_error`になる | 有: src/domain/validation.rs:334 judge が順を書く、:458 already_satisfiedの分岐 |
| Validation | 42-48 | DL-1 DL-5 | 宣言パスの差分の取り方と検査の順、parkの手順 | src/domain/validation.rs の Fact::Changes と judge の doc comment | 宣言外のパスはfailedでなくneeds_sessionに戻す（ADR-0029） | 一部: src/domain/validation.rs:181 Fact::Changes、src/domain/scope.rs:92 |
| Validation | 49-57 | DL-1 DL-5 | 要求evidenceの判定とparkから自動resumeまでの手順 | src/domain/receipt.rs の check_requiring、src/domain/run/recorded.rs の finish_validation | 欠けたevidenceはfailedでなくneeds_sessionで自動resume（ADR-0019決定5） | 一部: src/domain/receipt.rs:219、src/domain/run/recorded.rs:96。再判定: 50行のe2eと55-56行の終了・resumeは挙げた所に無い |
| Validation | 58-62 | DL-1 DL-5 | Spikeの結果の検査の範囲とparkの手順 | src/domain/execution_class.rs の missing_spike_result／headless-worker.md | Spikeの結果の欠けは`needs_session`に戻す。入口は`missing_spike_result` | 一部: src/domain/execution_class.rs:80 形だけ見ると書く。parkとcodeは書かない |
| Validation | 64-68 | DL-1 DL-5 | containerのrunの事実の出どころと拒む条件 | src/application/run_git.rs の module doc／docs/design/container-runtime.md の git | containerのrunは制御側がcloneでgitを走らせず事実を得る（container-runtime） | 一部: src/application/run_git.rs:4、docs/design/container-runtime.md:180 |
| Validation | 74-78 | DL-1 DL-5 | result_commitの残し方と結果の状態ごとの次の工程 | src/domain/run/history.rs の AfterValidation、src/domain/validation.rs の Validation | 着地まで依存taskを解放しない（76行）だけ残し、次の工程はAfterValidationへ | 一部: src/domain/run/history.rs:842 doc は1行で状態ごとの手順を書かない |
| already_satisfiedの検証 | 92-102 | DL-1 DL-5 | snapshotの決め方・流す場所・scratchの作り直し | src/application/integrate.rs の verify_snapshot、fresh_satisfied_scratch | snapshotの検証の入口は`integrate::verify_snapshot`。worktreeは信用せず作り直す | 有: src/application/integrate.rs:373、src/application/integrate.rs:607 |
| already_satisfiedの検証 | 104-109 | DL-1 | 判定・閉じ・askの分岐と上限 | src/domain/satisfied_judgment.rs の decide、src/domain/satisfied.rs の選択肢 | `high`のpassでもmainが進めば古い判定で閉じない。入口は`satisfied_judgment::decide` | 有: src/domain/satisfied_judgment.rs:151、src/domain/satisfied.rs:27 FAILED_OPTIONS |
| already_satisfiedの検証 | 111-116 | DL-1 | 人の答えごとのrunとtaskの遷移、流し直しの分岐 | src/application/supervise/execution/satisfied.rs の apply_satisfied_answers、domain::satisfied::close_step | 人の答えは受け入れ条件の判断なので聞き直さない。入口は`satisfied::close_step` | 有: src/application/supervise/execution/satisfied.rs:455、src/domain/satisfied.rs:265 |
| already_satisfiedの検証 | 118-123 | DL-1 | 死んだ位置ごとのやり直しと残ったprocessの止め方 | src/application/supervise/execution/satisfied.rs の module doc と停止の関数の doc comment | snapshotはeventから読むので替わっても同じ比較（117行）を残す | 一部: src/application/supervise/execution/satisfied.rs:96 停止だけ。やり直しの分岐は無 |
| 着地の検証 | 137-142 | DL-1 DL-5 | 置き換えの回数・順・記録・やり直しの効き方 | src/domain/landing_verification.rs の module doc と置き換えの関数 | 登録は書き換えず着地ごとに決め直す。入口は`domain::landing_verification` | 一部: landing_verification.rs:85 event、:95 log、:112 順。やり直しの効きは無 |
| 着地の検証 | 143-146 | DL-5 | 失敗の分類の読む順とこのrepositoryの印 | src/domain/verify_failure.rs の module doc | 失敗の分類の入口は`domain::verify_failure`と[Toolchain profiles] | 有: src/domain/verify_failure.rs:1、src/domain/verify_marks.rs:1 |
| 着地の検証 | 154-163 | DL-1 DL-2 | このrepositoryのITの選び方と全部流す条件の列挙 | scripts/landing-it.sh の冒頭のコメント／docs/plans/landing-it-selection.md | このrepositoryの選び方は`scripts/landing-it.sh`の冒頭、値の根拠はplansとADR | 有: scripts/landing-it.sh:43 helper、:53 古い表、:60 全部流す条件 |
| 着地の検証 | 165-167 | DL-5 | 既に落ちているtestの渡し方と修正runの除外 | src/domain/landing_verification.rs の env の doc comment／ci-watch.md | 既に落ちているtestは着地の検証から外す。一覧は[CI watch] | 一部: landing_verification.rs:47、:52、:145。修正runの項目の除外の記述は無 |
| runtimeが流すe2e | 178-179 | DL-5 | 関門の後始末の手順とlaunchdのe2eの動き方 | src/application/install.rs の e2e の設定の doc comment、tests/e2e.rs の module doc | 関門は全部のe2eを流し道具の有無で流さないe2eを持たない（177行）だけ残す | 一部: tests/e2e.rs:11 launchd は有、src/application/install.rs:175 後始末は一部 |
| runtimeが流すe2e | 185-198 | DL-1 DL-5 | 設定の読み直し・要否の判定・記録・resumeの扱い | src/domain/validation.rs の E2eRequirement と judge、domain::run_e2e::due、resume_required | 要否はvalidatingが決めて記録し、receiptは裏付けない。入口は`E2eRequirement` | 一部: src/domain/validation.rs:42、src/domain/run_e2e.rs:63、execution/resume.rs:15 |
| runtimeが流すe2e | 200-202 | DL-1 DL-2 | workerのpromptに書くe2eの1行の中身 | src/application/prompt/worker.rs の e2e_line の doc comment | workerのpromptの案内は`prompt::e2e_line` | 一部: src/application/prompt/worker.rs:2099 載せないものは doc に無い |
| runtimeが流すe2e | 203-204 | DL-2 | このrepositoryの[e2e] pathsの範囲 | dagq.toml の [e2e] のコメント | 残さない（`dagq.toml`のコメントが範囲と理由を持つ） | 有: dagq.toml:411 [e2e] のコメントが境目と除外を書く |

迷った行（違反の範囲と重なることがある）:

| 行 | 当てた規則 | 迷った点 | 寄せた側 |
| --- | --- | --- | --- |
| 38-41 | DL-4と「コードから読めない約束」 | 「欠番」は経緯の形だが、validatingで検証を流さない約束は他の工程が頼る | map |
| 70-72 | 約束に当たるのは部品の外の者が頼る結果 | 判定はregistration_brokenのdocで読めるが、sessionに返さず復旧に回す分岐は外の者の約束とも読める | map |
| 124-127 | DL-1と「してはいけないこと」 | 再計画とholdの間に閉じない約束だが、場面ごとの動作の列挙の形も持ち決めきれない | map |

### `queue-service-host-cli.md`

送る範囲の境界は地図で良い。コマンドごとのstepの手順と時間の境界をcontrolと各docへ移す
丸ごと移せる節: 届かないときの時間の境界、段の後の節、principalの発行と掃除。
分割: 不要。27KBで移せば約12KB。
同じ事実を持つ別の文書: docs/design/queue-service.md、docs/design/supervisor-lifecycle/commands.md、docs/design/supervisor-lifecycle/roles.md。

| 節 | 行 | 規則 | 細部 | 移し先 | 地図に残す 1 行 | 移し先に同じ事実 |
| --- | --- | --- | --- | --- | --- | --- |
| 送る範囲 | 37-40 | DL-5 | 拒否の記録の送り方とserviceが残す条件 | src/infrastructure/denials.rs のmodule doc | CLIの拒否はDBを開かず送り、届かなくても拒否は拒否のまま | 有: src/infrastructure/denials.rs:1 |
| 送る範囲 | 42 | DL-5 | 読み取りと断りの接続の使い分け | src/application/queue_service.rs の ServiceBackend::open_read・open_watch | 残さない。接続の使い分けはServiceBackendのdoc | 一部: src/application/queue_service.rs:202,208 |
| 送る範囲 | 48-50 | DL-5 | statsのGitの読みの扱いとall_eventsの断り | src/application/stats.rs のmodule doc | serviceはGitを起動しない（commands.mdの63行と合わせる） | 有: src/application/stats.rs:8,12 src/application/queue_reads/mod.rs:255 |
| 送る範囲 | 52,54 | DL-5 | service statusとbroker statusの答えの作り方 | src/application/queue_broker.rs と src/application/queue_service.rs の host_status | 届かなくても答え、どちらでもDBを開かない（53行） | 有: src/application/queue_broker.rs:169 |
| 対話と操作 | 64-66 | DL-1 | コマンドごとの認可の通し方とreplan resolveの手順 | src/application/queue_commands.rs の QueueCommand と docs/design/authorization.md | 認可はCLIと同じ。Gateかapplicationのどちらかで通す | 一部: src/application/queue_commands.rs:163 |
| 対話と操作 | 71-76,78-79 | DL-1 | reportのクライアントとserviceの手順と大きさ・fileの断り | src/application/report.rs のmodule docと accept_checkout | serviceが書くのはqueueのreports/の下だけで、--outへ書くのはクライアント | 一部: src/application/report.rs:1,284,297 同fileにFIFOの語なし |
| 対話と操作 | 80-83 | DL-1 | releaseの読み・保存・確定の手順 | src/application/commands/holds.rs の release_read | 成果を読むのと保存はクライアントで、serviceは読んだ版を確定で照らす | 一部: src/application/commands/holds.rs:176,691 |
| 対話と操作 | 84-85 | DL-1 | rebindで誰が何を読み書き直すか | src/application/rebind.rs のmodule doc | rebindはserviceだけ起こして打つ（86行の落とし穴） | 一部: src/application/rebind.rs:1 |
| 制御側のコマンド | 93-99 | DL-1 | コマンドごとの送る順とinstallの起動の分岐 | src/application/supervisor_service/control.rs のmodule doc と docs/design/supervisor-lifecycle/install.md | 制御側は自分のプロセスを扱い、queueの読み書きはstepとして送る | 一部: src/application/supervisor_service/control.rs:1 src/domain/queue_service.rs:1039 |
| 制御側のコマンド | 106-110 | DL-5 | コマンドごとに送れるoperationとhandoffの範囲 | src/application/supervisor_service/control.rs の lifecycle_ops・handoff_refusal・scope | 範囲の表はcontrol::scope。eventのactorは呼び出し元のprincipal。約束を残す: 渡せるbinaryはpeerの実行ファイルだけ（108） | 有: src/application/supervisor_service/control.rs:229,258,676 |
| 制御側のコマンド | 111-122 | DL-1 DL-5 | 着地の承認・検証・観測の壁・slotとtoken・telemetryのstep | src/application/supervisor_service/control.rs の own_hold・own_target とmodule doc | 着地のeventはintegratorのtokenでrequested_byが呼び出し元（ADR-t3102-1） | 一部: src/application/supervisor_service/control.rs:29,129,605 |
| 制御側のコマンド | 123-132 | DL-1 | コマンドごとの送り直しと届かないときの振る舞い | control.rs の resending、src/application/queue_service.rs の stop_record、queue_broker.rs | 結果の記録はserviceが取るまで送り直す。届かないときにDBを開かない | 有: control.rs:92 src/application/queue_service.rs:2800 queue_broker.rs:193,218 |
| principal | 138-141 | DL-5 | tokenの無い要求とtokenのfileの確かめ | src/application/queue_service.rs の authenticate と src/infrastructure/queue_service.rs の issued_by | 名乗るroleだけでは認めず、別のqueueへtokenを送らない | 有: src/domain/queue_service.rs:2509 src/infrastructure/queue_service.rs:2373 |
| principal | 143-148,150-152 | DL-5 | inbox・plannerのtokenの発行と掃除、APIの版 | src/domain/queue_service.rs の session_token_ended・planner_token_ended・request_version と credential_path_of | tokenはsessionが続くかぎり使え、serviceが答えなければinboxを開かない | 有: src/domain/queue_service.rs:106,2364,2480 infrastructure/queue_service.rs:415 |
| 計画のユースケース | 164-167,169-170 | DL-1 | ready --inheritのrefと記録の手順 | src/application/inherit.rs の keep_head と docs/design/supervisor-lifecycle/triage.md | 次のrunは記録されたheadから始まりrefの今の値に拠らない（168行） | 有: src/application/inherit.rs:308 src/application/queue_commands.rs:221 |
| 届かないとき | 176-178,180 | DL-2 DL-5 | watchとstatusの待ちの時間の境界と停止の記録 | src/application/watch.rs の look_limit・look_through と Client::look_within | 届かないときは直ちにunreachableで終わり、空の答えやDBへ戻らない（175行） | 有: src/application/watch.rs:578,631 src/domain/queue_service.rs:2572 |
| 段の後の一度だけの開き直し | 184-187 | DL-4 | 一度きりの入れ替えの経緯と手順 | ADR-t3105-1 と plugins/claude-dagq/skills/dagq-recover/reference/update.md | 残さない。tokenの無いroleはmissing_tokenで断るとprincipal節にある | 有: dagq-recover/reference/update.md、src/domain/queue_service.rs:2504 |

迷った行（違反の範囲と重なることがある）:

| 行 | 当てた規則 | 迷った点 | 寄せた側 |
| --- | --- | --- | --- |
| 103-105 | 部品の外の者がしてはいけないこと | requested_byを名乗れない等は外の者への禁止だが、判定はcontrol::scopeの1関数で読める | map |
| 77 | コードが書かない保証 | SVGの載せ方はbrowserの安全の保証で地図だが、svg.rsのmodule docにも同じ事実がある | map |
| 46 | ある部品が何を読むか | pushの名指すrepositoryは打つ場所に依らないという結果を外の者が頼るが、WatchLookの細部でもある | map |

### `architecture.md`

規則の正本で本体は地図に合う。検査の範囲の機械の検査の説明だけがtestのdocと重複
丸ごと移せる節: 検査の範囲の197-199・202-223。
分割: 不要（約4.7KB移せば約19KB）。
同じ事実を持つ別の文書: なし。

| 節 | 行 | 規則 | 細部 | 移し先 | 地図に残す 1 行 | 移し先に同じ事実 |
| --- | --- | --- | --- | --- | --- | --- |
| 検査の範囲 | 197-199 | DL-5 | lintの切り替えの場所と一覧の分け方 | clippy.toml の先頭のコメント、src/lib.rs のmodule doc | 残さない（200-201の「許す場所」の約束を残す） | 有: clippy.toml:1-8、src/lib.rs:30 がlintの有効化を書く |
| 検査の範囲 | 202-205 | DL-5 DL-1 | moduleの参照のtestの読み方と所有の表の置き場 | src/context_boundaries/modules.rs のmodule doc | L1…C8の参照の検査はcontext_boundaries/modules.rs（読み方はそのdoc） | 有: context_boundaries/modules.rs:1 に判定とWhat countsがある |
| 検査の範囲 | 206-207 | DL-2 | 所有の表が持つ項目の並び | src/context_boundaries/ownership.rs のmodule doc | 所有の表はpathから導けない所有だけ持つ（ownership.rs） | 有: context_boundaries/ownership.rs:1 に同じ項目の並びがある |
| 検査の範囲 | 208-212 | DL-5 | C1等のtestのSQLの読み方と落ちる条件 | src/context_boundaries.rs のmodule doc | 残さない（各規則の検査の行がtestを名指す） | 有: context_boundaries.rs:1 にWhat countsがある |
| 検査の範囲 | 213-221 | DL-5 | queue open・開始・方言・挿入ID・rusqlite名の各test | src/context_boundaries/queue_opens.rs・write_tx.rs・dialect.rs・inserted_ids.rs・rusqlite_names.rs のmodule doc | 残りの機械の検査はcontext_boundaries/の各testのdocが持つ | 有: queue_opens.rs:1、write_tx.rs:1、dialect.rs:1、inserted_ids.rs:1 |
| 検査の範囲 | 222-223 | DL-5 | C5の共有の部品の数え方とpathからの導き方 | src/context_boundaries/modules/ports.rs のmodule doc | 残さない（testのdocで分かる） | 一部: context_boundaries/modules/ports.rs:1 のmodule doc。C5の共有の扱いはそちら |

迷った行（違反の範囲と重なることがある）:

| 行 | 当てた規則 | 迷った点 | 寄せた側 |
| --- | --- | --- | --- |
| 80-91 | documents.md「入口」とAGENT.md DL-2「名前の並び」 | 共有の部品の定義そのもので規則L1・L3が頼るが、moduleの名前の並びでもあり入口か列挙か決めきれない | map |
| 162 | AGENT.md「複数のmoduleにまたがる約束」と「1つのmoduleの定義で分かる」 | C3の規則本文だがsuperviseのsubmodule構成の説明でもあり、execution.rsのdocで分かる部分が混じる | map |

### `supervisor-lifecycle/goal-review.md`

番号の手順全体が場面の書き写し。約束と落とし穴の一覧に組み替え、手順はportsとdomainのdocへ移す
丸ごと移せる節: 3のprovider、5の適用、7、9。
分割: 不要。23KBで移せば約8KB。
同じ事実を持つ別の文書: docs/design/supervisor-lifecycle/actor-model.md、docs/design/provider-lifecycle.md、docs/design/follow-up-membership.md、docs/design/supervisor-lifecycle/roles.md。

| 節 | 行 | 規則 | 細部 | 移し先 | 地図に残す 1 行 | 移し先に同じ事実 |
| --- | --- | --- | --- | --- | --- | --- |
| Goal review（冒頭） | 31-33 | DL-1 | 引き継いだ行のinterruptedの閉じ方 | docs/design/supervisor-lifecycle/handoff.md と src/application/ports/planning.rs の take_over_goal_review | interruptedはattemptに数えず、残った候補はやり直す | 一部: src/application/ports/planning.rs:1006 docは1行 |
| 1 記録 | 36-40 | DL-5 | 行の書き方と生きているsupervisorの判定 | src/application/ports/planning.rs の begin_goal_review、src/domain/mod.rs の job_supervisor_lives | 同時に1つはqueue全体（plan reviewと別）で、DBの制約でなくbeginで守る | 有: planning.rs:987 src/domain/mod.rs:2368 headless-job-processes.md:85 |
| 2 起動の条件 | 41-42 | DL-5 | 候補の条件の列挙 | src/domain/goal_review.rs の reviewable・tasks_done、ports の goal_review_candidates | 起動の条件はgoal_review_candidatesとreviewableのdoc | 有: src/domain/goal_review.rs:233,264 planning.rs:958 |
| 2 起動の条件 | 47-53 | DL-5 | fingerprintに入るものとrearmの書き方 | src/domain/goal_review.rs の review_fingerprint・with_replacements・rearmable | 入力が変わらなければ起動せず、人のgoal reviewだけが取り直させる | 有: src/domain/goal_review.rs:189,211,287 |
| 3 起動 | 54-60 | DL-1 | launch・event・session・envの起動の順 | src/application/supervise/planning/goal_review.rs の start_goal_review のdoc comment | 入口はstart_goal_review。jobは読むだけでqueue serviceを通る | 一部: src/application/supervise/planning/goal_review.rs:145 docは無 |
| 3 起動 provider | 63-70 | DL-1 | goal reviewのproviderの行き先の分岐とCodexの組み方 | src/application/supervise/planning/goal_review.rs の goal_review_route_of と docs/design/provider-lifecycle.md | 行き先はgoal_review_route_of（一般の規則はActor modelのprovider） | 一部: goal_review.rs:763 src/domain/actor_model.rs:612 provider-lifecycle.md:194 |
| 4 入力 | 71-72,74 | DL-5 | jobに渡す材料の列挙とpromptの指示 | src/application/supervise/planning/goal_review.rs の goal_review_material と prompt::goal_review_prompt | promptの上限と選び方はPromptの文書（73行） | 一部: src/application/supervise/planning/goal_review.rs:327 |
| 5 verdict | 75 | DL-5 | gapsの連続をaskに変える判定 | src/domain/goal_review.rs の decide・MAX_GOAL_GAPS | 残さない。docが持つ | 有: src/domain/goal_review.rs:90,95,246 |
| 5 verdict | 79-91 | DL-1 | verdictごとの適用の手順 | src/infrastructure/goal_reviews.rs の finish_goal_review・register_gaps と ports の doc | gapsはdraftになりruntimeのplannerが採否を決める。askは通知を送らない | 一部: src/infrastructure/goal_reviews.rs:203 planning.rs:1006 achieved・askの手順は無 |
| 6 失敗 | 96-99,101-105 | DL-1 | 使えないproviderの失敗の分類と控えの手順 | src/application/supervise/execution/jobs.rs の job_failure_route と docs/design/provider-lifecycle.md | 使えないproviderの失敗はinterruptedで閉じ、人を待たず起動し直す（100行） | 一部: src/application/supervise/execution/jobs.rs:1060,1217 |
| 7 answer | 108-113,115-118 | DL-1 | 答えごとの適用の条件とruntime_deliversの決まり方 | src/domain/goal_review.rs の GoalAnswer と src/infrastructure/goal_reviews.rs の decide_goal | inboxに渡したanswerを後でruntimeが適用することはない（114行） | 一部: src/domain/goal_review.rs:119 planning.rs:1025,1030 goal_reviews.rs:338 |
| 9 achievedの後の訂正 | 121-133 | DL-1 | correct_goalのaskの開き方と答えごとの適用 | src/domain/follow_up.rs の opens_correction・correction_question と ports の decide_correction、docs/design/follow-up-membership.md | 訂正はaskで人が決め、走っている依存taskを止めずeventも書き換えない（134行） | 一部: src/domain/follow_up.rs:1241,1297 planning.rs:1040 follow-up-membership.md:124 |
| 末尾（test） | 136 | DL-2 | testのfileの並び | コード自身（tests/it/goal_review.rs など） | 残さない。testの名前はコードから読める | 有: tests/it/goal_review.rs、tests/it/follow_up_membership.rs |

迷った行（違反の範囲と重なることがある）:

| 行 | 当てた規則 | 迷った点 | 寄せた側 |
| --- | --- | --- | --- |
| 76-78 | コードが書かない保証 | 適用の再検査はfinish_goal_reviewの細部だが、引き継ぎ後も同じ結果という保証は外の者が頼る | map |
| 43-44 | 読み手が踏む落とし穴 | 壊れた置換の記録は3経路にまたがり人の次の手が要るが、条件の一部としてはdocでも読める | map |
| 92-95 | コードを読めば分かる細部 | 失敗の後の人の次の手は地図だが、番号の手順の中でevent名の列挙にも読める | map |

### `supervisor-lifecycle/waiting.md`

待ちの出入り8KBは手順の書き写しでdocは薄い。地図は約束と掃除の保証に絞り手順をdocへ足す
丸ごと移せる節: 待ちの出入り、差し戻しと解消依頼の段、引き継ぎとadopt。
分割: 不要（節を移せば約10KBで収まる）。
同じ事実を持つ別の文書: docs/design/supervisor-lifecycle/needs-session.md, docs/design/supervisor-lifecycle/headless-worker.md。

| 節 | 行 | 規則 | 細部 | 移し先 | 地図に残す 1 行 | 移し先に同じ事実 |
| --- | --- | --- | --- | --- | --- | --- |
| 待ちの出入り（印） | 37-40 | DL-2 DL-5 | Slotの欄・phaseの名前・used_slotsの数え方 | src/application/supervise/execution/waiting.rs の Waiting・used_slots のdoc comment | 待ちはstatusでなくslotの印で、戻ればphaseの続きから進む（Slot::waiting） | 一部: execution/waiting.rs:46 は1行、:180 は数え方のみ。phaseの列挙は無 |
| 待ちの出入り（始める） | 43-46 | DL-1 | phaseごとの待ちに入る条件とaskの順、上限時の記録 | src/application/supervise/execution/waiting.rs の start_waits、domain::waiting の waits_for | askの古い順に上限まで待ちへ。超えた分はslotのまま進む（start_waits） | 一部: execution/waiting.rs:216、domain/waiting.rs:36。phaseごとの条件は無 |
| 待ちの出入り（見張り） | 47-66 | DL-1 | watch_waitingの判定の順と場面ごとの終わり方 | src/application/supervise/execution/waiting.rs の watch_waiting のdoc comment | 見張りはsessionへ送らずstatusも変えない。答えの送信は戻ったslotのtick | 一部: execution/waiting.rs:346 のdocは観察の項目だけで順と条件は無 |
| 待ちの出入り（終わり・戻す） | 67-72 | DL-1 DL-2 | end_waitの欄とcauseごとの戻り先、戻す順 | src/application/supervise/execution/waiting.rs の end_wait・return_waiting_runs、domain::waiting::WaitCause | 戻す処理はdrain中もfill_slotsより前（return_waiting_runs） | 一部: execution/waiting.rs:580,:644。causeごとの戻り先の表は無 |
| taskのhold | 78 | DL-1 | 解除の種類ごとに答えがどこへ入るかの分岐 | docs/design/supervisor-lifecycle/needs-session.md の「taskのhold」、domain::task_hold::release_decision | 新しいrunにする解除では答えはresumeに載らず前のrunの判断の節へ | 有: domain/task_hold.rs:267、needs-session.md:182 |
| 差し戻しと解消依頼の段 | 86-99 | DL-1 DL-5 | 計時の再開、質問中のidle、古いmarker、配送の手順 | src/application/supervise/execution/resume.rs の restart_clocks・ResumeWatch、revise.rs の ReviseWatch | slotの外で段のtimeoutを止め、戻ったら計時をやり直す（restart_clocks） | 一部: resume.rs:1573、revise.rs:219。質問中のidleの全条件は未統合 |
| 差し戻しと解消依頼の段（引き継ぎとadopt） | 102-103 | DL-1 | Revise・Resumeの組み立ての読み先 | src/application/supervise/execution/adopt.rs の resume、handoff_slots.rs | 残さない（下の「引き継ぎとadopt」の節が案内する） | 有: execution/adopt.rs:229,:330、handoff_slots.rs:107 |
| 差し戻しと解消依頼の段（cause） | 105 | DL-2 DL-5 | causeの追加分とWaitState::ofが読むeventの並び | src/domain/waiting.rs の WaitCause と WaitState::of のdoc comment | leaseを失ったプロセスは待ちの終わりを書かない（adopt側が続けるため） | 一部: domain/waiting.rs:79,:162。lease_lost等を書かない理由はdocに無 |
| 引き継ぎとadopt | 109-113 | DL-1 DL-5 | restore_waitingが戻すものと上限時の扱い | src/application/supervise/execution/waiting.rs の restore_waiting、src/application/supervise/execution/adopt.rs の adopt_stale_runs | 待ちはイベントから組み立て直し、event を記録し直さない（restore_waiting） | 有: execution/waiting.rs:683、adopt.rs:27、domain/waiting.rs:146 |
| 空き容量のための掃除 | 122-131 | DL-1 DL-5 | 渡し方・候補の選び方・消す前の確かめ方 | src/application/supervise/host_ops/cleanup.rs の waiting_candidate・still_waiting・worktree_busy | 待ちのrunの掃除はturnの終わった後だけ。消す前に確かめ直す（cleanup） | 有: host_ops/cleanup.rs:742,:1741,:814,:765、execution/waiting.rs:193 |

迷った行（違反の範囲と重なることがある）:

| 行 | 当てた規則 | 迷った点 | 寄せた側 |
| --- | --- | --- | --- |
| 31-33 | AGENT.md「部品の外の者が頼る結果」と「1つの型の定義で分かる」 | 着地待ちの軽い枠は複数の判定にまたがる約束だが、Slot::landing_turnと数え方の個別の事実も同じ行にある | map |
| 132-134 | AGENT.md「部品の外の者が頼る結果」と still_waiting のdoc comment | 掃除job中は答えを配送しない保証は待ちと掃除をまたぐが、still_waitingのdocにほぼ同じ文がある | map |

### `supervisor-lifecycle/ask.md`

kindと答えごとの手順が本文の大半。約束だけ残しdoc commentへ移す。AskKindのdocは薄く、足す量が多い
丸ごと移せる節: worker_questionの表、queue_hold、AIの推奨の欄・表示・集計。
分割: 不要。17KBで移せば10KB以下。
同じ事実を持つ別の文書: docs/design/authorization.md、docs/design/supervisor-lifecycle/queue-hold.md、docs/design/supervisor-lifecycle/review.md。

| 節 | 行 | 規則 | 細部 | 移し先 | 地図に残す 1 行 | 移し先に同じ事実 |
| --- | --- | --- | --- | --- | --- | --- |
| ask / answer / asks | 21-30 | DL-1 | askのkindごとの作り手・紐づけ・答えの適用先 | src/domain/mod.rs の known_ask_kinds!(AskKind) の各kindのdoc comment | kindごとの作り手と答えの行き先はAskKindのdoc（作れるroleはAuthorization） | 一部: src/domain/mod.rsのAskKindの//が作り手と答えの行き先を書く。answer_promptのdocが最も詳しい |
| ask / answer / asks | 31-38 | DL-1 DL-5 | dagq askの登録の手順と紐づけ・重複の扱い | src/application/commands/dialogue.rs の Dialogue::ask と src/domain/mod.rs の NewAsk | 同じ(task,run,kind,finding)のopenなaskは1件で、後の問いは捨てる | 一部: src/domain/mod.rs:1611 task/run必須は有。:1924 created:false |
| ask / answer / asks | 39-40 | DL-1 | roleごとに開ける・答えられるaskの判定の列挙 | docs/design/authorization.md | 判定はDialogueが書く前に呼び出し元のroleで行う（表はAuthorization） | 有: src/domain/authorization.rs:792、docs/design/authorization.md:118 |
| 人が要る理由 | 43,45-46 | DL-5 | --becauseの検査とruntimeのaskの理由の決まり方 | src/domain/mod.rs の NewAsk の reason の欄 | askは人が要る理由を必ず持ち、当てはまらない相談はaskにしない（44行） | 一部: src/domain/mod.rs:1611 欄のdoc、src/domain/error.rs:763 |
| 認証のaskは1件 | 47-50 | DL-1 | queue_holdの検知・affected・通知の手順 | docs/design/supervisor-lifecycle/queue-hold.md と src/domain/queue_hold.rs | 認証と費用のaskは1件にまとまる（手順はqueue-hold） | 有: src/domain/queue_hold.rs:1、queue-hold.md:34 |
| dagq answer | 51-52,54-60 | DL-1 | approve_landingの答えの形ごとの適用とruntime_delivers | src/domain/mod.rs の LandingAnswer と landing.rs の apply_landing_answer | approve_landingの答えの読みはLandingAnswer（Reviewの6） | 有: src/domain/mod.rs:798,843,3053 landing.rs:2893,3150 |
| dagq ask close | 61,64-66 | DL-1 | runtimeが自分で閉じる場面の列挙とanswered_by | src/infrastructure/asks.rs の close_by_runtime・close_asks_in | runtimeが閉じるときもask_closedをcloseと同じtransactionで書く | 一部: src/infrastructure/asks.rs:1054,1085 場面の列挙は無 |
| worker_questionの分類コード | 79-84 | DL-5 | --topicの検査と記録する欄・省略の扱い | src/domain/worker_question.rs のmodule docと NewAsk の topics | reason_categoryと分類コードは別の軸で一方から他方を推さない | 有: src/domain/worker_question.rs:1 src/domain/mod.rs:1611 |
| worker_questionの分類コード | 89-100 | DL-2 | 分類コードと定義の表（reason_categoryの目安を含む） | src/domain/worker_question.rs の WORKER_QUESTION_TOPICS | 残さない。一覧はWORKER_QUESTION_TOPICS（76行が既に指す） | 一部: src/domain/worker_question.rs:14 目安の列は無。module docが表を文書に託す |
| AIの推奨と確信度 | 108-113 | DL-2 DL-5 | 推奨の欄とCLIの検査の順序 | src/domain/mod.rs の NewAsk（offered_options・check_asker） | blockedは推奨が必須。leave it / waitの判定はCLIでなくpromptの規則 | 有: src/domain/mod.rs:1654,1675 |
| AIの推奨と確信度 | 114-117 | DL-5 | show・statusの出力のどこに推奨が載るか | src/view.rs の TaskDetail::asks | 残さない。showの絞りはviewのdoc | 有: src/view.rs:26,72,192 |
| AIの推奨と確信度 | 119-122 | DL-5 | AIが決めた記録と推奨の一致率の数え方 | src/domain/stats/recommendations.rs のmodule doc | 決めた記録を数える入口はDECIDED_WITHOUT_ASK | 有: src/domain/stats/recommendations.rs:8,77 |

迷った行（違反の範囲と重なることがある）:

| 行 | 当てた規則 | 迷った点 | 寄せた側 |
| --- | --- | --- | --- |
| 53 | AGENT.mdの「失敗の後も入力を落とさない」 | 断っても開いたまま残るのは人が頼る約束で地図。再判定でDL-1から外した。消せるならTriageの9との重複が理由 | map |
| 69 | 読み手が踏む落とし穴 | statsの対の作り方は1 moduleの細部だが、closeだけで閉じない規則の理由として68行を支える | map |
| 86-87 | コードが書かない保証 | 主と副の選び方はworkerへの規則で地図に当たりうるが、promptに載せる定義の移し先とも読める | map |

### `supervisor-lifecycle/actor-model.md`

中身はactor_model.rsのdocと重なり移す量は少ない。文書から消す作業が主
丸ごと移せる節: provider節のcheck・launch・route、runtimeのplannerの経路。
分割: 不要。16KBで移せば約8KB。
同じ事実を持つ別の文書: docs/design/provider-lifecycle.md、docs/design/supervisor-lifecycle/plan-planners.md、docs/design/supervisor-lifecycle/goal-review.md。

| 節 | 行 | 規則 | 細部 | 移し先 | 地図に残す 1 行 | 移し先に同じ事実 |
| --- | --- | --- | --- | --- | --- | --- |
| 役割と[roles.<role>] | 49-52 | DL-5 | 表が無い・片方だけ・読めないときの起動 | src/domain/actor_model.rs の RoleModels::launch | 設定は読む時点ごとに読み直し、再起動なしで次のjobから効く（53行） | 一部: src/domain/actor_model.rs:229 読めないときのwarnは無 |
| provider | 58-65 | DL-2 | providerのtomlの書式と省略の既定 | docs/design/supervisor-lifecycle/run-environment.md と RoleModels | 残さない。書式はRun environment | 一部: run-environment.md:138 src/domain/actor_model.rs:154 docは1行 |
| provider | 67-69,71-75 | DL-5 | Codexで動ける役割の検査とlaunchの既定 | src/domain/actor_model.rs の CODEX_ROLES・RoleModels::check・launch | 動けない役割のcodexはwarnで済ませず設定の誤りにする（ADR-t1063-1）。約束を残す: 誤りの間の起動とdoctorの表示（71） | 一部: src/domain/actor_model.rs:59,128,197,229。再判定: 71のwarnはsrc/application/supervise/execution.rs:2371。doctorの表示は未確認 |
| provider | 76-78,80-81 | DL-5 | job_routeの切り替えと待ちの分岐、setup_review | src/domain/actor_model.rs の job_route と docs/design/provider-lifecycle.md | 切り替えの入口はjob_route、設定と例外はProvider lifecycle | 有: src/domain/actor_model.rs:612、docs/design/provider-lifecycle.md:194 |
| provider | 82-83 | DL-5 | inboxの既定と起動の引数の優先 | src/domain/actor_model.rs の inbox_launch | inboxはjob_routeを通らず、providerの控えで止めない（84行） | 有: src/domain/actor_model.rs:541 src/application/lifecycle.rs:2510 |
| 差し戻しで開き直すplannerの段上げ | 90-91 | DL-5 | 差し戻しのplannerのeffortの段上げの値の決め方 | src/domain/actor_model.rs の raise と src/application/planner.rs の open_runtime_planner | 開き直すplannerだけeffortを1段上げる（生きているplannerは上げない） | 有: src/domain/actor_model.rs:97 module doc:1 planner.rs:141 |
| 記録（launch） | 99-107 | DL-1 | jobの種類ごとにlaunchをどのeventに書くか | src/domain/actor_model.rs の ActorLaunch のdoc comment | launchは起動の意図で、実際の値はtranscriptが持つ（110行） | 一部: src/domain/actor_model.rs:741,841 event名の列挙は無 |
| 記録（launch） | 115-119 | DL-5 | Codexのjobのthreadとmodelの写し方、headless_jobs.provider | src/domain/headless_job.rs の JobSession::record と JobSubject::provider | Codexのjobだけは終わりのeventしか出どころが無いので写す（112行の例外） | 一部: src/domain/headless_job.rs:294、src/domain/actor_model.rs:795 |
| runtimeのplannerの経路 | 125-135 | DL-5 | 旧keyのrouteの受け方とplanners.route・doctorの出力 | src/domain/actor_model.rs の ROUTE_KEY と application::health::roles | runtimeのplannerは非対話だけ。起動はHeadless workerの文書 | 一部: src/domain/actor_model.rs:120 src/application/health/host.rs:385 |

迷った行（違反の範囲と重なることがある）:

| 行 | 当てた規則 | 迷った点 | 寄せた側 |
| --- | --- | --- | --- |
| 45-47 | DL-4 経緯 | 廃止した人のplannerの行は残る設定の説明だが、新しく使われない経緯の記述にも読める | detail |
| 70 | DL-4 経緯 | warnにしなかった理由はADRへのリンクと地図の落とし穴だが「形にしなかった」は経緯にも読める | map |
| 112-114 | コードが書かない保証 | 値を2か所に持たないという不変条件だが、1つのevent定義のdocでも読める | map |
| 142-149 | 約束の一覧に足した約束 | 「守る約束」と称するが群の決め方は1関数の細部にも読め、plannerへの試しは実装前で移し先が無い | map |

### `supervisor-lifecycle/commands.md`

既に約束の一覧の地図。63・70行のGitの読みはapplication/stats.rs:12と食い違う疑い（全体のreview向け）
丸ごと移せる節: なし（logの2行だけ）。
分割: 不要。12KBで層を直せば収まる。
同じ事実を持つ別の文書: docs/design/queue-service-host-cli.md。

| 節 | 行 | 規則 | 細部 | 移し先 | 地図に残す 1 行 | 移し先に同じ事実 |
| --- | --- | --- | --- | --- | --- | --- |
| log | 99-101 | DL-5 | どのprocessがfileに書くかと古いlogの消し方 | src/infrastructure/telemetry.rs のmodule doc | logのfileが書けなくてもprocessは止めない（入口はtelemetry） | 一部: src/infrastructure/telemetry.rs:20 消し方は有。commandごとの書き分けは無 |

迷った行（違反の範囲と重なることがある）:

| 行 | 当てた規則 | 迷った点 | 寄せた側 |
| --- | --- | --- | --- |
| 53 | 複数のmoduleや仕組みにまたがる約束 | plan reviewとclaimの控えがstatsと同じ数え方という約束だが、関数名を挙げた手順にも読める | map |
| 70 | コードを読めば分かる細部 | areaの作り直しは利用者が頼る結果だが、63行とともに今のコード（台帳から作る）と食い違う疑いがある | map |

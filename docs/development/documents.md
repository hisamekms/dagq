---
id: development-documents
type: development
title: このrepositoryの文書の規則（判断の記録・ADR・design・plans・frontmatter・workerの文書の照合・AGENTS.md・commit）
status: current
created: 2026-10-03
updated: 2026-10-04
owners:
  - hisamekms
tags:
  - documentation
  - conventions
related:
  - adr-t1453-2
  - adr-t598-1
  - adr-t1091-1
  - adr-t1428-1
  - adr-t1662-2
  - docs-frontmatter
  - development-task-registration
---

# このrepositoryの文書の規則

`docs/`の文書とcommitを書く・変えるときの今の規則。読むのは、ADR・design・plansを書くかcommitするworker（AGENTS.mdの「### worker」から辿る）、ADRを書くtaskを登録するplannerとそれを見るplan review（[taskの登録](task-registration.md)の「ADRを書くtask」から辿る）、人。ADRの書き方の規則はこの文書が正本で、[文書の案内](../README.md)・[ADRの索引](../adr/README.md)・[frontmatter仕様](../frontmatter.md)はここを指す。frontmatterの欄・ADRの状態の意味・注記の書式はfrontmatter仕様、経緯はADR（[ADR-t598-1](../adr/2026-09-26-t598-1-adr-id-is-task-id-small-adrs-and-design-holds-current-state.md)ほか）が持つ。

## 判断の記録

人の判断はADR・Goalの記述・`Task.context`・receiptの`summary`に残す（作業記録のジャーナルは[ADR-0036](../adr/0036-delete-frozen-work-records.md)で削除した）。

## ADR

- 将来の実装や運用に大きな影響を与える決定は`docs/adr/`にADRを追加する。アーキテクチャ全体に影響し手戻りが大きい決定は、実装より先にADRを作る。新しいADRは[template](../adr/0000-template.md)をコピーして作る。
- 既存のADRは書き換えない（append-only）。後から変えてよいのは`status`・`accepted_on`・`superseded_by`・`superseded_on`・`deprecated_on`・`amended_by`とH1直後の注記1行だけで、これらだけの変更に新しいADRは要らず、`updated`も動かさない。`supersedes`と`amends`は置き換え・amendsの本文と一緒に書く。決定を足す・変える・除くのは、下の置き換えか`amends`の新しいADRで行う。
- どのADRが今の決定か（`accepted`・`amended_by`・`superseded_by`の辿り方・`deprecated`）は[frontmatter仕様](../frontmatter.md)の「ADR fields」の状態と欄の表が持つ。
- 小さなADR: 1 ADRに決定1つ（密に結びついた数個まで）、本文はおおむね100行以内。書くのは変えるのに人の判断が要るもの（問題と文脈、方針・原則・境界・不変条件、退けた案、結果。目安は「これを変えるとき人に聞くか」）で、eventの種類と欄・CLIのflagの綴り・JSONの形・既定値や閾値の数値・関数やmoduleやファイルの名前・migrationの番号・testの名前は`docs/design/`に書く（ADR-t598-1決定2・3）。
- 置き換えか`amends`か: 元のADRの決定の数と変える範囲で決め、IDの形（4桁か新しい形か）では決めない（[ADR-t1091-1](../adr/2026-09-30-t1091-1-amend-or-replace-by-number-of-decisions.md)）。番号付きの決定を複数持つADR（4桁でも新しい形でも。0047・0073・t813-2など）の一部の決定を変えるときは、小さな新しいADRの`amends`に変える決定（例: `adr-0047 decision 24`）を書き、元のADRの`amended_by`にそのIDを足し、同じ変更で`docs/design/`を今の姿に直す。決定が1つのADRを変えるときと、決定の大半を変えるときは、新しいADRで丸ごと置き換え、まだ有効な古い決定を書き直して引き継ぎ、古いADRを丸ごと`superseded`にする（1つのADRが複数を置き換えてもよい）。どちらにするかをtaskに書くことは[taskの登録](task-registration.md)の「ADRを書くtask」が持つ。
- 置き換えは後継を`accepted`にする変更と同じ変更で行い、古いADRの`superseded_on`は後継の`accepted_on`と同じ日にする。`proposed`の後継は何も置き換えない（`supersedes`に予定のIDを書いてよいが、古いADRの状態は後継がacceptedになるまで変えない）。
- ADRの状態を変える変更は、同じ変更で[ADRの索引](../adr/README.md)の表を直す。新しい形の行は4桁の行の後ろに`accepted_on`の順で並べる。

## ADRのID

- 新しいADRのIDはそれを書くtaskのIDと枝番の`adr-t<task ID>-<N>`（Nは1から。1本だけでも`-1`）、ファイル名は`docs/adr/<YYYY-MM-DD>-t<task ID>-<N>-<slug>.md`で、日付は`accepted_on`（ADRを書くtaskではworkerが書いてacceptedにした日）にする（ADR-t598-1決定1）。`proposed`のADRは書いた日の名前にし、acceptedにする変更で`accepted_on`に合わせて名前とそこへのlinkを直す。IDはtaskのIDから決まるので、mainの次の空き番号を探さず、plannerもplan reviewも番号の割り当ての棚卸しをしない。
- 後続のtaskはADRを`ADR-t<ID>-<N>`で参照する（日付を含めないので着地前から書ける）。今どうなっているかを指すときは`docs/design/`の文書を、なぜそうしたかを指すときはADRを指す（決定4）。
- 既存の4桁のADR（0001〜、IDは`adr-NNNN`）と、登録済みのtaskが予約した4桁の番号はそのまま使い、振り直さない。新しく登録するADRのtaskは新しい形にする。
- 例外: 予約した4桁の番号がmainですでに埋まっていたら（`integrate`の`check-adr-numbers.sh`が重複で落ちてresumeされたときも同じ）、番号の衝突は人が要る理由（[ADR-0047](../adr/0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)決定41）に当たらないので、workerは`dagq ask`にせず、自分のtaskのIDの新しい形（上の1つ目）にファイル名とfrontmatterの`id`を書き直す。自分の変更の中の参照も合わせ、元の番号と新しいIDをreceiptの`summary`に書く。
- `scripts/check-adr-numbers.sh`（CIも実行する）が検出するものはscriptの冒頭のcommentが持つ。scriptの名前は登録済みのtaskのverifyが使うので変えない。

## design

実装を変えたら`docs/design/`の該当文書の内容と`updated` / `last_verified`を更新する。内容を変える必要がない文書には日付だけの差分を作らない（[ADR-t1428-1](../adr/2026-10-03-t1428-1-decide-the-documents-to-update-when-the-code-changes.md)）。

計測が読む・書くストアやビュー（表・ファイル・外の記録）を足すtaskは、同じ変更で[計測](../design/measurement.md)の「SSOTとビュー」の節に区分（SSOT・ビュー・材料）と今のアダプタを書く（[ADR-t1662-2](../adr/2026-10-04-t1662-2-measurement-stores-ssot-and-views.md)決定9）。

## plans

- ステップの状態が変わったら`docs/plans/current.md`を更新する。
- 計画は実際の依存関係と完了条件に合わせて更新する。
- 進行中の計画は`active`、完了した計画は`completed`に更新し、消さずに履歴として残す（状態の値は[frontmatter仕様](../frontmatter.md)の「Type-specific fields」）。

## frontmatter

frontmatterは[frontmatter仕様](../frontmatter.md)に従う。

## 権限の表を写す文書

pluginの`dagq-planner`の`SKILL.md`の「Where your authority ends」と`dagq`の`reference/authority.md`の「What each role is refused」は、[Security](../design/security.md)の「actor × capability」と[Authorization](../design/authorization.md)の「Policy」の表と一致させる。表を変える変更は同じ変更でこれらを直す。

## pluginの汎用性

pluginとrepositoryの規則・値・経緯の受け持ちは[ADR-t1453-2](../adr/2026-10-03-t1453-2-ownership-of-agents-md-plugin-development-docs-and-config.md)決定1の表、runtimeとの境界は決定5。検査する記述の一覧は[棚卸し](../plans/agents-slim-inventory.md)「4. pluginの固有の記述の項目」。固有の印（このrepositoryのtest・coverageのコマンド、`tests/it`、task・goal・askの番号の逸話、日付、「fixed binary」、`docs/design/`・`docs/plans/`のpathと「design docs」への参照、pluginのdirectoryを出る相対リンクなど）の戻りは`tests/plugin.rs`の`no_skill_carries_this_repository_s_rules`が検査し、印の語の一覧はそのtestが持つ。

## workerの文書の照合

workerはreceiptの前に、受け入れ条件の対応づけ（[手元の検証](local-checks.md)の「受け入れ条件の対応づけ」）に続けて、workerのpromptが指示する文書の照合（仕組みは[prompt](../design/supervisor-lifecycle/prompt.md)の「文書の照合」、[ADR-t1428-1](../adr/2026-10-03-t1428-1-decide-the-documents-to-update-when-the-code-changes.md)）を行う。このrepositoryで差分と照合する文書は、taskが名指す文書と、変えた挙動を説明する`docs/design/`・pluginのskillとreference・AGENTS.md・`docs/development/`・ADRの索引で、`summary`に更新したpath・節か不要の理由を書く。taskのpathsの外のずれは`docs_drift`のfollow_upにする。

## AGENTS.md

- AGENTS.mdは概要・役割と変更の範囲ごとの読む案内・本番queueと開発環境の境界・開始時の短い制約・検証と文書の規則への参照だけを持つ（[ADR-t1453-2](../adr/2026-10-03-t1453-2-ownership-of-agents-md-plugin-development-docs-and-config.md)決定1・3）。規則の本文はAGENTS.mdに足さず、その正本（`docs/development/`の該当の文書、実装は`docs/design/`、経緯はADRと`docs/plans/`、設定値は`dagq.toml`）に書き、AGENTS.mdには読む案内が要るときだけ参照の1行を足す。
- 上限は9,216 byte（9 KiB）。`scripts/check-agents-md-size.sh`がAGENTS.mdのbyte数（`wc -c`）を測り、上限を超えればbyte数と上限を出してexit 1にし、CIも実行する。行数では測らない（日本語の1行は長く、行数が大きさを表さない。ADR-t1453-2決定6）。
- 決め方: 組み直した後（task 1461、2026-10-04）のAGENTS.mdは8,340 byte。余裕は876 byte（約10%）で、案内の参照の行を2〜3行足せるが、規則の本文の節を足せば超える大きさにした。候補の4〜8 KiBは、どれも今の案内（役割と変更の範囲ごとに読む文書の名指し）が入らない（8 KiB＝8,192 byteでも148 byte足りない）。
- 上限を上げるのは、案内（読む文書の名指し）が増えて規則の本文を正本へ移してもなお超えるときだけで、同じ変更でこの節の値と決め方、scriptの`limit`を直す。

## commit

- メッセージは`feat:` / `fix:` / `docs:` / `test:`の接頭辞、本文は何をなぜ変えたか。
- run sessionが自分のrun branchにcommitし、mainへの着地とpushを`dagq integrate`だけが行うこと（1 task 1 squash commit、メッセージはtaskのtitleとreceiptの`summary`からruntimeが作る）は[integrate](../design/supervisor-lifecycle/integrate.md)とworkerのpromptが持つ。`push_failed`のattentionの手当てはpluginの`dagq-recover`の`reference/review-by-hand.md`の「A failed push」が持つ。

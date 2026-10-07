---
id: development-documents
type: development
title: このrepositoryの文書の規則（判断の記録・ADR・design・plans・frontmatter・workerの文書の照合・AGENTS.md・commit）
status: current
created: 2026-10-03
updated: 2026-10-07
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
  - adr-t1942-1
  - adr-t1942-2
  - docs-frontmatter
  - development-task-registration
---

# このrepositoryの文書の規則

`docs/`の文書とcommitを書く・変えるときの今の規則。読むのは、ADR・design・plansを書くかcommitするworker（AGENTS.mdの「### worker」から辿る）、ADRを書くtaskを登録するplannerとそれを見るplan review（[taskの登録](task-registration.md)の「ADRを書くtask」から辿る）、人。ADRの書き方の規則はこの文書が正本で、[文書の案内](../README.md)・[ADRの索引](../adr/README.md)・[frontmatter仕様](../frontmatter.md)はここを指す。frontmatterの欄・ADRの状態の意味・注記の書式はfrontmatter仕様、経緯はADR（[ADR-t598-1](../adr/2026-09-26-t598-1-adr-id-is-task-id-small-adrs-and-design-holds-current-state.md)ほか）が持つ。

## 判断の記録

人の判断はADR・Goalの記述・`Task.context`・receiptの`summary`に残す（作業記録のジャーナルは[ADR-0036](../adr/0036-delete-frozen-work-records.md)で削除した）。

## ADR

- 将来の実装や運用に大きな影響を与える決定は`docs/adr/`にADRを追加する。アーキテクチャ全体に影響し手戻りが大きい決定は、実装より先にADRを作る。新しいADRは[template](../adr/0000-template.md)をコピーして作る。
- 既存のADRは書き換えない（append-only）。後から変えてよいのは`status`・`accepted_on`・`superseded_by`・`superseded_on`・`deprecated_on`・`amended_by`とH1直後の注記1行だけで、これらだけの変更に新しいADRは要らず、`updated`も動かさない。frontmatterの日付の行の行末のコメントは欄の値でも本文でもないので、値を変えずにコメントだけを消すのはこの制限に当たらない（下の「frontmatter」、[ADR-t1854-1](../adr/2026-10-07-t1854-1-frontmatter-date-lines-hold-only-the-date.md)決定3）。`supersedes`と`amends`は置き換え・amendsの本文と一緒に書く。決定を足す・変える・除くのは、下の置き換えか`amends`の新しいADRで行う。
- どのADRが今の決定か（`accepted`・`amended_by`・`superseded_by`の辿り方・`deprecated`）は[frontmatter仕様](../frontmatter.md)の「ADR fields」の状態と欄の表が持つ。
- 小さなADR: 1 ADRに決定1つ（密に結びついた数個まで）、本文はおおむね100行以内。書くのは変えるのに人の判断が要るもの（問題と文脈、方針・原則・境界・不変条件、退けた案、結果。目安は「これを変えるとき人に聞くか」）で、eventの種類と欄・CLIのflagの綴り・JSONの形・既定値や閾値の数値・関数やmoduleやファイルの名前・migrationの番号・testの名前はコード（定義のそばのdoc comment）か`docs/design/`の地図に書く（ADR-t598-1決定2・3、決定3は[ADR-t1942-1](../adr/2026-10-07-t1942-1-design-docs-in-four-layers-with-size-budgets.md)がamends。下の「design」）。
- 置き換えか`amends`か: 元のADRの決定の数と変える範囲で決め、IDの形（4桁か新しい形か）では決めない（[ADR-t1091-1](../adr/2026-09-30-t1091-1-amend-or-replace-by-number-of-decisions.md)）。番号付きの決定を複数持つADR（4桁でも新しい形でも。0047・0073・t813-2など）の一部の決定を変えるときは、小さな新しいADRの`amends`に変える決定（例: `adr-0047 decision 24`）を書き、元のADRの`amended_by`にそのIDを足し、同じ変更で今の姿（`docs/design/`かdoc comment）を直す。決定が1つのADRを変えるときと、決定の大半を変えるときは、新しいADRで丸ごと置き換え、まだ有効な古い決定を書き直して引き継ぎ、古いADRを丸ごと`superseded`にする（1つのADRが複数を置き換えてもよい）。どちらにするかをtaskに書くことは[taskの登録](task-registration.md)の「ADRを書くtask」が持つ。
- 置き換えは後継を`accepted`にする変更と同じ変更で行い、古いADRの`superseded_on`は後継の`accepted_on`と同じ日にする。`proposed`の後継は何も置き換えない（`supersedes`に予定のIDを書いてよいが、古いADRの状態は後継がacceptedになるまで変えない）。
- ADRの状態を変える変更は、同じ変更で[ADRの索引](../adr/README.md)の表を直す。新しい形の行は4桁の行の後ろに`accepted_on`の順で並べる。

## ADRのID

- 新しいADRのIDはそれを書くtaskのIDと枝番の`adr-t<task ID>-<N>`（Nは1から。1本だけでも`-1`）、ファイル名は`docs/adr/<YYYY-MM-DD>-t<task ID>-<N>-<slug>.md`で、日付は`accepted_on`（ADRを書くtaskではworkerが書いてacceptedにした日）にする（ADR-t598-1決定1）。`proposed`のADRは書いた日の名前にし、acceptedにする変更で`accepted_on`に合わせて名前とそこへのlinkを直す。IDはtaskのIDから決まるので、mainの次の空き番号を探さず、plannerもplan reviewも番号の割り当ての棚卸しをしない。
- 後続のtaskはADRを`ADR-t<ID>-<N>`で参照する（日付を含めないので着地前から書ける）。今どうなっているかを指すときは`docs/design/`の文書かコードを、なぜそうしたかを指すときはADRを指す（決定4、ADR-t1942-1がamends）。
- 既存の4桁のADR（0001〜、IDは`adr-NNNN`）と、登録済みのtaskが予約した4桁の番号はそのまま使い、振り直さない。新しく登録するADRのtaskは新しい形にする。
- 例外: 予約した4桁の番号がmainですでに埋まっていたら（`integrate`の`check-adr-numbers.sh`が重複で落ちてresumeされたときも同じ）、番号の衝突は人が要る理由（[ADR-0047](../adr/0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)決定41）に当たらないので、workerは`dagq ask`にせず、自分のtaskのIDの新しい形（上の1つ目）にファイル名とfrontmatterの`id`を書き直す。自分の変更の中の参照も合わせ、元の番号と新しいIDをreceiptの`summary`に書く。
- `scripts/check-adr-numbers.sh`（CIも実行する）が検出するものはscriptの冒頭のcommentが持つ。scriptの名前は登録済みのtaskのverifyが使うので変えない。

## design

`docs/design/`の書き方の規則。経緯は[ADR-t1942-1](../adr/2026-10-07-t1942-1-design-docs-in-four-layers-with-size-budgets.md)が持つ。

### 4層

- 概念: 下の「形と予算」で概念の文書とした文書と、各サブシステムの文書の冒頭。
  目的・全体の流れの図・責務と境界・不変条件を書く。
  識別子の一覧と既定値は書かない。
  人とAIが読む。
- 地図: 各design文書の本体。
  「Xを知りたいならコードのここ」という入口、コードから読めない約束と落とし穴、ADRへのリンクを書く。
  AIが読む。
- 詳細: 型・event・設定の定義のそばのRustのdoc comment。
  欄・flag・既定値の意味はここに書く。
- 経緯: ADRとgitの履歴だけ。

### 書くもの・書かないもの

- 書くのは、流れ・責務と境界・不変条件、コードから読めない約束と落とし穴、コードへの入口（module・型・関数の名前を入口として指すのはよい）、ADRへのリンク。
- 書かないのは、eventの欄・CLIのflag・既定値と閾値の数値・関数名・test名の列挙、コードを読めば分かる手順の書き写し、経緯（task・goal・requestの番号、「以前は」「〜から変えた」、文字数や件数の変遷）。
- 名前・欄・既定値の意味が要るなら、定義のそばのdoc commentに書く。
- designを書き直すのは、流れ・境界・不変条件・コードから読めない約束が変わったときと、記述が今のコードかacceptedのADRと食い違うときだけ。
  変えた名前がdesignに無いことはずれではない（[ADR-t1942-2](../adr/2026-10-07-t1942-2-document-check-and-review-in-both-directions.md)、下の「workerの文書の照合」）。
- 内容を変えたら`updated`（確かめたなら`last_verified`も）を直し、内容を変える必要がない文書には日付だけの差分を作らない（[ADR-t1428-1](../adr/2026-10-03-t1428-1-decide-the-documents-to-update-when-the-code-changes.md)）。
- 計測が読む・書くストアやビュー（表・ファイル・外の記録）を足すtaskは、同じ変更で[計測](../design/measurement.md)の「SSOTとビュー」の節に区分（SSOT・ビュー・材料）と今のアダプタを書く（[ADR-t1662-2](../adr/2026-10-04-t1662-2-measurement-stores-ssot-and-views.md)決定9）。

### 形と予算

- 1文1行: 本文は1文（「。」まで）ごとに改行する。
  gitの行単位のマージで同じ段落への別々の追記が衝突せず、grepが段落でなく1文を返すため。
  markdownは続く行を1つの段落に描く（改行は空白1つになる）。
  箇条の続きの文は字下げして次の行に書く。
- 何でも入る巨大な文書を作らない。
  予算を超えるなら、文書を分けるか、細かい事実をdoc commentへ移す。
- 予算の値:

  | 指標 | 上限 |
  | --- | --- |
  | 地図の文書の大きさ | 30,720 byte（30 KiB） |
  | 概念の文書の大きさ | 16,384 byte（16 KiB） |
  | 1行の長さ | 1,024 byte |
  | 本文のtask番号 | 0個 |

- 概念の文書は`docs/design/overview.md`と`docs/design/supervisor-lifecycle.md`の2つで、ここに挙げた一覧だけで決める。
  残りの`docs/design/**/*.md`（`docs/design/README.md`を含む）は全て地図の文書とする。
  概念の文書を足すか外すときは、同じ変更でこの一覧と検査のscriptを直す。
- 決め方（2026-10-07の分布: 97文書・約3.0MB、中央値は約17KB、30 KiB超が29文書、1行が1,000 byte超の行が715行・2,000 byte超が197行）:
  - 地図の30 KiBは、1文書を1回の読みで丸ごと読める大きさ（約1万token）で、今の文書の約3分の2が収まる。
  - 概念の16 KiBは、人が通して読む文書として地図の約半分にした。
    `supervisor-lifecycle.md`（約15KB）は収まり、`overview.md`（約30KB）は書き直す。
  - 1行の1,024 byteは、1文1行で日本語の約340字で、これを超える1文は分ける。
  - AGENTS.mdの上限（下の「AGENTS.md」）と同じく行数では測らずbyteで測り、今の値に余裕を足すのではなく、書き直しの後に収まるべき大きさで決めた。
- 上限を超える既存の文書は、検査のscriptが持つ許可の一覧に、文書ごとに超えている指標の今の値（大きさのbyte数・最も長い行のbyte数・task番号の数）を書き、その値を上限として増えないように止める。
  予算内に入った文書の行は同じ変更で一覧から消し、検査は予算内なのに一覧にある行も誤りにする。
  一覧に文書や値を足して上限を緩めない。
- 予算の値を変えるのは、書き直しの後も概念と地図の形のままでは収まらない文書が続くときだけで、同じ変更でこの節の値と決め方、検査のscriptを直す。

### 検査の境界

検査のscriptは次の境界で測り、判断を足さない。

- 対象: `docs/design/`の下の全ての`*.md`（深さを問わない）。
- frontmatter: 1行目が`---`だけの行（行末の空白と`\r`を許さない）なら、そこから次の`---`だけの行までの組（両方の`---`の行と改行を含む）を除き、大きさ・行の長さ・task番号のどれにも数えない。
  閉じる`---`が無ければfrontmatterは無いものとしてファイル全体を測る。
- 大きさ: frontmatterを除いた残りのbyte数（改行を含む）。
- 行の長さ: frontmatterを除いた各行のbyte数で、行末の改行（`\n`）は数えない。
  文字数でなくbyteで測る（`scripts/check-agents-md-size.sh`と同じ単位）。
- fenced code block: 行頭の空白を除いて`` ``` ``で始まる行が開き、次に同じく`` ``` ``で始まる行が（info stringや長さを問わず）閉じる（閉じなければファイルの終わりまで）。
  `~~~`はfenceとして扱わず、字下げだけのcode blockも区別しない（どちらも今の`docs/design/`に無い）。
  開きと閉じの行を含めてblockの中は、大きさと行の長さには数え、task番号には数えない。
- task番号: fenced code blockの外の各行で、まずinline codeを除き（行の左から、n個続く`` ` ``の並びを開きとし、同じ行で次にちょうどn個続く並びまでを除く。閉じる並びが無ければその`` ` ``は文字として残す）、次にmarkdownのリンクの行き先（`](`から次の`)`まで、と行頭の`[名前]: `の後ろ）を除いた残りを、次の拡張正規表現で探す。
  - `(^|[^A-Za-z0-9_])[Tt][Aa][Ss][Kk][Ss]? ?#?[0-9]+`（例: `task 1400`・`Task 1400`・`tasks 1400`・`task#1400`）
  - `タスク ?#?[0-9]+`
  - 当たりごとに判定し、数字の直後が`件`・`つ`・`個`・`本`・`回`の当たりだけを件数として数えない（例: `task 3件`）。同じ行の他の当たりは数える。
  - `goal`・`request`の番号は機械では数えず、reviewが経緯の混入として指摘する（上の「書かないもの」）。
  - リンクの文言（`[`と`]`の間）は除かずに数える。
  - ADRのID（`ADR-t<ID>-<N>`・`adr-t<ID>-<N>`）とADRのファイル名の`t<ID>`は`task`の語を含まないので当たらず、除くための規則を足さない。
- 出力: 超えたものごとにファイル（task番号と行の長さは行番号と測った値も）と上限を出して落ちる。

## plans

- ステップの状態が変わったら`docs/plans/current.md`を更新する。
- 計画は実際の依存関係と完了条件に合わせて更新する。
- 進行中の計画は`active`、完了した計画は`completed`に更新し、消さずに履歴として残す（状態の値は[frontmatter仕様](../frontmatter.md)の「Type-specific fields」）。

## frontmatter

frontmatterは[frontmatter仕様](../frontmatter.md)に従う。

`created`・`updated`・`last_verified`の行は`YYYY-MM-DD`の日付だけを書き、行末にどのtaskが何を変えたかのコメント（`# task N`など）を書かない。どのtaskが変えたかはgitの履歴（着地のcommitの`Dagq-Task` trailer）が持つ。既存の文書（ADRも含む。決定と本文に触れないのでappend-onlyに当たらない）の日付の行にコメントがあれば、日付の値を変えずにコメントだけを消してよい。同じ文書を変える2つのtaskが日付の行で衝突しないためで、経緯は[ADR-t1854-1](../adr/2026-10-07-t1854-1-frontmatter-date-lines-hold-only-the-date.md)。`scripts/check-frontmatter-dates.sh`はfrontmatterの日付の行（frontmatter仕様が持たせるもの）にコメントがあれば、ファイルと行を出して落ちる検査で、CIが実行する（docsを変えるtaskのverifyへの足し方は[taskの登録](task-registration.md)の「推奨の組み合わせ」）。

`scripts/check-doc-frontmatter.sh`はADR以外の`docs/`の文書のfrontmatter（必須のkey、designの`last_verified`、typeとstatusの値、idの形と重複）を、`scripts/check-doc-links.sh`は`docs/`とrootの`.md`の相対リンクが実在のファイルかdirectoryを指すことを検査し、違反のファイルと理由を出して落ちる。どちらもCIが実行し、`docs/`を変えるtaskのverifyに付ける（[taskの登録](task-registration.md)の「推奨の組み合わせ」）。検査の範囲と例外（ADRのfrontmatterを見ないこと、append-onlyのADRから削除済みの`docs/journal/`へのリンク）はscriptの冒頭のcommentが持つ。

## 権限の表を写す文書

pluginの`dagq-planner`の`SKILL.md`の「Where your authority ends」と`dagq`の`reference/authority.md`の「What each role is refused」は、[Security](../design/security.md)の「actor × capability」と[Authorization](../design/authorization.md)の「Policy」の表と一致させる。表を変える変更は同じ変更でこれらを直す。

## pluginの汎用性

pluginとrepositoryの規則・値・経緯の受け持ちは[ADR-t1453-2](../adr/2026-10-03-t1453-2-ownership-of-agents-md-plugin-development-docs-and-config.md)決定1の表、runtimeとの境界は決定5。検査する記述の一覧は[棚卸し](../plans/agents-slim-inventory.md)「4. pluginの固有の記述の項目」。固有の印（このrepositoryのtest・coverageのコマンド、`tests/it`、task・goal・askの番号の逸話、日付、「fixed binary」、`docs/design/`・`docs/plans/`のpathと「design docs」への参照、pluginのdirectoryを出る相対リンクなど）の戻りは`tests/plugin.rs`の`no_skill_carries_this_repository_s_rules`が検査し、印の語の一覧はそのtestが持つ。

## workerの文書の照合

workerはreceiptの前に、受け入れ条件の対応づけ（[手元の検証](local-checks.md)の「受け入れ条件の対応づけ」）に続けて、workerのpromptが指示する文書の照合（仕組みは[prompt](../design/supervisor-lifecycle/prompt.md)の「文書の照合」、[ADR-t1428-1](../adr/2026-10-03-t1428-1-decide-the-documents-to-update-when-the-code-changes.md)・[ADR-t1942-2](../adr/2026-10-07-t1942-2-document-check-and-review-in-both-directions.md)）を行う。このrepositoryで差分と照合する文書は、taskが名指す文書と、変えた挙動を説明する`docs/design/`・pluginのskillとreference・AGENTS.md・`docs/development/`・ADRの索引で、`summary`に更新したpath・節か不要の理由を書く。taskのpathsの外のずれは`docs_drift`のfollow_upにする。

- 候補は、変えた名前（コマンド・flag・設定のkey・役割・fileのpath）で`docs/design/`・`docs/development/`・`plugins/claude-dagq/skills`の`SKILL.md`と`reference/`・AGENTS.md・ADRの索引（`docs/adr/README.md`）を探して拾い、`summary`に探した名前を書く（ADR-t1942-2決定1・3。探す手段は問わない）。
- 見つけた`docs/design/`の文書を書き直すのは、流れ・境界・不変条件・コードから読めない約束が変わったときと、記述が差分と食い違うときだけ（ADR-t1942-2決定2）。
  変えた名前がdesignに無いことはずれではなく、名前・欄・既定値を書き足さない。
  その意味は定義のそばのdoc commentに書く。
  直すときは経緯（task番号・「以前は」・変遷）を書き込まず、上の「design」の形と予算に従う。
- runのreviewは、欠けに加えて差分が文書に入れたコードの書き写しと経緯の混入を指摘する（ADR-t1942-2決定4）。
  このrepositoryでそれとsummaryの探した名前を確かめるreviewのdesign-consistencyのsubagentが当たる変更の範囲は、`dagq.toml`の`[review.subagents.design-consistency] paths`が持つ。
- 見落としやすい型: 役割・権限の説明は複数の文書に散らばる（task 1400: `docs/design/overview.md`の用語集の役割の行・`docs/design/authorization.md`・`docs/design/supervisor-lifecycle/roles.md`・`triage.md`）。testやscriptが何を検査するかを列挙する節も古くなりやすい（task 1462: `docs/design/plugin-integration.md`「読み込みと検証」）。

## AGENTS.md

- AGENTS.mdは概要・役割と変更の範囲ごとの読む案内・本番queueと開発環境の境界・開始時の短い制約・検証と文書の規則への参照だけを持つ（[ADR-t1453-2](../adr/2026-10-03-t1453-2-ownership-of-agents-md-plugin-development-docs-and-config.md)決定1・3）。規則の本文はAGENTS.mdに足さず、その正本（`docs/development/`の該当の文書、実装は`docs/design/`、経緯はADRと`docs/plans/`、設定値は`dagq.toml`）に書き、AGENTS.mdには読む案内が要るときだけ参照の1行を足す。
- 上限は9,216 byte（9 KiB）。`scripts/check-agents-md-size.sh`がAGENTS.mdのbyte数（`wc -c`）を測り、上限を超えればbyte数と上限を出してexit 1にし、CIも実行する。行数では測らない（日本語の1行は長く、行数が大きさを表さない。ADR-t1453-2決定6）。
- 決め方: 組み直した後（task 1461、2026-10-04）のAGENTS.mdは8,340 byte。余裕は876 byte（約10%）で、案内の参照の行を2〜3行足せるが、規則の本文の節を足せば超える大きさにした。候補の4〜8 KiBは、どれも今の案内（役割と変更の範囲ごとに読む文書の名指し）が入らない（8 KiB＝8,192 byteでも148 byte足りない）。
- 上限を上げるのは、案内（読む文書の名指し）が増えて規則の本文を正本へ移してもなお超えるときだけで、同じ変更でこの節の値と決め方、scriptの`limit`を直す。

## commit

- メッセージは`feat:` / `fix:` / `docs:` / `test:`の接頭辞、本文は何をなぜ変えたか。
- run sessionが自分のrun branchにcommitし、mainへの着地とpushを`dagq integrate`だけが行うこと（1 task 1 squash commit、メッセージはtaskのtitleとreceiptの`summary`からruntimeが作る）は[integrate](../design/supervisor-lifecycle/integrate.md)とworkerのpromptが持つ。`push_failed`のattentionの手当てはpluginの`dagq-recover`の`reference/review-by-hand.md`の「A failed push」が持つ。

---
id: development-documents
type: development
title: このrepositoryの文書の規則（判断の記録・ADR・design・plans・deploy・frontmatter・workerの文書の照合・AGENTS.md・commit）
status: current
created: 2026-10-03
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
  - adr-t5395-1
  - adr-t1942-2
  - adr-t2352-1
  - adr-t3990-5
  - docs-frontmatter
  - development-task-registration
---

# このrepositoryの文書の規則

`docs/`の文書とcommitを書く・変えるときの今の規則。読むのは、ADR・design・plans・deployを書くかcommitするworker（AGENTS.mdの「### worker」から辿る）、ADRを書くtaskを登録するplannerとそれを見るplan review（[taskの登録](task-registration.md)の「ADRを書くtask」から辿る）、人。ADRの書き方の規則はこの文書が正本で、[文書の案内](../README.md)・[ADRの案内](../adr/README.md)・[frontmatter仕様](../frontmatter.md)はここを指す。frontmatterの欄・ADRの状態の意味・注記の書式はfrontmatter仕様、経緯はADR（[ADR-t598-1](../adr/2026-09-26-t598-1-adr-id-is-task-id-small-adrs-and-design-holds-current-state.md)ほか）が持つ。

## 判断の記録

人の判断はADR・Goalの記述・`Task.context`・receiptの`summary`に残す（作業記録のジャーナルは[ADR-0036](../adr/0036-delete-frozen-work-records.md)で削除した）。

## ADR

- 将来の実装や運用に大きな影響を与える決定は`docs/adr/`にADRを追加する。アーキテクチャ全体に影響し手戻りが大きい決定は、実装より先にADRを作る。新しいADRは[template](../adr/0000-template.md)をコピーして作る。
- 既存のADRは書き換えない（append-only）。後から変えてよいのは`status`・`accepted_on`・`superseded_by`・`superseded_on`・`deprecated_on`・`amended_by`とH1直後の注記1行だけで、これらだけの変更に新しいADRは要らず、`updated`も動かさない。frontmatterの日付の行の行末のコメントは欄の値でも本文でもないので、値を変えずにコメントだけを消すのはこの制限に当たらない（下の「frontmatter」、[ADR-t1854-1](../adr/2026-10-07-t1854-1-frontmatter-date-lines-hold-only-the-date.md)決定3）。`supersedes`と`amends`は置き換え・amendsの本文と一緒に書く。決定を足す・変える・除くのは、下の置き換えか`amends`の新しいADRで行う。
- append-onlyのADRがリンクする文書を消すときは、ADRを直さず、文書を消すtaskより先にmainに着地するtaskで`scripts/check-doc-links.sh`の冒頭の削除した文書の一覧にpathと移管先を状態を書かない移行中の行として足し、文書を消すtaskはそのtaskに依存させる（ADRからのリンクだけを読み飛ばし、ADR以外の文書からのリンクは落ちる）。
  runのプログラムのreviewはmainのscriptを読むので、文書を消すtaskのcommitで足した行はそのreviewで効かない（登録は[taskの登録](task-registration.md)の「ADRがリンクする文書を消すtask」）。
  その行を確定にするのは、文書を消したgoalの最後のtaskである（行の書式と状態の意味は下の「frontmatter」とscriptの冒頭）。
- どのADRが今の決定か（`accepted`・`amended_by`・`superseded_by`の辿り方・`deprecated`）は[frontmatter仕様](../frontmatter.md)の「ADR fields」の状態と欄の表が持つ。
- 小さなADR: 1 ADRに決定1つ（密に結びついた数個まで）、本文はおおむね100行以内。書くのは変えるのに人の判断が要るもの（問題と文脈、方針・原則・境界・不変条件、退けた案、結果。目安は「これを変えるとき人に聞くか」）で、eventの種類と欄・CLIのflagの綴り・JSONの形・既定値や閾値の数値・関数やmoduleやファイルの名前・migrationの番号・testの名前はコード（型・名前・test）が持ち、決定1の(a)(b)(c)に当たることは`docs/design/`に書く（ADR-t598-1決定2・3。決定3・4は[ADR-t5395-1](../adr/2026-11-20-t5395-1-design-docs-hold-what-code-cannot-show.md)がamends。下の「design」）。
- 置き換えか`amends`か: 元のADRの決定の数と変える範囲で決め、IDの形（4桁か新しい形か）では決めない（[ADR-t1091-1](../adr/2026-09-30-t1091-1-amend-or-replace-by-number-of-decisions.md)）。番号付きの決定を複数持つADR（4桁でも新しい形でも。0047・0073・t813-2など）の一部の決定を変えるときは、小さな新しいADRの`amends`に変える決定（例: `adr-0047 decision 24`）を書き、元のADRの`amended_by`にそのIDを足し、今の姿を直す必要があれば、同じ変更でコードか、下の「design」の(a)(b)(c)に当たる設計文書に反映する。決定が1つのADRを変えるときと、決定の大半を変えるときは、新しいADRで丸ごと置き換え、まだ有効な古い決定を書き直して引き継ぎ、古いADRを丸ごと`superseded`にする（1つのADRが複数を置き換えてもよい）。どちらにするかをtaskに書くことは[taskの登録](task-registration.md)の「ADRを書くtask」が持つ。
- 置き換えは後継を`accepted`にする変更と同じ変更で行い、古いADRの`superseded_on`は後継の`accepted_on`と同じ日にする。`proposed`の後継は何も置き換えない（`supersedes`に予定のIDを書いてよいが、古いADRの状態は後継がacceptedになるまで変えない）。
- ADRの索引は各ADRのfrontmatterから`scripts/adr-index.sh`が生成するgitignoreした`docs/adr/INDEX.md`で、commitしない（[ADR-t1967-1](../adr/2026-10-07-t1967-1-adr-index-generated-from-frontmatter.md)）。状態と後継の正本はfrontmatterの欄（`status`・`accepted_on`・`superseded_by`・`superseded_on`・`deprecated_on`）で、ADRを足す・状態を変える変更は欄だけを直し、索引も[ADRの案内](../adr/README.md)も書き換えない。
- 索引を読む前に、無ければ`sh scripts/adr-index.sh`で作る（あれば何もしない。`--force`で作り直し、`--stdout`はファイルに書かず出す。CIは`--stdout`で全てのADRを読めることを確かめる）。表は有効なADRと置き換え・廃止のADRの2つで、4桁の行の後ろに`accepted_on`の順で並ぶ。有効なADRの表は`accepted_on`の右に`amended_by`も示す。
- cloneの後に1回`git config core.hooksPath .githooks`を打つ。`.githooks/post-checkout`がcheckoutと`git worktree add`（dagqのrunのworktreeも）のたびに、`.githooks/post-merge`がmerge（着地でmain checkoutを進めるfast-forwardも）のたびに`sh scripts/adr-index.sh --force`を打ち、失敗してもgitの操作を止めない。同じ設定で`.githooks/commit-msg`がcommitのたびに接頭辞を検査する（prekとcommittedの無いhostでは通すだけ。下の「commit」）。containerで動くrunの着地では、制御側のgitがhookを走らせない（[ADR-t2179-1](../adr/2026-10-12-t2179-1-control-side-runs-no-git-in-a-container-runs-clone-and-lands-in-its-own-worktree.md)決定8）ので、着地用のworktreeの作成でもmain checkoutを進めるfast-forwardでも索引は作り直されない。hookが無い環境や、自分の変更でADRを足した・状態を変えたworktree、containerのrunが着地した後のmain checkoutでは`--force`で作り直してから読む。このrepositoryのhostへの設定は人かinboxが行い、workerはhostのgitの設定を変えない。

## ADRのID

- 新しいADRのIDはそれを書くtaskのIDと枝番の`adr-t<task ID>-<N>`（Nは1から。1本だけでも`-1`）、ファイル名は`docs/adr/<YYYY-MM-DD>-t<task ID>-<N>-<slug>.md`で、日付は`accepted_on`（ADRを書くtaskではworkerが書いてacceptedにした日）にする（ADR-t598-1決定1）。`proposed`のADRは書いた日の名前にし、acceptedにする変更で`accepted_on`に合わせて名前とそこへのlinkを直す。IDはtaskのIDから決まるので、mainの次の空き番号を探さず、plannerもplan reviewも番号の割り当ての棚卸しをしない。
- 後続のtaskはADRを`ADR-t<ID>-<N>`で参照する（日付を含めないので着地前から書ける）。今どうなっているかを指すときは`docs/design/`の文書かコードを、なぜそうしたかを指すときはADRを指す（決定4、ADR-t5395-1がamends）。
- 既存の4桁のADR（0001〜、IDは`adr-NNNN`）と、登録済みのtaskが予約した4桁の番号はそのまま使い、振り直さない。新しく登録するADRのtaskは新しい形にする。
- 例外: 予約した4桁の番号がmainですでに埋まっていたら（`integrate`の`check-adr-numbers.sh`が重複で落ちてresumeされたときも同じ）、番号の衝突は人が要る理由（[ADR-0047](../adr/0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)決定41）に当たらないので、workerは`dagq ask`にせず、自分のtaskのIDの新しい形（上の1つ目）にファイル名とfrontmatterの`id`を書き直す。自分の変更の中の参照も合わせ、元の番号と新しいIDをreceiptの`summary`に書く。
- `scripts/check-adr-numbers.sh`（CIと、差分がそのpathに当たるrunの[プログラムのreview](../design/supervisor-lifecycle/review-programs.md)も実行する。どのscriptを挙げるかは`dagq.toml`の`[review.programs]`）が検出するものはscriptの冒頭のcommentが持つ。scriptの名前は登録済みのtaskのverifyが使うので変えない。

## design

迷ったら書かない。
`docs/design/`に書くのはコードを読んでもわからないことだけで、経緯は[ADR-t5395-1](../adr/2026-11-20-t5395-1-design-docs-hold-what-code-cannot-show.md)が持つ。

### 書くもの・書かないもの

- 書くものは決定1の次の三つだけとする。
  - (a) 複数の集約や係をまたぐ順序・一貫性・受け渡し・途中の失敗。
  - (b) 複数のBCをまたぐ受け渡しと持ち主（queue・supervisor・service・workerの境目）。
  - (c) コードからわからないインフラ（processの配置、disk上のファイルと置き場、GitHub・Claude・Codex・podmanなど外部との境目、更新と引き継ぎの手順、壊れ方と復旧）。
- 三つのいずれにも当たらない単位内部の状態の移り方・欄・規則・関数名・処理の手順や、内部の部品どうしの順序・受け渡しは書かない。
  経緯はADRとgitの履歴だけに置く。
- DS-1: 差分が`docs/design`に足した・変えた行が、(a)(b)(c)のどれにも当たらず、1つのコードの単位の中で閉じる話を書いた。
  1つの単位は1つの集約か、集約に属さずcommand・job・役割から直接呼ばれる入口と、自分の状態か出力を持つ1つの係とする。
  storeの行から帳簿を作り直す係や、統計・指標・報告をそれぞれ作る係がこれに当たる。
  その単位からだけ呼ぶhelperのmodule・関数・型は同じ単位の部品とし、部品がいくつ登場しても内部の順序や受け渡しはDS-1に当たる。
  (a)は2つ以上の単位（集約どうし・集約と係・係どうし）をまたぐ順序・一貫性・受け渡し・途中の失敗だけを指す。
  集約とBCの一覧（`aggregates.md`）は境目を確かめる補助で、一覧の有無で判じ方を変えず、一覧に無い係も1つの単位とする。
- 型・名前・enum・構造でコードを宣言的にし、コードから読める事実はdoc commentにも書き写さず、designから消した記述もそこへ移さない。
- designに行を足すのは(a)(b)(c)に当たる変更があるときだけで、既存の記述を直すのは今のコードかacceptedのADRと食い違うときだけとする。
  1つのコードの単位で閉じる細部はコードの型・名前・testが持つ。
  変えた名前がdesignに無いことはずれではない（[ADR-t1942-2](../adr/2026-10-07-t1942-2-document-check-and-review-in-both-directions.md)、下の「workerの文書の照合」）。
- 直し対象の印は、今のコードかacceptedのADRと食い違う記述の直しを後のtaskに回すときだけ使い、記述の追加の代わりには使わない。
  対象の節の見出しの直後に、``> 直し対象（正: `<コードの入口>`、直す: `task <ID>`）。``の形で1文1行の引用を置く。
  見出しの無い箇所では、対象の箇条書きか図のfenced code blockの直前に前後を空行で区切って置き、箇条書きや図の中身は変えない。
  印は短い指し先にとどめ、食い違う文と今の振る舞いは印を入れたtaskのreceiptに節ごとに1行で書き、直すtaskはそのreceiptを読む。
  印は1節に1つとし、同じ直すtaskの印が既にある節に後のtaskが印を足さない。
  印を置くと`sh scripts/check-design-docs.sh`の上限を超えるときは、食い違う句や節だけを削って印の分を空け、同じ文と同じ節に今も有効な約束とコードへの入口を残す。
  文を丸ごと消せるのはその文の全部が食い違うときだけで、食い違う部分だけを削っても収まらなければ本文に手を加えず印を省く（上限のために今も有効な約束と入口を消さない）。
  印とこの削除以外の本文は変えない。
  task番号は直すtaskが消す一時の指し先としてinline codeで書く（task番号の予算に数えない）。
  直すtaskが印を消す。
  印の直すtaskはverifyに`sh scripts/check-design-docs.sh --fixed <自分の ID>`を持ち（印の検査が着地した後に流れる直すtaskは全て）、印は直すtaskの着地まで残ってよく、通常の検査は印が直すtaskの番号を持つことだけを見る。
  印の付いた節を他のtaskが削るときは、印の行と印が指す食い違いの文を残し、直すtaskに任せる。
- designは`updated`・`last_verified`を持たず（下の「frontmatter」）、内容を変える必要がない文書に差分を作らない（[ADR-t1428-1](../adr/2026-10-03-t1428-1-decide-the-documents-to-update-when-the-code-changes.md)）。
- 「変更と一緒にdesignも直す」（ADR-t1428-1）は記述を足す義務でなく、差分とdesignの食い違いを無くすことで、場面・分岐ごとの処理や関数・欄はコードの型・名前・testが持ち、doc commentにも書き写さない（登録の側は[taskの登録](task-registration.md)の「文書の変更を求めるtask」）。
- 計測の層の論理ストアか、runtime・CI・`scripts/`が続けて書き他の仕組みが読む共有の記録を足す・変えるtaskは、同じ変更で[計測](../design/measurement.md)の「SSOTとビュー」の節に区分（SSOT・ビュー・材料）と今のアダプタを書く（[ADR-t2065-1](../adr/2026-10-08-t2065-1-measurement-ssot-table-holds-stores-and-shared-records-only.md)、元は[ADR-t1662-2](../adr/2026-10-04-t1662-2-measurement-stores-ssot-and-views.md)決定9）。
  `docs/plans/`の1回きりや週次の見直しの測定の出力（scriptとCSV）は設計書に足さず、そのplansの文書に区分と作り直せる範囲を書く。

### 形と予算

- 1文1行: 本文は1文（「。」まで）ごとに改行する。
  gitの行単位のマージで同じ段落への別々の追記が衝突せず、grepが段落でなく1文を返すため。
  markdownは続く行を1つの段落に描く（改行は空白1つになる）。
  箇条の続きの文は字下げして次の行に書く。
  検査のscriptは`scripts/check-design-sentences.sh`で、差分が足した・変えた行だけに下の「検査の境界」を当てはめ、2つ目の文が続く行を出して落ちる。
  既存の行は検査しないので、既存の文書の違反を一括で直さなくてよいが、変えた行は直す。
  CIとrunのreviewのプログラム（`dagq.toml`の`[review.programs]`）が流す。
- 何でも入る巨大な文書を作らない。
  予算を超えるなら、次の順に直す。
  1. 消してよいのは、本文のtask番号・goal番号と経緯、同じ文書か他の文書に同じ事実がある重複、コードから読める列挙（欄・flag・既定値・test名・関数やmoduleの名前の並び）だけにする。
  2. コードから読める細かい事実は消す（コードの型・名前・testが持ち、doc commentへ移さない）。
  3. それでも収まらなければ文書を分ける。
     新しい文書は地図の文書の予算に従い、元の文書から相対リンクで辿れるようにする。
  4. 地図として要る行（知りたいこと→コードの入口、コードから読めない約束と落とし穴、規則、他に記述の無い事実）は予算のために消さない。
     消すときは、同じ事実が残る場所（他の文書か既存のdoc comment）を確かめる。
     予算に収めるために、差分と関係の無い既存の地図の事実を、同じ事実が他の文書にも既存のdoc commentにも残らないのに消すことを禁じる。
     全体のreviewが差分の消した行で判じる（下の「文書を理由に差し戻す条件」の(v)）。
- 予算の値:

  | 指標 | 上限 |
  | --- | --- |
  | 地図の文書の大きさ | 30,720 byte（30 KiB） |
  | 層を直した地図の文書の大きさ | 20,480 byte（20 KiB） |
  | 概念の文書の大きさ | 16,384 byte（16 KiB） |
  | 1行の長さ | 1,024 byte |
  | 本文のtask番号 | 0個 |

- 概念の文書は`docs/design/overview.md`と`docs/design/supervisor-lifecycle.md`の2つで、ここに挙げた一覧だけで決める。
  残りの`docs/design/**/*.md`（`docs/design/README.md`を含む）は全て地図の文書とする。
  概念の文書を足すか外すときは、同じ変更でこの一覧と検査のscriptを直す。
- 決め方（2026-10-07の分布: 97文書・約3.0MB、中央値は約17KB、30 KiB超が29文書、1行が1,000 byte超の行が715行・2,000 byte超が197行）:
  - 地図の30 KiBは、1文書を1回の読みで丸ごと読める大きさ（約1万token）で、今の文書の約3分の2が収まる。
  - 層を直した地図の20 KiBは、2026-11-01の分布で地図の文書の中央値が約17KBで、measurement.mdから場面ごとの列挙の節（約11KB）をdoc commentへ移すと約20KBになることから決めた。
    文書ごとの今の大きさでなく一律にしたのは、上限に余白を残し、他のtaskの追記で事実を削らないため。
  - 概念の16 KiBは、人が通して読む文書として地図の約半分にした。
    `supervisor-lifecycle.md`（約15KB）は収まり、`overview.md`（約30KB）は書き直す。
  - 1行の1,024 byteは、1文1行で日本語の約340字で、これを超える1文は分ける。
  - AGENTS.mdの上限（下の「AGENTS.md」）と同じく行数では測らずbyteで測り、今の値に余裕を足すのではなく、書き直しの後に収まるべき大きさで決めた。
- 検査のscriptは`scripts/check-design-docs.sh`で、この節の予算の値と下の「検査の境界」を当てはめ、超えた文書・行・値と上限を出して落ちる。
  CIとrunのプログラムのreview（`dagq.toml`の`[review.programs]`に挙げたとき）も実行し、`docs/design/`を変えうるtaskのverifyにも付ける（[taskの登録](task-registration.md)の「推奨の組み合わせ」）。
- 上限を超える既存の文書は、許可の一覧`.config/design-docs-allow.toml`に、文書ごとに超えている指標の今の値（大きさのbyte数・最も長い行のbyte数・task番号の数）を書き、その値を上限として増えないように止める。
  一覧の値は下げるか消すだけにする。
  新しい文書を一覧に足さず、文書を増やすために値を引き上げない（検査は一覧をbaseの一覧と比べ、足した行・値と上げた値を誤りにする）。
  書き直して予算内に入った文書の行（または値）は同じ変更で一覧から外し、検査は予算内なのに一覧にある行と、無い文書の行も誤りにする。
- 層を直した地図の文書は、層を直した一覧（`.config/design-docs-tight.toml`と`.config/design-docs-tight.d/*.toml`の項目を合わせたもの）に挙げ、地図の30 KiBの代わりに層を直した地図の20 KiBを大きさの上限として当てる。
  文書の層を直すtaskは、同じ変更でその文書の項目を足すtaskのファイル`.config/design-docs-tight.d/t<task ID>.toml`に足す（別々のtaskの追加が同じファイルに触れないため）。
  一覧は足すだけにし、項目を外すこととpathを変えることを誤りにする（検査は一覧をbaseの一覧と比べ、2つの置き場の間で項目を移すことは外したことにしない）。
  同じpathを2か所に挙げることも誤りにする。
  例外は、消した文書の項目を外すことと、名前を変えた文書のpathを同じ変更で新しいpathに置き換えることだけにする。
  許可の一覧にある文書と概念の文書は挙げない。
  全ての地図の文書が一覧に入ったら、地図の予算を20 KiBに下げて一覧を退役する。
- 予算の値を変えるのは、書き直しの後も概念と地図の形のままでは収まらない文書が続くときだけで、同じ変更でこの節の値と決め方、検査のscriptを直す。

### 検査の境界

予算の検査のscript（`scripts/check-design-docs.sh`）は次の境界で測り、判断を足さない。

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
- 大きさの上限: 概念の文書は概念の予算、層を直した一覧（`.config/design-docs-tight.toml`と`.config/design-docs-tight.d/*.toml`の両方）に挙げた地図の文書は層を直した地図の予算、残りの地図の文書は地図の予算で測る。
- 一覧とbaseの比べ: baseはmainとのmerge base（`--base`で変える）で、baseに一覧が無ければ比べない。
  層を直した一覧は2つの置き場の和どうしを比べ、baseの和の項目が今の和に無いとき、文書が残っていれば誤りにし、文書が無ければ外してよい。
  ただしgitがbaseからwork treeへの名前の変更（`git diff -M`）と見つけた文書は、新しいpathが一覧に無ければ誤りにする。
- 出力: 超えたものごとにファイル（task番号と行の長さは行番号と測った値も）と上限を出して落ちる。
  層を直した一覧の誤り（外した・pathを変えた項目、無い文書・概念の文書・許可の一覧にもある文書の項目）は、項目のpathと理由を出して落ちる。
  同じpathが2か所にある層を直した一覧は、書式の誤りと同じく読めないものとして落ちる。

1文1行の検査のscript（`scripts/check-design-sentences.sh`）は次の境界で測り、判断を足さない。
frontmatterとfenced code blockとinline codeの見分け方は上と同じ。

- 対象: `docs/design/`の下の`*.md`で、base（`--base`、既定は上のscriptと同じくmainとのmerge baseで、mainもorigin/mainも無ければHEAD）から作業treeまでの差分が足した・変えた行。
  gitがまだ追っていないファイルは全ての行を足した行とする。
  名前を変えた文書はgitの設定に依らず名前の変更として見るので、変えていない行は検査しない（gitがまだ追っていない先への移動は全ての行を検査する）。
  gitが引用する名前（`"`・`\`・制御文字を含む名前）は元の名前に戻して検査し、改行を含む名前は検査できないのでexit 2で止まる（下の`--all`も同じ）。
  `--all`は全ての文書の全ての行に当て、境界の当たり方を見るためのもので、CIとreviewは使わない。
- 除く行: frontmatter、fenced code blockの中と開き・閉じの行、表の行（行頭の空白を除いて`|`で始まる行）、見出し（同じく`#`で始まる行）。
  箇条（`-`・`1.`）・字下げした続きの行・引用（`>`）は除かない。
- 違反: inline codeを除いた行で、括弧と引用の外の「。」の後に、空白（全角の空白と`\r`を含む）・閉じの括弧と引用・強調の`*`と`_`のほかの文字が同じ行に続くもの。
  「？」「！」や英文の`.`は文の終わりとしない。
- 括弧と引用: 開きは`（`・`「`・`『`・`【`・`〔`・`［`・`｛`・`〈`・`《`・`“`・`‘`・`(`・`[`、閉じはそれぞれの対で、種類を問わず開きで1つ深く、閉じで1つ浅くする（0より浅くしない）。
  深さが0でない間の「。」は文の終わりとしないので、「…。」・（…。）の閉じの直前の「。」とリンクの文言の中の「。」は違反にならない。
  閉じない括弧の後の「。」は行の終わりまで数えない。
- 出力: 違反の行ごとにファイル・行番号・行を出して落ちる（exit 1）。

識別子の並びと列挙の検査のscript（`scripts/check-design-identifiers.sh`）は、上の「書くもの・書かないもの」の書かないもの（1つのコードの単位で閉じる細部）のうち機械で安く見分けられる形だけを拾う。
拾い漏れはreviewの`design-layers`のagentが見るので、閾値は誤検出を0にする側に寄せ、値と決め方はscriptの冒頭が持つ。
CIとrunのreviewのプログラムが流し、[taskの登録](task-registration.md)の「推奨の組み合わせ」のverifyには足さない（どの`docs/design/`の差分にも両方が流れるので、既存のtaskの登録を変えずに済む）。

- 対象・除く行: 1文1行の検査のscriptと同じ（base・gitがまだ追っていないファイル・名前の変更・gitが引用する名前・`--all`の扱いも同じ）。
  inline codeの見分け方は上と同じで、閉じない`` ` ``は数えない。
- 違反（A）: 対象の行で、inline codeの数が閾値以上のもの（識別子の並び）。
- 違反（B）: 箇条（行頭の空白を除いて`-`か`1.`のような数字と`.`で始まる行）が閾値の本数以上続く塊で、どの箇条も対象の行で、その行か字下げした続きの行にinline codeを含むもの（場面ごと・識別子ごとの列挙）。
  入れ子の箇条も同じ塊に数える。
  inline codeの無い箇条・対象でない箇条（既にある箇条）・空行・字下げの無い箇条でない行・除く行が塊を切るので、既にある箇条は塊に数えず、既にある列挙に足した箇条は足した本数だけで数える。
  このため既にある場面ごとの列挙への1〜2本の追記は拾わず、それは`design-layers`のagentが見る（devのケース`ab08fe0a1-hold-appended-to-scene-lists`・`ab08fe0a1-hold-appended-to-claim-scene-list`）。
- 出力: 違反ごと（（B）は塊の各箇条の行。（A）と（B）の両方に当たる行は2回）にファイル・行番号・違反の種類・行を出して落ちる（exit 1）。

### 機械の検査

機械の検査を先に当て、検査の本文はscriptとtestのコメントに置く。

- 識別子と実在のunit test（予定）は、本文のinline codeにあるコードの識別子（関数名・型やeventの欄・enumの値・constの名前）と、実在しないmoduleのpath・集約・BC・型の名前への参照を落とす。
- 大きさの検査（`scripts/check-design-docs.sh`）は文書ごとの本文のbyte数と1行の長さの上限を超えたもの、本文でtaskかタスクに番号が続く形を落とす（frontmatter・fenced code block・inline code・リンクの行き先（`](`から次の`)`までと、行頭の参照リンク定義`[名前]: `の後ろ）を除き、リンクの表示文言は数え、数字の直後が件・つ・個・本・回の形は数えない）。
- 1文1行の検査（`scripts/check-design-sentences.sh`）は、差分が足した・変えた行で1行に複数の文が続くものを落とす。
- reviewの前ふるい（`scripts/check-design-identifiers.sh`）は、差分が足した・変えた行でinline codeが閾値以上の行（違反（A））と、行自体か字下げした続きの行にinline codeを含む足した・変えた箇条が閾値以上続く塊（違反（B））を落とす。
- 直し対象の印の検査は、直すtaskの番号が無い印を落とす。

`design-layers`のDS-1は、差分が足した・変えた行のうち1つのコードの単位で閉じる話だけを判じる。
識別子・大きさ・本文のtask番号・印・経緯・消した行はDS-1の判定外とし、予算のための削除は全体のreviewが下の(v)で判じる。
機械の検査もDS-1も拾わない経緯（番号を持たない「以前は」「〜から変えた」の文、goal・request・ADRの番号や日付・件数で書く変遷）は全体のreviewが指摘する。

### 文書を理由に差し戻す条件

コード・acceptedのADR・testを正とし、designはまたがる話の地図として読む。
runのreviewが文書を理由に差し戻すのは次の場合に限る。

- (i) 差分が変えた文書が差分・コード・acceptedのADR・testと食い違う。
- (ii) 差分がacceptedのADR・test・コードの持つ不変条件・約束・境界を破る。
- (iii) 差分が`docs/design/`に足した・変えた行が機械の検査かDS-1に当たる。
- (iv) 差分が足した・変えた行に、このrepositoryの従来の文書の規則が禁じる`docs/design/`の外の文書でのコードの書き写し、または同じ規則が当たる文書での経緯がある。
  `docs/design/`の書き写しは(iii)が受け持ち、機械の検査が拾わない経緯は全体のreviewが従来どおり指摘する。
- (v) 予算に収めるために、差分と関係の無い既存の地図の事実（コードへの入口、コードから読めない約束と落とし穴、規則、他に記述の無い事実）を`docs/design/`から消し、同じ事実が他の文書にも既存のdoc commentにも残らない。
  全体のreviewが差分の消した行を見て、削除の目的と同じ事実が残る場所を確かめる。
  予算に収める目的の無い削除や、言い換え・参照先への短縮で事実が残るもの、既存のdoc commentに同じ事実が残るものは対象外とする。
  1つの単位で閉じる記述・重複・経緯・コードから読める列挙の削除や、直し対象の印を置くために食い違う句や節を削る変更も対象外とする。

差分がdesignとだけ食い違うときは、designの直しを`docs_drift`のfollow_upにする。

plan reviewは同じ基準を使い、受け入れ条件がdesignに必須とする記述に機械の検査の境界を先に当て、該当すれば`revise`にする。
機械が拾わなくても、1つの集約か集約に属さない1つの係で閉じる話を必須にするなど、DS-1に明確に当たれば`revise`にする（識別子を使わない地の文も含む）。
DS-1に当たるか迷う記述は`revise`の理由にせず、designの記述とだけ食い違うことは`revise`・`concern`の理由にしない。

## plans

- ステップの状態が変わったら`docs/plans/current.md`を更新する。
- 計画は実際の依存関係と完了条件に合わせて更新する。
- 進行中の計画は`active`、完了した計画は`completed`に更新し、消さずに履歴として残す（状態の値は[frontmatter仕様](../frontmatter.md)の「Type-specific fields」）。

## deploy

`docs/deploy/`は、チームの版の制御側を配置する人に配る文書を置く（[ADR-t3990-5](../adr/2026-11-04-t3990-5-team-version-deploy-documents-and-reference-configurations-without-iac.md)決定1・2）。
frontmatterは`type: deploy`で、idは`deploy-<slug>`、状態の値は[frontmatter仕様](../frontmatter.md)の「Type-specific fields」が持つ。

- `docs/design/`と違い、読むのはこのrepositoryを開発する人とagentではなく配置する人で、配置するreleaseと同じtagの版を読む。
- 今の実装の地図ではない。配置する側が守ること、配られる部品の使い方、参考の構成を書き、コードの入口と内部の仕組みはdesignに任せる。
- TerraformやCloudFormationなどの特定の配置のIaCを置かない。参考の構成は文と図と要る値の表で書く。

## frontmatter

frontmatterは[frontmatter仕様](../frontmatter.md)に従う。

ADR以外の文書（design・development・plans・deploy、`docs/README.md`・`docs/frontmatter.md`・`docs/adr/README.md`も）のfrontmatterは`updated`と`last_verified`を持たない。文書を変えるtaskがこの行を書き換えると、同じ文書を変える2つのtaskが内容が重ならなくてもこの行で衝突するためで、経緯は[ADR-t1964-1](../adr/2026-10-07-t1964-1-non-adr-docs-drop-updated-and-last-verified.md)。いつ・どのtaskが変えたかは`git log -- <path>`で辿る。

`created`とADRの`updated`の行は`YYYY-MM-DD`の日付だけを書き、行末にどのtaskが何を変えたかのコメント（`# task N`など）を書かない。どのtaskが変えたかはgitの履歴（着地のcommitの`Dagq-Task` trailer）が持つ。既存の文書（ADRも含む。決定と本文に触れないのでappend-onlyに当たらない）の日付の行にコメントがあれば、日付の値を変えずにコメントだけを消してよい。経緯は[ADR-t1854-1](../adr/2026-10-07-t1854-1-frontmatter-date-lines-hold-only-the-date.md)。`scripts/check-frontmatter-dates.sh`はfrontmatterの日付の行（frontmatter仕様が持たせるもの）にコメントがあれば、ファイルと行を出して落ちる検査で、CIとrunのプログラムのreview（`dagq.toml`の`[review.programs]`に挙げたとき）が実行する（docsを変えるtaskのverifyへの足し方は[taskの登録](task-registration.md)の「推奨の組み合わせ」）。

`scripts/check-doc-frontmatter.sh`はADR以外の`docs/`の文書のfrontmatter（必須のkey、typeとstatusの値、idの形と重複、`updated`と`last_verified`の行が無いこと（ADR-t1964-1））を、`scripts/check-doc-links.sh`は`docs/`とrootの`.md`の相対リンクが実在のファイルかdirectoryを指すことを検査し、違反のファイルと理由を出して落ちる。どちらもCIとrunのプログラムのreview（`dagq.toml`の`[review.programs]`に挙げたとき）が実行し、`docs/`を変えるtaskのverifyに付ける（[taskの登録](task-registration.md)の「推奨の組み合わせ」）。検査の範囲と例外（ADRのfrontmatterを見ないこと、append-onlyのADRから削除済みの`docs/journal/`と削除したdesignの文書へのリンク）はscriptの冒頭のcommentが持つ。
削除したdesignの文書の一覧（path・移管先・状態）は`scripts/check-doc-links.sh`の冒頭が持つ。
状態を書かない移行中の行は、ADRからのリンクの例外だけを持ち、pathに文書が在っても無くても落ちない。
`final`の確定の行のpathには文書を作り直さず、中身は移管先に書く（作り直すと移管先を示して落ちる）。
移行中の行を確定にするのは、その文書を消したgoalの最後のtaskだけである。

## 権限の表を写す文書

pluginの`dagq-planner`の`SKILL.md`の「Where your authority ends」と`dagq`の`reference/authority.md`の「What each role is refused」は、[Security](../design/security.md)の「actor × capability」と[Authorization](../design/authorization.md)の「Policy」の表と一致させる。表を変える変更は同じ変更でこれらを直す。

## pluginの汎用性

pluginとrepositoryの規則・値・経緯の受け持ちは[ADR-t1453-2](../adr/2026-10-03-t1453-2-ownership-of-agents-md-plugin-development-docs-and-config.md)決定1の表、runtimeとの境界は決定5。検査する記述の一覧は[棚卸し](../plans/agents-slim-inventory.md)「4. pluginの固有の記述の項目」。固有の印（このrepositoryのtest・coverageのコマンド、`tests/it`、日付、hook・`bin`を含むpluginの全ファイルでは「fixed binary」・dagqのADRの番号（`ADR-NNNN`・`ADR-tN-N`。規則は番号でなく言葉で書く）・task・goal・askの番号の逸話、`docs/design/`・`docs/plans/`のpathと「design docs」への参照、pluginのdirectoryを出る相対リンクなど）の戻りは`tests/plugin.rs`の`no_skill_carries_this_repository_s_rules`が検査し、印の語の一覧はそのtestが持つ。

runtimeの汎用性（言語・道具・外への宛先・hostで流すコマンド・cache・資源の値をruntimeに固定しないこと）は[ADR-t2257-1](../adr/2026-10-12-t2257-1-container-runtime-takes-project-and-host-values-from-config.md)と[taskの登録](task-registration.md)の「runtimeの汎用性」が持つ。

## workerの文書の照合

workerはreceiptの前に、受け入れ条件の対応づけ（[手元の検証](local-checks.md)の「受け入れ条件の対応づけ」）に続けて、workerのpromptが指示する文書の照合（仕組みは[prompt](../design/supervisor-lifecycle/prompt.md)の「文書の照合」、[ADR-t1428-1](../adr/2026-10-03-t1428-1-decide-the-documents-to-update-when-the-code-changes.md)・[ADR-t1942-2](../adr/2026-10-07-t1942-2-document-check-and-review-in-both-directions.md)）を行う。このrepositoryで差分と照合する文書は、taskが名指す文書と、変えた挙動を説明する`docs/design/`・pluginのskillとreference・AGENTS.md・`docs/development/`・ADRの索引（`docs/adr/INDEX.md`、無ければ`sh scripts/adr-index.sh`で作る。生成物なので直さず、ずれはADRのfrontmatterで直す）で、`summary`に更新したpath・節か不要の理由を書く。taskのpathsの外のずれは`docs_drift`のfollow_upにする。

- 候補は、変えた名前（コマンド・flag・設定のkey・役割・fileのpath）で`docs/design/`・`docs/development/`・`plugins/claude-dagq/skills`の`SKILL.md`と`reference/`・AGENTS.md・ADRの索引（`docs/adr/INDEX.md`、無ければ`sh scripts/adr-index.sh`で作る）を探して拾い、`summary`に探した名前を書く（ADR-t1942-2決定1・3。探す手段は問わない）。
- 見つけた`docs/design/`の文書に行を足すのは「書くもの・書かないもの」の(a)(b)(c)に当たる変更があるときだけで、既存の記述を直すのは差分かacceptedのADRと食い違うときだけとする（ADR-t1942-2決定2、[ADR-t5395-1](../adr/2026-11-20-t5395-1-design-docs-hold-what-code-cannot-show.md)がamends）。
  変えた名前がdesignに無いことはずれではなく、名前・欄・既定値を書き足さない。
  名前・欄・既定値の意味と単位内部の細部はコードの型・名前・testが持つ。
  直すときは経緯（task番号・「以前は」・変遷）を書き込まず、上の「design」の規則に従う。
- runのreviewは欠けを指摘し、`docs/design/`のコードの書き写しは「design」の機械の検査と`design-layers`のDS-1が見る（ADR-t1942-2決定4、[ADR-t5395-1](../adr/2026-11-20-t5395-1-design-docs-hold-what-code-cannot-show.md)がamends）。
  全体のreviewは、`docs/design/`の外の文書の書き写し、経緯（`docs/design/`では機械の検査が拾わない形）、予算のための削除（上の「文書を理由に差し戻す条件」の(v)）を指摘する。
  差し戻しは上の「文書を理由に差し戻す条件」に従う（[ADR-t2352-1](../adr/2026-10-16-t2352-1-review-sends-back-only-documents-the-diff-touched-and-follows-up-the-rest.md)決定1、[ADR-t5395-1](../adr/2026-11-20-t5395-1-design-docs-hold-what-code-cannot-show.md)がamends）。
  差分がdesignの記述とだけ食い違うときは、designの直しを`docs_drift`のfollow_upにする。
  差分が触っていない文書の古い記述（差分が変えた挙動の説明がその文書に無いか古いことを含む）は差し戻さず、reviewがverdictの`follow_ups`で返し、runtimeが`docs_drift`のfollow_upのdraftにする。
  このrepositoryではそれとsummaryの探した名前の確かめを全体のreviewが全ての差分でこの節の規則により行い、`.dagq/review/builtin-acceptance.md`の「文書の照合」はその要点を`builtin:acceptance`の規則として持つ。
- 見落としやすい型: 役割・権限の説明は複数の文書に散らばる（task 1400: `docs/design/overview.md`の用語集の役割の行・`docs/design/authorization.md`・`docs/design/supervisor-lifecycle/roles.md`・`triage.md`）。testやscriptが何を検査するかを列挙する節も古くなりやすい（task 1462: `docs/design/plugin-integration.md`「読み込みと検証」）。

## AGENTS.md

- AGENTS.mdは概要・役割と変更の範囲ごとの読む案内・本番queueと開発環境の境界・開始時の短い制約・検証と文書の規則への参照だけを持つ（[ADR-t1453-2](../adr/2026-10-03-t1453-2-ownership-of-agents-md-plugin-development-docs-and-config.md)決定1・3）。規則の本文はAGENTS.mdに足さず、その正本（`docs/development/`の該当の文書、実装は`docs/design/`、経緯はADRと`docs/plans/`、設定値は`dagq.toml`）に書き、AGENTS.mdには読む案内が要るときだけ参照の1行を足す。
- 上限は9,216 byte（9 KiB）。`scripts/check-agents-md-size.sh`がAGENTS.mdのbyte数（`wc -c`）を測り、上限を超えればbyte数と上限を出してexit 1にし、CIとrunのプログラムのreview（`dagq.toml`の`[review.programs]`に挙げたとき）も実行する。行数では測らない（日本語の1行は長く、行数が大きさを表さない。ADR-t1453-2決定6）。
- 決め方: 組み直した後（task 1461、2026-10-04）のAGENTS.mdは8,340 byte。余裕は876 byte（約10%）で、案内の参照の行を2〜3行足せるが、規則の本文の節を足せば超える大きさにした。候補の4〜8 KiBは、どれも今の案内（役割と変更の範囲ごとに読む文書の名指し）が入らない（8 KiB＝8,192 byteでも148 byte足りない）。
- 上限を上げるのは、案内（読む文書の名指し）が増えて規則の本文を正本へ移してもなお超えるときだけで、同じ変更でこの節の値と決め方、scriptの`limit`を直す。

## commit

- 接頭辞は`feat`・`fix`・`docs`・`test`・`refactor`・`perf`・`chore`の7つ（`feat: ...`の形。`(scope)`と`!`を付けてもよい）で、run branchのcommitとmainのsquash commit（件名はtaskのtitle）の両方に当てる。本文は何をなぜ変えたかを目安に書く。
- 機械の検査は、run branchのcommitをcommit-msgのhookとrunのプログラムのreviewが、mainの件名をtaskのtitleの検査が受け持ち、runのreviewのLLMは書式を判定しない（[ADR-t2596-1](../adr/2026-10-17-t2596-1-seven-commit-prefixes-checked-by-hook-review-program-and-task-title.md)）。接頭辞の一覧の正本は`.config/committed.toml`。titleの選び方は[taskの登録](task-registration.md)の「taskのtitle」。
- run sessionが自分のrun branchにcommitし、mainへの着地とpushを`dagq integrate`だけが行うこと（1 task 1 squash commit、メッセージはtaskのtitleとreceiptの`summary`からruntimeが作る）は[integrate](../design/supervisor-lifecycle/integrate.md)とworkerのpromptが持つ。`push_failed`のattentionの手当てはpluginの`dagq-recover`の`reference/review-by-hand.md`の「A failed push」が持つ。

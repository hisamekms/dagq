# このrepositoryの規則（全体のreview）

差分を判定するときに当てる、このrepositoryの規則の要点。
各項の出典のpathは参照で、規則はこの文書の文が持つ。
細かい規則の検査は`.dagq/agents/`のpjのagent（receipt-evidence・test-rules・migration-rules・adr-rules・config-rules・architecture-boundaries・plugin-generic）が持つので、ここではそれを写さず、差分と受け入れ条件の判定に要る要点だけを持つ。

## 受け入れ条件の対応づけと根拠

出典: `docs/development/local-checks.md`の「受け入れ条件の対応づけ」「askにしないもの」。

- receiptの`summary`は受け入れ条件の各項目を根拠（変えたfile、testの`<module>::<test>`の名前と流したコマンド、文書のpathと節、測定のscript・CSV・区切り）へ対応づける。根拠の無い項目があれば差し戻す。
- 満たせない項目をfollow_upに回して`succeeded`にしていないこと。人の判断が要るものは`worker_question`、taskの範囲の外の作業が要るものは`failed`のreceiptにする。
- 範囲を広く書く条件（「ほかに同じ形のもの」）は洗い出した一覧と各々の扱いが要り、項目ごとの値を求める条件は合計でなく求めた単位の値が要る。
- 測定のtaskは条件が求める周回数を満たしていること。少ない周回で`succeeded`にしない。
- 実装の選び方・ADRやmigrationの番号の衝突はworkerが自分で決めて`summary`に書くもので、askにしない。

## 手元の検証とtestの範囲

出典: `docs/development/local-checks.md`の「workerの手元の検証」「testの範囲」「stress」「e2eを流さない」、`docs/development/operations.md`の「workerがhostと実queueでできないこと」。

- workerは全体の`cargo test --locked`と`cargo llvm-cov`を流さない（例外は着地の検証の失敗でresumeされたrunの再現だけ）。流すのはfmt・clippy、`src/`を変えたら`scripts/check-layer-deps.sh`、変えた・関係するmoduleを名指したtest、verifyのうち全体とcoverage以外。
- receiptの`tests`のevidenceは流した範囲（コマンドと件数）を持つ。足した・変えたtestがあればstressの周回と結果を持ち、無ければstressをしない理由を持つ。
- e2eはworkerが流さず、receiptの`e2e`は理由つきの`not_applicable`にする（例外は`e2e_failed`でresumeされたrunが落ちたtestを名前で絞って1本ずつ流す再現だけ）。
- 実バイナリや実queueでの手の確認が要るものはworkerが行わず、receiptの`follow_ups`（`ops`）にする。

## test

出典: `docs/development/testing.md`の「判断と境界のtest」「testの置き場所」「testの書き方」。

- 状態の判断（遷移・回数と上限・時刻の判定・文面・次の一手の選び方）は`src/`の副作用のない関数にして`#[cfg(test)]`のunit testで確かめる。unit testは外部process・git・SQLiteのfile・sleep・実時間の時計を使わない。
- `tests/it`はSQLite・Git・process・supervisorの配線・復旧とadoptの境界を代表の1 caseで確かめ、判断のcaseごとにfixtureを起動し直さない。
- integration testを減らすときは、確かめていた中身の行き先（unit testか残すintegration test）がある。

## migration

出典: `docs/development/migrations.md`。

- 足し方・番号・リリース済みの不変の検査は`.dagq/agents/migration-rules`が持つ。
- migrationを足す差分は、変えた表・列の今の姿を`docs/design/persistence.md`の該当の箇所で直している（列の一覧の書き写しと、migrationごとの経緯は足さない）。

## 文書の置き場と役割

出典: `docs/README.md`、`docs/development/documents.md`の「AGENTS.md」「pluginの汎用性」。

- `docs/adr/`は決定の理由（append-only）、`docs/design/`は今の実装（流れ・責務と境界・不変条件・コードから読めない約束・コードへの入口）、`docs/development/`は開発の今の規則の正本、`docs/plans/`は計画の順序と完了条件・測定と経緯を持つ。
- `plugins/claude-dagq/`はどのrepositoryにも当たる汎用の操作だけを持ち、このrepositoryの規則・値・経緯を持たない。
- rootの開発の案内の文書は案内だけを持ち、規則の本文を足す差分は正本（`docs/development/`）へ移す。pluginとreviewのagentの定義も本文を写さず正本を指す。
- 今の設定値は`dagq.toml`（値の理由はそのコメント）が持ち、文書は値を写さず指す。task・goalの一覧・状態・依存・完了はqueueだけが持ち、文書に写さない。
- ADRの索引`docs/adr/INDEX.md`はfrontmatterから生成するgitignoreのfileで、直さない（ずれはADRのfrontmatterで直す）。

## 文書の照合

出典: `docs/development/documents.md`の「design」（「書くもの・書かないもの」）「workerの文書の照合」、ADR-t1942-1、ADR-t1942-2の決定1〜4。

- 照合する文書は、taskが名指す文書と、変えた挙動を説明する`docs/design/`・`docs/development/`・pluginの`SKILL.md`と`reference/`・AGENTS.md・ADRの索引。役割と権限を変える差分は、その説明を持つ文書を全て照合する（`docs/design/overview.md`の用語集・`docs/design/authorization.md`・`docs/design/supervisor-lifecycle/roles.md`など）。testやscriptが何を検査するかを列挙する節も照合する。
- `summary`の探した名前が、差分の変える名前（コマンド・flag・設定のkey・役割・fileのpath）を覆っているか確かめる。足りなければその名前で探し直し、差分と食い違う記述や、変わった流れ・境界・不変条件・コードから読めない約束の書き漏れを`docs_drift`にする。`summary`が書く更新したpath・節が、差分の直した文書と合っていることも確かめる。
- 変えた挙動に対応する`docs/design/`の文書が、流れ・境界・不変条件・約束の変わりに合っていること。内容を変える必要がない文書に差分を作っていないこと。
- 変えた名前・欄・既定値が`docs/design/`に無いことだけでは`docs_drift`にしない（その意味は定義のそばのdoc commentが持つ）。
- 差分が文書に入れたコードの書き写し（eventの欄・flag・既定値と閾値・関数名・test名の列挙、コードを読めば分かる手順）と経緯の混入（goal・requestの番号、「以前は」、文字数や件数の変遷）を指摘する。指摘するのは差分が入れたものだけで、既存の記述は対象にしない。
- 文書が差分と食い違うときは、taskが名指したかに依らず`docs_drift`にする。文書の差分があることだけでは正しいとせず、`summary`が文書を直さない理由も他の主張と同じく確かめる。
- taskのpathsの外のずれは差分で直さず、`docs_drift`のfollow_up（pathと節つき）にあること。
- 1文1行の崩れ、大きさの予算と1行の長さの超過、本文のtask番号は`dagq.toml`の`[review.programs]`のプログラムが見るので、ここでは指摘しない。

## 文書の形とcommit

出典: `docs/development/documents.md`の「ADR」「ADRのID」「plans」「frontmatter」「権限の表を写す文書」「commit」。

- 既存のADRは書き換えない（append-only）。後から変えてよいのは`status`・`accepted_on`・`superseded_by`・`superseded_on`・`deprecated_on`・`amended_by`の欄とH1直後の注記1行、値を変えない日付の行の行末のコメントの削除だけで、決定を変えるのは`amends`か置き換えの新しいADR。
- ADRは1つの決定（密に結びついた数個まで）で、変えるのに人の判断が要るものを書く。決定の一部を変えるときは新しいADRの`amends`に変える決定を書き、元のADRの`amended_by`を足し、同じ変更で今の姿（designかdoc comment）を直す。新しいADRのIDはそれを書くtaskの`adr-t<task ID>-<N>`。
- 計画のステップの状態が変われば`docs/plans/current.md`を直し、完了した計画は消さず`completed`にする。
- ADR以外の文書のfrontmatterは`updated`・`last_verified`を持たず、日付の行は日付だけを書く。
- 権限の表（`docs/design/security.md`・`docs/design/authorization.md`）を変える差分は、同じ変更でpluginの`dagq-planner`の「Where your authority ends」と`dagq`の`reference/authority.md`を直す。
- run branchのcommitのメッセージは`feat:` / `fix:` / `docs:` / `test:`のどれかの接頭辞で始め、本文に何をなぜ変えたかを書く。着地のcommitのメッセージはtaskのtitleと`summary`から作られるので、`summary`は何をなぜ変えたかを持つ。

## 本番queueと固定バイナリの境界

出典: `docs/development/operations.md`の「本番queueと固定バイナリ」「workerがhostと実queueでできないこと」、`docs/development/documents.md`の「ADR」（hostのgitの設定）。

- 本番queueの状態とschemaを変えるのは固定バイナリ`~/.local/bin/dagq`だけで、開発中のバイナリ（`target/`）で変えない。
- 本番queueのDBと固定バイナリを作業成果で置き換えない。DBを手で直さない。
- workerは使い捨てのqueueも操作せず、hostにツールを入れず、hostのgitの設定を変えない。
- 差分がこれらを破る手順やscriptを足していれば差し戻す。

## taskの範囲

出典: `docs/development/local-checks.md`の「受け入れ条件の対応づけ」「askにしないもの」。

- taskがpathsを持つときは、差分はその中だけを変える。
- 受け入れ条件に要らない範囲の外の作業は直さずfollow_upにし、受け入れ条件に要る範囲の外の作業は`failed`のreceiptにする。

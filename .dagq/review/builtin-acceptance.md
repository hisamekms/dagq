# このrepositoryの規則（全体のreview）

差分の判定に当てるこのrepositoryの規則の要点。
出典は参照で（pathの無いfileは`docs/development/`の下）、規則はこの文書の文が持つ。
細かい検査は`.dagq/agents/`のpjのagentが持ち、ここに写さない。

## 受け入れ条件の対応づけと根拠

出典: local-checks.md。

- receiptの`summary`は受け入れ条件の各項目を根拠（変えたfile、testの`<module>::<test>`とコマンド、文書のpathと節、測定のscript・CSV）へ対応づける。根拠の無い項目は差し戻す。
- taskにpathsがあれば差分はその中だけを変える。受け入れ条件に要らない範囲の外の作業はfollow_upにする。
- 満たせない項目をfollow_upに回して`succeeded`にしない。人の判断が要るものは`worker_question`、範囲の外の作業が要るものは`failed`。
- 広く書く条件（「ほかに同じ形のもの」）は洗い出した一覧と各々の扱いが、項目ごとの値の条件は求めた単位の値が要る。測定のtaskは求める周回数を満たす。
- 実装の選び方・ADRやmigrationの番号の衝突はworkerが決めて`summary`に書き、askにしない。

## 手元の検証とtestの範囲

出典: local-checks.md、operations.md。

- workerは全体の`cargo test --locked`と`cargo llvm-cov`を流さない（例外は着地の検証の失敗でresumeされたrunの再現）。流すのはfmt・clippy、`src/`を変えたら`scripts/check-layer-deps.sh`、変えた・関係するmoduleを名指したtest、verifyのうち全体とcoverage以外。
- fmt・clippyの関門は着地の検証で、receiptにその実行の根拠が無いことでは差し戻さない（ADR-t2588-1）。
- 手元の検証の実行（流したコマンド・exit status・stressの周回と結果・時間の関門の結果）と、変えたtestが無いので関門を省いたことは、資料の記録の節と関門の節が示していれば、receiptの`tests`のevidenceに無いことでは差し戻さない（ADR-t2589-1）。節が無ければevidenceで見る。
- receiptの`tests`のevidenceは記録で示せないこと（testの範囲を選んだ理由と着地の検証・CIに任せた範囲、流したtestの件数、流せなかった理由、許可の一覧に足した項目の理由）を持つ。testの範囲の選び方の妥当さと受け入れ条件の対応づけは引き続き見て、要る関門の記録が無いか失敗しているときはevidenceと合わせて判じる。
- e2eはworkerが流さず、`e2e`は理由つきの`not_applicable`（例外は`e2e_failed`でresumeされたrunの落ちたtestを1本ずつ流す再現）。
- 実バイナリや実queueでの手の確認はworkerが行わず`follow_ups`（`ops`）にする。

## test

出典: testing.md。

- 状態の判断（遷移・上限・時刻・文面・次の一手）は`src/`の副作用のない関数にしunit testで確かめる。unit testは外部process・git・SQLiteのfile・sleep・実時間を使わない。
- `tests/it`はSQLite・Git・process・配線・復旧とadoptの境界を代表の1 caseで確かめ、判断のcaseごとにfixtureを起動し直さない。
- integration testを減らすときは、確かめていた中身の行き先（unit testか残すintegration test）がある。

## migration

出典: migrations.md。

- 足し方・番号・リリース済みの不変は`.dagq/agents/migration-rules`が見る。
- migrationを足す差分は、変えた表・列の今の姿を`docs/design/persistence.md`で直す（列の一覧の書き写しとmigrationごとの経緯は足さない）。

## runtimeの境界の規則

出典: `docs/design/architecture.md`の「検査できる規則」「検査の範囲」、ADR-t2391-1。

- `src/`の差分が足したものに、各規則の「検査」が全体のreviewを名指す部分を当てる: L5、L7の判断とeventのpayloadの組み立て、L8、C2の型で守られない部分、C3の残り、C4のuse caseの切り方、C5のscriptが見ない部分、C6、C8の例外の中身と`crate::application::X`を通す名指し、X2、X3。
- 規則の本文はarchitecture.mdの各IDが持ち、指摘には規則のIDを添える。scriptが見る部分と所有の一覧への記録の漏れは指摘しない。

## 文書の置き場と役割

出典: `docs/README.md`、documents.md。

- `docs/adr/`は決定の理由（append-only）、`docs/design/`は今の実装（流れ・境界・不変条件・コードから読めない約束・入口）、`docs/development/`は規則の正本、`docs/plans/`は計画と測定と経緯。
- `plugins/claude-dagq/`は汎用の操作だけを持ち、このrepositoryの規則・値・経緯を持たない。rootの案内の文書・plugin・reviewのagentの定義は規則の本文を写さず正本を指す。
- 今の設定値は`dagq.toml`（理由はそのコメント）が持ち、文書は値を写さない。task・goalの一覧・状態・依存・完了はqueueだけが持つ。
- `docs/adr/INDEX.md`はfrontmatterから生成するgitignoreのfileで直さない（ずれはADRのfrontmatterで直す）。

## 文書の照合

出典: documents.md、ADR-t1942-1、ADR-t1942-2、ADR-t2352-1。

- `reasons`に入れる文書の指摘は、差分が変えた文書が差分と食い違うこと（差分が入れた書き写しと経緯を含む）と、差分（コード・設定）が文書の述べる不変条件・約束・境界と食い違うことだけ。taskが名指したかに依らない。
- 差分が触っていない文書の古い記述（前から古いもの、変えた挙動の説明が無いか古いこと）は差し戻さず、verdictの`follow_ups`に文書のpath・節・理由で返す。`follow_ups`だけでは`revise`にしない。
- 照合する文書は、taskが名指す文書と、変えた挙動を説明する`docs/design/`・`docs/development/`・pluginの`SKILL.md`と`reference/`・AGENTS.md・ADRの索引、検査を列挙する節。役割と権限の差分はその説明を持つ文書を全て（`docs/design/`のoverview.mdの用語集・authorization.md・`supervisor-lifecycle/roles.md`など）。
- `summary`の探した名前が差分の変える名前（コマンド・flag・設定のkey・役割・path）を覆うか確かめ、足りなければ探し直す。`summary`の書く更新したpath・節が差分と合うことも確かめる。文書の差分があることだけで正しいとせず、文書を直さない理由も確かめる。
- 差分が変えた`docs/design/`の文書が流れ・境界・不変条件・約束の変わりに合い、変える必要のない文書に差分が無いこと。変えた名前・欄・既定値が`docs/design/`に無いことだけでは`docs_drift`にしない（意味は定義のそばのdoc commentが持つ）。
- 差分が文書に入れたコードの書き写し（eventの欄・flag・既定値・関数名・test名の列挙、コードで分かる手順）と経緯（goal・requestの番号、「以前は」、件数の変遷）は`reasons`で差し戻す。既存の記述は対象外。
- taskのpathsの外の文書を差分で直していないこと。
- 1文1行、大きさと1行の長さ、本文のtask番号、frontmatterの欄と日付の行は`[review.programs]`が見るので指摘しない。

## 文書の形とreceiptのsummary

出典: documents.md。

- ADRの形（append-onlyと後から変えてよい欄、`amends`・`amended_by`、ID）は`.dagq/agents/adr-rules`が見る。決定の一部を変えるADRは、同じ変更で今の姿（designかdoc comment）を直す。
- 計画のステップの状態が変われば`docs/plans/current.md`を直し、完了した計画は消さず`completed`にする。
- 権限の表（`docs/design/`のsecurity.md・authorization.md）を変える差分は、同じ変更でpluginの`dagq-planner`の「Where your authority ends」と`dagq`の`reference/authority.md`を直す。
- run branchのcommitのメッセージの書式（接頭辞・本文）は判定しない（接頭辞は`[review.programs]`のcheck-commit-messagesが見る）。着地のcommitのメッセージはtaskのtitleと`summary`から作られるので、`summary`は何をなぜ変えたかを持つ。

## 本番queueと固定バイナリの境界

出典: operations.md、documents.md。

- 本番queueの状態とschemaは固定バイナリ`~/.local/bin/dagq`だけが変え、`target/`のバイナリで変えない。DBと固定バイナリを作業成果で置き換えず、DBを手で直さない。
- workerは使い捨てのqueueも操作せず、hostにツールを入れず、hostのgitの設定を変えない。
- 差分がこれらを破る手順やscriptを足していれば差し戻す。

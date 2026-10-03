# AGENTS.md

dagq は cmux と Git worktree で依存関係付きの開発タスクを実行する Rust runtime。文書の分類は [docs/README.md](docs/README.md)。この repository 自身の開発タスクも dagq で流す（ドッグフーディング）。

CLI の使い方（登録・起動・監視・レビューと着地・復旧）は plugin の skill が持つ。この文書はこの repository でだけ必要な注意を書く。

## セッション開始時に読む

1. `dagq list` と、担当タスクの `dagq show ID`。タスクの一覧・状態・依存・run 履歴はキューだけが持つ
2. [docs/plans/current.md](docs/plans/current.md) の現在のステップと完了条件
3. 触る範囲の `docs/design/*.md`

## 作業中

全ての session が開始時に読む、本番 queue と開発環境の境界の短い規則。補足（どの session に当たるか・理由）と運用の手順は [docs/development/operations.md](docs/development/operations.md) が持つ。

- 本番 queue（この repository の queue DB）の登録と、状態を変えるコマンド・`migrate`・`up` / `down` / `install` は、どの session でも固定バイナリ `~/.local/bin/dagq` で打つ。`target/debug` や `target/release` の開発中のバイナリで本番の状態と schema を変えない。状態を変えないコマンド（`status`・`show`・`list`・`events`・`doctor` など）は読み取り専用で開くので、開発中のバイナリで本番を読むのはよい
- session 開始時に `which dagq` が `~/.local/bin/dagq` に解決することを確かめ、コマンドは repository の中（どの worktree でもよい）で打つ
- 本番 queue の DB と `~/.local/bin/dagq` を作業成果で置き換えない。固定バイナリの入れ替えは `dagq install` か `up --auto-update` の自動更新だけで行う（手順は plugin の `dagq-recover` skill の `reference/update.md`）。DB は手で直さず（例外は無い）、CLI の外で状態を持たない
- 新しいビルドの確認とスモークは使い捨ての queue で、人か inbox が行う。worker は使い捨ての queue も操作できず、host にツールを入れない（worker が実バイナリや実 queue での確認が要ると思ったら operations.md の「workerがhostと実queueでできないこと」を読む）
- 人・inbox・planner は、`install`・`up` / `down`、`dagq.toml`、host のツール（sccache・`d2`・TALA）、KPI の印、push と secret に触る前に operations.md の該当の節を読む。今の設定値とその理由は `dagq.toml` のコメント、`[run.env]` の渡し方は [Run environment](docs/design/supervisor-lifecycle/run-environment.md)、KPI・印・レポート・push の汎用の使い方は dagq skill の `reference/kpi.md` が持つ

## 変更後に必ず通す

手元の検証の規則は [docs/development/local-checks.md](docs/development/local-checks.md) が持つ。worker は変更を終えて receipt を書く前に読む（手元で流すもの・test の範囲・全体を比べる test・stress・e2e を流さないこと・resume での再現の例外）。人が dagq を通さず checkout で直接変えたときは、その「人の手元の検証」の 3 本を通す。

## テストの制約

test と task の登録の規則は次の開発文書が持つ。役割と変更の範囲に合うものだけを読む。経緯は [docs/plans/local-checks-history.md](docs/plans/local-checks-history.md) と ADR が持つ。

- [docs/development/task-registration.md](docs/development/task-registration.md): `--verify`・`--paths`・`--evidence`・`--change` の推奨の組み合わせと、plan review が当てはめる規則。task を登録・修正する planner と、plan review job が読む
- [docs/development/testing.md](docs/development/testing.md): coverage の関門、test の置き場所・書き方・ファイルの行数・待ちの上限、e2e とその印、手動スモーク。test（`tests/`・`crates/*/tests/`・`#[cfg(test)]`）や `.config/e2e-quarantine.toml` を変える worker と、それを登録する planner が読む
- [docs/development/migrations.md](docs/development/migrations.md): migration の足し方・番号・リリース済みの migration の不変。migration を足す worker と planner が読む

## 文書のルール

- 人の判断は ADR・Goal の記述・`Task.context`・receipt の `summary` に残す（作業記録のジャーナルは [ADR-0036](docs/adr/0036-delete-frozen-work-records.md) で削除した）
- 決定は `docs/adr/` に追加する。既存 ADR は書き換えない
- ADR は `accepted` だけが現在の決定で、`superseded` なら `superseded_by` を辿り、`deprecated` は後継なしの廃止（日付は `superseded_on` ではなく `deprecated_on`）。決定を変えるとき、変える ADR の決定が 1 つか、決定の大半を変えるなら古い ADR を丸ごと置き換える ADR を書き、番号付きの決定を複数持つ ADR（4 桁でも新しい形でも）の一部の決定を変えるなら下の amends で直す（ID の形でなく決定の数と変える範囲で決める。[ADR-t1091-1](docs/adr/2026-09-30-t1091-1-amend-or-replace-by-number-of-decisions.md)、[ADR-t598-1](docs/adr/2026-09-26-t598-1-adr-id-is-task-id-small-adrs-and-design-holds-current-state.md)、索引は [docs/adr/README.md](docs/adr/README.md)）
- 実装を変えたら `docs/design/` の該当文書の内容と `updated` / `last_verified` を更新する。内容を変える必要がない文書には日付だけの差分を作らない（[ADR-t1428-1](docs/adr/2026-10-03-t1428-1-decide-the-documents-to-update-when-the-code-changes.md)）
- ステップの状態が変わったら `docs/plans/current.md` を更新する
- frontmatter は [docs/frontmatter.md](docs/frontmatter.md) に従う
- ADR の ID はそれを書く task の ID と枝番の `adr-t<task ID>-<N>`（N は 1 から。1 本だけでも `-1`）、ファイル名は `docs/adr/<YYYY-MM-DD>-t<task ID>-<N>-<slug>.md` で、日付は `accepted_on`（ADR を書く task では worker が書いて accepted にした日）にする（[ADR-t598-1](docs/adr/2026-09-26-t598-1-adr-id-is-task-id-small-adrs-and-design-holds-current-state.md) 決定 1）。以前の「planner が main の次の空き番号を選ぶ」規則は、未完了の task の予約と計画時に衝突したのでやめた
  - ADR を書く task を登録するときは、planner が description に本数と各 ID の中身を書き（自分の ID は `add` が返すまで分からないので「この task の ID で ADR-t<ID>-1 を書く」と書くか、`add` の後に draft を直す）、`--verify 'sh scripts/check-adr-numbers.sh'` を付ける。ID は task の ID から決まるので、planner も plan review も番号の割り当ての棚卸しをしない
  - 後続 task は ADR を `ADR-t<ID>-<N>` で参照する（日付を含めないので着地前から書ける）。今どうなっているかを指すときは `docs/design/` の文書を、なぜそうしたかを指すときは ADR を指す（決定 4）
  - 既存の 4 桁の ADR（0001〜）と、登録済みの task が予約した 4 桁の番号はそのまま使い、振り直さない。新しく登録する ADR の task は新しい形にする。ただし例外として、予約した 4 桁の番号が main ですでに埋まっていたら（`integrate` の `check-adr-numbers.sh` が重複で落ちて resume されたときも同じ）、番号の衝突は人が要る理由（[ADR-0047](docs/adr/0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md) 決定 41）に当たらないので、worker は `dagq ask` にせず、自分の task の ID の新しい形に書き直す（ファイル名を `docs/adr/<accepted_on>-t<task ID>-<N>-<slug>.md` に、frontmatter の `id` を `adr-t<task ID>-<N>` にする）。自分の変更の中の参照も合わせ、元の番号と新しい ID を receipt の `summary` に書く（衝突しない ID なので、main の次の空き番号は探さない）
  - 1 ADR に決定 1 つ（密に結びついた数個まで）、本文はおおむね 100 行以内。ADR には変えるのに人の判断が要るものを書き、event の kind や欄名・flag の綴り・既定値や閾値の数値・関数やファイルの名前・migration の番号・test の名前は `docs/design/` に書く（決定 2・3）。番号付きの決定を複数持つ ADR（4 桁でも新しい形でも。0047・0073・t813-2 など）の一部の決定を変えるときは、丸ごと置き換えずに小さな新しい ADR の `amends` に変える決定を書き、元の ADR に `amended_by` を足し、design を今の姿に直す。決定が 1 つの ADR と決定の大半を変えるときは丸ごと置き換える。どちらにするかは ADR を書く task の planner が description に書き、plan review が見る（決定 5 を amends した ADR-t1091-1）。決定と実装が明らかなものは ADR と実装を 1 task にする（決定 12）
  - `scripts/check-adr-numbers.sh`（名前は登録済みの task の verify が使うので変えない）は、4 桁の番号の重複と `id` が `adr-<ファイルの番号>` と食い違う ADR、新しい形のファイル名の形（枝番の欠け）・`id` が `adr-t<ID>-<N>` と食い違う ADR・`t<ID>-<N>` の重複・ファイル名の日付と `accepted_on` の食い違いを検出して exit 1 にする（CI も実行する）

## タスクを閉じるとき

- タスクの完了はキューが持つ。`integrate` が run を `integrated`、タスクを `completed` にする

## plan review

runtime の plan review の prompt は repository の規則を持たず、この文書とそれが名指す文書・規則を読ませる（task 625）。この repository の plan review job は、この文書の規則に加えて次を当てはめる。

- [docs/adr/README.md](docs/adr/README.md)（ADR の索引）と、task が名指す ADR と `docs/design/` の文書を読む。`accepted` の ADR の決定と矛盾する task は `concern` にする（`superseded` なら `superseded_by` を辿る）
- ADR を書く task は「文書のルール」の ADR の ID の形（`adr-t<task ID>-<N>`、ファイル名 `docs/adr/<YYYY-MM-DD>-t<task ID>-<N>-<slug>.md`）に従い、`--verify 'sh scripts/check-adr-numbers.sh'` を持つこと。足りなければ `revise`。ID は task の ID から決まるので、番号の割り当ての棚卸しはしない
- task の verify・paths・evidence・change と、測定の task・範囲を「同じ形のもの」で広げる task の書き方は、[docs/development/task-registration.md](docs/development/task-registration.md) の「plan reviewが当てはめる規則」を当てはめる
- 挙動や仕様を変える task は、関連文書（`docs/design/`・plugin の skill と reference・AGENTS.md・ADR の索引）の path・節と更新が要る理由を description か context に書くこと。欠けていて関連する文書が明らかなら、見つけた path を理由に書いて `revise` にする。文書の差分を求める verify や evidence は求めない（[ADR-t1428-1](docs/adr/2026-10-03-t1428-1-decide-the-documents-to-update-when-the-code-changes.md)）

## コミット

- run session は自分の run branch `dagq/<run-id>` にコミットする。main への着地は `dagq integrate` だけが行い（1 タスク 1 squash commit）、push は integrate が行う（`push_failed` の attention が inbox に出たら、人の指示で原因を直して `git push origin main`）
- メッセージは `feat:` / `fix:` / `docs:` / `test:` の接頭辞、本文は何をなぜ変えたか。着地時の commit メッセージはタスクの title と receipt の summary から runtime が作る

## 役割: supervisor と worker と planner と inbox と observer

役割はこの 5 つ（[ADR-0044](docs/adr/0044-findings-proposals-from-findings-and-quiet-observer.md) の決定 1、[docs/design/overview.md](docs/design/overview.md) の用語集）。runtime の `supervise` プロセスが **supervisor**（claim・worker の起動・validating・run ごとの headless の review job と復旧 job（triage job を広げたもの）・submitted の proposal ごとの headless の plan review job・resume・着地・runtime が立てる planner の起動・後始末）、run ごとに worktree で作業する Claude session が **worker**、proposal（goal と task の束）ごとのオンデマンドの session が **planner**（人が `dagq plan` で開くものと、supervisor が立てるものがある）、人に届くもの（ask と attention）の窓口になる唯一の常駐 session が **inbox**、supervisor が timer で起動する headless の job が **observer**。常駐の planner と goal 22 の follow-up triage job（ADR-0037）は ADR-0041 で廃止した（ADR-0044 が引き継ぐ）。以前の常駐 session（ADR-0010〜0023 に出てくる英字の役割名）は ADR-0024 で退役し（ADR-0044 が引き継ぐ）、既存 ADR のその記述は overview の用語集で読み替える。

同じ commit に対する verification は `integrate` の 1 回が正で、validating は receipt・commit・clean・要求 evidence だけを見て `verification_commands` を実行しない。`integrate` は rebase の有無に関わらず rebase 後に必ず `verification_commands` を実行し（試行ごとの `integrate-<attempt>-verify-N.log`）、失敗すれば run は `needs_session` になって supervisor が resume する（[ADR-0049](docs/adr/0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md) 決定 1）。

### 起動と停止（`up` / `down`）

- 人・inbox・planner が `up` / `down` / `install` を打つ前に、[docs/development/operations.md](docs/development/operations.md) の「`up`のコマンド」を読む（この repository の `up` のコマンドと、付ける flag・付けない flag の規則とその正本への案内）
- `up` / `down` の汎用の操作と出力は plugin の `dagq-recover` skill の `reference/up-down.md`、自動更新とその ask は同じ skill の `reference/update.md`、仕組みは [up / down](docs/design/supervisor-lifecycle/up-down.md) と [Auto-update](docs/design/supervisor-lifecycle/auto-update.md)、workspace の識別と名前は [run-workspaces](docs/design/supervisor-lifecycle/run-workspaces.md) と [naming](docs/design/supervisor-lifecycle/naming.md) が持つ。並列数と `runtime_planners` は `dagq.toml` の `[supervisor]`（値と理由はそのコメント）

### 着地と人の判断

- 着手と着地の報告と、人の判断が要るときに ask にして待つ規則は [docs/development/operations.md](docs/development/operations.md) の「人への報告」が持つ
- 着地・review・復旧 job・イレギュラーの 3 つの層・ask の理由・待ちの仕組みは `docs/design/supervisor-lifecycle/` の [review](docs/design/supervisor-lifecycle/review.md)・[triage](docs/design/supervisor-lifecycle/triage.md)・[background-recovery-job](docs/design/supervisor-lifecycle/background-recovery-job.md)・[ask](docs/design/supervisor-lifecycle/ask.md)・[waiting](docs/design/supervisor-lifecycle/waiting.md) が持つ。人と inbox が手で行うもの（review by hand・triage by hand・verify を直してからの `retry_inherit`・`push_failed` など）と、runtime と復旧 job が直すものを手で行わないことは、plugin の `dagq-inbox` skill と `dagq-recover` skill（とその reference）が持つ
- worker の provider と経路、フォールバック、`queue_hold` は plugin の dagq skill の `reference/provider.md` と [provider-lifecycle](docs/design/provider-lifecycle.md) が持つ

### worker

- runtime の prompt に従う。割り当てられた worktree（branch `dagq/<run-id>`）の中だけで作業し、main、queue DB、`runs/` 配下の runtime ファイル、他の run の worktree は触らない。merge も push も workspace の close もしない
- 変更後の手元の検証（fmt・clippy・変えた・関係する module を名指しした test・stress・e2e を流さないこと・`integrate` の検証が落ちた resume での再現の例外・cargo-nextest が無いときの `failed` の receipt・subagent review）は、receipt を書く前に [docs/development/local-checks.md](docs/development/local-checks.md) に従う。test を書く・置くときは [docs/development/testing.md](docs/development/testing.md)、migration を足すときは [docs/development/migrations.md](docs/development/migrations.md) も読む
- コミットしてから receipt を書く。receipt の commit は run branch の clean head で、base commit の上に乗っている
- 判断が要るときは terminal に質問を書いて待つのではなく、`dagq ask --run <run-id> --kind worker_question --because <scope|discard> --topic <code> --question '...'` を打ち、短く報告して止まる。回答は supervisor が `answer to ask <id>: ...` として同じ terminal に送る（ADR-0022 決定 2）。`--because` は人が要る理由（[ADR-0047](docs/adr/0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md) 決定 41 の `reason_category`）で、ask にしてよいのはその分類に当たるときだけ（worker では主に、受け入れ条件や範囲が変わる `scope` と、成果を捨てるかどうかの `discard`）。当たらないもの（実装の選び方、ADR や migration の番号の衝突など）は自分で決めて receipt の `summary` に書き、task の範囲の外に出るなら `failed` の receipt に理由（必要なパスや作業）を書く
- `worker_question` には問いの中身の分類コードを `--topic` で必ず付ける（[ADR-t947-2](docs/adr/2026-09-28-t947-2-worker-questions-carry-topic-codes.md)。`--because` とは別の軸で、集計にだけ使う）。先頭が主で、止まったきっかけ（最初に満たせなくなったもの）を 1 つ、それを解くのに一緒に決める必要があるものを副として `--topic` を重ねて付ける。きっかけが 2 つ同時で決められないときだけ重い方を主にする。重い順に `discard_work`（成果を捨てるかやり直すか）・`adr_conflict`（受け入れ条件や唯一のやり方が accepted の ADR・design・人の決定と両立しない）・`acceptance_conflict`（受け入れ条件どうしか条件と description が両立しない）・`acceptance_infeasible`（調べた事実のために条件をそのままでは満たせない）・`out_of_scope_change`（paths・description・verify に無い変更が要る）・`task_overlap`（並行する task か直前の着地と重なる）・`precondition_missing`（測る対象・先行 task の着地・件数などの前提が揃っていない）・`host_environment`（host のツールや設定が妨げ、worker は host に手を入れられない）・`design_choice`（条件・ADR・範囲に触れない実装の選び方。本来は ask にせず自分で決めるもので、付けても拒まれず数えられる）・`other`（どれにも当たらない。問いの文で説明する）。定義は [ask](docs/design/supervisor-lifecycle/ask.md#worker_questionの分類コード) が持ち、worker の prompt と `dagq ask --help` にも載る
- receipt の前に受け入れ条件の各項目を根拠へ対応づける（[ADR-t1420-1](docs/adr/2026-10-03-t1420-1-worker-maps-each-acceptance-criterion-before-the-receipt.md)。満たせない項目を follow_up に回して `succeeded` にせず、上の ask の規則どおり `worker_question` か `failed` の receipt にする）。この repository での根拠の書き方: test は `<module>::<test>` の名前と流したコマンド（流す範囲は [docs/development/local-checks.md](docs/development/local-checks.md) の規則のまま）、文書は path と節、測定は文書の節・CSV と script の path・コマンドと `--since` / `--until` の区切り。測定の task では条件が求める周回数と実際に流した回数を比べ、足りなければ理由とともに `worker_question` か `failed` にし、少ない周回の結果で `succeeded` にしない（task 961）。条件が「ほかに同じ形のもの」のように範囲を広く書くときは、grep などで洗い出した一覧と各々の扱い（直した・残す理由）を `summary` に書く。条件が「各 test の前後の秒」のように値を項目ごとに求めるときは、合計だけでなく求めた単位で書く（どちらも task 1075）
- 受け入れ条件の対応づけに続けて、task が名指す文書と作業中に見つけた関連文書を差分と照合し、`summary` に更新した path・節か不要の理由を書く（[ADR-t1428-1](docs/adr/2026-10-03-t1428-1-decide-the-documents-to-update-when-the-code-changes.md)。worker の prompt にも同じ指示がある）。task の paths の外のずれは `docs_drift` の follow_up にする
- receipt の `follow_ups` は 1 件ごとに `title`・`description` と種類の `category`（`defect`・`flaky_test`・`test_gap`・`docs_drift`・`remaining_scope`・`improvement`・`measurement`・`decision`・`ops`・`other` のどれか 1 つ）を持つ。迷ったら、それを片付けたときに何が変わるかで選び、`flaky_test` と `test_gap` は description に test の名前を書く。重複かどうかは種類にしない。一覧と定義は [Receipt and session exit](docs/design/supervisor-lifecycle/receipt-and-session-exit.md#follow_upsの分類コード)（[ADR-t947-3](docs/adr/2026-09-28-t947-3-follow-ups-carry-category-codes.md)）。欠けても receipt は拒まれず `unlabeled` と数えられる
- receipt を書く前に、自分が起動した background の処理（`run_in_background` の shell、待ちループ、watch など）をすべて止める。残っていると supervisor の `/exit` が Claude Code の「Background work is running」の確認画面で止まり、`exit_request_timed_out` になる
- worker の `dagq` はクライアントモードで動き（[Queue service](docs/design/queue-service.md#クライアントモード)）、queue service のユースケースに無いコマンド（`integrate`・`answer`・`ask close`・`ready`・`cancel`・計画系（`add`・`edit`・`submit`・`goal add` など）・`recover`・`review`・`up` / `down` / `install` / `migrate`・`mark` など）は client の側で `no_use_case` として拒まれ、event は残らない。service に届くもののうち worker に許されないもの（`finding` の記録・解決、自分の run と task 以外への `ask`・`note`）は service が拒み、`authorization_denied` に記録する（[Security](docs/design/security.md)）。拒否をそのまま答えとし、`DAGQ_ROLE` などの env を外す・書き換える、path や script から打つ、DB に触るといった迂回をしない（host 実行の判定は advisory で sandbox ではないので、迂回できても許されていない）。task の範囲の外の操作が要るなら receipt か `worker_question` の ask にする
- signal を送るのは自分が起動したプロセスだけにし、pid か task で止める。`pkill` / `killall` / `kill $(pgrep ...)` のように名前やパターンで選ばない。どの run の session も command line に prompt（検証コマンドの名前を含む）を持つので、`pkill -f llvm-cov` は他の run の session と `integrate` の検証も止める（task 359。runtime は run の settings の `permissions.deny` で `pkill` と `killall` を拒む）
- receipt を書いたら結果を短く報告して止まる。`/exit` は自分で打たない。supervisor が idle を見て送る

### inbox と planner

- inbox は `up` が開き、初期 prompt（`inbox_prompt`）で起動する唯一の常駐 session。planner は人が `dagq plan` で開くもの（`planner_prompt`）と、supervisor が立てるもの（plan review が差し戻した proposal の planner が閉じていたとき、plan review が submitted に戻した ready の task を直させるとき、runtime や job が作った draft ごと、proposal を求める印の付いた finding ごと）がある。どれも workspace の `--env` に `DAGQ_ROLE=inbox` / `planner` と `DAGQ_QUEUE` を持ち、compaction と `/clear` の後は plugin の SessionStart hook が `status --role <role>` を出す
- inbox は `dagq-inbox` skill に従い、`status --role inbox` から始め、`watch --role inbox` を background で回し、`ask_opened` の question と options を人に見せ、人の答えを `answer` で書く。attention はすべて inbox 宛てで、回答済みの ask、止まった supervisor、失敗した review / 復旧 job（`triage_failed`。ADR-t609-1 より前の runtime が記録した `recovery_failed` も）/ plan review（`plan_review_failed`）/ goal review（`goal review by hand`、`goal_review_failed`）、応答しない planner（`planner_unresponsive`）、runtime の planner が決めきれなかった draft と finding（`finding_planner_exhausted`）、`main` の push の失敗（`push_failed`）、KPI の push のコマンドの失敗（`kpi_push_abandoned`）も人に知らせ、人の指示があるときだけ `dagq-recover` skill の手順を実行する。plan review の concern（`approve_plan`：`ready` / `send_back` / `cancel`）、finding に紐づく `blocked` の ask の `propose`（提案にする）/ `dismiss` と `stalled` の ask の `propose`、runtime の planner の `planner_question` の answer、goal review の `approve_goal`（`achieved` / `abandoned` / `gaps` / `keep_open` と job が足した option。job の option の答えは inbox に戻る）の answer、自動更新の `update_failed`（`retry` / `skip`）の answer は supervisor が適用・配送する（`update_failed` は `--auto-update` の生きている supervisor が居るときだけ）。自分では判断しない。inbox の権限は人（`DAGQ_ROLE` なし）と同じで、全ての ask に answer でき、`dagq-recover` の手作業も人の言葉で代行する。そのかわり記録で人自身と区別される（[ADR-t728-3](docs/adr/2026-09-27-t728-3-answer-and-delegated-authority-of-the-inbox.md)）: event の actor は `inbox`（人自身は `user`）、answer の `authority` は `delegated`（人自身は `user`）、着地は Integrator の event の `requested_by` が `inbox`。inbox の terminal で人が打つ `!` のコマンドも `DAGQ_ROLE=inbox` を継いで代行として記録されるので、人自身の操作として残すなら `DAGQ_ROLE` の無い別の terminal で打つ
- planner は `dagq-planner` skill に従い、人の課題を聞き、dagq skill で goal と draft の task を書き、`lint` を通して `submit` し、plan review の revise を受けたら直して `submit --proposal ID` で出し直す。自分では `ready` にしない。人が開いた planner も runtime が立てた planner も、推奨が出せる判断（follow_up の draft の採否、既存の実装や ADR に合わせて書く、重複をまとめる、revise の指摘どおりに直すなど）は人に聞かずに推奨どおり決めて進め、理由を note か proposal の task の context に残す。人に上げる（人が開いた planner ならその workspace で人に聞き、runtime が立てた planner なら推奨を載せた `planner_question` の ask にする）のは、計画の意図が変わる修正を含め、人が要る理由（[ADR-0047](docs/adr/0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md) 決定 41 の `scope`・`discard`・`authentication`・`cost`）に当たり AI の材料で決めきれないものと、確信が持てないものだけで、ADR-t808-1 の自動で採用しない上限（深さ 3 以上と、goal が無いか閉じた goal の follow_up）は今までどおり人の adopt を経る（[ADR-t451-1](docs/adr/2026-10-02-t451-1-ai-decides-recommendable-asks-and-escalates-only-the-undecidable.md) の決定 1・5）。交通整理（draft の棚卸し、重複・実装済みの検出、ADR 番号の衝突、依存の付け替え、他の task の退避）は plan review job に任せて抱えない。goal の達成の判断と close は planner の役ではない（[ADR-0047](docs/adr/0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md) 決定 16・43）。所属 task がすべて completed / canceled で draft の残らない open の goal は、supervisor の headless の goal review job が receipt と acceptance を照合し、achieved なら acceptance の項目ごとの根拠を記録して goal を閉じ、足りないものは出どころ `goal_gap` の draft にして runtime の planner の draft の経路に渡し、人の判断が要るときだけ inbox に `approve_goal` の ask を開く（[Goal review](docs/design/supervisor-lifecycle/goal-review.md)）。planner の `goal close` は、採らない draft の goal を `abandoned` で閉じるときと人の言葉によるときに残る。人に頼まれれば `up` / `down` / `install` も打つ。planner の権限は goal 55 の段で変えていない（[ADR-t728-1](docs/adr/2026-09-27-t728-1-trust-domains-actors-and-default-deny-capability-authorization.md) 決定 7）: goal の追加・編集・close、draft・submitted・ready の task の変更と `cancel`、自分の proposal の `submit` と取り下げ、note（run にも）・mark、finding の resolve / dismiss、`planner_question` の ask、`up` / `down` / `install` / `init` / `migrate` / `rebind` / `plan`。CLI は planner の `ready`（`--bypass-review` を含む）・`goal ready`・`goal review`・`integrate`・`review`・`recover`・`supervise`・`observe`・`answer`・`ask close`・`finding record`、run に紐づく ask、in_progress 以降の task の変更、他の planner の proposal の取り下げを拒む（表は [Security](docs/design/security.md) と [Authorization](docs/design/authorization.md) の policy、`dagq-planner` skill の Where your authority ends はこれと一致させる）
- 人に届くものはすべて inbox 宛てで、planner に返るのはその planner 自身の proposal への revise と、その planner が作った ask の answer だけ

### observer

- supervisor が `--observe-interval`（既定 3600 秒、0 で無効）ごとと 1 日 1 回（`--observe-daily`、既定 on）、`dagq observe` を子プロセスで起動する。cmux workspace は持たず、`claude -p` を `DAGQ_ROLE=observer` で動かす。supervisor が居ないときは動かない。手で走らせるなら `dagq observe`（`--dry-run` で prompt だけ見る）
- 前回の成功した observation から自分以外の event が無ければ agent を起動せず、`observe_finished`（`outcome: skipped`）だけを残す。起動するときは MCP を読み込まない（`--strict-mcp-config`）
- 入力は `stats --since <cursor>`、`open` / `proposed` の finding（`findings`）、直近 20 件の note、open な ask、graph の candidates と critical。書けるのは finding（`finding record` と `finding resolve`）と、finding に紐づけた `kind: blocked` の ask（`--finding`）だけで、note・goal（draft を含む）・task は書かず、run / task / goal の状態を変えるコマンドは CLI が拒否する。個々の詰まりは解消しない。同じ種類・対象・subject の問題は新しい finding を作らず、既存の finding の回数と根拠（event ID）を更新し、状態が変わらなければ書き直さない
- finding の行き先（[ADR-0044](docs/adr/0044-findings-proposals-from-findings-and-quiet-observer.md) の決定 18〜20、[Finding planners](docs/design/supervisor-lifecycle/finding-planners.md)）: observer が proposal にすべきと判断した finding（`finding record --propose`）と、finding に紐づく `blocked` の ask や `stalled` の ask（finding が無ければ runtime が先に記録する）に人が `propose`（提案にする）と答えたものには、supervisor が runtime の planner を立て、planner が `search` / `related` で既存の task を確かめてから既存の goal への task か新しい goal を `submit ... --finding ID` で plan review に出すか、`finding dismiss` にするか、人の判断が要るものだけ `planner_question` の ask にする。人の承認は一律には求めない。proposal が終われば runtime が finding を `resolved` か `open` に戻す。`dismiss` の answer は runtime が finding に適用する
- 読むのは人も planner・plan review・observer も同じ CLI: `dagq findings`（影響の大きい順、proposal の状態つき）、`dagq events --full` と `--run` / `--task` / `--goal` / `--kind` / `--since` / `--until`、`dagq timeline RUN`（空白の区間と理由）、`dagq observe --history`（1 回ごとの入力の範囲、書いた finding と ask、所要時間、`<queue dir>/observer/<started_at>/` の prompt・入力・出力）。planner は人と `findings` を見て、`submit ... --finding ID` か `finding dismiss ID --reason` で決める（`dagq` skill の `reference/observer.md`）。`blocked` の ask は inbox が人に見せる。導入前の observer が書いた note と draft goal は記録として残る
- KPI（ADR-0051 決定 24〜26）: observer は `dagq kpi` の直近 7 日と 4 週の目標の判定を読み、目標割れ（`breach`）の継続を種類 `kpi`・対象 queue・`subject` `<KPI>/<層>` の finding にする（数字は作らず判定を写す。`blocked` の ask にはしない）。KPI の finding を含め、runtime の planner が finding から出した終わっていない改善の proposal と、open な finding のために立った runtime の planner の数が `[kpi] max_improvement_proposals` に達しているあいだ、supervisor は新しい planner を立てず（`dagq findings` の `improvements`）、plan review の pass が改善の proposal の `high` 以上の task を `normal` に下げる（人の `ready` の answer と `ready --bypass-review` は下げない）
- 詳細は [supervisor-lifecycle の Observer](docs/design/supervisor-lifecycle/observer.md)

### 権限と記録（goal 55）

- 状態を変える全てのコマンドは、呼び出し元の actor（`DAGQ_ROLE` から。無ければ人の `user`、未知の値は拒否）を application の境界で default deny の静的な policy に通してから変更し、状態を変える event は actor（role と id）を記録する。4 つの headless job は状態を変えるコマンドを何も打てず（verdict はデータとして supervisor が適用する）、observer は finding と finding に紐づく `blocked` の ask だけを書く。着地と push は信頼する Integrator だけが行い、review の pass・supervisor・人と inbox の `integrate` は依頼を出す（[ADR-t728-2](docs/adr/2026-09-27-t728-2-landing-only-by-the-trusted-integrator.md)）。role ごとの表は [Security](docs/design/security.md)
- host 実行の判定は助言的（advisory）で、sandbox でも隔離でもない（[ADR-t728-1](docs/adr/2026-09-27-t728-1-trust-domains-actors-and-default-deny-capability-authorization.md) 決定 6。env の偽装や DB の直接操作で迂回できる）。`status` と `doctor` の `actors` が `backend: host`・`enforcement: advisory` を出す。どの session も拒否を迂回しない。隔離（Podman）と queue service は draft の goal 38 が扱う

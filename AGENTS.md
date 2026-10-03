# AGENTS.md

dagq は cmux と Git worktree で依存関係付きの開発タスクを実行する Rust runtime。文書の分類は [docs/README.md](docs/README.md)。この repository 自身の開発タスクも dagq で流す（ドッグフーディング）。

CLI の使い方（登録・起動・監視・レビューと着地・復旧）は plugin の skill が持つ。この文書はこの repository でだけ必要な注意を書く。

## セッション開始時に読む

役割と変更の範囲に合うものだけを読む。タスクの一覧・状態・依存・run 履歴はキューだけが持つ（文書に写さない）。

- worker: runtime の prompt が名指すものと、この文書の「worker」の節。`dagq list` / `dagq show` は打たない（prompt の指示）
- planner・inbox・人: `dagq list` と担当タスクの `dagq show ID`、[docs/plans/current.md](docs/plans/current.md) の現在のステップと完了条件。task を登録するなら [task-registration.md](docs/development/task-registration.md)
- plan review: 下の「plan review」の節
- 変更の範囲ごと: 触る範囲の `docs/design/*.md`。文書（ADR・design・plans）を書くなら [docs/development/documents.md](docs/development/documents.md)

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

test と task の登録の規則は次の開発文書が持つ（判断と境界の test の分け方の方針だけはこの節の項が持つ）。役割と変更の範囲に合うものだけを読む。経緯は [docs/plans/local-checks-history.md](docs/plans/local-checks-history.md) と ADR が持つ。

- [docs/development/task-registration.md](docs/development/task-registration.md): `--verify`・`--paths`・`--evidence`・`--change` の推奨の組み合わせと、plan review が当てはめる規則。task を登録・修正する planner と、plan review job が読む
- [docs/development/testing.md](docs/development/testing.md): coverage の関門、test の置き場所・書き方・ファイルの行数・待ちの上限、e2e とその印、手動スモーク。test（`tests/`・`crates/*/tests/`・`#[cfg(test)]`）や `.config/e2e-quarantine.toml` を変える worker と、それを登録する planner が読む
- 判断は unit test、境界は integration test（[ADR-t1410-1](docs/adr/2026-10-03-t1410-1-decisions-in-unit-tests-boundaries-in-integration-tests.md)）: runtime の task の worker と planner が守る。状態の判断（状態の遷移・回数と上限・時刻を値で受けた時間の判定・verdict や answer から操作への対応・ask や error の文面・次の一手の選び方）は `src/` の副作用のない関数にして `#[cfg(test)]` の unit test で確かめる。unit test は外部プロセス・git・SQLite のファイル・sleep・実時間の時計を使わない（時刻は値で渡す）。`tests/it` は SQLite・Git・プロセス・supervisor の配線・復旧と adopt・cmux の境界を代表の 1 case で確かめ、判断の case ごとに fixture と supervisor を起動し直さない。e2e は実バイナリ・実 Git・実 cmux のハッピーパスと境界だけにする（流し方は testing.md の「e2e」）。integration test を減らすときは確かめていた中身を unit test か残す integration test に対応づけ、行き先の無いまま消さない。test の置き場所・行数・待ちの上限は testing.md のまま
- [docs/development/migrations.md](docs/development/migrations.md): migration の足し方・番号・リリース済みの migration の不変。migration を足す worker と planner が読む

## 文書のルール

ADR・design・plans・frontmatter・commit の規則（判断の記録の置き場、ADR の ID と形・4 桁の番号の扱いと衝突したときの例外・置き換えと amends・design の `updated` / `last_verified` と日付だけの差分を作らないこと、権限の表を写す文書）は [docs/development/documents.md](docs/development/documents.md) が持つ。文書を書く・変える worker と人が読む。ADR を書く task の登録と plan review の確かめ方は [task-registration.md](docs/development/task-registration.md) の「ADRを書くtask」「plan reviewが当てはめる規則」が持つ。

## タスクを閉じるとき

タスクの完了はキューが持つ（仕組みは [integrate](docs/design/supervisor-lifecycle/integrate.md)）。

## plan review

runtime の plan review の prompt が名指す「plan review の部分」。この repository の plan review job は [docs/development/task-registration.md](docs/development/task-registration.md) の「plan reviewが当てはめる規則」を読んで当てはめる（読む文書もそこが名指す）。

## コミット

commit メッセージの規則は [docs/development/documents.md](docs/development/documents.md) の「commit」。run branch への commit・`dagq integrate` だけが行う着地と push は worker の prompt と [integrate](docs/design/supervisor-lifecycle/integrate.md)、`push_failed` の手当ては plugin の `dagq-recover` skill の `reference/review-by-hand.md` が持つ。

## 役割: supervisor と worker と planner と inbox と observer

5 つの役割の定義は [overview の用語集](docs/design/overview.md)、actor と権限は [roles](docs/design/supervisor-lifecycle/roles.md)・[Security](docs/design/security.md)・[Authorization](docs/design/authorization.md)、verification を `integrate` の 1 回にすることは [Validation](docs/design/supervisor-lifecycle/validation.md) と [integrate](docs/design/supervisor-lifecycle/integrate.md) が持つ。

### 起動と停止（`up` / `down`）

- 人・inbox・planner が `up` / `down` / `install` を打つ前に、[docs/development/operations.md](docs/development/operations.md) の「`up`のコマンド」を読む（この repository の `up` のコマンドと、付ける flag・付けない flag の規則とその正本への案内）
- `up` / `down` の汎用の操作と出力は plugin の `dagq-recover` skill の `reference/up-down.md`、自動更新とその ask は同じ skill の `reference/update.md`、仕組みは [up / down](docs/design/supervisor-lifecycle/up-down.md) と [Auto-update](docs/design/supervisor-lifecycle/auto-update.md)、workspace の識別と名前は [run-workspaces](docs/design/supervisor-lifecycle/run-workspaces.md) と [naming](docs/design/supervisor-lifecycle/naming.md) が持つ。並列数と `runtime_planners` は `dagq.toml` の `[supervisor]`（値と理由はそのコメント）

### 着地と人の判断

- 着手と着地の報告と、人の判断が要るときに ask にして待つ規則は [docs/development/operations.md](docs/development/operations.md) の「人への報告」が持つ
- 着地・review・復旧 job・イレギュラーの 3 つの層・ask の理由・待ちの仕組みは `docs/design/supervisor-lifecycle/` の [review](docs/design/supervisor-lifecycle/review.md)・[triage](docs/design/supervisor-lifecycle/triage.md)・[background-recovery-job](docs/design/supervisor-lifecycle/background-recovery-job.md)・[ask](docs/design/supervisor-lifecycle/ask.md)・[waiting](docs/design/supervisor-lifecycle/waiting.md) が持つ。人と inbox が手で行うもの（review by hand・triage by hand・verify を直してからの `retry_inherit`・`push_failed` など）と、runtime と復旧 job が直すものを手で行わないことは、plugin の `dagq-inbox` skill と `dagq-recover` skill（とその reference）が持つ
- worker の provider と経路、フォールバック、`queue_hold` は plugin の dagq skill の `reference/provider.md` と [provider-lifecycle](docs/design/provider-lifecycle.md) が持つ

### worker

runtime の worker の prompt が名指す「worker の部分」。作業の場所（worktree）・merge と push・background の処理・signal の送り方・ask の打ち方と `--topic`・follow_ups の `category`・receipt の後の振る舞いは prompt が持つ。この repository の規則は次を読む。

- 変更後の手元の検証（fmt・clippy・test の範囲・stress・e2e を流さないこと・resume での再現・cargo-nextest が無いとき）、receipt の前の受け入れ条件の対応づけの根拠の書き方、ask にせず自分で決めるもの（ADR や migration の番号の衝突など）: [docs/development/local-checks.md](docs/development/local-checks.md)。receipt を書く前に読む
- receipt の前の文書の照合（どの task でも）と、文書（ADR・design・plans）と commit の規則: [docs/development/documents.md](docs/development/documents.md)
- test を書く・置く: [testing.md](docs/development/testing.md)、migration を足す: [migrations.md](docs/development/migrations.md)、実バイナリや実 queue での確認が要ると思ったら: [operations.md](docs/development/operations.md) の「workerがhostと実queueでできないこと」
- `dagq` の拒否（`no_use_case`・`authorization_denied`）の扱い: plugin の dagq skill の `reference/authority.md`（仕組みは [Queue service](docs/design/queue-service.md#クライアントモード) と [Security](docs/design/security.md)）

### inbox と planner

inbox は plugin の `dagq-inbox` skill、planner は `dagq-planner` skill に従う（人に上げるもの・自分では判断しないもの・goal の close・権限の範囲は各 skill の本文と「Where your authority ends」）。起動と prompt は [session-prompts](docs/design/supervisor-lifecycle/session-prompts.md)、goal review は [Goal review](docs/design/supervisor-lifecycle/goal-review.md)、権限の表は [Security](docs/design/security.md) と [Authorization](docs/design/authorization.md) が持つ。

### observer

observer の起動・入力・書けるもの・finding の行き先・KPI の finding は [supervisor-lifecycle の Observer](docs/design/supervisor-lifecycle/observer.md) と [Finding planners](docs/design/supervisor-lifecycle/finding-planners.md)、読む CLI は plugin の dagq skill の `reference/observer.md` が持つ。

### 権限と記録

actor と default deny の policy、event の actor、Integrator の着地と push、host の判定が advisory であることと拒否の扱いは [Security](docs/design/security.md) と [Authorization](docs/design/authorization.md)、操作の側は plugin の dagq skill の `reference/authority.md` が持つ。

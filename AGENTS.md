# AGENTS.md

## 概要

dagq は cmux と Git worktree で依存関係付きの開発タスクを実行する Rust runtime。この repository 自身の開発タスクも dagq で流す。文書の分類は [docs/README.md](docs/README.md)。

この文書は、この repository の開発に要る短い案内だけを持つ。CLI の使い方は plugin の skill、規則の本文は `docs/development/`、実装は `docs/design/`、経緯は ADR、測定は `docs/plans/`、今の設定値は `dagq.toml` が持つ（[ADR-t1453-2](docs/adr/2026-10-03-t1453-2-ownership-of-agents-md-plugin-development-docs-and-config.md)）。タスクの一覧・状態・依存・run 履歴・完了はキューだけが持つ（文書に写さない）。

## 本番 queue と開発環境の境界

当たる session・理由・運用は [operations.md](docs/development/operations.md) が持つ。

- 本番 queue（この repository の queue DB）の登録と、状態を変えるコマンド・`migrate`・`up` / `down` / `install` は、どの session でも固定バイナリ `~/.local/bin/dagq` で打つ。`target/` の開発中のバイナリで本番の状態と schema を変えない。状態を変えないコマンド（`status`・`show`・`list`・`events`・`doctor` など）は読み取り専用で開くので、開発中のバイナリで本番を読むのはよい
- 本番 queue の DB と `~/.local/bin/dagq` を作業成果で置き換えない。固定バイナリの入れ替えは `dagq install` か `up --auto-update` の自動更新だけで行う（手順は `dagq-recover` skill の `reference/update.md`）。DB は手で直さず（例外は無い）、CLI の外で状態を持たない
- 新しいビルドの確認とスモークは使い捨ての queue で、人か inbox が行う。worker は使い捨ての queue も操作できず、host にツールを入れない（実バイナリや実 queue での確認が要ると思ったら operations.md の「workerがhostと実queueでできないこと」）

## 開始時の短い制約

- session 開始時に `which dagq` が `~/.local/bin/dagq` に解決することを確かめ、コマンドは repository の中（どの worktree でもよい）で打つ
- planner・inbox・人は `dagq list` と担当の `dagq show ID`、[docs/plans/current.md](docs/plans/current.md) の現在のステップと完了条件を読む（worker は読まない）
- 人・inbox は `install`・`up` / `down`、host のツール（sccache・`d2`・TALA）、push と secret に、人・inbox・planner は `dagq.toml` と KPI の印に触る前に operations.md の該当の節（`up` は「`up`のコマンド」）を読む（planner は `up` / `down` / `install` を打たない）
- inbox と planner は cmux を直接打たない（settings の `permissions.deny` の `Bash(cmux:*)`、[ADR-t1228-2](docs/adr/2026-10-02-t1228-2-deny-raw-cmux-to-inbox-and-planner-as-a-guardrail.md)）
- AGENTS.md に規則の本文を足さず、正本に書いてここには案内だけを置く。大きさの上限は `scripts/check-agents-md-size.sh` と CI が検査する（値と決め方は [documents.md](docs/development/documents.md) の「AGENTS.md」）

## 役割と変更の範囲ごとの読む案内

役割と変更の範囲に合うものだけを読む。役割の定義は [overview の用語集](docs/design/overview.md)、actor と権限は [roles](docs/design/supervisor-lifecycle/roles.md)・[Security](docs/design/security.md)・[Authorization](docs/design/authorization.md)、検証を `integrate` の 1 回にすることは [Validation](docs/design/supervisor-lifecycle/validation.md) と [integrate](docs/design/supervisor-lifecycle/integrate.md) が持つ。

### worker

runtime の worker の prompt が名指す「worker の部分」。作業の場所・merge・push・ask・receipt の後は prompt が持つ。`dagq list` / `dagq show` は打たない。

- receipt の前: [local-checks.md](docs/development/local-checks.md)（手元の検証・test の範囲・stress・e2e を流さないこと・受け入れ条件の対応づけ・ask にしないもの）と [documents.md](docs/development/documents.md)（文書の照合・commit）
- 変更の範囲に応じて下の「変更の範囲ごと」
- `dagq` の拒否（`no_use_case`・`authorization_denied`）: dagq skill の `reference/authority.md`（仕組みは [Queue service](docs/design/queue-service.md#クライアントモード)）

### planner

planner は runtime だけが立て、`dagq-planner` skill に従う。基本方針は skill の「Basic policy (ADR-t451-1)」: 推奨が出せる判断は自分で決めて進め、理由を note か context に残す。人に上げるのは人が要る理由に当たり材料で決めきれないものと確信度 low のものだけ（条件と権限は本文と「Where your authority ends」）。人が開く planner は廃止し、`dagq plan` は拒む（[ADR-t1394-1](docs/adr/2026-10-03-t1394-1-abolish-person-planners-and-route-planning-through-inbox-requests.md)）。task の `--verify`・`--paths`・`--evidence`・`--change` の取り方は [task-registration.md](docs/development/task-registration.md)（ADR を書く task は「ADRを書くtask」）。runtime・test・migration を変える task は「変更の範囲ごと」も読む。

### plan review

runtime の plan review の prompt が名指す「plan review の部分」。[task-registration.md](docs/development/task-registration.md) の「plan reviewが当てはめる規則」を当てはめる（読む文書もそこが名指す）。follow-up の所属の検査は ADR-t1504-1 と dagq skill の `reference/register.md`。

### review

run の review は差分の変更の範囲の規則で判定する。この repository の review の subagent は `.dagq/review-agents/` に定義し、`dagq.toml` の `[review.subagents.<agent>]` が path ごとに有効にする（仕組みは [review](docs/design/supervisor-lifecycle/review.md) の「reviewのsubagent」）。

### inbox と人

inbox は `dagq-inbox` skill に従い、復旧と手での操作は `dagq-recover` skill（inbox を開き直す手順は `reference/up-down.md` の「Open the inbox again」）。人が頼んだ計画は inbox が `dagq request add` で runtime の planner に移譲する（手順は `dagq-inbox` skill の `reference/requests.md`、仕組みは [plan-planners](docs/design/supervisor-lifecycle/plan-planners.md) の「inboxからの計画の依頼」）。着手と着地の報告と ask にして待つ規則は operations.md の「人への報告」。着地・review・triage・復旧 job・ask・待ちの仕組みは [review](docs/design/supervisor-lifecycle/review.md)・[triage](docs/design/supervisor-lifecycle/triage.md)・[background-recovery-job](docs/design/supervisor-lifecycle/background-recovery-job.md)・[ask](docs/design/supervisor-lifecycle/ask.md)・[waiting](docs/design/supervisor-lifecycle/waiting.md)、provider は dagq skill の `reference/provider.md` と [provider-lifecycle](docs/design/provider-lifecycle.md)。inbox と planner の起動と prompt は [session-prompts](docs/design/supervisor-lifecycle/session-prompts.md)、goal の close は [Goal review](docs/design/supervisor-lifecycle/goal-review.md)。background の run と planner の出力は `run log` / `planner log`（`dagq-inbox` skill の `reference/status.md` の「Watching a background session」）。人が dagq を通さず直接変えたときは local-checks.md の「人の手元の検証」。

### observer

[Observer](docs/design/supervisor-lifecycle/observer.md)・[Finding planners](docs/design/supervisor-lifecycle/finding-planners.md)、読む CLI は dagq skill の `reference/observer.md`。

### 変更の範囲ごと

- runtime（`src/`・`crates/`）: 触る範囲の `docs/design/*.md`。判断は unit test、境界は integration test（[testing.md](docs/development/testing.md) の「判断と境界のtest」）
- tests（`tests/`・`#[cfg(test)]`・`.config/e2e-quarantine.toml`）: [testing.md](docs/development/testing.md)（macOS に固有の test は「macOSに固有のtest」）
- migrations: [migrations.md](docs/development/migrations.md)
- docs（ADR・design・plans）: [documents.md](docs/development/documents.md)
- plugin: documents.md の「pluginの汎用性」「権限の表を写す文書」
- config（`dagq.toml`・`.config/`・`.dagq/`）: operations.md の「`dagq.toml`を変えるとき」「`[run.env]`とtestの並列度の置き場」、[Run environment](docs/design/supervisor-lifecycle/run-environment.md)
- 非対話の wrapper は `dagq.toml` の `[headless]` で background（出力は `run log`）

## 検証と文書の規則

- 手元の検証: [local-checks.md](docs/development/local-checks.md)
- task の登録: [task-registration.md](docs/development/task-registration.md)（test・migration は上）
- 文書（ADR・design・plans・frontmatter）と commit メッセージ: [documents.md](docs/development/documents.md)。着地と push は `dagq integrate` だけが行う（[integrate](docs/design/supervisor-lifecycle/integrate.md)）

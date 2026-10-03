---
id: development-operations
type: development
title: このrepositoryの本番queueの運用（固定バイナリ・使い捨てのqueue・hostのツール・dagq.toml・upのコマンド・KPIの印・secret・人への報告）
status: current
created: 2026-10-03
updated: 2026-10-03
owners:
  - hisamekms
tags:
  - operations
  - conventions
related:
  - adr-t1453-2
  - plan-operation-rules-history
---

# このrepositoryの本番queueの運用

このrepository（dagq自身）の本番queueを動かし、固定バイナリ・`dagq.toml`・hostのツールに触るときの今の規則。読むのは、人・inbox・plannerが`up` / `down` / `install`、`dagq.toml`、hostのツール、KPIの印、pushに触る前と、workerが実バイナリや実queueでの確認が要ると思ったとき（[workerがhostと実queueでできないこと](#workerがhostと実queueでできないこと)だけ）。

本番queueと開発環境の境界の短い規則はAGENTS.mdの「作業中」だけが持ち、この文書は繰り返さない。この文書が持つのは、その規則が当たるsession・理由・実装への参照と、AGENTS.mdに無いこのrepositoryの運用の規則。dagqの汎用の操作（どのrepositoryにも当たる手順と、その理由）はpluginのskillとreference、実装の姿は`docs/design/`、経緯は[運用の規則の経緯](../plans/operation-rules-history.md)とADR、今の設定値は`dagq.toml`（値の理由はそのコメント）が持ち、ここではそれを指すだけにする。

## 本番queueと固定バイナリ

AGENTS.mdの「作業中」の固定バイナリ・開発中のバイナリ・入れ替え・DBの規則について:

- 当たるsession: supervisor・inbox・planner（人が開いたものもruntimeが立てたものも）のどれにも当たる。開発中のバイナリ（`target/debug`・`target/release`）を本番に使わない理由は、commitされていない変更や未着地のmigrationを含みうるため。
- 開発中のバイナリで本番を読める理由（実装）は[ADR-0073](../adr/0073-kind-additions-are-compatible.md)決定5・7・18と[Persistence](../design/persistence.md)の「Database setup and migrations」、queueがどのdirectoryから解決されるか（repositoryの中で打つ理由）はpluginの`dagq`の`reference/locate.md`が持つ。
- 入れ替えの決定は[ADR-0073](../adr/0073-kind-additions-are-compatible.md)決定10〜17。手順と、人に伝えてから入れ替えること（`--skip-e2e`・`--rollback`・`--allow-breaking`・`cp`で上書きしないことを含む）はpluginの`dagq-recover`の`reference/update.md`、仕組みは[install](../design/supervisor-lifecycle/install.md)と[Auto-update](../design/supervisor-lifecycle/auto-update.md)が持つ。
- repositoryを移動したときの束縛の付け替えは`rebind`で行う（[ADR-0020](../adr/0020-rebind-queue-to-a-moved-repository.md)。手順はpluginの`dagq`の`reference/locate.md`）。

## 使い捨てのqueue

- 使い捨てのrepositoryのディレクトリ名（人かinboxが手で作るものは`dagq-smoke`、e2eのfixtureは`dagq-e2e`）と、確かめる手順と、スモークの後にworkspace groupを`cmux workspace-group delete <group> --close-workspaces`でanchorごと消す手順は[manual-smoke](../design/manual-smoke.md)が持つ。workerが使い捨てのqueueを操作できない理由は次の節。

## workerがhostと実queueでできないこと

- workerが使い捨てのqueueも操作できない（AGENTS.md）理由は、workerに`queue.admin`が無いこと（[ADR-t728-1](../adr/2026-09-27-t728-1-trust-domains-actors-and-default-deny-capability-authorization.md)、[Authorization](../design/authorization.md)）。拒まれ方（クライアントモードの`no_use_case`・`queue_named`）は[Queue service](../design/queue-service.md)の「クライアントモード」、拒否を迂回しない規則はpluginの`dagq`の`reference/authority.md`が持つ。
- workerが実バイナリでの振る舞いを確かめたいときは、integration test（`tests/it`のfixture）かe2eに書く。実queueでの手の確認（`doctor`・`stats`・`kpi`を打って見るなど）が要るものは、receiptの`follow_ups`（`ops`）にして人かinboxに任せる。
- workerがhostに入れないツール（AGENTS.md）は、sccache・cargo-nextest・`d2`・TALAなど、次の節のものすべて。

## hostのツール

hostのツールは人が入れ、miseのshimへのlinkを`~/.local/bin`に置く（ツールを更新しても同じpathで解決できるように）。

- sccache: 入れ方は`dagq.toml`の`[run.env]`のコメント（[ADR-0049](../adr/0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)決定7）、見つからないときの止まり方は[Run environment](../design/supervisor-lifecycle/run-environment.md)の「`[run.env]`が名指すプログラムの検査」と`dagq doctor`の`run_env`が持つ。
- cargo-nextest: 人が`mise use -g cargo:cargo-nextest`で入れ、`ln -s ~/.local/share/mise/shims/cargo-nextest ~/.local/bin/cargo-nextest`でmiseのshimへのlinkを置く（[ADR-0076](../adr/0076-run-the-coverage-gate-tests-with-nextest.md)決定3）。runtimeは事前に検査しないので、無いhostでは`cargo llvm-cov nextest`が`integrate`の検証の失敗として`needs_session`になる（そのhostでのtaskの登録は[taskの登録](task-registration.md)の「coverageの関門」、workerの扱いは[手元の検証](local-checks.md)の「stress」と「hostに触らない」）。
- 当面の依存図の`d2`とTALA（`d2plugin-tala`）: `mise use -g d2 github:terrastruct/TALA`で入れ、`ln -s ~/.local/share/mise/shims/d2 ~/.local/bin/d2`と`ln -s ~/.local/share/mise/shims/d2plugin-tala ~/.local/bin/d2plugin-tala`。supervisorのPATHは`up`の時点で固定されるので、入れた後はsupervisorを起動し直す。無いときの振る舞いは[当面の依存図](../design/supervisor-lifecycle/dependency-diagram.md)と[doctor](../design/supervisor-lifecycle/doctor.md)の`d2`欄。

## `[run.env]`とtestの並列度の置き場

`[run.env]`の今の値とその理由、`[run.env]`に置かないもの（targetを共有しない）とその理由は`dagq.toml`の`[run.env]`のコメント（[ADR-0049](../adr/0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)決定6）、testの並列度を書かない場所は`.config/nextest.toml`の冒頭のコメント、渡し先は[Run environment](../design/supervisor-lifecycle/run-environment.md)、並列度の経緯と測定は[運用の規則の経緯](../plans/operation-rules-history.md)（`CARGO_BUILD_JOBS`）と[nextest-test-threads](../plans/nextest-test-threads.md)（testの並列度）が持つ。

- targetを共有しない理由（並行するrunの`target/debug/dagq`の上書きと、llvm-covのprofrawの消し合い）は、envでbuildとtestの並列度を絞ることには当たらない。
- CIとdagqを通さない`cargo`は`dagq.toml`を読まないので、`[run.env]`の値の影響を受けない。

## `dagq.toml`を変えるとき

- main checkoutの`dagq.toml`は本番queueの全runに効く。変える前に、runtimeが読む場所・効く時点・壊れたときに止まるものを[Run environment](../design/supervisor-lifecycle/run-environment.md)の冒頭と「main checkoutの決め方」で、新しいtableや欄を足すなら旧バイナリとの順序をそのtableの項で確かめる。
- 並列数と`runtime_planners`を変えるときは、`dagq.toml`の`[supervisor]`と`[run.env]`のコメント（値どうしの関係）と、[Run environment](../design/supervisor-lifecycle/run-environment.md)の`[supervisor]`の項（効く時点と`status`での確かめ方）を読む。

## `up`のコマンド

このrepositoryの`up`は次のとおり。当面はin-cmux modeで運用する（cmuxのsocket passwordを設定していないため。[ADR-0011](../adr/0011-cmux-socket-password-and-in-cmux-fallback.md)）。`up`を打つsession、止まったsupervisorの起動し直し方、launchd modeが止まるときの振る舞い、出力の読み方はpluginの`dagq-recover`の`reference/up-down.md`が持つ。

```sh
dagq up --in-cmux --claude ~/.local/bin/claude --codex ~/.local/bin/codex --plugin-dir <この repository>/plugins/claude-dagq --auto-update
```

- `--parallel`を付けない規則とその理由は`dagq.toml`の`[supervisor]`の`parallel`のコメントが持つ。`--runtime-planners`も同じ理由で付けない（`runtime_planners`は`[supervisor]`で決める）。flagが`[supervisor]`より優先して残る仕組みと、付けて起動したsupervisorを`[supervisor]`に従わせる手順は[Run environment](../design/supervisor-lifecycle/run-environment.md)の`[supervisor]`の項（「優先順」と「起動し直すときの引き継ぎ」）。
- このrepositoryは自動更新を使う（上のコマンドの`--auto-update`）。打ち直す`up`にも付け続けることとその理由はpluginの`dagq-recover`の`reference/update.md`の「Automatic updates (`up --auto-update`)」が持つ。
- `--claude`と`--codex`の値は上のコマンドのpath。pathで渡す理由、Claude CodeやCodexを更新したときの解決し直し、`codex`が無いときの振る舞いはpluginの`dagq-recover`の`reference/up-down.md`の「up」、`install --allow-breaking`のdrainが起動し直す`up`に渡すものは同じskillの`reference/update.md`の「install」が持つ。

## KPIの読み方と印

KPI・印・レポートの使い方（設定・運用・hostを変えたときの印の打ち方と`kpi --compare`、runtimeが自分で印にするもの）はpluginの`dagq`の`reference/kpi.md`、仕組みは[kpi](../design/supervisor-lifecycle/kpi.md)・[marks](../design/supervisor-lifecycle/marks.md)・[report](../design/supervisor-lifecycle/report.md)が持つ。このrepositoryの作業時間の前後比較は`kpi --compare`を`--area runtime`（か`--change <値>`）で読み、`all`で読まない（層で読む汎用の理由はpluginの`dagq`の`reference/kpi.md`の「Raising throughput: the weekly review」。changeの値は[taskの登録](task-registration.md)の「change」）。

## secretと外部送信

- レポート（queue dirの`reports/`）は外部に送らない。
- pushの`[push]`の置き場と、ntfyのtopic・SlackのwebhookのURL・tokenなどのsecretの置き場（repositoryの外）は[push](../design/supervisor-lifecycle/push.md)の「設定（host.tomlの`[push]`）」「設定例」とpluginの`dagq`の`reference/kpi.md`の「Push to a person away from the screen」が持つ。

## 人への報告

- 着手と着地は人に報告する。
- 人の判断が要るとき（受け入れ条件の変更、固定バイナリの更新、DBに触らずに解消できない詰まり）はaskにして待つ。

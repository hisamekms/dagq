---
id: adr-t1340-1
type: adr
title: Claude の worker の既定の経路を非対話にし、対話の経路は task ごとに選んだときだけ使う（ADR-t813-1 決定 7 を amends）
status: superseded
created: 2026-10-02
updated: 2026-10-02
accepted_on: 2026-10-02
superseded_by: adr-t1433-2
superseded_on: 2026-10-03
amends:
  - adr-t813-1 decision 7
owners:
  - hisamekms
tags:
  - runtime
  - provider
  - worker
related:
  - adr-t813-1
  - adr-t813-2
  - adr-t1091-1
  - adr-0073
  - plan-headless-worker-measurement
---

# ADR-t1340-1: Claude の worker の既定の経路を非対話にし、対話の経路は task ごとに選んだときだけ使う（ADR-t813-1 決定 7 を amends）

> **置き換え済み（2026-10-03）**: このADRの決定は現在有効ではない。現行の決定は[ADR-t1433-2](2026-10-03-t1433-2-abolish-the-interactive-route.md)を読む。

## Context

[ADR-t813-1](2026-09-28-t813-1-headless-worker-path.md) 決定 7 は、Claude の run は既定で対話の経路を使い、非対話の経路は選んだときだけ使うとし、既定を非対話に切り替えるかは両経路の測定の後に別の ADR で決めるとした。

goal 57 の測定（[headless-worker-measurement](../plans/headless-worker-measurement.md)、task 1132）は、条件付きで Claude の既定を非対話に切り替えることを推奨した。条件は (1) task 1179 の着地、(2) worker の質問の answer の turn・`needs_session` の resume・L の task・`[e2e] paths` に触れる task が非対話の Claude で本番を通ること、(3) cost の記録の 3 つである。

本番では、task 1132 が測った 7 本（1097・1096・1042・1058・1110・1107・1050）と、ask 242 で人が `--headless` を付けた 6 本（1021・1053・1098・1119・1129・1197）が、どれも Claude の非対話の経路で turn が成功して着地した。同じ層の対話の run より work が短く、worker の token が少なかった。`[e2e] paths` に触れる run（1098・1021・1197）も runtime の e2e を通った。条件 1 と条件 3 は着地した。

条件 2 のうち、`worker_question` の answer を載せた turn と `needs_session` の resume は、非対話の Claude の run ではまだ本番で起きていない（Claude から Codex にフォールバックした非対話の run では通っている）。2026-10-02 に人は planner の session で、これを待たずに今の材料で既定を切り替えると決めた（goal 85）。残りの確認は task 1200 が続ける。

## Decision

1. **Claude の worker の既定の経路を非対話にする（ADR-t813-1 決定 7 を amends）。** 経路を指定しない Claude の task は非対話の経路で動く。対話の経路は、task ごとに選んだ（`--interactive`）ときだけ使う。対話の経路とその決定（ADR-0027・ADR-0071・ADR-0047 の画面・idle・`/exit`・ダイアログの規則）は、対話を選んだ run にそのまま効く。Codex の run は今までどおり非対話の経路だけを使う。
   - 既定は runtime の既定として変え、`dagq.toml` の欄にはしない。古い固定バイナリが読めない欄を足す段を作らないため。
   - 既定と明示した経路は保存の上で区別する。経路を指定しない task は既定に従い、のちに既定が変わればそれにも従う。人が選んだ経路（`--interactive` / `--headless`）は既定が変わっても変わらない。
   - まだ claim されていない登録済みの task のうち、経路を指定せずに登録されて古い既定（対話）を保存していたものは、新しい既定に従わせる。人の急ぎ（goal 85 の優先 interrupt）に合わせ、今の backlog にも既定の切り替えを効かせるためである。経路を明示した task、claim 済みの task、過去の run の記録は変えない。

保存の形・flag の綴り・migration の扱いは [provider-lifecycle](../design/provider-lifecycle.md) と [非対話の worker](../design/supervisor-lifecycle/headless-worker.md) が書く。

## Alternatives

- **条件 2 がそろうまで待つ**: `worker_question` の answer と `needs_session` の resume は本番で起きる頻度が低く、いつそろうか分からない。同じ流れは Codex の非対話の run で本番を通っており、非対話の Claude で問題が出たら、直す task を作るか、その task に `--interactive` を付けて対処できる。
- **既定を `dagq.toml` の欄にする**: repository ごとに選べるが、古い固定バイナリが知らない欄で起動できなくなる段を作る。既定を変える理由（測定）は repository に依らない。
- **新しく登録する task だけを切り替える**: 経路を指定せずに登録した ready の task（約 170 件）が古い既定のまま残り、既定の切り替えが本番に効くのは backlog が掃けた後になる。

## Consequences

- 非対話の run が増え、画面の判定・idle の印・`/exit`・既知のダイアログの自動修正が効く run は対話を選んだ run だけになる。人は非対話の worker の terminal に打ち込めず、ask の answer か `dagq-recover` で手を入れる（ADR-t813-1 決定 4）。
- 古い固定バイナリは経路を保存しない task を対話の Claude として読む。入れ替えの間に古いバイナリが claim すれば対話で動くだけで、壊れない。
- 登録済みの task の保存を既定に戻す migration は、値を `NULL` にするだけの `UPDATE` で、古いバイナリが読める形のまま互換として宣言する（[ADR-0073](0073-kind-additions-are-compatible.md) 決定 6 の「古いバイナリが読み書きできる変更だけを互換にする」に当てはめる）。
- 非対話の Claude で `worker_question` の answer の turn か `needs_session` の resume に問題が見つかれば、task 1200 の記録と planner が直す task にする。

---
description: このrepositoryのdesign文書とfrontmatterの対応を検査する
tools: [read, grep, glob]
---

- A-153・A-261: 変更した挙動に対応するdesign文書の適合性と、内容を変える必要がない文書に差分を作っていないこと — [docs/development/documents.md「design」](../../../docs/development/documents.md#design)。
- A-260: このrepositoryの文書の対応先とsummaryのpath・節の適合性 — [docs/development/documents.md「workerの文書の照合」](../../../docs/development/documents.md#workerの文書の照合)。
- ADR-t1942-2決定1・3: summaryの探した名前が差分の変える名前（コマンド・flag・設定のkey・役割・fileのpath）を覆っているか。足りなければその名前で探し直し、差分と食い違う記述や、変わった流れ・境界・不変条件・コードから読めない約束の書き漏れを`docs_drift`で指摘する — [docs/development/documents.md「workerの文書の照合」](../../../docs/development/documents.md#workerの文書の照合)。
- ADR-t1942-2決定2・4: 変えた名前・欄・既定値がdesignに無いことだけではdriftにしない — [docs/development/documents.md「design」](../../../docs/development/documents.md#design)の「書くもの・書かないもの」。
- ADR-t1942-1・ADR-t1942-2決定4: 差分がdesignに入れたコードの書き写し（eventの欄・flag・既定値と閾値・関数名・test名の列挙、コードを読めば分かる手順）と経緯の混入（task・goal・requestの番号、「以前は」、文字数や件数の変遷）、1文1行の崩れ、大きさの予算の超過を指摘する。指摘するのは差分が入れたものだけで、既存の記述は対象にしない — [docs/development/documents.md「design」](../../../docs/development/documents.md#design)の「書くもの・書かないもの」と「形と予算」。

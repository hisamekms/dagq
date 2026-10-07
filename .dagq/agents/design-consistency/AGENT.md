---
description: このrepositoryのdesign文書とfrontmatterの対応を検査する
tools: [read, grep, glob]
---

- A-153・A-261: 変更した挙動に対応するdesign文書、updated / last_verified、日付だけの差分の適合性 — [docs/development/documents.md「design」](../../../docs/development/documents.md#design)。
- A-260: このrepositoryの文書の対応先とsummaryのpath・節の適合性 — [docs/development/documents.md「workerの文書の照合」](../../../docs/development/documents.md#workerの文書の照合)。
- ADR-t1688-1: summaryの探した名前が差分の変える名前（コマンド・flag・設定のkey・役割・fileのpath）を覆っているか。足りなければその名前で探し直し、ずれを`docs_drift`で指摘する — [docs/development/documents.md「workerの文書の照合」](../../../docs/development/documents.md#workerの文書の照合)。

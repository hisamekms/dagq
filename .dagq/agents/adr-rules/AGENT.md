---
description: このrepositoryのADRと文書の形式を検査する
tools: [read, grep, glob]
---

- A-150・A-151・A-152・A-162・A-163: ADRの適合性 — [docs/development/documents.md「ADR」](../../../docs/development/documents.md#adr)。
- A-155: frontmatterの適合性 — [docs/development/documents.md「frontmatter」](../../../docs/development/documents.md#frontmatter)。
- A-156・A-159・A-160・A-161: ADRのIDの適合性（IDとファイル名の形・IDの重複・名前の日付と`accepted_on`の対応はプログラムのreview（`check-adr-numbers.sh`）が見るので、A-156ではIDのtaskがそのADRを書くtaskか、`proposed`のADRの名前が書いた日かを見る） — [docs/development/documents.md「ADRのID」](../../../docs/development/documents.md#adrのid)。

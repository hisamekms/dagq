---
description: このrepositoryのADRと文書の形式を検査する
tools: [read, grep, glob]
---

- A-150・A-151・A-152・A-162・A-163: ADRの適合性 — [docs/development/documents.md「ADR」](../../../docs/development/documents.md#adr)。
- A-155: frontmatterの適合性 — [docs/development/documents.md「frontmatter」](../../../docs/development/documents.md#frontmatter)。
- A-156・A-159・A-160・A-161: ADRのIDの適合性（IDとファイル名の形・IDの重複・名前の日付と`accepted_on`の対応はプログラムのreview（`check-adr-numbers.sh`）が見るので、A-156ではIDのtaskがそのADRを書くtaskか、`proposed`のADRの名前が書いた日かを見る） — [docs/development/documents.md「ADRのID」](../../../docs/development/documents.md#adrのid)。
- 差分が触っていない既存のADRの本文のリンクとfrontmatterの`related`が削除した文書（`scripts/check-doc-links.sh`のADRの例外が扱う、冒頭の削除した文書の一覧と`docs/journal/`）を指していても、append-onlyなので直さないのが規則で、`follow_ups`にも`reasons`にも入れない — [docs/development/documents.md「ADR」](../../../docs/development/documents.md#adr)のappend-onlyの箇条と、append-onlyのADRがリンクする文書を消すときの箇条。差分が足した・変えたADR（新しいADRを含む）の本文のリンクと`related`が実在しない文書を指すことは、従来どおり指摘する（`related`はA-155、本文のリンクはプログラムのreviewの`check-doc-links`が見る）。

---
id: plan-design-docs-criterion
type: plan
title: designの記載基準を変えた前後の文書量とreviewの比較
status: active
created: 2026-11-20
owners:
  - hisamekms
tags:
  - documentation
  - measurement
related:
  - adr-t5395-1
  - development-documents
---

# designの記載基準を変えた前後の文書量とreviewの比較

[ADR-t5395-1](../adr/2026-11-20-t5395-1-design-docs-hold-what-code-cannot-show.md)の記載基準と、既存文書を減らす変更の前後で、designの総量と文書を理由とする差し戻しを比べる。

## 測定期間と入力

- 前は `[2026-11-12T00:00:00Z, 2026-11-19T00:00:00Z)` の7日間。
- 後は、goal 324の削減の最後の着地時刻をTとして、`[T, T + 7日)`。事後測定はtask 5431で行う。

plannerが各期間の開始と終了を `--since` と `--until` に渡して、次の2種類のeventのJSONを取得する。
種類ごとに最後のeventのIDを `--after` に渡して全ページを読み、開始を含み終了を含まない区間で数える。

```sh
dagq events --full --kind review_finished --since "$since" --until "$until"
dagq events --full --kind plan_review_finished --since "$since" --until "$until"
```

## 判定と数え方

eventのIDごとに1回と数える。
runのreviewは `payload.verdict` が `pass` 以外、plan reviewは `payload.decision` が `pass` 以外（`revise`・`concern`）の回を「passでない回」とする。

理由は `reasons` の添字 `i` ごとに判定する。

- runのreviewは、`reason_codes[i]` が `docs_drift` を含むか、`reason_paths[i]` が空でなく全て `docs/` の下なら文書が理由。そのうち `reason_paths[i]` か `reasons[i]` の本文に `docs/design/` を含むものはdesignが理由。
- plan reviewは、`reason_codes[i]` が `docs_drift`・`stale_adr_reference`・`adr_design_mismatch` のいずれかを含むか、`reasons[i]` の本文に `docs/` か `docs/design/` の文書のファイル名（`README.md` を除く）があれば文書が理由。本文に `docs/design/` か同じファイル名があればdesignが理由。

passでない回に文書が理由の項目が一つでもあれば、文書が理由の回に数える。
designが理由の回も同様に数える。
割合は文書が理由の回数を、それぞれpassでない回数と全回数で割る。
所要時間は文書が理由の回の `duration_secs` を各回につき一度だけ足す。

## 変更前の基準値

2026-11-19にplannerが上の定義で集計した値。

| 期間 | 種類 | 回数 | passでない回数 | 文書が理由の回数 | designが理由の回数 | 文書が理由の割合（÷passでない回数） | 文書が理由の割合（÷全回数） | 文書が理由の回の `duration_secs` の合計 |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 前の7日間 | runのreview | 349 | 131 | 45 | 25 | 34.4% | 12.9% | 8,769秒 |
| 前の7日間 | plan review | 376 | 211 | 119 | 72 | 56.4% | 31.6% | 15,476秒 |

2026-11-16以降の177件中36件と、proposal 2297〜2299の38回・約95分は、集計の定義が異なるためこの比較には使わない。

## designの大きさ

基準のcommit `b3f5f582b` では78文書、本文は988,089 byte、frontmatterを含むと1,021,443 byte。
本文は[文書の規則「検査の境界」](../development/documents.md#検査の境界)のとおり、先頭の閉じたfrontmatterを区切り行ごと除き、fenced code blockと改行を含めて `LC_ALL=C` のbyteで測る。
後も削減の最後の着地のcommitを指定し、同じ方法で測る。

```sh
revision=b3f5f582b
export LC_ALL=C
git ls-tree -r --name-only "$revision" -- docs/design |
  while IFS= read -r path; do
    case "$path" in
      *.md)
        bytes=$(git show "$revision:$path" | wc -c)
        body_bytes=$(
          git show "$revision:$path" |
            awk '
              NR == 1 && $0 == "---" { frontmatter = 1; next }
              frontmatter && $0 == "---" { frontmatter = 0; pending = 0; next }
              frontmatter { pending += length($0) + 1; next }
              { body += length($0) + 1 }
              END { print body + (frontmatter ? pending + 4 : 0) }
            '
        )
        printf '%s %s\n' "$body_bytes" "$bytes"
        ;;
    esac
  done |
  awk '{ body += $1; total += $2 }
       END { printf "%d documents, %d body bytes, %d total bytes\n", NR, body, total }'
```

---
id: design-supervisor-lifecycle-worker-receipt
type: design
title: workerのreceiptの約束
status: current
created: 2026-11-08
scope: runtime
related:
  - design-supervisor-lifecycle-headless-worker
  - adr-t947-3
  - adr-t1504-2
  - adr-t1487-1
  - adr-t1818-1
---

# workerのreceiptの約束

[workerのsession](headless-worker.md)から受け取るreceiptの約束とコードへの入口。

## receiptの欄の約束

receiptの形と受理の検査は`Receipt`のdoc commentと[Validation](validation.md)、workerへの書き方の文面は[Prompt](prompt.md)が持つ。

### follow_upsの分類コード<a id="follow_upsの分類コード"></a>

一覧と定義は`FOLLOW_UP_CATEGORIES`（`src/domain/follow_up.rs`）だけが持ち、workerのpromptはそこから出す（[ADR-t947-3](../../adr/2026-09-28-t947-3-follow-ups-carry-category-codes.md)）。
runのreview（[Review](review.md#差し戻しの分類コード)）と同じ種類の問題には同じ名前を使う。

- 分類の無い要素と一覧に無い値もreceiptの受理を変えず、runtimeは値をそのまま記録する。
- 記録は`integrate`のfollow_upの登録（[integrate](integrate.md#follow_upsの登録)）がdraftの出どころの材料に載せ、runtimeのplannerとの突き合わせは[Draft planners](draft-planners.md#follow_upの種類と判断の集計)。
- 迷ったら、follow_upを片付けたときに何が変わるかで選び、重複や採否はコードにしない（ADR-t947-3決定2）。

### follow_upsの所属の提案<a id="follow_upsの所属の提案"></a>

workerはfollow_upごとに元goalのacceptanceとの関係を提案として書けるが確定はせず、所属の判断を記録するのはruntimeのplannerと人（[ADR-t1504-2](../../adr/2026-10-04-t1504-2-runtime-records-and-enforces-follow-up-membership-judgements.md)決定11、[所属の判断](../follow-up-membership.md)）。

- 欄の意味は`FOLLOW_UP_PROPOSAL`が持ち、欄の無い要素も形の違う値もreceiptの受理を変えない。
- `integrate`の登録は提案を書いたまま材料に載せ、runtimeは提案を判断として扱わず、plannerがそれを起点に判断する（[Draft planners](draft-planners.md#所属の判断)）。

### Spikeの結果<a id="spikeの結果"></a>

実行区分（`domain::execution_class`）が`spike`のtaskのrunは、結果をreceiptの`spike_result`に書き、欄の意味はそのdoc commentが持つ（[ADR-t1487-1](../../adr/2026-10-04-t1487-1-spike-is-an-execution-class-with-a-judged-result-and-a-durable-replanning-request.md)決定3・4）。

- 検査するのはSpikeのrunの欄がそろっていることだけで、根拠が判定を支えるかは判じず、否定的な結果と未解決は成功のreceiptで返す。
- 上限（時間・回数）はagentへの指示で、runtimeは強制しない。
- 欠けは要求evidenceと同じく`needs_session`にしてresumeが書き足させる（[Validation](validation.md)）。

### baseがすでに満たすとき（already_satisfied）<a id="baseがすでに満たすときalready_satisfied"></a>

workerはbaseが受け入れ条件をすでに満たし差分が要らないとき、commitせずにreceiptの`result`を`already_satisfied`にする（[ADR-t1818-1](../../adr/2026-10-05-t1818-1-worker-claims-already-satisfied-and-runtime-closes-on-a-verified-snapshot.md)決定1）。

- 根拠の無い主張とcommitしたrunの主張は受け付けずrunを`failed`にする。
- 根拠は[local-checks](../../development/local-checks.md)の「workerの手元の検証」で許された確かめだけで、receiptの検査はコマンドの中身を判じず、verifyの全体はruntimeがsnapshotで流す（[Validation](validation.md)の「already_satisfiedの検証」）。

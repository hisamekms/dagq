---
id: adr-t2125-1
type: adr
title: resource brokerの撤去に合わせ、e2eの関門はcmuxだけを確かめ（ADR-t1162-1を置き換え、ADR-t2105-1決定2・4とADR-t1582-1決定1をamends）、`dagq.toml`の`[broker]`は受け付けて読まず、古い`broker_*`のeventは知らないkindとして読み、build識別子の規則はdagqが持つ
status: accepted
created: 2026-10-08
updated: 2026-10-08
accepted_on: 2026-10-08
supersedes:
  - adr-t1162-1
amends:
  - adr-t2105-1 decision 2
  - adr-t2105-1 decision 4
  - adr-t1582-1 decision 1
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - testing
  - broker
related:
  - adr-t2113-1
  - adr-t963-1
  - adr-t1162-1
  - adr-t2105-1
  - adr-t1582-1
  - adr-0045
  - adr-0073
  - design-supervisor-lifecycle-validation
  - design-supervisor-lifecycle-auto-update
  - design-supervisor-lifecycle-install
  - design-supervisor-lifecycle-build-identifier
  - design-supervisor-lifecycle-run-environment
  - design-supervisor-lifecycle-events-watch
---

# ADR-t2125-1: resource brokerの撤去に合わせ、e2eの関門はcmuxだけを確かめ、`dagq.toml`の`[broker]`は受け付けて読まず、古い`broker_*`のeventは知らないkindとして読み、build識別子の規則はdagqが持つ

## Context

[ADR-t2113-1](2026-10-08-t2113-1-remove-the-resource-broker.md)はresource brokerを外すと決め、撤去の実装に、brokerのe2eに依るADR（[ADR-t1162-1](2026-09-30-t1162-1-e2e-gate-skips-podman-e2e-only-when-podman-is-unreachable.md)・[ADR-t1582-1](2026-10-04-t1582-1-temporarily-leave-broker-and-cmux-only-e2e-cases-out.md)など）の扱いを任せた。
撤去でdagqのpackageからbrokerのe2e（`broker::`）とpodmanを要るtestが無くなり、関門がpodmanを確かめる対象が無くなる。

ADR-t1162-1は、関門がpodmanの接続を確かめ、繋がらないときだけpodmanに頼るe2eを流さずに残りで判定し（決定1）、流さなかったことを記録してinboxに届け（決定2）、それ以外は[ADR-t963-1](2026-09-29-t963-1-e2e-required-by-diff-and-run-in-full-before-auto-update.md)決定1のままとする（決定3）と決めた。
[ADR-t2105-1](2026-10-08-t2105-1-e2e-gate-skips-cmux-e2e-only-when-cmux-does-not-answer.md)はcmuxに同じ形を当て、決定2でpodmanとcmuxの両方を流さなかったときは両方を並べ、決定4でpodmanの扱いを変えないとした。
ADR-t1582-1決定1は、brokerのe2e 1ケースを担当taskが復帰させるまで登録から外した。

撤去の後も、`[broker]`を書いたままの`dagq.toml`と、`broker_*`のeventを持つqueueが残りうる。
build識別子の規則（[ADR-0045](0045-build-identifier-explicit-migrate-schema-compat-handoff-and-auto-update.md)決定2）は、3つのバイナリが同じbuildを名乗るためにbrokerのprotocolのcrateに置いていた。

## Decision

1. **ADR-t1162-1を丸ごと置き換える。** 決定1・2はpodmanのe2eを流さないことと記録で、対象が無くなる。決定3はADR-t963-1決定1を言い直しただけで、ADR-t963-1決定1がそのまま有効である。
   引き継ぐもの: 関門はpodmanを確かめず、cmuxのほかに「繋がらないので流さない」e2eを持たない。cmuxが答えて流したe2eが落ちれば落ちで、置き換えない（ADR-t963-1決定1のまま）。
2. **ADR-t2105-1決定2・4のpodmanの部分を除く。** 決定2の「podmanとcmuxの両方を流さなかったときは両方を並べる」と、決定4の「podmanの扱い（ADR-t1162-1決定1・2）は変えない」は対象が無くなるので除く。
   cmuxが答えないときだけ実cmuxを要るe2eを流さないこと（決定1）、記録とinboxへの知らせ（決定2）、後始末を次の関門に残すこと（決定3）、socketのpasswordを案内しないこと（決定4）はそのまま。
   決定1が引くADR-t1162-1決定3は、ADR-t963-1決定1と読む。
3. **ADR-t1582-1決定1のbrokerのケースは戻さず消す。** `broker::a_preferred_worker_does_its_task_through_the_broker_and_lands`は、復帰させずにe2eの`broker::`とともに消す。cmuxの3ケースの部分は変えない。
4. **`dagq.toml`の`[broker]`と`[broker.package]`は受け付けて読まない。** 撤去の前に書いた表が残るrepositoryでも`up`・supervisor・`doctor`を止めないため、この2つの見出しは中身ごと読み飛ばす。
   ほかの未知の表は今までどおりerrorにする（読み飛ばすのは撤去した表だけで、綴りの誤りを黙って通さない）。
   `host.toml`の`[broker]`は、ほかの読み手の表と同じく誰も読まない。
5. **`broker_*`のeventのkindはバイナリから除き、古いeventは知らないkindとして読む。** 書くものが無くなり、読み手は知らないkindも文字列のまま読む（[ADR-0073](0073-kind-additions-are-compatible.md)決定21）ので、名前を残さない。
   古い`broker_unhealthy`・`broker_claims_held`はattentionにもclaimの控えにもならない。
6. **build識別子の規則はdagqが持つ。** 規則（ADR-0045決定2）は変えず、置き場をdagq自身のsourceに移し、dagqのbuild scriptがそれを呼ぶ。この識別子を名乗るのはdagqだけになる。

[ADR-t2113-1](2026-10-08-t2113-1-remove-the-resource-broker.md)決定4〜6（後のworkerのcontainerのためのmachineの決定）は変えない。

## Alternatives

- **関門にpodmanの確認を残す**: 確かめる対象のe2eが無く、hostの状態で関門が遅れるだけになる。workerのcontainer化がpodmanを要るe2eを足すときに、その実装が決め直す。
- **`[broker]`も未知の表としてerrorにする**: 撤去の前に書いた表が残るrepositoryで、入れ替えた途端に`dagq.toml`全体が読めずqueueが止まる。
- **未知の表をすべて読み飛ばす**: 綴りの誤りや、まだ読めない新しい表を黙って通し、旧バイナリが未知の表を拒む今の約束と食い違う。
- **`broker_*`のkindの名前を残す**: 書く経路が無い名前をバイナリが持ち続ける。読み手は知らないkindを読めるので、残す利点が無い。

## Consequences

- e2eの関門の`skipped`に出るのはcmuxの理由だけになる。
- `[broker]`を書いた`dagq.toml`は、表を消すまで黙って読み飛ばされる。
- 古い`broker_*`のeventは`events`・`timeline`・`show`・`status`・`watch`に知らないkindとして出る。
- 読み飛ばす表は[Run environment](../design/supervisor-lifecycle/run-environment.md)、古い`broker_*`のeventの読み方は[Events and watch](../design/supervisor-lifecycle/events-watch.md)、build識別子の置き場は[Build identifier](../design/supervisor-lifecycle/build-identifier.md)、関門がcmuxだけを確かめることは[Validation](../design/supervisor-lifecycle/validation.md)とコードのdoc commentが持つ。
